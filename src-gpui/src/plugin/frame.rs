//! 数据面框帧：一行一帧、未换行攒到这一只连接的上限即断（§2.3.5「JSON Lines + 1 MiB 帧上限」）。
//!
//! 这一层不解析消息内容，只管「哪一段字节算一条完整消息」。半条消息在这里只能是「还没收完」，
//! 不能被拼成一个默认合法的对象 —— 那是把传输错误读成业务错误的开始。

/// 单帧上限，与宿主 `pg_core::ipc::MAX_FRAME_BYTES` 同值：超过 1 MiB 还没换行，只可能是对端跑飞
/// 或有人把整张图塞进了 JSON，宁可断开也不让缓冲无界增长。
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// 一帧读不懂的两档原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// 未换行就超过这一只框帧器的上限。
    TooLong { got: usize, limit: usize },
    /// 一帧不是合法 UTF-8：线上是 JSON 文本，解不出就整帧作废。
    NotUtf8,
}

/// 把管道里陆续读到的字节攒成「一行一帧」。
pub struct Framer {
    buf: Vec<u8>,
    /// `buf[..scanned]` 已确认不含换行：只在追加后从 `scanned` 往后找，避免大包每次全扫。
    scanned: usize,
    /// 一条没换行的消息最多攒到多少字节。**每只连接一格**：这一位管的是"对端在塞不该塞的东西"，
    /// 不是内存顶，所以两端各按自己那一向的对端定。共用一个数只会把合法长答复读成对端跑飞。
    limit: usize,
}

impl Default for Framer {
    /// 缺省是两端同值那一格（[`MAX_FRAME_BYTES`]）。读侧要更大格子的连接得显式 [`Framer::with_limit`]。
    fn default() -> Self {
        Self::with_limit(MAX_FRAME_BYTES)
    }
}

impl Framer {
    /// 换一格这一只。
    pub fn with_limit(limit: usize) -> Self {
        Self {
            buf: Vec::new(),
            scanned: 0,
            limit,
        }
    }

    /// 追加从 socket 读到的一段字节。
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// 取下一整帧原文（到 `\n`，不含换行）。
    ///
    /// 没有完整帧 ⇒ `Ok(None)`；未换行就超限或整帧非 UTF ⇒ [`FrameError`]（超限那一帧直接作废，
    /// 已取出的行不留在缓冲里）。缓冲里若攒了多帧，调用方循环取到 `None` 为止。
    pub fn next_line(&mut self) -> Result<Option<String>, FrameError> {
        if let Some(off) = self.buf[self.scanned..].iter().position(|b| *b == b'\n') {
            let mut raw = self.buf.drain(..=(self.scanned + off)).collect::<Vec<u8>>();
            self.scanned = 0;
            raw.pop(); // 去掉换行本身
            if raw.len() > self.limit {
                return Err(FrameError::TooLong {
                    got: raw.len(),
                    limit: self.limit,
                });
            }
            let text = String::from_utf8(raw).map_err(|_| FrameError::NotUtf8)?;
            return Ok(Some(text));
        }
        // 没有换行：把扫描游标推到末尾，并判断「未终止那一段」是否已越过上限。
        self.scanned = self.buf.len();
        if self.buf.len() > self.limit {
            return Err(FrameError::TooLong {
                got: self.buf.len(),
                limit: self.limit,
            });
        }
        Ok(None)
    }

    /// 缓冲里尚未成帧的字节数（诊断与判据用）。
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_line_comes_back_without_the_newline() {
        let mut f = Framer::default();
        f.push(b"hello ");
        assert_eq!(f.next_line(), Ok(None), "还没换行不该交帧");
        f.push(b"world\n");
        assert_eq!(f.next_line(), Ok(Some("hello world".to_string())));
        assert_eq!(f.next_line(), Ok(None), "取完就空");
        f.push(b"");
        assert_eq!(f.next_line(), Ok(None), "空读不该造出空行");
    }

    #[test]
    fn several_frames_in_one_read_are_drained_in_order() {
        let mut f = Framer::default();
        f.push(b"a\nb\nc\n");
        assert_eq!(f.next_line().unwrap().as_deref(), Some("a"));
        assert_eq!(f.next_line().unwrap().as_deref(), Some("b"));
        assert_eq!(f.next_line().unwrap().as_deref(), Some("c"));
        assert_eq!(f.next_line().unwrap(), None);
    }

    #[test]
    fn an_unterminated_run_past_the_cap_is_refused() {
        let mut f = Framer::default();
        f.push(&vec![b'x'; MAX_FRAME_BYTES]);
        assert_eq!(
            f.next_line(),
            Ok(None),
            "刚好到上限、还没换行：不许提前拒（阈值是顶到才算满）"
        );
        f.push(b"x");
        assert_eq!(
            f.next_line(),
            Err(FrameError::TooLong {
                got: MAX_FRAME_BYTES + 1,
                limit: MAX_FRAME_BYTES
            }),
            "多一个字节就断，不能让缓冲继续长"
        );
    }

    #[test]
    fn a_connection_may_be_handed_a_wider_frame_cap() {
        // 一格 4 KiB：顶到不算满，多一字节才断，断时报的是自己那一格。
        const CAP: usize = 4096;
        let mut wide = Framer::with_limit(CAP);
        wide.push(&vec![b'y'; CAP]);
        assert_eq!(
            wide.next_line(),
            Ok(None),
            "自定义上限同样是'顶到才算满'，不是差一点就红"
        );
        wide.push(b"z");
        assert_eq!(
            wide.next_line(),
            Err(FrameError::TooLong {
                got: CAP + 1,
                limit: CAP
            },),
            "超限时报的必须是这一只自己的格子"
        );
    }

    #[test]
    fn two_connections_do_not_share_one_frame_cap() {
        // 同一串字节：窄的那只判跑飞，宽的那只只当还没收完。
        let mut narrow = Framer::with_limit(8);
        let mut roomy = Framer::with_limit(4096);
        let blob = vec![b'q'; 64];
        narrow.push(&blob);
        roomy.push(&blob);
        assert_eq!(
            narrow.next_line(),
            Err(FrameError::TooLong { got: 64, limit: 8 }),
            "窄格子没生效"
        );
        assert_eq!(
            roomy.next_line(),
            Ok(None),
            "宽格子不该被邻居那一位的 1 MiB 或 8 字节牵连"
        );
    }

    #[test]
    fn a_non_utf8_frame_is_dropped_not_defaulted() {
        let mut f = Framer::default();
        f.push(&[0xff, 0xfe, b'\n']);
        assert_eq!(f.next_line(), Err(FrameError::NotUtf8));
        assert_eq!(f.pending(), 0, "坏帧要被取走，别堵住后面的好帧");
        f.push(b"ok\n");
        assert_eq!(f.next_line().unwrap().as_deref(), Some("ok"));
    }
}
