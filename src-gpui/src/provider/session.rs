//! 一个 provider 的读写字段调度：同时最多一趟元数据读，命令按到的顺序排，满了报回而不是丢。
//!
//! §3.7.9 那几格的症状都很直白。翻页答复比更新的一次展开晚落，屏上就会出现"点开的格子是空的，
//! 上一屏的内容还挂在别处"；断线时把已经看得见的目录清掉，等于说用户看错了；写命令在断线那一刻
//! 静默消失，用户以为那条已经删了。所以这一截只管代次、在飞那一趟与命令队三件事——页留多少归
//! `provider::cache`，帧形与落点归 `provider::control`，真发东西的那根线程归后续接线。
use std::collections::VecDeque;

use super::cache::CacheKey;

/// 命令队列上限，与宿主每连接那把尺同值：本地排到顶就报回，不再往那根管道上压。
pub(crate) const MAX_QUEUED_COMMANDS: usize = 64;

/// 排队等的三条写命令。读不在这里——读只有一趟，见 `ReadTicket`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Open { note_id: i64 },
    RemoveRecent { note_id: i64 },
    ClearRecent,
}

/// 开出去的那一趟读：键是谁＋哪一代开的。答复回来时凭这两样判断它还作不作数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadTicket {
    pub(crate) key: CacheKey,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Begin {
    /// 本趟就是当下唯一在飞的读。
    Admitted(ReadTicket),
    /// 前一趟被本趟作废：调用方拿它去丢弃那次答复，别让它落进缓存。
    Displaced { stale: ReadTicket, ticket: ReadTicket },
    /// 离线时不开新读，也不在这儿攒着；重连之后由调用方重新发起。
    Offline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settle {
    /// 只有当前在飞、且代次对得上、且连接还在线的那一趟允许落。
    Applied,
    /// 落不得。原因只用于日志与判据点名。
    Discarded(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Enqueue {
    Queued,
    /// 同一条命令还在队里等着，不必排第二次（连点两下只该有一次宿主往返）。
    Duplicate,
    /// 队已经排满，这一句没进去——报回去，别让调用方以为已经交了。
    Full,
    /// 离线期间不排写命令：那条管道现在接不住。
    Offline,
}

#[derive(Debug)]
pub(crate) struct ProviderSession {
    generation: u64,
    online: bool,
    in_flight: Option<ReadTicket>,
    commands: VecDeque<Command>,
}

impl Default for ProviderSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderSession {
    /// 一个新发现的 provider 从第一代在线状态开始：断线与重连各占一代，旧代次自然过期。
    pub(crate) fn new() -> Self {
        Self {
            generation: 1,
            online: true,
            in_flight: None,
            commands: VecDeque::new(),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn is_online(&self) -> bool {
        self.online
    }

    pub(crate) fn in_flight(&self) -> Option<CacheKey> {
        self.in_flight.map(|ticket| ticket.key)
    }

    pub(crate) fn pending(&self) -> usize {
        self.commands.len()
    }

    /// 开一趟元数据读。§3.7.9 明写每个 provider 同时最多一趟，所以已有一趟时本趟把它挤掉：
    /// 用户最新点的那一格才是要显示的内容，被挤掉的那一趟答复回来也无处可落。
    pub(crate) fn begin_read(&mut self, key: CacheKey) -> Begin {
        if !self.online {
            return Begin::Offline;
        }
        let ticket = ReadTicket {
            key,
            generation: self.generation,
        };
        match self.in_flight.replace(ticket) {
            None => Begin::Admitted(ticket),
            Some(stale) => Begin::Displaced { stale, ticket },
        }
    }

    /// 答复到齐。返回 `Applied` 才允许写缓存：晚到的、被挤掉的、上一根连接的那三类都只丢不落。
    pub(crate) fn settle_read(&mut self, ticket: &ReadTicket) -> Settle {
        if !self.online {
            return Settle::Discarded("offline");
        }
        if ticket.generation != self.generation {
            return Settle::Discarded("stale_generation");
        }
        match self.in_flight {
            Some(current) if current == *ticket => {
                self.in_flight = None;
                Settle::Applied
            }
            Some(_) => Settle::Discarded("superseded"),
            None => Settle::Discarded("no_read_in_flight"),
        }
    }

    /// 排一条写命令。重复先查、上限后查：连点两下不该把一次点击报成"排不下了"。
    pub(crate) fn enqueue(&mut self, command: Command) -> Enqueue {
        if !self.online {
            return Enqueue::Offline;
        }
        if self.commands.contains(&command) {
            return Enqueue::Duplicate;
        }
        if self.commands.len() >= MAX_QUEUED_COMMANDS {
            return Enqueue::Full;
        }
        self.commands.push_back(command);
        Enqueue::Queued
    }

    /// 取下一条要发的命令。取出来就算在飞：那之后再断线由调用方逐条报错，这一截不回收。
    pub(crate) fn next_command(&mut self) -> Option<Command> {
        self.commands.pop_front()
    }

    /// 断线：在飞的读作废，没发出去的命令**逐条交回**。上次可见的目录不在这里清——那是缓存的事，
    /// 断线只改在线位，屏上继续显示旧目录并标离线，而不是假装什么都没读过。
    pub(crate) fn go_offline(&mut self) -> Vec<Command> {
        self.online = false;
        self.in_flight = None;
        self.commands.drain(..).collect()
    }

    /// 重连：起新一代，旧连接上任何还在飞的东西都不再对得上号。调用方据此重发 roots 与 recent。
    pub(crate) fn reconnect(&mut self) -> u64 {
        self.online = true;
        self.generation += 1;
        self.in_flight = None;
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache::{Cached, ProviderCache};
    use super::super::node::{Page, ProviderNode};
    use super::*;

    fn children(folder_id: i64) -> CacheKey {
        CacheKey::Children { folder_id }
    }

    fn open(note_id: i64) -> Command {
        Command::Open { note_id }
    }

    /// §3.7.9「每个 provider 同时最多一趟元数据读」：后开的把先开的作废，先开那趟的答复落不得。
    #[test]
    fn only_the_newest_metadata_read_survives() {
        let mut session = ProviderSession::new();
        let first = match session.begin_read(CacheKey::Roots) {
            Begin::Admitted(ticket) => ticket,
            other => panic!("第一趟读没被准入：{other:?}"),
        };
        assert_eq!(session.in_flight(), Some(CacheKey::Roots));

        let second = match session.begin_read(children(3)) {
            Begin::Displaced { stale, ticket } => {
                assert_eq!(stale, first, "被作废的不是先开的那一趟");
                ticket
            }
            other => panic!("第二趟读没有挤掉先开的那一趟：{other:?}"),
        };
        assert_eq!(session.in_flight(), Some(children(3)));
        assert_eq!(
            session.settle_read(&first),
            Settle::Discarded("superseded"),
            "被挤掉的答复照样落了缓存"
        );
        assert_eq!(session.settle_read(&second), Settle::Applied);
        assert_eq!(session.in_flight(), None);
    }

    /// 重连之后，上一根连接晚到的答复必须丢：同一枚键在新连接上会被重新问一遍，两趟答同一格看不出来。
    #[test]
    fn an_answer_from_the_previous_connection_is_dropped() {
        let mut session = ProviderSession::new();
        let stale = match session.begin_read(CacheKey::Recent) {
            Begin::Admitted(ticket) => ticket,
            other => panic!("{other:?}"),
        };
        assert_eq!(session.reconnect(), 2);
        assert_eq!(
            session.settle_read(&stale),
            Settle::Discarded("stale_generation"),
            "旧代次的答复被当成新一代的读数"
        );

        let fresh = match session.begin_read(CacheKey::Recent) {
            Begin::Admitted(ticket) => ticket,
            other => panic!("重连后开不了新读：{other:?}"),
        };
        assert_eq!(fresh.generation, 2);
        assert_eq!(session.settle_read(&fresh), Settle::Applied);
    }

    #[test]
    fn commands_leave_in_the_order_they_arrived() {
        let mut session = ProviderSession::new();
        assert_eq!(session.enqueue(open(7)), Enqueue::Queued);
        assert_eq!(
            session.enqueue(Command::RemoveRecent { note_id: 7 }),
            Enqueue::Queued
        );
        assert_eq!(session.enqueue(Command::ClearRecent), Enqueue::Queued);
        assert_eq!(session.pending(), 3);
        assert_eq!(
            (
                session.next_command(),
                session.next_command(),
                session.next_command(),
                session.next_command()
            ),
            (
                Some(open(7)),
                Some(Command::RemoveRecent { note_id: 7 }),
                Some(Command::ClearRecent),
                None
            ),
            "命令队不是先进先出"
        );
        assert_eq!(session.pending(), 0);
    }

    #[test]
    fn a_full_queue_reports_instead_of_dropping() {
        let mut session = ProviderSession::new();
        for note in 0..MAX_QUEUED_COMMANDS as i64 {
            assert_eq!(session.enqueue(open(note)), Enqueue::Queued);
        }
        assert_eq!(session.enqueue(open(9_999)), Enqueue::Full, "排满了却没报回");
        assert_eq!(session.pending(), MAX_QUEUED_COMMANDS);
        // 到顶之后取走一条就腾出一格，而不是永远满着。
        assert!(session.next_command().is_some());
        assert_eq!(session.enqueue(open(9_999)), Enqueue::Queued);
    }

    #[test]
    fn the_same_command_is_not_queued_twice() {
        let mut session = ProviderSession::new();
        assert_eq!(session.enqueue(open(5)), Enqueue::Queued);
        assert_eq!(
            session.enqueue(open(5)),
            Enqueue::Duplicate,
            "连点两下排了两次队"
        );
        assert_eq!(session.enqueue(open(6)), Enqueue::Queued);
        assert_eq!(session.pending(), 2);
        assert_eq!(
            session.enqueue(Command::RemoveRecent { note_id: 5 }),
            Enqueue::Queued,
            "同一条笔记的打开与移除被当成重复"
        );
        // 发出去的那一条不算还在队里，用户再点一次还得排得进去。
        assert_eq!(session.next_command(), Some(open(5)));
        assert_eq!(session.enqueue(open(5)), Enqueue::Queued);
    }

    /// 断线那一刻：在飞的读作废、未发出的命令逐条交回，一条都不许静默消失。
    #[test]
    fn going_offline_hands_back_every_unsent_command() {
        let mut session = ProviderSession::new();
        let ticket = match session.begin_read(CacheKey::Roots) {
            Begin::Admitted(ticket) => ticket,
            other => panic!("{other:?}"),
        };
        assert_eq!(session.enqueue(open(3)), Enqueue::Queued);
        assert_eq!(session.enqueue(Command::ClearRecent), Enqueue::Queued);

        let handed = session.go_offline();
        assert_eq!(handed, vec![open(3), Command::ClearRecent], "交回的命令缺条");
        assert_eq!(session.pending(), 0);
        assert!(!session.is_online());
        assert_eq!(session.in_flight(), None);
        assert_eq!(
            session.settle_read(&ticket),
            Settle::Discarded("offline"),
            "断线后的答复还算成了读数"
        );
        assert_eq!(session.enqueue(open(4)), Enqueue::Offline);
        assert_eq!(session.begin_read(CacheKey::Recent), Begin::Offline);
    }

    #[test]
    fn reconnect_admits_reads_again() {
        let mut session = ProviderSession::new();
        session.go_offline();
        assert_eq!(session.begin_read(CacheKey::Roots), Begin::Offline);
        assert_eq!(session.reconnect(), 2, "重连没有起新一代");
        match session.begin_read(CacheKey::Roots) {
            Begin::Admitted(ticket) => assert_eq!(ticket.generation, session.generation()),
            other => panic!("重连后仍然开不了读：{other:?}"),
        }
        assert_eq!(session.enqueue(Command::ClearRecent), Enqueue::Queued);
    }

    /// 断线要两头各取一次事实：在线位翻过去，上次可见的那一格还得留在缓存里。
    #[test]
    fn an_offline_link_keeps_the_last_visible_page() {
        let mut session = ProviderSession::new();
        let mut cache = ProviderCache::default();
        let ticket = match session.begin_read(children(1)) {
            Begin::Admitted(ticket) => ticket,
            other => panic!("{other:?}"),
        };
        assert_eq!(session.settle_read(&ticket), Settle::Applied);
        assert!(
            cache.insert(
                children(1),
                Cached::Directory(Page {
                    nodes: vec![ProviderNode::Folder {
                        id: "folder:1".to_string(),
                        folder_id: 1,
                        name: "甲".to_string(),
                        locked: false,
                    }],
                    next_cursor: None,
                })
            )
        );

        session.go_offline();
        assert!(!session.is_online());
        let kept = cache
            .get(children(1))
            .and_then(Cached::as_directory)
            .expect("断线把上次可见的目录清空了");
        assert_eq!((kept.nodes.len(), kept.nodes[0].name()), (1, Some("甲")));
    }
}
