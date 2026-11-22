//! 数据面框帧：一行一帧、未换行攒到 1 MiB 即断（§2.3.5「JSON Lines + 1 MiB 帧上限」）。
//!
//! 这一层不解析消息内容，只管「哪一段字节算一条完整消息」。半条消息在这里只能是「还没收完」，
//! 不能被拼成一个默认合法的对象 —— 那是把传输错误读成业务错误的开始。

/// 单帧上限，与宿主 `pg_core::ipc::MAX_FRAME_BYTES` 同值：超过 1 MiB 还没换行，只可能是对端跑飞
/// 或有人把整张图塞进了 JSON，宁可断开也不让缓冲无界增长。
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// 一帧读不懂的两档原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// 未换行就超过 [`MAX_FRAME_BYTES`]。
    TooLong { got: usize, limit: usize },
    /// 一帧不是合法 UTF-8：线上是 JSON 文本，解不出就整帧作废。
    NotUtf8,
}

/// 把管道里陆续读到的字节攒成「一行一帧」。
#[derive(Default)]
pub struct Framer {
    buf: Vec<u8>,
    /// `buf[..scanned]` 已确认不含换行：只在追加后从 `scanned` 往后找，避免大包每次全扫。
    scanned: usize,
}

impl Framer {
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
            if raw.len() > MAX_FRAME_BYTES {
                return Err(FrameError::TooLong {
                    got: raw.len(),
                    limit: MAX_FRAME_BYTES,
                });
            }
            let text = String::from_utf8(raw).map_err(|_| FrameError::NotUtf8)?;
            return Ok(Some(text));
        }
        // 没有换行：把扫描游标推到末尾，并判断「未终止那一段」是否已越过上限。
        self.scanned = self.buf.len();
        if self.buf.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLong {
                got: self.buf.len(),
                limit: MAX_FRAME_BYTES,
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
    fn a_non_utf8_frame_is_dropped_not_defaulted() {
        let mut f = Framer::default();
        f.push(&[0xff, 0xfe, b'\n']);
        assert_eq!(f.next_line(), Err(FrameError::NotUtf8));
        assert_eq!(f.pending(), 0, "坏帧要被取走，别堵住后面的好帧");
        f.push(b"ok\n");
        assert_eq!(f.next_line().unwrap().as_deref(), Some("ok"));
    }
}
