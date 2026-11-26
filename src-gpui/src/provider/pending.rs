//! 问出去还没回来的那几笔：每笔一个号，答复按号找回来路，到点没回的逐条报回。
//!
//! 宿主按回显的 `request_id` 把定向答复送回，客户端这边就得反过来记住"这个号当时问的是什么"。
//! 号漏了或不唯一，症状是答复落错格子；到点不追，症状是那格永远转圈。这两件事都跟"哪一趟还作数"
//! （`provider::session`）和"页留多少"（`provider::cache`）分开住：这里只管号与来路的对应。
use std::collections::VecDeque;

use super::session::{Command, ReadTicket, MAX_QUEUED_COMMANDS};

/// 一次答复允许占的最长本地等待：比它长就当没回来，交回调用方报错或重试。
pub(crate) const READ_TIMEOUT_MS: u64 = 5_000;

/// 写命令的等待比读长一档：宿主那侧打开文档要做真活，早一步报失败就白让用户点第二次。
pub(crate) const COMMAND_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Routed {
    /// 这是一趟元数据读的答复：代次与在飞那一趟还得问 `ProviderSession`，别绕过它落缓存。
    Read(ReadTicket),
    /// 这是一条写命令的回执。
    Command(Command),
    /// 本机没问过这个号，或它属于上一根连接——只丢，不落。
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Expired {
    Read(ReadTicket),
    Command(Command),
}

/// 在飞登记簿。时钟由调用方递进来（毫秒读数），这一截自己不读表，测试因此能精确踩边界。
#[derive(Debug)]
pub(crate) struct Pending {
    connection: u64,
    seq: u64,
    read: Option<(String, ReadTicket, u64)>,
    commands: VecDeque<(String, Command, u64)>,
}

impl Pending {
    /// `connection` 取 `ProviderSession::generation()`：换一代就是换一根连接，旧号自然作废。
    pub(crate) fn new(connection: u64) -> Self {
        Self {
            connection,
            seq: 0,
            read: None,
            commands: VecDeque::new(),
        }
    }

    pub(crate) fn connection(&self) -> u64 {
        self.connection
    }

    /// 下一个号：连接位在前，序号在后，所以重连之后旧连接上晚到的答复再也不会撞上新问出去的那笔。
    pub(crate) fn next_id(&mut self) -> String {
        self.seq += 1;
        format!("{}-{}", self.connection, self.seq)
    }

    /// 记下问出去的那一趟读。返回被顶掉的那一枚——它的答复再回来也已经无处可落，调用方据此收尾。
    pub(crate) fn attach_read(
        &mut self,
        id: String,
        ticket: ReadTicket,
        deadline_ms: u64,
    ) -> Option<ReadTicket> {
        self.read.replace((id, ticket, deadline_ms)).map(|(_, stale, _)| stale)
    }

    /// 记下写出去的那条命令。`false`＝这本账已经和宿主每连接的队列一样长了，不再往那根管道上压。
    pub(crate) fn attach_command(
        &mut self,
        id: String,
        command: Command,
        deadline_ms: u64,
    ) -> bool {
        if self.commands.len() >= MAX_QUEUED_COMMANDS {
            return false;
        }
        self.commands.push_back((id, command, deadline_ms));
        true
    }

    /// 答复按号取回来路。取走一次就没：同一枚号再答一次（宿主重发、乱序、或自己读重了行）只当不认识。
    pub(crate) fn route(&mut self, id: &str) -> Routed {
        if let Some((at, _, _)) = self.read.as_ref() {
            if *at == id {
                let (_, ticket, _) = self.read.take().expect("刚查到的一位");
                return Routed::Read(ticket);
            }
        }
        if let Some(at) = self.commands.iter().position(|(at, _, _)| *at == id) {
            let (_, command, _) = self.commands.remove(at).expect("刚查到的一位");
            return Routed::Command(command);
        }
        Routed::Unknown
    }

    /// 到点的逐条交回，一条都不留在这儿：读要还 `ProviderSession` 清在飞，命令要还给调用方报错。
    pub(crate) fn expire(&mut self, now_ms: u64) -> Vec<Expired> {
        let mut expired = Vec::new();
        let read_due = match &self.read {
            Some((_, _, at)) if now_ms >= *at => true,
            _ => false,
        };
        if read_due {
            let (_, ticket, _) = self.read.take().expect("刚查到的一位");
            expired.push(Expired::Read(ticket));
        }
        for (id, command, at) in std::mem::take(&mut self.commands) {
            if now_ms >= at {
                expired.push(Expired::Command(command));
            } else {
                self.commands.push_back((id, command, at));
            }
        }
        expired
    }

    /// 连接换了：所有号一律撤账。返回撤了几笔，调用方据此把对应的等待一次性报错。
    pub(crate) fn reset_connection(&mut self, connection: u64) -> usize {
        let dropped = usize::from(self.read.is_some()) + self.commands.len();
        self.read = None;
        self.commands.clear();
        self.connection = connection;
        dropped
    }

    pub(crate) fn open(&self) -> usize {
        usize::from(self.read.is_some()) + self.commands.len()
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache::CacheKey;
    use super::super::control::NotesRequest;
    use super::*;

    fn roots_ticket() -> ReadTicket {
        ReadTicket {
            key: CacheKey::Roots,
            generation: 1,
        }
    }

    fn open(note_id: i64) -> Command {
        Command::Open { note_id }
    }

    /// 同一句话问两次也不能共用一个号，否则两趟答复互相顶位，屏上落的是哪一次没人知道。
    #[test]
    fn identical_requests_never_share_an_id() {
        let mut pending = Pending::new(1);
        let first = pending.next_id();
        let second = pending.next_id();
        assert_ne!(first, second, "两次问出去拿到同一个号");
        assert!(pending.attach_read(first.clone(), roots_ticket(), 1_000).is_none());
        let other = ReadTicket {
            key: CacheKey::Recent,
            generation: 1,
        };
        let stale = pending
            .attach_read(second.clone(), other, 1_000)
            .expect("后一趟没顶掉先一趟");
        assert_eq!(stale, roots_ticket());
        assert_eq!(pending.route(&first), Routed::Unknown, "被顶掉的那笔还挂着号");
        assert_eq!(pending.route(&second), Routed::Read(other));
    }

    /// 号只对一次：同一枚号再答一次只当不认识，不然一次翻页能落两次缓存。
    #[test]
    fn an_answer_is_routed_back_to_the_request_that_asked() {
        let mut pending = Pending::new(3);
        let read_id = pending.next_id();
        let command_id = pending.next_id();
        let ticket = ReadTicket {
            key: CacheKey::Children { folder_id: 7 },
            generation: 3,
        };
        assert!(pending.attach_read(read_id.clone(), ticket, 2_000).is_none());
        assert!(pending.attach_command(command_id.clone(), open(9), 4_000));

        assert_eq!(pending.route(&command_id), Routed::Command(open(9)));
        assert_eq!(pending.route(&command_id), Routed::Unknown);
        assert_eq!(pending.route(&read_id), Routed::Read(ticket));
        assert_eq!(pending.route(&read_id), Routed::Unknown);
        assert_eq!(pending.route("1-999"), Routed::Unknown, "没问过的号被认下了");
        assert_eq!(pending.open(), 0);
    }

    /// 换连接＝撤账：旧号晚到的答复必须不认识，否则上一根管道上那一次翻页会盖掉新连接刚拿到的目录。
    #[test]
    fn a_reply_from_a_retired_connection_is_unknown() {
        let mut pending = Pending::new(1);
        let read_id = pending.next_id();
        let command_id = pending.next_id();
        assert!(pending.attach_read(read_id.clone(), roots_ticket(), 9_000).is_none());
        assert!(pending.attach_command(command_id.clone(), Command::ClearRecent, 9_000));
        assert_eq!(pending.reset_connection(2), 2, "撤账没报出撤了几笔");
        assert_eq!(pending.connection(), 2);
        assert_eq!(pending.open(), 0);
        assert_eq!(pending.route(&read_id), Routed::Unknown);
        assert_eq!(pending.route(&command_id), Routed::Unknown);

        let fresh = pending.next_id();
        assert!(
            fresh.starts_with("2-"),
            "重连后的号还带旧连接位：{fresh}"
        );
    }

    /// 边界：正好到点算超时，差一毫秒不算。两笔各自的截止点互不相干。
    #[test]
    fn nothing_expires_before_its_own_deadline() {
        let mut pending = Pending::new(1);
        let read_id = pending.next_id();
        let soon_id = pending.next_id();
        let late_id = pending.next_id();
        assert!(pending.attach_read(read_id.clone(), roots_ticket(), 1_000).is_none());
        assert!(pending.attach_command(soon_id.clone(), open(1), 1_000));
        assert!(pending.attach_command(late_id.clone(), open(2), 5_000));

        assert!(pending.expire(999).is_empty(), "没到点就被报成超时");
        assert_eq!(pending.route(&late_id), Routed::Command(open(2)));

        let expired = pending.expire(1_000);
        assert_eq!(
            expired,
            vec![Expired::Read(roots_ticket()), Expired::Command(open(1))],
            "到点的没逐条交回"
        );
        assert!(pending.expire(1_000).is_empty(), "交回过的又交了一次");
        assert_eq!(pending.route(&soon_id), Routed::Unknown);
        assert_eq!(pending.route(&read_id), Routed::Unknown);
    }

    /// 这本账不能比宿主每连接那把尺更长：到顶就记不下，而不是无声地把已经问出去的那笔挤掉。
    #[test]
    fn the_answer_book_refuses_to_grow_past_the_host_queue() {
        let mut pending = Pending::new(1);
        let mut ids = Vec::new();
        for note in 0..MAX_QUEUED_COMMANDS as i64 {
            let id = pending.next_id();
            assert!(pending.attach_command(id.clone(), open(note), 3_000));
            ids.push(id);
        }
        let extra = pending.next_id();
        assert!(
            !pending.attach_command(extra.clone(), open(999), 3_000),
            "记不下了却报成记上了"
        );
        assert_eq!(pending.open(), MAX_QUEUED_COMMANDS);
        assert_eq!(pending.route(&extra), Routed::Unknown, "没记上的号也能配到答复");
        assert_eq!(pending.route(&ids[0]), Routed::Command(open(0)));
    }

    /// 号是自己拼的，就得过宿主那一把闸：拿 `to_line` 的真实校验对账，而不是在本地另立一套字符集。
    #[test]
    fn minted_ids_clear_the_gates_the_host_reads() {
        let mut pending = Pending::new(7);
        for _ in 0..400 {
            let id = pending.next_id();
            assert!(id.len() <= 128, "号超出宿主那把尺：{id}");
            let line = NotesRequest::Roots {
                request_id: id.clone(),
                cursor: None,
                limit: None,
            }
            .to_line()
            .unwrap_or_else(|err| panic!("{id} 过不了出帧闸：{err:?}"));
            assert!(line.contains(&format!("\"request_id\":\"{id}\"")), "{line}");
        }
    }

    /// 两档等待是两个数：宿主打开一篇笔记要做真活，比一次目录读宽一档；写成相等就按翻页的表掐死了打开。
    #[test]
    fn the_write_budget_is_wider_than_the_read_one() {
        assert!(READ_TIMEOUT_MS > 0 && COMMAND_TIMEOUT_MS > READ_TIMEOUT_MS);
        assert!(COMMAND_TIMEOUT_MS < 60_000, "等到一分钟后才报失败的等待等于没有等待");
    }
}
