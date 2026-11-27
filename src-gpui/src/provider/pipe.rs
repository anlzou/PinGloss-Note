//! 管子那一层：拨到宿主控制面那颗名字上，把攒好的帧体写走，再把到齐的整行交给链路。
//!
//! 这一层只做三件事：**拨**（落点与读侧那一格）、**发**（一整批写在一个总预算里）、**收**（一次吸、
//! 把已经攒齐的整行逐条交上去）。它不判答复配哪一格（那在 `link`），也不重连——断了就把在飞的每一笔
//! 逐条报回，重问哪几格、要不要再拨一根归调用方那根线程。
//!
//! ⚠ 读侧那一格必须用控制面那把宽尺（[`control::REPLY_MAX_BYTES`]）。拿数据面的 1 MiB 去拨宿主，
//! 症状是合法长答复被判成对端跑飞、整条连接断开，而屏幕上只剩"离线"两个字。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::plugin::transport::Stream;

use super::control;
use super::link::{Link, LinkEvent};

/// 一根控制面的管子。`None`＝已经收掉：之后每一拍都只交回空事件，等调用方拨新的一根。
pub(crate) struct Pipe {
    stream: Option<Stream>,
    /// 一整批请求行写走的总预算：每发一条用的是剩下那一截，所以最坏情况也只占这一位。
    write_budget: Duration,
}

impl Pipe {
    /// 拨到 `control` 推导出的那一颗（覆盖位有值就用它，否则按宿主同一套拼法定名）。
    pub(crate) fn dial(dial_budget: Duration, write_budget: Duration) -> Result<Self, String> {
        let path = control::control_endpoint().map_err(|err| err.message())?;
        Self::dial_to(&path, dial_budget, write_budget)
    }

    /// 同上，落点由调用方点名：判据要能指到一颗自己绑的管子上，而 `dial` 那一头读的是环境变量。
    pub(crate) fn dial_to(
        path: &Path,
        dial_budget: Duration,
        write_budget: Duration,
    ) -> Result<Self, String> {
        let mut stream = Stream::connect_with(path, dial_budget, control::REPLY_MAX_BYTES)
            .map_err(|err| format!("拨向 {} 失败：{err}", path.display()))?;
        // 这根管子随后交给自己起的那根线程：移交前收净挂起读，下一条读才由接手那一侧发起。
        stream
            .settle()
            .map_err(|err| format!("移交线程前收净挂起读失败：{err}"))?;
        Ok(Self {
            stream: Some(stream),
            write_budget,
        })
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.stream.is_some()
    }

    /// 走一拍：先写后读。写不出去、对端收掉、帧读不通都当场把这条链路按断线收账，
    /// 在飞的每一笔各报一次——留下的症状只能是"这一格没读到"，不能是一个永远转圈的槽。
    pub(crate) fn drive(&mut self, link: &mut Link, now_ms: u64) -> Vec<LinkEvent> {
        let Some(mut stream) = self.stream.take() else {
            return Vec::new();
        };
        let deadline = Instant::now() + self.write_budget;
        for body in link.take_outbound() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if let Err(reason) = stream.send_line_budget(body, remaining) {
                return broken(link, &format!("请求行没能写走：{reason}"), Vec::new());
            }
        }
        let mut events = Vec::new();
        match stream.poll() {
            Err(reason) => return broken(link, &format!("读这一侧失败：{reason}"), events),
            // `false` 只有一种含义：对端收掉了这条管子。
            Ok(false) => return broken(link, "对端关闭了这条管子", events),
            Ok(true) => {}
        }
        loop {
            match stream.next_line() {
                Ok(Some(line)) => {
                    let ingested = link.ingest_line(&line, now_ms);
                    if let Some(LinkEvent::Broken(reason)) =
                        ingested.iter().find(|event| matches!(event, LinkEvent::Broken(_)))
                    {
                        let reason = reason.clone();
                        events.extend(ingested);
                        return broken(link, &reason, events);
                    }
                    events.extend(ingested);
                }
                Ok(None) => break,
                Err(err) => {
                    return broken(link, &format!("那一帧读不通：{err:?}"), events);
                }
            }
        }
        self.stream = Some(stream);
        events
    }
}

/// 收掉这一拍：链路上在飞的读与命令逐条报回，没写走的帧整排丢掉。
fn broken(link: &mut Link, reason: &str, mut events: Vec<LinkEvent>) -> Vec<LinkEvent> {
    events.extend(link.disconnect(reason));
    events
}

#[cfg(all(test, windows))]
mod tests {
    use std::thread;
    use std::time::Duration;

    use serde_json::{Value, json};

    use super::*;
    use crate::plugin::transport::Listener;
    use crate::provider::cache::CacheKey;

    /// 宿主那一侧的落点：与数据面判据同一套命名，一颗管子一对测试。
    fn endpoint(tag: &str) -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\nomo-provider-pipe-{tag}-{}",
            std::process::id()
        ))
    }

    /// 等一小会儿再收：写侧那一路是另一根句柄，字节到齐总要几拍。
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

    /// 像那根线程一样一拍一拍地走，直到有事件或预算用完：一整帧要几拍才吸齐，一次 `poll` 不算数。
    fn drive_until(pipe: &mut Pipe, link: &mut Link, at: u64) -> Vec<LinkEvent> {
        for tick in 0..250 {
            let events = pipe.drive(link, at + tick);
            if !events.is_empty() {
                return events;
            }
            thread::sleep(Duration::from_millis(2));
        }
        Vec::new()
    }

    /// 一头拨进去、一头接住：交回 Nomo 那侧的管子与宿主那侧的句柄。
    fn pair(tag: &str) -> (Pipe, Stream) {
        let name = endpoint(tag);
        let mut listener = Listener::bind(&name).expect("绑不上测试管子");
        let pipe = Pipe::dial_to(&name, Duration::from_secs(5), Duration::from_secs(2))
            .expect("拨不通测试管子");
        let host = listener
            .accept()
            .expect("接宿主那一侧失败")
            .expect("没有客户端进来");
        (pipe, host)
    }

    fn roots() -> CacheKey {
        CacheKey::Roots { cursor: None }
    }

    /// 一页根目录的答复，`extra` 是宿主多带、投影不留的那一位。
    fn roots_reply(request_id: &str, extra: Option<(&str, Value)>) -> String {
        let mut payload = json!({
            "protocol": 1, "provider_id": "notes", "online": true,
            "items": [{
                "kind": "folder", "id": "folder:4", "folder_id": 4,
                "name": "工作记录", "locked": false,
            }],
            "next_cursor": Value::Null,
        });
        if let Some((field, value)) = extra {
            payload.as_object_mut().unwrap().insert(field.to_string(), value);
        }
        format!(r#"{{"ok":true,"payload":{payload},"request_id":"{request_id}"}}"#)
    }

    /// 落在线上的那一行只算一帧：帧体不带换行，帧尾由管子补那一个。
    #[test]
    fn one_queued_request_leaves_as_exactly_one_frame() {
        let (mut pipe, mut host) = pair("one-frame");
        let mut link = Link::new();
        assert!(link.want_page(roots(), 0).is_empty());
        assert!(pipe.drive(&mut link, 1).is_empty(), "刚写走那一拍就该没有答复");

        let lines = drain(&mut host);
        assert_eq!(lines.len(), 1, "对端读到的不是正好一帧：{lines:?}");
        let asked: Value = serde_json::from_str(&lines[0]).expect("请求行该是合法 JSON");
        assert_eq!(asked["cmd"], json!("notes_roots"));
        assert!(
            host.next_line().expect("不该有坏帧").is_none(),
            "帧尾多补了一个换行，对端读到一条空帧"
        );
    }

    /// 对端在听之前先问的那一格：答复回来要把格子落上，事件由链路那一头报出。
    #[test]
    fn an_answer_on_the_wire_lands_as_a_ready_page() {
        let (mut pipe, mut host) = pair("answer");
        let mut link = Link::new();
        link.want_page(roots(), 0);
        pipe.drive(&mut link, 1);

        let asked = drain(&mut host);
        let request_id = serde_json::from_str::<Value>(&asked[0]).expect("请求行是 JSON")
            ["request_id"]
            .as_str()
            .expect("请求行没带回显号")
            .to_string();
        host.send_line(roots_reply(&request_id, None))
            .expect("答复写不走");

        assert_eq!(
            pipe.drive(&mut link, 2),
            vec![LinkEvent::PageReady(roots())],
            "到齐的那一行没被交回链路"
        );
        assert_eq!(link.cache().len(), 1, "报回 PageReady 却没留那一格");
        assert_eq!(link.open_waits(), 0, "答复落定后那一格还占着");
    }

    /// 坏帧不是普通事件：管子当场落下，在飞的那一格也要报回，不能留着在线位空转。
    #[test]
    fn a_malformed_line_retires_the_pipe_and_reports_the_open_wait() {
        let (mut pipe, mut host) = pair("malformed");
        let mut link = Link::new();
        link.want_page(roots(), 0);
        pipe.drive(&mut link, 1);
        drain(&mut host);
        host.send_line("not-json".to_string()).expect("坏帧写不走");

        let events = drive_until(&mut pipe, &mut link, 2);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LinkEvent::Broken(_))),
            "坏帧没报 Broken：{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LinkEvent::PageFailed(key, _) if *key == roots())),
            "坏帧收摊却没还那一格：{events:?}"
        );
        assert!(!pipe.is_alive(), "坏帧之后管子还在");
        assert!(!link.is_online(), "坏帧之后链路还写在线");
        assert!(pipe.drive(&mut link, 3).is_empty(), "落下的管子又走了一拍");
    }

    /// 超过数据面那一格的合法答复要整帧落地：读侧那一格是控制面的宽尺，不是 1 MiB。
    #[test]
    fn an_answer_past_the_data_plane_cap_still_arrives_whole() {
        let (mut pipe, mut host) = pair("wide");
        let mut link = Link::new();
        link.want_page(roots(), 0);
        pipe.drive(&mut link, 1);
        let asked = drain(&mut host);
        let request_id = serde_json::from_str::<Value>(&asked[0]).expect("请求行是 JSON")
            ["request_id"]
            .as_str()
            .expect("请求行没带回显号")
            .to_string();

        // 多带的那一位不进投影：帧长过 1 MiB，落进缓存的那一格却还是原来的大小。
        let reply = roots_reply(&request_id, Some(("blob", json!("x".repeat(1 << 20)))));
        assert!(reply.len() > crate::plugin::frame::MAX_FRAME_BYTES);
        // 宿主写回的答复本来就允许超过数据面那一格（宿主自家客户端也按更宽的尺读），所以这里绕开
        // `send_line` 的出站闸直接灌线上字节：要测的是读侧那一格，不是出站合同。
        let raw = format!("{reply}\n").into_bytes();
        let pump = thread::spawn(move || {
            let mut host = host;
            host.write_raw(&raw).expect("长答复写不走");
        });

        assert_eq!(
            drive_until(&mut pipe, &mut link, 2),
            vec![LinkEvent::PageReady(roots())],
            "长答复被读成了对端跑飞"
        );
        pump.join().unwrap();
        assert!(pipe.is_alive(), "整帧到齐之后这根管子不该收掉");
    }

    /// 还没到一整帧不等于断了：空了两拍之后，答复照样落格。
    #[test]
    fn a_quiet_poll_keeps_the_pipe_for_the_next_answer() {
        let (mut pipe, mut host) = pair("idle");
        let mut link = Link::new();
        link.want_page(roots(), 0);
        assert!(pipe.drive(&mut link, 1).is_empty(), "刚写走那一拍就该一事不发");
        assert!(pipe.is_alive(), "对端还没答就该留着这根管子");

        let asked = drain(&mut host);
        let request_id = serde_json::from_str::<Value>(&asked[0]).expect("请求行是 JSON")
            ["request_id"]
            .as_str()
            .expect("请求行没带回显号")
            .to_string();

        assert!(
            pipe.drive(&mut link, 2).is_empty(),
            "对端没写东西被当成了断线"
        );
        assert!(pipe.is_alive(), "空了一拍管子就收了");

        host.send_line(roots_reply(&request_id, None))
            .expect("答复写不走");
        assert_eq!(
            pipe.drive(&mut link, 3),
            vec![LinkEvent::PageReady(roots())],
            "空了一拍之后那一行读不回来了"
        );
    }

    /// 对端收掉：在飞的那一笔要当场报回"没读到"，管子也要落下，不许留一个永远转圈的槽。
    #[test]
    fn the_peer_closing_reports_the_open_wait_and_retires_the_pipe() {
        let (mut pipe, mut host) = pair("closed");
        let mut link = Link::new();
        link.want_page(roots(), 0);
        pipe.drive(&mut link, 1);
        let asked = drain(&mut host);
        assert_eq!(asked.len(), 1, "对端收掉之前那一帧该已经到齐");
        drop(host);

        let events = pipe.drive(&mut link, 2);
        assert!(
            events.iter().any(|event| matches!(
                event,
                LinkEvent::PageFailed(key, _) if *key == roots()
            )),
            "断线没有在飞的读各报一次：{events:?}"
        );
        assert!(!pipe.is_alive(), "报完断线这根管子还挂在结构体上");
        assert!(!link.is_online(), "断线之后链路还写着在线");
        assert_eq!(link.open_waits(), 0, "断线把在飞的那笔留下了");
        // 收掉之后再走一拍：一行都不该发，也不该重复报回。
        assert!(pipe.drive(&mut link, 3).is_empty());
    }
}
