//! 五层串成一条链路：问一格、发一帧、按号找回落点、把结果交回调用方。
//!
//! 这里不拨号、不握句柄，也不写字节——那一截归 `provider` 之外的工作线程。本模块是一台**字节进、
//! 事件出**的泵：进来的字节交给 `control::ReplyLines` 攒行，每一行按 `pending` 的号找回当初问的是
//! 哪一格，再由 `session` 裁定这趟还作不作数、`node` 投影、`cache` 留页。为什么要中间这一层：判断
//! 直接写进线程的话，"答复落错格子""断线丢命令"这两类病只能在真机上复现；抽出来之后每件事都能
//! 当场对账。事件只说发生了什么，缓存是唯一事实出处，调用方读 `cache()` 取页——一份状态不会两头漂。
//!
//! ⚠ 翻页那一格还没进来：`CacheKey` 里没有游标位，所以答复里的 `next_cursor` 只随页存着，本机从不问
//! 第二页。`Link` 也不持句柄，因而是单线程使用的状态机；工作线程把读到的字节送进来、把要写的字节取走。
use serde_json::Value;

use super::cache::{CacheKey, Cached, ProviderCache};
use super::control::{NotesRequest, ReplyLines, Triage, triage};
use super::node;
use super::pending::{COMMAND_TIMEOUT_MS, Expired, Pending, READ_TIMEOUT_MS, Routed};
use super::session::{Begin, Command, Enqueue, MAX_QUEUED_COMMANDS, ProviderSession, Settle};

/// 宿主推的那一行的事件名。变更那一发要作废格子，状态那一发不碰目录。
const EVENT_NOTES_CHANGED: &str = "notes_changed";

/// 首页那一格按打开时间排序、行上显示名字，所以只有动到这两位的那两种改动会作废它：删掉的那一条要消失，
/// 改了名字的那一条字变了。保存只推 `updated_at_ms`，新建的那一条还没被打开过——两位都没动。
const STALE_RECENT_KINDS: [&str; 2] = ["deleted", "renamed"];

/// 链路对外交出的事。每个名字只有一处来路，调用方按名字决定重绘、报错还是收管道。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LinkEvent {
    /// 这一格现在缓存里有，可以画了。
    PageReady(CacheKey),
    /// 这一格没读出来：离线、答非所问、形状坏了、或那一行压根发不出去。
    PageFailed(CacheKey, String),
    /// 宿主把这条命令办完了。
    CommandDone(Command),
    /// 这条命令没办成，或被断线／超时逐条报回。
    CommandFailed(Command, String),
    /// 缓存里那一格被作废：调用方据此重问或改画，上次看得见的那一页不由这一步清空。
    Invalidated(CacheKey),
    /// 这条连接读不出下一行了。管道去留由调用方判，本模块不关句柄。
    Broken(String),
}

/// 一条链路的状态：调度、号账、缓存、攒行的缓冲，加上等着写进管道的那些行。
#[derive(Debug)]
pub(crate) struct Link {
    session: ProviderSession,
    pending: Pending,
    cache: ProviderCache,
    reader: ReplyLines,
    outbound: Vec<String>,
}

impl Default for Link {
    fn default() -> Self {
        Self::new()
    }
}

impl Link {
    /// 新发现的 provider：第一代在线，号从这一代起头。
    pub(crate) fn new() -> Self {
        let session = ProviderSession::new();
        // 号账的代次位跟着 session 走，而不是各写一个 1：两处各自起点，重连后旧号就能撞上新问的那笔。
        let pending = Pending::new(session.generation());
        Self {
            session,
            pending,
            cache: ProviderCache::default(),
            reader: ReplyLines::default(),
            outbound: Vec::new(),
        }
    }

    pub(crate) fn cache(&self) -> &ProviderCache {
        &self.cache
    }

    pub(crate) fn is_online(&self) -> bool {
        self.session.is_online()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.session.generation()
    }

    /// 当下占着"每 provider 一趟元数据读"那格的是谁。
    pub(crate) fn read_in_flight(&self) -> Option<CacheKey> {
        self.session.in_flight()
    }

    /// 排在本地、还没轮到发出去的命令条数。
    pub(crate) fn queued(&self) -> usize {
        self.session.pending()
    }

    /// 问出去还没回来的笔数（读加命令）：调用方据此判断现在能不能再压。
    pub(crate) fn open_waits(&self) -> usize {
        self.pending.open()
    }

    /// 取走要写进管道的那些行（含结尾换行）。取走即清：写一半断了由调用方决定剩下几行怎么办——
    /// 那些行对应的命令已记在 `pending` 上，最终由超时或断线逐条报回，不会静默消失。
    pub(crate) fn take_outbound(&mut self) -> Vec<String> {
        std::mem::take(&mut self.outbound)
    }

    /// 要画某一格。缓存里有就一行都不发；离线当场报回，不在这一截攒着等重连。
    pub(crate) fn want_page(&mut self, key: CacheKey, now_ms: u64) -> Vec<LinkEvent> {
        let mut events = Vec::new();
        if self.cache.get(key).is_some() {
            return events;
        }
        // 先过自家形状闸：问不出去的形状不留账，否则症状是"点开了却永远转圈"。
        let Some(request) = read_request(self.pending.next_id(), key) else {
            events.push(LinkEvent::PageFailed(
                key,
                "这一格的号问不出去：文件夹号不是正整数".to_string(),
            ));
            return events;
        };
        let Ok(line) = request.to_line() else {
            events.push(LinkEvent::PageFailed(
                key,
                format!("{} 那一行没通过自家形状闸", request.cmd()),
            ));
            return events;
        };
        let ticket = match self.session.begin_read(key) {
            Begin::Offline => {
                events.push(LinkEvent::PageFailed(key, "离线".to_string()));
                return events;
            }
            Begin::Admitted(ticket) | Begin::Displaced { ticket, .. } => ticket,
        };
        // 换号同时把上一趟的来路撤账：它晚点回来只交 `Unknown`。被挤掉的那一格不会有人再等到答复，
        // 当场报回，屏上才不会留一个永远转圈的槽。
        let stale = self
            .pending
            .attach_read(request.request_id().to_string(), ticket, now_ms + READ_TIMEOUT_MS);
        if let Some(stale) = stale {
            events.push(LinkEvent::PageFailed(stale.key, "被更近的一次展开挤掉".to_string()));
        }
        self.outbound.push(line);
        events
    }

    /// 交一条写命令。排队与去重归 `session`，发得出去几条归 `pending` 那本账。
    pub(crate) fn submit(&mut self, command: Command, now_ms: u64) -> Vec<LinkEvent> {
        let mut events = Vec::new();
        match self.session.enqueue(command) {
            // 同一条还在队里：连点两下只该有一次宿主往返，这一句静默收下。
            Enqueue::Queued => {}
            Enqueue::Duplicate => return events,
            Enqueue::Full => {
                events.push(LinkEvent::CommandFailed(
                    command,
                    "命令队已经和宿主每连接的队列一样长".to_string(),
                ));
                return events;
            }
            Enqueue::Offline => {
                events.push(LinkEvent::CommandFailed(command, "离线".to_string()));
                return events;
            }
        }
        self.pump(now_ms, &mut events);
        events
    }

    /// 喂进来的那一段字节。返回的事件按字节顺序排；一旦判读不通就当场收束，剩下几行不再分派。
    pub(crate) fn ingest(&mut self, bytes: &[u8], now_ms: u64) -> Vec<LinkEvent> {
        let mut events = Vec::new();
        let Some(lines) = self.reader.push(bytes) else {
            events.push(LinkEvent::Broken(
                "答复那一行超出自家上限，等不到换行了".to_string(),
            ));
            return events;
        };
        for line in lines {
            match triage(&line) {
                Triage::Answer { request_id, payload } => {
                    self.land(request_id, payload, &mut events)
                }
                Triage::Refused {
                    request_id,
                    field,
                    code,
                    message,
                } => self.refuse(request_id, format!("{message}（宿主报 {field}／{code}）"), &mut events),
                Triage::Announce { event, folder, kind } => {
                    self.announce(&event, folder, kind.as_deref(), &mut events)
                }
                Triage::Garbage(reason) => {
                    events.push(LinkEvent::Broken(reason.to_string()));
                    return events;
                }
            }
        }
        // 答复腾出的格子用来发排着的命令：除重连外，这是命令队唯一的推进时机。
        self.pump(now_ms, &mut events);
        events
    }

    /// 到点没回的逐条报回，然后把排着的命令接着推。线程每次睡醒都可以叫这一句。
    pub(crate) fn tick(&mut self, now_ms: u64) -> Vec<LinkEvent> {
        let mut events = Vec::new();
        for overdue in self.pending.expire(now_ms) {
            match overdue {
                Expired::Read(ticket) => {
                    // 格子要还给调度，那一格才不会永远转圈；已经被新读挤掉的还不动，报回一次就够。
                    self.session.settle_read(&ticket);
                    events.push(LinkEvent::PageFailed(ticket.key, "答复超时".to_string()));
                }
                Expired::Command(command) => {
                    events.push(LinkEvent::CommandFailed(command, "回执超时".to_string()))
                }
            }
        }
        self.pump(now_ms, &mut events);
        events
    }

    /// 断线：在飞的读与命令逐条报回，没发出去的命令同样一条不丢，最后清掉没写走的帧。
    /// 上次看得见的那一页留在缓存里——断线只改在线位，不该把用户已经读到的目录假装成没读过。
    pub(crate) fn disconnect(&mut self, reason: &str) -> Vec<LinkEvent> {
        let mut events = Vec::new();
        for overdue in self.pending.expire(u64::MAX) {
            match overdue {
                Expired::Read(ticket) => {
                    events.push(LinkEvent::PageFailed(ticket.key, reason.to_string()))
                }
                Expired::Command(command) => {
                    events.push(LinkEvent::CommandFailed(command, reason.to_string()))
                }
            }
        }
        for command in self.session.go_offline() {
            events.push(LinkEvent::CommandFailed(command, reason.to_string()));
        }
        self.outbound.clear();
        events
    }

    /// 重连：起新一代并撤净旧号，攒行的缓冲也整段丢掉——半行旧字节会把新连接的第一行读坏。
    /// 重问哪几格由调用方决定：重连后旧连接上晚到的答复只会交 `Unknown`，所以留着缓存不会说谎。
    pub(crate) fn reconnect(&mut self) -> u64 {
        let generation = self.session.reconnect();
        self.pending.reset_connection(generation);
        self.reader = ReplyLines::default();
        self.outbound.clear();
        generation
    }

    /// 一行"办成了"的答复。落不落由调度那一层判：被挤掉的、上一根连接的、断线后的都只丢不落。
    fn land(&mut self, request_id: String, payload: Value, events: &mut Vec<LinkEvent>) {
        match self.pending.route(&request_id) {
            Routed::Read(ticket) => {
                if self.session.settle_read(&ticket) != Settle::Applied {
                    return;
                }
                match project(ticket.key, &payload) {
                    Ok(page) => {
                        if self.cache.insert(ticket.key, page) {
                            events.push(LinkEvent::PageReady(ticket.key));
                        } else {
                            // 留不住的一格等于没读到：报回去，别让调用方去缓存里取那一格却取到空。
                            events.push(LinkEvent::PageFailed(
                                ticket.key,
                                "这一格比整个缓存还大，留不住".to_string(),
                            ));
                        }
                    }
                    Err(reason) => events.push(LinkEvent::PageFailed(ticket.key, reason)),
                }
            }
            Routed::Command(command) => match receipt(command, &payload) {
                Ok(changed) => {
                    events.push(LinkEvent::CommandDone(command));
                    // 首页那一格现在一定旧了：那一条要么上了顶，要么被摘走。没动过账就不作废。
                    if changed && self.cache.invalidate(CacheKey::Recent) {
                        events.push(LinkEvent::Invalidated(CacheKey::Recent));
                    }
                }
                Err(reason) => events.push(LinkEvent::CommandFailed(command, reason)),
            },
            // 没问过的号、上一根连接的号、答过第二次的号：丢掉，落进缓存就是串台。
            Routed::Unknown => {}
        }
    }

    /// 一行"没办成"的答复：号要撤、读占的格子要还，报回的字符串留宿主的原话。
    fn refuse(&mut self, request_id: String, detail: String, events: &mut Vec<LinkEvent>) {
        match self.pending.route(&request_id) {
            Routed::Read(ticket) => {
                self.session.settle_read(&ticket);
                events.push(LinkEvent::PageFailed(ticket.key, detail));
            }
            Routed::Command(command) => events.push(LinkEvent::CommandFailed(command, detail)),
            Routed::Unknown => {}
        }
    }

    /// 宿主主动推的一行。只作废它点名的那一夹：根目录那一页只有文件夹，笔记改动动不到它。
    fn announce(
        &mut self,
        event: &str,
        folder: Option<i64>,
        kind: Option<&str>,
        events: &mut Vec<LinkEvent>,
    ) {
        if event != EVENT_NOTES_CHANGED {
            return;
        }
        if let Some(key) = folder.map(|folder_id| CacheKey::Children { folder_id }) {
            if self.cache.invalidate(key) {
                events.push(LinkEvent::Invalidated(key));
            }
        }
        let touches_recent = kind.is_some_and(|which| STALE_RECENT_KINDS.contains(&which));
        if touches_recent && self.cache.invalidate(CacheKey::Recent) {
            events.push(LinkEvent::Invalidated(CacheKey::Recent));
        }
    }

    /// 把队首能发的命令都发出去：发一条要占一笔账，账满就停在这一拍，剩下的留在队列里等下一发。
    fn pump(&mut self, now_ms: u64, events: &mut Vec<LinkEvent>) {
        while self.session.pending() > 0 && self.pending.open() < MAX_QUEUED_COMMANDS {
            let Some(command) = self.session.next_command() else {
                break;
            };
            let request = command_request(self.pending.next_id(), command);
            let Ok(line) = request.to_line() else {
                // 命令队这一头没有号闸，非正整数那位只能在这里拦下。
                events.push(LinkEvent::CommandFailed(
                    command,
                    format!("{} 那一行没通过自家形状闸", request.cmd()),
                ));
                continue;
            };
            self.pending.attach_command(
                request.request_id().to_string(),
                command,
                now_ms + COMMAND_TIMEOUT_MS,
            );
            self.outbound.push(line);
        }
    }
}

/// 那一格要问的那一行。`Children{0}` 是宿主整行拒收的形状，在这里就地拦下。
fn read_request(request_id: String, key: CacheKey) -> Option<NotesRequest> {
    match key {
        CacheKey::Roots => Some(NotesRequest::Roots {
            request_id,
            cursor: None,
            limit: None,
        }),
        CacheKey::Children { folder_id } if folder_id > 0 => Some(NotesRequest::Children {
            request_id,
            folder_id,
            cursor: None,
            limit: None,
        }),
        CacheKey::Children { .. } => None,
        CacheKey::Recent => Some(NotesRequest::Recent {
            request_id,
            limit: None,
        }),
    }
}

fn command_request(request_id: String, command: Command) -> NotesRequest {
    match command {
        Command::Open { note_id, folder_id } => NotesRequest::Open {
            request_id,
            note_id,
            folder_id,
        },
        Command::RemoveRecent { note_id } => NotesRequest::RemoveRecent { request_id, note_id },
        Command::ClearRecent => NotesRequest::ClearRecent { request_id },
    }
}

/// 答复那一格 → 缓存里那一格。投影读不出来就报位名与原话：半页画上树比空一页更难解释。
fn project(key: CacheKey, payload: &Value) -> Result<Cached, String> {
    let page = match key {
        CacheKey::Roots => node::parse_roots(payload).map(Cached::Directory),
        CacheKey::Children { folder_id } => {
            node::parse_children(payload, folder_id).map(Cached::Directory)
        }
        CacheKey::Recent => node::parse_recent(payload).map(Cached::Recent),
    };
    page.map_err(|err| format!("{}：{}", err.field, err.reason))
}

/// 写命令的回执。交 `true`＝宿主账本确实动了，该作废首页那一格。
fn receipt(command: Command, payload: &Value) -> Result<bool, String> {
    let outcome = match command {
        // 打开成功就是把那条笔记顶到首页最前，两位回显由投影那一头各对一次。
        Command::Open { note_id, .. } => node::parse_open(payload, note_id).map(|()| true),
        Command::RemoveRecent { .. } => node::parse_removed(payload),
        Command::ClearRecent => node::parse_cleared(payload).map(|removed| removed > 0),
    };
    outcome.map_err(|err| format!("{}：{}", err.field, err.reason))
}

#[cfg(test)]
mod tests {
    use super::super::cache::MAX_CACHE_BYTES;
    use super::*;
    use serde_json::json;

    /// 一个小型收包器：写端取走请求行，读端只喂完整帧。于是"答复配的是哪一行"变成测试与产品对
    /// 同一份线上字节的对账，而不是测试自己造一枚号。
    struct Harness {
        link: Link,
        asked: Vec<Value>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                link: Link::new(),
                asked: Vec::new(),
            }
        }

        /// 记下这一拍发走的请求行，原样交回事件。
        fn collect(&mut self, events: Vec<LinkEvent>) -> Vec<LinkEvent> {
            for line in self.link.take_outbound() {
                self.asked
                    .push(serde_json::from_str(&line).expect("请求行该是合法 JSON"));
            }
            events
        }

        fn want(&mut self, key: CacheKey, at: u64) -> Vec<LinkEvent> {
            let events = self.link.want_page(key, at);
            self.collect(events)
        }

        fn send(&mut self, command: Command, at: u64) -> Vec<LinkEvent> {
            let events = self.link.submit(command, at);
            self.collect(events)
        }

        fn read(&mut self, bytes: &[u8], at: u64) -> Vec<LinkEvent> {
            let events = self.link.ingest(bytes, at);
            self.collect(events)
        }

        fn clock(&mut self, at: u64) -> Vec<LinkEvent> {
            let events = self.link.tick(at);
            self.collect(events)
        }

        fn id_at(&self, index: usize) -> String {
            self.asked[index]["request_id"]
                .as_str()
                .unwrap_or_else(|| panic!("第 {index} 行没回显号：{:?}", self.asked.get(index)))
                .to_string()
        }

        /// 最近一次问出的那一行的号：连发两条同类命令时，答复配的是最新那一枚。
        fn id_of(&self, cmd: &str) -> String {
            let at = self
                .asked
                .iter()
                .rposition(|line| line["cmd"] == json!(cmd))
                .unwrap_or_else(|| panic!("没发过 {cmd}：{:?}", self.asked));
            self.id_at(at)
        }

        fn payload_of(kind: &str, folder_id: i64, name: &str) -> Value {
            let items = match kind {
                "roots" => json!([{
                    "kind": "folder", "id": format!("folder:{folder_id}"),
                    "folder_id": folder_id, "name": name, "locked": false,
                }]),
                "children" => json!([{
                    "kind": "note", "id": "note:9", "note_id": 9, "folder_id": folder_id,
                    "name": name, "updated_at_ms": 1, "locked": false,
                }]),
                _ => json!([{
                    "kind": "note", "id": "note:9", "note_id": 9, "folder_id": 3,
                    "name": name, "updated_at_ms": 1, "opened_at_ms": 2, "locked": false,
                }]),
            };
            match kind {
                "roots" => json!({
                    "protocol": 1, "provider_id": "notes", "online": true,
                    "items": items, "next_cursor": Value::Null,
                }),
                "children" => json!({
                    "folder_id": folder_id, "items": items, "next_cursor": Value::Null,
                }),
                _ => json!({ "items": items, "as_of_ms": 10 }),
            }
        }

        /// 三种页各一份逐字照宿主 `to_value` 的答复帧。
        fn page(&self, kind: &str, folder_id: i64, name: &str) -> Vec<u8> {
            let payload = Harness::payload_of(kind, folder_id, name);
            self.frame(&format!("notes_{kind}"), &payload)
        }

        /// 指定命令号的一行"办成了"：号由产品自己发出去的那一行里取。
        fn frame(&self, cmd: &str, payload: &Value) -> Vec<u8> {
            let line = format!(
                "{{\"ok\":true,\"payload\":{payload},\"request_id\":\"{}\"}}\n",
                self.id_of(cmd)
            );
            line.into_bytes()
        }

        fn reply(&mut self, cmd: &str, payload: Value, at: u64) -> Vec<LinkEvent> {
            let frame = self.frame(cmd, &payload);
            self.read(&frame, at)
        }

        fn cached_name(&self, key: CacheKey) -> Option<&str> {
            let page = self.link.cache.get(key)?;
            Some(match page {
                Cached::Directory(page) => page.nodes.first()?.name()?,
                Cached::Recent(page) => page.items.first()?.node.name()?,
            })
        }
    }

    fn children(folder_id: i64) -> CacheKey {
        CacheKey::Children { folder_id }
    }

    fn none_events() -> Vec<LinkEvent> {
        Vec::new()
    }

    /// 缓存命中一行都不发，未命中才发一行，且发的那一行是宿主认得的命令名。
    #[test]
    fn a_cached_page_is_never_asked_twice() {
        let mut at = Harness::new();
        assert_eq!(at.want(CacheKey::Roots, 100), none_events());
        assert_eq!(at.asked.len(), 1);
        assert_eq!(at.asked[0]["cmd"], json!("notes_roots"));
        assert_eq!(at.link.read_in_flight(), Some(CacheKey::Roots));

        let frame = at.page("roots", 4, "工作记录");
        let events = at.read(&frame, 200);
        assert_eq!(events, vec![LinkEvent::PageReady(CacheKey::Roots)]);
        assert_eq!(at.cached_name(CacheKey::Roots), Some("工作记录"));
        assert_eq!(at.link.read_in_flight(), None, "答复落定后那一格还占着");

        assert_eq!(at.want(CacheKey::Roots, 300), none_events());
        assert_eq!(at.asked.len(), 1, "缓存里已有的那一格被重问了一遍");
    }

    /// 挤掉上一趟之后：旧号的答复既不落缓存也不报回，只把被挤掉那一格当场报出去。
    #[test]
    fn only_the_newest_page_answer_is_allowed_to_land() {
        let mut at = Harness::new();
        at.want(CacheKey::Roots, 0);
        let events = at.want(children(3), 1);
        assert_eq!(
            events,
            vec![LinkEvent::PageFailed(
                CacheKey::Roots,
                "被更近的一次展开挤掉".to_string()
            )],
            "被挤掉那一格没有报回，屏上会留一个永远转圈的槽"
        );
        assert_eq!(at.link.read_in_flight(), Some(children(3)));
        let roots_id = at.id_at(0);
        assert_ne!(roots_id, at.id_at(1));

        let stale = at.frame("notes_roots", &Harness::payload_of("roots", 4, "旧的一页"));
        assert_eq!(at.read(&stale, 2), none_events());
        assert_eq!(at.link.cache().len(), 0, "被挤掉那一趟的答复落进了缓存");
        // 那一行带的确实是产品问出去的号，否则上面的"没落"是白测。
        assert!(stale.ends_with(format!("\"{roots_id}\"}}\n").as_bytes()));

        let frame = at.page("children", 3, "标题");
        assert_eq!(
            at.read(&frame, 3),
            vec![LinkEvent::PageReady(children(3))]
        );
        assert_eq!(at.link.cache().len(), 1);
    }

    /// 宿主明确报"没办成"也要还格子：只丢答复不还槽，那一格就永远问不出下一页。
    #[test]
    fn a_refusal_frees_the_slot_it_used() {
        let mut at = Harness::new();
        at.want(CacheKey::Recent, 0);
        let line = format!(
            "{{\"ok\":false,\"request_id\":\"{}\",\"error\":{{\"field\":\"limit\",\"code\":\"bad_request\",\"message\":\"页长超了\"}}}}\n",
            at.id_of("notes_recent")
        );
        let events = at.read(line.as_bytes(), 1);
        assert_eq!(
            events,
            vec![LinkEvent::PageFailed(
                CacheKey::Recent,
                "页长超了（宿主报 limit／bad_request）".to_string()
            )]
        );
        assert_eq!(at.link.read_in_flight(), None, "拒收那一条把格子留死了");
        assert_eq!(at.link.cache().len(), 0);

        assert_eq!(at.want(CacheKey::Recent, 2), none_events());
        assert_eq!(at.asked.len(), 2);
        assert_ne!(at.id_at(0), at.id_at(1), "重问用的是同一个号");
    }

    /// 一枚号只答一次：答第二次的行、以及本机没问过的号，都不许改动任何格子。
    #[test]
    fn an_answer_only_lands_once() {
        let mut at = Harness::new();
        at.want(CacheKey::Roots, 0);
        let frame = at.page("roots", 4, "甲");
        assert_eq!(
            at.read(&frame, 1),
            vec![LinkEvent::PageReady(CacheKey::Roots)]
        );

        let twice = at.frame("notes_roots", &Harness::payload_of("roots", 9, "重发的那一页"));
        let never = b"{\"ok\":true,\"payload\":{\"protocol\":1},\"request_id\":\"99-9\"}\n".to_vec();
        for stray in [twice, never] {
            assert_eq!(at.read(&stray, 2), none_events(), "同一枚号答了两次，第二次照样落了格");
        }
        assert_eq!(at.cached_name(CacheKey::Roots), Some("甲"));
        assert_eq!(at.link.cache().len(), 1);
    }

    /// 作废只发生在那一发点名的格子上：`status` 那一发不该让目录重问一遍。
    #[test]
    fn a_changed_note_invalidates_the_folder_it_names() {
        let mut at = Harness::new();
        for (kind, at_ms) in [("roots", 1), ("children", 3), ("recent", 5)] {
            at.want(
                match kind {
                    "roots" => CacheKey::Roots,
                    "children" => children(3),
                    _ => CacheKey::Recent,
                },
                at_ms - 1,
            );
            let frame = at.page(kind, 3, if kind == "roots" { "工作记录" } else { "旧名" });
            at.read(&frame, at_ms);
        }
        assert_eq!(at.link.cache().len(), 3);

        let events = at.read(
            b"{\"event\":\"notes_changed\",\"folder\":3,\"note_id\":9,\"kind\":\"renamed\"}\n",
            6,
        );
        assert_eq!(
            events,
            vec![
                LinkEvent::Invalidated(children(3)),
                LinkEvent::Invalidated(CacheKey::Recent),
            ]
        );
        assert_eq!(
            at.cached_name(CacheKey::Roots),
            Some("工作记录"),
            "根目录那一页跟着被清了：那一格只有文件夹"
        );
        assert_eq!(at.link.cache().len(), 1);

        // 没缓存的格子不作废也不报：作废一件不存在的东西不是事件。
        assert_eq!(
            at.read(b"{\"event\":\"notes_changed\",\"folder\":8,\"note_id\":9,\"kind\":\"deleted\"}\n", 7),
            none_events()
        );
        assert_eq!(
            at.read(b"{\"event\":\"status\",\"plugins\":[]}\n", 8),
            none_events()
        );
    }

    /// 半行等下一拍；读不出一行形状的那一行结束这条链路，剩下的行不再分派。
    #[test]
    fn a_half_line_waits_and_a_line_that_is_not_an_answer_ends_the_link() {
        let mut at = Harness::new();
        assert_eq!(
            at.read(b"{\"ok\":true,\"pay", 0),
            none_events(),
            "没等到换行就把半行当一行交了出去"
        );
        let tail = b"load\":{\"items\":[]},\"request_id\":\"1-9\"}\n";
        assert_eq!(at.read(tail, 1), none_events(), "没问过的号被当成了答复");

        assert_eq!(
            at.read(b"hello\n", 2),
            vec![LinkEvent::Broken("不是合法 JSON".to_string())]
        );
        assert_eq!(
            at.read(&vec![b'x'; 5 << 20], 3),
            vec![LinkEvent::Broken(
                "答复那一行超出自家上限，等不到换行了".to_string()
            )]
        );
    }

    /// 到点没回的读只报一次，且格子要还给调度。
    #[test]
    fn an_expired_wait_is_reported_once_and_the_slot_opens_up() {
        let mut at = Harness::new();
        at.want(CacheKey::Roots, 1_000);
        at.send(Command::ClearRecent, 1_000);
        assert_eq!(at.link.open_waits(), 2);

        assert_eq!(at.clock(1_000 + READ_TIMEOUT_MS - 1), none_events());
        assert_eq!(
            at.clock(1_000 + READ_TIMEOUT_MS),
            vec![LinkEvent::PageFailed(CacheKey::Roots, "答复超时".to_string())]
        );
        assert_eq!(at.link.read_in_flight(), None, "超时后那一格还占着");
        assert_eq!(
            at.clock(1_000 + READ_TIMEOUT_MS + 1),
            none_events(),
            "撤过账的读被报回了两次"
        );

        assert_eq!(
            at.clock(1_000 + COMMAND_TIMEOUT_MS),
            vec![LinkEvent::CommandFailed(Command::ClearRecent, "回执超时".to_string())]
        );
        assert_eq!(at.link.open_waits(), 0);
    }

    /// 断线留页、清账、丢没写走的帧；重连之后旧号的答复一律不作数。
    #[test]
    fn a_broken_pipe_keeps_the_last_page_and_a_reconnect_retires_the_old_ids() {
        let mut at = Harness::new();
        at.want(CacheKey::Roots, 0);
        let frame = at.page("roots", 4, "工作记录");
        at.read(&frame, 1);
        at.want(children(3), 2);
        let children_id = at.id_at(1);
        at.send(Command::ClearRecent, 3);
        // 留半行在缓冲里，再留一行没写走的帧在门口：断线要把两者都丢掉，否则新管道的第一行会被
        // 旧连接的字节读坏，旧连接的帧会被塞进新连接。
        assert_eq!(at.read(b"{\"ok\":true,", 4), none_events());
        assert_eq!(
            at.link.submit(Command::RemoveRecent { note_id: 1 }, 4),
            none_events()
        );

        let events = at.link.disconnect("管道断了");
        assert_eq!(
            events,
            vec![
                LinkEvent::PageFailed(children(3), "管道断了".to_string()),
                LinkEvent::CommandFailed(Command::ClearRecent, "管道断了".to_string()),
                LinkEvent::CommandFailed(Command::RemoveRecent { note_id: 1 }, "管道断了".to_string()),
            ],
            "断线时在飞的那几笔没有逐条报回"
        );
        assert!(!at.link.is_online());
        assert_eq!(at.link.open_waits(), 0);
        assert_eq!(at.link.queued(), 0);
        assert_eq!(
            at.link.take_outbound(),
            Vec::<String>::new(),
            "断线后还有没写走的帧"
        );
        assert_eq!(
            at.cached_name(CacheKey::Roots),
            Some("工作记录"),
            "断线把上次可见的页清空了"
        );
        assert_eq!(
            at.want(CacheKey::Recent, 5),
            vec![LinkEvent::PageFailed(CacheKey::Recent, "离线".to_string())]
        );
        assert_eq!(
            at.send(Command::ClearRecent, 5),
            vec![LinkEvent::CommandFailed(Command::ClearRecent, "离线".to_string())]
        );

        assert_eq!(at.link.reconnect(), 2);
        assert!(at.link.is_online());
        assert_eq!(at.want(children(3), 6), none_events());
        let fresh_id = at.id_at(at.asked.len() - 1);
        assert!(
            fresh_id.starts_with("2-"),
            "重连后的号还是旧连接那一代：{fresh_id}"
        );
        // 旧连接上晚到的答复与新连接上的答复各问一次：一头落、一头丢，才看得出代次位在管。
        let stale = Harness::frame_with(&children_id, &Harness::payload_of("children", 3, "旧的一页"));
        assert_eq!(at.read(&stale, 7), none_events());
        assert_eq!(at.link.cache().len(), 1, "旧连接上的答复落进了新一代的格子");
        let fresh = Harness::frame_with(&fresh_id, &Harness::payload_of("children", 3, "新的一页"));
        assert_eq!(
            at.read(&fresh, 8),
            vec![LinkEvent::PageReady(children(3))]
        );
    }

    /// 问不出去的那一格不留等待、不留帧：那格缓存里也没东西可画。
    #[test]
    fn a_request_that_cannot_leave_leaves_no_wait_behind() {
        let mut at = Harness::new();
        assert_eq!(
            at.want(children(0), 0),
            vec![LinkEvent::PageFailed(
                children(0),
                "这一格的号问不出去：文件夹号不是正整数".to_string()
            )]
        );
        assert!(at.asked.is_empty(), "问不出去的那一行照样发走了");
        assert_eq!(at.link.read_in_flight(), None);
        assert_eq!(at.link.open_waits(), 0);
        assert_eq!(at.cached_name(children(0)), None);

        // 写命令那一头同样：号没记账就没人在等，报回一次就完。
        let open = Command::Open { note_id: 0, folder_id: 3 };
        assert_eq!(
            at.send(open, 1),
            vec![LinkEvent::CommandFailed(
                open,
                "notes_open 那一行没通过自家形状闸".to_string()
            )]
        );
        assert!(at.asked.is_empty());
        assert_eq!(at.link.open_waits(), 0);
        assert_eq!(at.link.queued(), 0);
    }

    /// 写命令的两份号要一起走：宿主对 `notes_open` 是两位都点名要正整数。
    #[test]
    fn a_write_command_carries_both_ids_the_host_asks_for() {
        let mut at = Harness::new();
        at.want(CacheKey::Recent, 0);
        let frame = at.page("recent", 3, "旧名");
        at.read(&frame, 1);
        at.want(children(3), 2);
        let frame = at.page("children", 3, "旧名");
        at.read(&frame, 3);

        assert_eq!(at.send(Command::Open { note_id: 7, folder_id: 3 }, 4), none_events());
        let sent = &at.asked[2];
        assert_eq!(sent["cmd"], json!("notes_open"));
        assert_eq!(sent.get("note_id"), Some(&json!(7)));
        assert_eq!(sent.get("folder_id"), Some(&json!(3)));

        // 答的不是本机问的那一条：回执对不上号就不算办成，也不算作废。
        let lying = json!({ "accepted": true, "note_id": 8, "folder_id": 3 });
        assert_eq!(
            at.reply("notes_open", lying, 5),
            vec![LinkEvent::CommandFailed(
                Command::Open { note_id: 7, folder_id: 3 },
                "note_id：打开回执答的不是本机问的那一条".to_string()
            )]
        );
        assert_eq!(at.link.cache().len(), 2);

        assert_eq!(at.send(Command::RemoveRecent { note_id: 9 }, 6), none_events());
        assert_eq!(at.asked[3]["cmd"], json!("notes_remove_recent"));
        assert_eq!(at.asked[3]["note_id"], json!(9));
        // 账上本来没这条：宿主答 `removed:false`，首页那一格还画得着，不作废。
        assert_eq!(
            at.reply("notes_remove_recent", json!({ "removed": false }), 7),
            vec![LinkEvent::CommandDone(Command::RemoveRecent { note_id: 9 })]
        );
        assert_eq!(at.cached_name(CacheKey::Recent), Some("旧名"));
        // 同一枚号再答一次：那条命令已经交过账了。
        assert_eq!(
            at.reply("notes_remove_recent", json!({ "removed": true }), 8),
            none_events()
        );

        assert_eq!(at.send(Command::ClearRecent, 9), none_events());
        assert_eq!(at.asked[4]["cmd"], json!("notes_clear_recent"));
        assert_eq!(
            at.reply("notes_clear_recent", json!({ "removed": 0 }), 10),
            vec![LinkEvent::CommandDone(Command::ClearRecent)],
            "一条没动，首页那一格却作废了"
        );
        assert_eq!(at.send(Command::ClearRecent, 11), none_events());
        assert_eq!(
            at.reply("notes_clear_recent", json!({ "removed": 12 }), 12),
            vec![
                LinkEvent::CommandDone(Command::ClearRecent),
                LinkEvent::Invalidated(CacheKey::Recent),
            ]
        );
        assert_eq!(
            at.link.cache().len(),
            1,
            "首页以外的格子被写命令动到了"
        );
    }

    /// 账与宿主每连接的队列一样长就停在门口：第 65 条留在队列里等下一发腾出的格子，一条都不丢。
    #[test]
    fn a_full_book_keeps_the_extra_command_queued_until_a_slot_opens() {
        let mut at = Harness::new();
        // `note_id=0` 是自家形状闸要拒收的那一形，所以从 1 起头。
        for note in 1..=MAX_QUEUED_COMMANDS as i64 {
            assert_eq!(at.send(Command::RemoveRecent { note_id: note }, 0), none_events());
        }
        assert_eq!(at.asked.len(), MAX_QUEUED_COMMANDS);
        assert_eq!(at.link.open_waits(), MAX_QUEUED_COMMANDS);
        assert_eq!(at.link.queued(), 0);

        let first = at.id_at(0);
        assert_eq!(
            at.send(Command::RemoveRecent { note_id: 999 }, 1),
            none_events(),
            "多出来那一条被报成了失败，可它还排在队里"
        );
        assert_eq!(at.link.queued(), 1);
        assert_eq!(at.asked.len(), MAX_QUEUED_COMMANDS);

        let frame = format!("{{\"ok\":true,\"payload\":{{\"removed\":true}},\"request_id\":\"{first}\"}}\n");
        assert_eq!(
            at.read(frame.as_bytes(), 2),
            vec![LinkEvent::CommandDone(Command::RemoveRecent { note_id: 1 })]
        );
        assert_eq!(at.link.queued(), 0, "腾出的格子没用来发排队那一条");
        assert_eq!(at.asked.last().expect("第 65 行")["note_id"], json!(999));

        // 断线那一刻：排在门口没轮到的那一条要与账上那 64 笔一起报回，一条都不许静默消失。
        assert_eq!(
            at.send(Command::RemoveRecent { note_id: 1000 }, 3),
            none_events()
        );
        assert_eq!(at.link.queued(), 1);
        let handed = at.link.disconnect("管道断了");
        assert_eq!(
            handed.len(),
            MAX_QUEUED_COMMANDS + 1,
            "断线报回的条数少了：{handed:?}"
        );
        assert!(
            handed.contains(&LinkEvent::CommandFailed(
                Command::RemoveRecent { note_id: 1000 },
                "管道断了".to_string(),
            )),
            "排在队列里没发出去的那一条断线时静默消失了"
        );
    }

    /// 单格比整个缓存还大：留不住就报成没读到，而不是让调用方去取一格空。
    #[test]
    fn a_page_too_large_to_keep_is_reported_as_a_miss() {
        let mut at = Harness::new();
        at.want(children(3), 0);
        // 一格名字就超掉整本缓存的预算：投影照样读得出，留不住的是账。
        let payload = Harness::payload_of("children", 3, &"长".repeat(MAX_CACHE_BYTES / 2));
        assert_eq!(
            at.reply("notes_children", payload, 1),
            vec![LinkEvent::PageFailed(
                children(3),
                "这一格比整个缓存还大，留不住".to_string()
            )]
        );
        assert_eq!(at.link.cache().len(), 0);
        assert_eq!(at.link.read_in_flight(), None, "留不住的页还占着读的那一格");
    }

    impl Harness {
        /// 指定号的一行"办成了"：跨连接对账那几支要拿旧代次的号问同一格。
        fn frame_with(request_id: &str, payload: &Value) -> Vec<u8> {
            format!("{{\"ok\":true,\"payload\":{payload},\"request_id\":\"{request_id}\"}}\n")
                .into_bytes()
        }
    }
}
