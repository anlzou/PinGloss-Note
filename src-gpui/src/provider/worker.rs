//! worker 那一层：把链路泵与管子搬到一根线程上，屏那边只递意图、只收事件。
//!
//! 分成两样东西各有理由。[`Pump`] 是一台逐拍推进的状态机，毫秒读数由调用方递进来，于是"拨不通只报
//! 一次离线""重连换号""答复到点"这些边界能在测试里精确踩到；[`Worker`] 只负责把这台泵搬到线程上，
//! 加上有界的意图队列、事件回传与一条确定的收尾路径。屏不握句柄，也不越过通道去读线程那一侧的缓存
//! ——一份状态有两个读者就会两头漂。
//!
//! ⚠ 重连**不**由这一层自动发起：管子断了或拨不通，链路落到离线之后，只有 [`Intent::Retry`] 再拨一次。
//! 宿主可能压根没在跑，热重试会把一根拨不通的管子问成后台常驻循环。
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::cache::CacheKey;
use super::link::{Link, LinkEvent};
use super::pipe::Pipe;
use super::session::Command;

/// 门口攒的意图条数上限：到顶把原物交回调用方，而不是线程被拨号堵住时无限堆。
const INTENT_QUEUE: usize = 64;
/// 有事做时一拍的长度。两档等待预算是秒级的，250 ms 足够把线上到齐的行读回来。
const BEAT: Duration = Duration::from_millis(250);
/// 没事做时睡长一点：那一拍不可能产出事件。
const IDLE_BEAT: Duration = Duration::from_secs(2);
const DIAL_BUDGET: Duration = Duration::from_secs(2);
const WRITE_BUDGET: Duration = Duration::from_secs(2);

/// 屏递进线程的一桩事。`Shutdown` 走通道而不是只靠句柄析构，线程才能在预算内把手上的管子放下。
pub(crate) enum Intent {
    Page(CacheKey),
    Command(Command),
    Retry,
    Shutdown,
}

/// 线程上那台状态机：一条链路、至多一根管子、两把拨号与写字节的那把尺。
pub(crate) struct Pump {
    link: Link,
    pipe: Option<Pipe>,
    /// `None`＝按宿主那套拼法现推落点（生产来路）；`Some`＝调用方点名的一根管子。
    endpoint: Option<PathBuf>,
    dial_budget: Duration,
    write_budget: Duration,
}

impl Pump {
    pub(crate) fn new() -> Self {
        Self::at(None, DIAL_BUDGET, WRITE_BUDGET)
    }

    /// 落点与两把尺由调用方点名：判据既不读环境变量，也不必为了等拨号超时睡上两秒。
    pub(crate) fn at(
        endpoint: Option<PathBuf>,
        dial_budget: Duration,
        write_budget: Duration,
    ) -> Self {
        Self {
            link: Link::new(),
            pipe: None,
            endpoint,
            dial_budget,
            write_budget,
        }
    }

    pub(crate) fn link(&self) -> &Link {
        &self.link
    }

    /// 这一拍不可能有任何事：没管子、没问出去的东西、也没排着的命令。线程据此决定睡多久。
    pub(crate) fn idle(&self) -> bool {
        self.pipe.is_none() && self.link.open_waits() == 0 && self.link.queued() == 0
    }

    /// 记下一桩意图要办的事。`Shutdown` 在这儿只是空一事——收尾归线程那一头判。
    pub(crate) fn apply(&mut self, intent: Intent, now_ms: u64) -> Vec<LinkEvent> {
        match intent {
            Intent::Page(key) => self.link.want_page(key, now_ms),
            Intent::Command(command) => self.link.submit(command, now_ms),
            Intent::Retry => self.retry(),
            Intent::Shutdown => Vec::new(),
        }
    }

    /// 走一拍：该拨就拨，吸一次管子，最后收超时。拨不通与管子断了走同一条收账——都是离线。
    pub(crate) fn beat(&mut self, now_ms: u64) -> Vec<LinkEvent> {
        if self.pipe.is_none() && !self.idle() {
            if let Err(reason) = self.dial() {
                return self.link.disconnect(&reason);
            }
        }
        let mut events = Vec::new();
        if let Some(pipe) = self.pipe.as_mut() {
            events.extend(pipe.drive(&mut self.link, now_ms));
        }
        events.extend(self.link.tick(now_ms));
        events
    }

    fn dial(&mut self) -> Result<(), String> {
        let opened = match &self.endpoint {
            Some(path) => Pipe::dial_to(path, self.dial_budget, self.write_budget),
            None => Pipe::dial(self.dial_budget, self.write_budget),
        };
        match opened {
            Ok(pipe) => {
                self.pipe = Some(pipe);
                Ok(())
            }
            Err(reason) => Err(reason),
        }
    }

    /// 屏上"重试"那一枚：上一次问的逐条报回（不留一个永远转圈的槽），换代撤净旧号，再拨一根。
    fn retry(&mut self) -> Vec<LinkEvent> {
        let mut events = self.link.disconnect("重连：上一趟问的作废");
        self.pipe = None;
        self.link.reconnect();
        if let Err(reason) = self.dial() {
            events.extend(self.link.disconnect(&reason));
        }
        events
    }
}

/// 一根 provider 线程的句柄：这头只递意图、只取事件。
pub(crate) struct Worker {
    intents: SyncSender<Intent>,
    events: Receiver<LinkEvent>,
    thread: Option<JoinHandle<()>>,
    running: Arc<AtomicBool>,
}

impl Worker {
    pub(crate) fn start(pump: Pump) -> Result<Self, String> {
        let (intents, incoming) = mpsc::sync_channel(INTENT_QUEUE);
        let (reports, events) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let flag = running.clone();
        let thread = thread::Builder::new()
            .name("nomo-provider".to_string())
            .spawn(move || {
                run(pump, incoming, reports);
                flag.store(false, Ordering::Release);
            })
            .map_err(|err| format!("provider 线程起不来：{err}"))?;
        Ok(Self {
            intents,
            events,
            thread: Some(thread),
            running,
        })
    }

    /// 递一桩意图。排不下就把原物交回，调用方这一拍放弃即可——线程不替屏攒无界的队列。
    pub(crate) fn ask(&self, intent: Intent) -> Result<(), Intent> {
        match self.intents.try_send(intent) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(intent)) => Err(intent),
            // 线程已经收手（多半是 `Shutdown` 之后还没 join）：与排不下同样是这一拍没递上。
            Err(TrySendError::Disconnected(intent)) => Err(intent),
        }
    }

    /// 取走线程报回来的事件，不阻塞：屏每一帧叫一次，攒够再画。
    pub(crate) fn take_events(&self) -> Vec<LinkEvent> {
        self.events.try_iter().collect()
    }

    /// 线程还在不在跑。收尾路径的判据就落在这位上。
    pub(crate) fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// 叫停并在预算内等线程放下管子。`false`＝没等到：线程随后自己收，句柄不留字节。
    pub(crate) fn shutdown(mut self, budget: Duration) -> bool {
        let _ = self.intents.try_send(Intent::Shutdown);
        self.join_until(budget)
    }

    fn join_until(&mut self, budget: Duration) -> bool {
        let Some(thread) = self.thread.take() else {
            return true;
        };
        let deadline = Instant::now() + budget;
        while !thread.is_finished() {
            if Instant::now() >= deadline {
                self.thread = Some(thread);
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        thread.join().is_ok()
    }
}

impl Drop for Worker {
    /// 没走 [`Worker::shutdown`] 也得让线程收手。这里只叫停不等待：析构里阻塞会把窗口关闭拖成一次同步等。
    fn drop(&mut self) {
        let _ = self.intents.try_send(Intent::Shutdown);
    }
}

/// 线程本体：一次把门口攒的意图吃光（滚动一下能问出几十格，逐条走拍会把同一目录问成一串往返），
/// 再走一拍，最后把事件送出去——收不到就代表屏那侧不要了，线程随之收手。
fn run(mut pump: Pump, incoming: Receiver<Intent>, reports: Sender<LinkEvent>) {
    loop {
        let beat = if pump.idle() { IDLE_BEAT } else { BEAT };
        let mut batch = Vec::new();
        match incoming.recv_timeout(beat) {
            Ok(first) => {
                if matches!(first, Intent::Shutdown) {
                    return;
                }
                batch.push(first);
                while let Ok(next) = incoming.try_recv() {
                    if matches!(next, Intent::Shutdown) {
                        return;
                    }
                    batch.push(next);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let now_ms = clock_ms();
        let mut reported = Vec::new();
        for intent in batch {
            reported.extend(pump.apply(intent, now_ms));
        }
        reported.extend(pump.beat(now_ms));
        for event in reported {
            if reports.send(event).is_err() {
                return;
            }
        }
    }
}

/// `pending` 的截止点用的就是这一枚毫秒读数，所以线程踩的边界与判据里算的是同一把尺。
fn clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod offline_tests {
    use super::super::cache::CacheKey;
    use super::*;
    use std::path::PathBuf;

    /// 不存在的那颗落点：unix 是一个没人绑的 socket 文件，windows 是一颗没有实例的管道名。
    fn nowhere() -> PathBuf {
        PathBuf::from(if cfg!(windows) {
            r"\\.\pipe\nomo-provider-nowhere-4e7c"
        } else {
            "/tmp/nomo-provider-nowhere-4e7c.sock"
        })
    }

    /// 拨不通＝一次离线，不是每一拍一次离线。屏上反复闪的"离线"就是这么来的。
    #[test]
    fn a_failed_dial_reports_the_wait_once_and_stays_quiet() {
        let mut pump = Pump::at(
            Some(nowhere()),
            Duration::from_millis(200),
            Duration::from_secs(1),
        );
        assert!(pump.idle(), "刚造出来的泵不该被当成有活在等");
        let key = CacheKey::Roots { cursor: None };
        assert!(pump.apply(Intent::Page(key.clone()), 1).is_empty());
        let events = pump.beat(2);
        assert!(
            events.iter().any(|event| matches!(
                event,
                LinkEvent::PageFailed(asked, _) if *asked == key
            )),
            "拨不通却没把问出去的那一格报回：{events:?}"
        );
        assert!(!pump.link().is_online(), "拨不通还写着在线");
        assert!(pump.idle(), "报完离线该没事再等");
        assert!(pump.beat(3).is_empty(), "同一颗拨不通的落点报到了第二遍");
        assert!(pump.beat(4).is_empty(), "第三遍也一样");
        // 离线之后再来一格：当场报回，既不发帧也不再拨。
        let late = pump.apply(Intent::Page(CacheKey::Recent), 5);
        assert!(matches!(
            late.as_slice(),
            [LinkEvent::PageFailed(CacheKey::Recent, _)]
        ));
        assert!(
            pump.beat(6).is_empty(),
            "新的一格不该把拨不通变成每一拍重拨"
        );

        // 再给同一颗落点绑上宿主：空闲的一拍若偷偷重拨，这里会当场接住第二根管子。
        let mut listener = crate::plugin::transport::Listener::bind(&nowhere())
            .expect("拨不通之后绑得上同一颗落点");
        assert!(pump.beat(7).is_empty());
        assert!(
            listener.accept().expect("旁观连接不该报错").is_none(),
            "离线且空闲之后又拨了一根"
        );
    }

    /// 队列到顶就把原物交回，让调用方这一拍放弃；线程只处理已经收下的意图。
    #[test]
    fn a_full_intent_queue_hands_the_same_intent_back() {
        let (intents, _incoming) = mpsc::sync_channel(INTENT_QUEUE);
        let (_reports, events) = mpsc::channel();
        let worker = Worker {
            intents,
            events,
            thread: None,
            running: Arc::new(AtomicBool::new(false)),
        };
        for _ in 0..INTENT_QUEUE {
            assert!(worker.ask(Intent::Page(CacheKey::Recent)).is_ok());
        }
        let command = Command::Open {
            note_id: 77,
            folder_id: 4,
        };
        assert!(matches!(
            worker.ask(Intent::Command(command.clone())),
            Err(Intent::Command(returned)) if returned == command
        ));
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::thread;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::plugin::transport::{Listener, Stream};
    use crate::provider::pending::READ_TIMEOUT_MS;

    fn endpoint(tag: &str) -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\nomo-provider-worker-{tag}-{}",
            std::process::id()
        ))
    }

    /// 测试用的两把尺：拨号那一格短到能当场看出"落点不存在"，判据也不必睡两秒。
    fn pump_at(name: &PathBuf) -> Pump {
        Pump::at(
            Some(name.clone()),
            Duration::from_millis(300),
            Duration::from_secs(2),
        )
    }

    fn accept(listener: &mut Listener) -> Stream {
        for _ in 0..250 {
            match listener.accept() {
                Ok(Some(stream)) => return stream,
                Ok(None) => thread::sleep(Duration::from_millis(2)),
                Err(err) => panic!("接宿主那一侧失败：{err}"),
            }
        }
        panic!("没有客户端进来");
    }

    fn drain(host: &mut Stream) -> Vec<String> {
        let mut lines = Vec::new();
        for _ in 0..250 {
            if !host.poll().expect("宿主那一侧不该报管道错") {
                break;
            }
            while let Some(line) = host.next_line().expect("收来的该是完整一帧") {
                lines.push(line);
            }
            if !lines.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        lines
    }

    fn id_of(line: &str) -> String {
        serde_json::from_str::<Value>(line).expect("请求行是 JSON")["request_id"]
            .as_str()
            .expect("请求行没带回显号")
            .to_string()
    }

    fn roots() -> CacheKey {
        CacheKey::Roots { cursor: None }
    }

    fn roots_reply(request_id: &str) -> String {
        format!(
            r#"{{"ok":true,"payload":{{"protocol":1,"provider_id":"notes","online":true,"items":[{{"kind":"folder","id":"folder:4","folder_id":4,"name":"工作记录","locked":false}}],"next_cursor":null}},"request_id":"{request_id}"}}"#
        )
    }

    /// 意图落在泵上就该变成线上一帧：先问才拨号，滚动时不空拨。
    #[test]
    fn a_page_intent_dials_and_asks_once_on_the_wire() {
        let name = endpoint("ask-once");
        let mut listener = Listener::bind(&name).expect("绑不上测试管子");
        let mut pump = pump_at(&name);
        assert!(pump.apply(Intent::Page(roots()), 1).is_empty());
        assert!(pump.beat(2).is_empty(), "刚问出去那一拍就该没有答复");

        let mut host = accept(&mut listener);
        let asked = drain(&mut host);
        assert_eq!(asked.len(), 1, "对端读到的不是正好一帧：{asked:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&asked[0]).expect("请求行是 JSON")["cmd"],
            json!("notes_roots")
        );
        assert!(!pump.idle(), "问出去还没回来不该算空闲");
    }

    /// 线上到齐的答复要变成事件：泵只管拍，落格那件事由链路报回。
    #[test]
    fn an_answer_on_the_wire_comes_back_as_a_ready_page() {
        let name = endpoint("answer");
        let mut listener = Listener::bind(&name).expect("绑不上测试管子");
        let mut pump = pump_at(&name);
        pump.apply(Intent::Page(roots()), 1);
        pump.beat(2);
        let mut host = accept(&mut listener);
        let request_id = id_of(&drain(&mut host)[0]);
        host.send_line(roots_reply(&request_id))
            .expect("答复写不走");

        assert_eq!(
            pump.beat(3),
            vec![LinkEvent::PageReady(roots())],
            "到齐的那一行没被交回链路"
        );
        assert_eq!(pump.link().cache().len(), 1, "报回 PageReady 却没留那一格");
    }

    /// 对端收掉之后只有"重试"能再拨：断线自动重拨会把宿主没跑的时候问成后台循环。
    #[test]
    fn a_closed_peer_stays_down_until_the_screen_asks_for_retry() {
        let name = endpoint("retry");
        let mut listener = Listener::bind(&name).expect("绑不上测试管子");
        let mut pump = pump_at(&name);
        pump.apply(Intent::Page(roots()), 1);
        pump.beat(2);
        let mut host = accept(&mut listener);
        let first_id = id_of(&drain(&mut host)[0]);
        drop(host);

        let events = pump.beat(3);
        assert!(
            events
                .iter()
                .any(|event| *event
                    == LinkEvent::PageFailed(roots(), "对端关闭了这条管子".to_string())),
            "对端收掉却没把在飞的那一笔报回：{events:?}"
        );
        assert!(!pump.link().is_online(), "断线之后还写着在线");
        assert!(pump.beat(4).is_empty(), "断线之后每一拍都在自己重拨");

        // 重试那一发没有还悬着的读，所以它只换代与再拨，不再报回任何事。
        assert!(pump.apply(Intent::Retry, 5).is_empty());
        assert!(pump.link().is_online(), "重试没把链路带回在线");
        pump.apply(Intent::Page(roots()), 6);
        pump.beat(7);
        let mut again_host = accept(&mut listener);
        let again = id_of(&drain(&mut again_host)[0]);
        assert_ne!(first_id, again, "重连之后问出去的号还是旧连接那一枚");
        again_host
            .send_line(roots_reply(&again))
            .expect("答复写不走");
        assert_eq!(pump.beat(8), vec![LinkEvent::PageReady(roots())]);
    }

    /// 线程睡醒那一拍要能自己收到点：到点没回的逐条报回，格子也要还回去。
    #[test]
    fn an_overdue_wait_is_reported_by_the_beat_that_passes_it() {
        let name = endpoint("overdue");
        let mut listener = Listener::bind(&name).expect("绑不上测试管子");
        let mut pump = pump_at(&name);
        pump.apply(Intent::Page(roots()), 1_000);
        pump.beat(1_001);
        let mut host = accept(&mut listener);
        drain(&mut host);

        let due = 1_000 + READ_TIMEOUT_MS;
        assert!(pump.beat(due - 1).is_empty(), "差一毫秒就到点了就报");
        assert_eq!(
            pump.beat(due),
            vec![LinkEvent::PageFailed(roots(), "答复超时".to_string())]
        );
        assert_eq!(pump.link().open_waits(), 0, "到点报回却没还格子");
    }

    /// 写命令也走同一根管子：两位都点名要、回执按问出去的那一条对。
    #[test]
    fn a_write_command_round_trips_through_the_pipe() {
        let name = endpoint("write");
        let mut listener = Listener::bind(&name).expect("绑不上测试管子");
        let mut pump = pump_at(&name);
        let command = Command::Open {
            note_id: 77,
            folder_id: 4,
        };
        assert!(pump.apply(Intent::Command(command.clone()), 1).is_empty());
        assert!(pump.beat(2).is_empty());

        let mut host = accept(&mut listener);
        let asked = serde_json::from_str::<Value>(&drain(&mut host)[0]).expect("请求行是 JSON");
        assert_eq!(asked["cmd"], json!("notes_open"));
        assert_eq!(asked["note_id"], json!(77));
        assert_eq!(asked["folder_id"], json!(4));
        let request_id = asked["request_id"].as_str().expect("回显号").to_string();
        host.send_line(format!(
            r#"{{"ok":true,"payload":{{"accepted":true,"note_id":77,"folder_id":4}},"request_id":"{request_id}"}}"#
        ))
        .expect("回执写不走");

        assert_eq!(pump.beat(3), vec![LinkEvent::CommandDone(command)]);
    }

    /// 线程那一头：意图递进去、答复出来，收尾要能在预算内把线程收干净。
    #[test]
    fn the_thread_answers_and_shutdown_retires_it() {
        let name = endpoint("thread");
        let mut listener = Listener::bind(&name).expect("绑不上测试管子");
        let worker = Worker::start(pump_at(&name)).expect("线程起不来");
        let running = worker.running.clone();
        assert!(worker.ask(Intent::Page(roots())).is_ok());

        let mut host = accept(&mut listener);
        let asked = drain(&mut host);
        assert_eq!(asked.len(), 1, "线程没把那一页问上线：{asked:?}");
        let request_id = id_of(&asked[0]);
        host.send_line(roots_reply(&request_id))
            .expect("答复写不走");

        let mut seen = Vec::new();
        for _ in 0..400 {
            seen.extend(worker.take_events());
            if seen
                .iter()
                .any(|event| *event == LinkEvent::PageReady(roots()))
            {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            seen.iter()
                .any(|event| *event == LinkEvent::PageReady(roots())),
            "线程那一拍没把答复落回来：{seen:?}"
        );

        assert!(
            worker.shutdown(Duration::from_secs(3)),
            "线程没在预算内收手"
        );
        assert!(!running.load(Ordering::Acquire), "线程说收手了却没落下标记");
    }

    /// 屏忘了叫 shutdown（窗口直接析构）时，Drop 也要把线程收干净——不留一根读不到东西的常驻线程。
    #[test]
    fn a_dropped_handle_stops_the_thread_without_shutdown() {
        let name = endpoint("dropped");
        let _listener = Listener::bind(&name).expect("绑不上测试管子");
        let worker = Worker::start(pump_at(&name)).expect("线程起不来");
        let running = worker.running.clone();
        assert!(worker.ask(Intent::Page(roots())).is_ok());
        assert!(worker.running());

        drop(worker);
        for _ in 0..600 {
            if !running.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("句柄没了线程还在跑");
    }

    /// 换一根管子就是把号账清空：旧连接上晚到的答复配不上新问出去的那一笔，屏上不能落两页。
    #[test]
    fn a_retired_id_never_lands_on_the_new_generation() {
        let name = endpoint("retired-id");
        let mut listener = Listener::bind(&name).expect("绑不上测试管子");
        let mut pump = pump_at(&name);
        pump.apply(Intent::Page(roots()), 1);
        pump.beat(2);
        let mut host = accept(&mut listener);
        let stale = id_of(&drain(&mut host)[0]);

        assert_eq!(
            pump.apply(Intent::Retry, 3),
            vec![LinkEvent::PageFailed(
                roots(),
                "重连：上一趟问的作废".to_string()
            )],
            "重连没把旧连接上还在飞的那一格报回"
        );
        pump.apply(Intent::Page(roots()), 4);
        pump.beat(5);
        let mut fresh_host = accept(&mut listener);
        let fresh = id_of(&drain(&mut fresh_host)[0]);
        assert_ne!(stale, fresh, "换代之后号没换，旧答复正好能配上新问的那一格");
        fresh_host
            .send_line(roots_reply(&stale))
            .expect("旧答复写不走");
        assert!(pump.beat(6).is_empty(), "上一根连接的答复落进了新格子");
        fresh_host
            .send_line(roots_reply(&fresh))
            .expect("新答复写不走");
        assert_eq!(pump.beat(7), vec![LinkEvent::PageReady(roots())]);
    }
}
