//! 数据面传输：Unix socket 与 Windows named pipe 上的 JSON Lines，协议形状与 `pg_core::ipc` 一致。
//!
//! 方向再钉一次：数据面上**插件是 server、宿主是主客户端** —— 绑这条连接的是 Nomo，拨进来的是宿主。
//! 控制面 `plugin.sock` 上宿主才是 server，那颗不归本模块。
//!
//! 这一层只管「字节从哪只句柄进出、怎么攒成一行」，**不解析消息内容**（那在 [`crate::plugin::envelope`]）。
//! 半条消息在这里只能是「还没收完」，不能被拼成一个默认合法的帧。Windows 的异步读写由
//! [`windows`] 负责，两端仍共用同一个 [`Framer`]。

#[cfg(unix)]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::plugin::frame::{FrameError, Framer, MAX_FRAME_BYTES};

#[cfg(windows)]
#[path = "windows.rs"]
mod windows;

/// 传输层的失败。
pub type Res<T> = Result<T, String>;

fn check_outbound_len(len: usize) -> Res<()> {
    if len > MAX_FRAME_BYTES {
        return Err(format!("帧超过 {MAX_FRAME_BYTES} 字节上限：{len}"));
    }
    Ok(())
}

/// 等一帧时的两种结局：预算用完不等于连接坏了，对端关闭或字节流损坏才必须收掉整条连接。
#[derive(Debug)]
pub enum WaitError {
    TimedOut,
    Broken(String),
}

impl std::fmt::Display for WaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => f.write_str("等一帧的预算用完了"),
            Self::Broken(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for WaitError {}

/// 单帧缓冲：一次 socket 读最多吸这么多字节（读循环靠 [`crate::plugin::frame`] 的 1 MiB 闸兜底无界增长）。
const READ_CHUNK: usize = 64 * 1024;

/// 一条握过手的连接上的字节管道。
pub struct Stream {
    #[cfg(unix)]
    sock: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    pipe: windows::Stream,
    framer: Framer,
    #[cfg(unix)]
    write_buf: Vec<u8>,
}

/// 数据面服务端句柄：绑住 `plugin-<id>.sock` 或同名 named pipe 等宿主进来。
pub struct Listener {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixListener,
    #[cfg(windows)]
    pipe: windows::Listener,
    path: PathBuf,
}

impl Listener {
    /// 绑上数据面那个名字。会先清掉**残留的同名 socket 文件**（上次崩了没退干净），否则 bind 直接 EADDRINUSE。
    pub fn bind(path: &Path) -> Res<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixListener;
            if path.exists() {
                let _ = std::fs::remove_file(path);
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let inner = UnixListener::bind(path).map_err(|e| e.to_string())?;
            inner.set_nonblocking(true).map_err(|e| e.to_string())?;
            Ok(Self {
                inner,
                path: path.to_path_buf(),
            })
        }
        #[cfg(windows)]
        {
            let pipe = windows::Listener::bind(path).map_err(|e| e.to_string())?;
            Ok(Self {
                pipe,
                path: path.to_path_buf(),
            })
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 非阻塞接一条：没有客户端在等 ⇒ `Ok(None)`（不是错）。
    pub fn accept(&mut self) -> Res<Option<Stream>> {
        #[cfg(unix)]
        {
            match self.inner.accept() {
                Ok((sock, _addr)) => {
                    sock.set_nonblocking(true).map_err(|e| e.to_string())?;
                    Ok(Some(Stream {
                        sock,
                        framer: Framer::default(),
                        write_buf: Vec::new(),
                    }))
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        }
        #[cfg(windows)]
        {
            Ok(self.pipe.accept().map_err(|e| e.to_string())?.map(|pipe| Stream {
                pipe,
                framer: Framer::default(),
            }))
        }
    }
}

#[cfg(any(unix, test))]
fn poll_unix(
    mut read: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
    framer: &mut Framer,
) -> std::io::Result<bool> {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match read(&mut buf) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                framer.push(&buf[..n]);
                // 对端可持续写满；先交回调用方执行帧上限检查。
                return Ok(true);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

impl Stream {
    /// 宿主侧拨进去（判据/自检里模拟宿主时才用；Nomo 作为插件走 [`Listener::accept`]）。
    pub fn connect(path: &Path, budget: Duration) -> Res<Self> {
        Self::connect_with(path, budget, MAX_FRAME_BYTES)
    }

    /// 同上，但读侧那一格由调用方定。控制面上宿主写回的答复比数据面这一格大（宿主自家客户端也按
    /// 更大的那一格读），拿 1 MiB 去读它的症状是合法长答复被判成对端跑飞、整条连接断开。
    /// ⚠ 这一位只管读：写侧仍按 [`MAX_FRAME_BYTES`] 拒，那是宿主入站那一向的合同。
    pub fn connect_with(path: &Path, budget: Duration, read_max: usize) -> Res<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;
            let sock = UnixStream::connect(path).map_err(|e| e.to_string())?;
            sock.set_nonblocking(true).map_err(|e| e.to_string())?;
            let _ = budget; // 连接预算：非阻塞 connect 已足够；保留签名与宿主对齐。
            Ok(Stream {
                sock,
                framer: Framer::with_limit(read_max),
                write_buf: Vec::new(),
            })
        }
        #[cfg(windows)]
        {
            let pipe = windows::Stream::connect(path, budget).map_err(|e| e.to_string())?;
            Ok(Stream {
                pipe,
                framer: Framer::with_limit(read_max),
            })
        }
    }

    /// 吸一格：把当前可读字节读进框帧缓冲。
    ///
    /// 交回 `false` **只有一种**含义——对端关闭（读到 EOF）。没数据（WouldBlock）交回 `true`。
    pub fn poll(&mut self) -> Res<bool> {
        #[cfg(unix)]
        {
            let sock = &mut self.sock;
            let framer = &mut self.framer;
            poll_unix(|buf| sock.read(buf), framer).map_err(|e| e.to_string())
        }
        #[cfg(windows)]
        {
            self.pipe.poll(&mut self.framer).map_err(|e| e.to_string())
        }
    }

    /// 移交线程前收净在飞读。Unix 没有跨线程异步读归属，空操作。
    pub fn settle(&mut self) -> Res<()> {
        #[cfg(unix)]
        {
            Ok(())
        }
        #[cfg(windows)]
        {
            self.pipe.settle(&mut self.framer).map_err(|e| e.to_string())
        }
    }

    /// 取缓冲里下一整帧原文（没有 ⇒ `None`）。超限/非 UTF ⇒ [`FrameError`]（由调用方决定断开还是丢帧）。
    pub fn next_line(&mut self) -> Result<Option<String>, FrameError> {
        self.framer.next_line()
    }

    /// 已经攒好的一帧原文里解出 JSON；没攒好 ⇒ `None`（不阻塞）。
    pub fn next_msg(&mut self) -> Res<Option<serde_json::Value>> {
        match self.next_line() {
            Ok(Some(line)) => serde_json::from_str(&line).map(Some).map_err(|e| e.to_string()),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("框帧坏了：{e:?}")),
        }
    }

    /// 发一帧（一行 + 换行）。非阻塞 socket 上的部分写用 `write_buf` 续着写完。
    pub fn send_line(&mut self, line: String) -> Res<()> {
        self.send_line_budget(line, Duration::from_secs(5))
    }

    /// 判据用：绕过 [`check_outbound_len`] 把原始字节直接写走，用来演"对端送来一帧比自家格子还大"。
    #[cfg(test)]
    pub fn write_raw(&mut self, bytes: &[u8]) -> Res<()> {
        #[cfg(unix)]
        {
            self.sock.write_all(bytes).map_err(|e| e.to_string())
        }
        #[cfg(windows)]
        {
            self.pipe.write_raw(bytes).map_err(|e| e.to_string())
        }
    }

    /// 在同一个绝对预算内发完一帧；预算也覆盖部分写与重试。
    pub fn send_line_budget(&mut self, line: String, budget: Duration) -> Res<()> {
        check_outbound_len(line.len())?;
        let deadline = Instant::now() + budget;
        #[cfg(unix)]
        {
            let mut line = line;
            line.push('\n');
            self.write_buf = line.into_bytes();
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    self.write_buf.clear();
                    return Err("发送一整帧的预算用完了".to_string());
                }
                match self.sock.write(&self.write_buf) {
                    Ok(0) => {
                        self.write_buf.clear();
                        return Err("socket 写入 0 字节".to_string());
                    }
                    Ok(n) => {
                        self.write_buf.drain(..n);
                        if self.write_buf.is_empty() {
                            return Ok(());
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(remaining.min(Duration::from_millis(1)));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        self.write_buf.clear();
                        return Err(e.to_string());
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            self.pipe
                .send_line_until(line, deadline)
                .map_err(|e| e.to_string())
        }
    }

    /// 发一个 `serde` 可序列化对象编成的那一帧。
    pub fn send<T: serde::Serialize>(&mut self, value: &T) -> Res<()> {
        let line = serde_json::to_string(value).map_err(|e| e.to_string())?;
        self.send_line(line)
    }

    /// 序列化与部分写共用调用方给的一个预算。
    pub fn send_with_budget<T: serde::Serialize>(
        &mut self,
        value: &T,
        budget: Duration,
    ) -> Res<()> {
        let line = serde_json::to_string(value).map_err(|e| e.to_string())?;
        self.send_line_budget(line, budget)
    }

    /// 在预算内等**下一帧 JSON**（握手那一步宿主回 `HelloOk` 用的就是它；"没有事件循环的那一侧"才允许阻塞）。
    pub fn wait_msg(&mut self, budget: Duration) -> Res<serde_json::Value> {
        self.wait_msg_typed(budget).map_err(|error| error.to_string())
    }

    /// 同上，但保留「超时／连接坏了」的分野，供常驻连接决定这一趟落空后还能不能继续。
    pub fn wait_msg_typed(&mut self, budget: Duration) -> Result<serde_json::Value, WaitError> {
        let deadline = Instant::now() + budget;
        loop {
            // 先取缓冲里已经攒好的完整帧。
            if let Some(value) = self.next_msg().map_err(WaitError::Broken)? {
                return Ok(value);
            }
            // 再吸一次；EOF 与本帧同到时，下一圈先把完整帧交出去，再判关闭。
            if !self.poll().map_err(WaitError::Broken)? {
                if let Some(value) = self.next_msg().map_err(WaitError::Broken)? {
                    return Ok(value);
                }
                return Err(WaitError::Broken("等答复时连接被对端关了".to_string()));
            }
            if Instant::now() >= deadline {
                return Err(WaitError::TimedOut);
            }
            // 没吸到完整帧就让出一点，别把 CPU 烧满（下一圈回到顶部再取）。
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[cfg(test)]
mod unix_poll_tests {
    use super::*;
    use std::io::ErrorKind;

    #[test]
    fn a_full_read_yields_before_the_peer_runs_dry() {
        let mut framer = Framer::default();
        let mut reads = 0;
        let open = poll_unix(
            |_buf| {
                reads += 1;
                if reads == 1 {
                    Ok(READ_CHUNK)
                } else {
                    Err(std::io::Error::from(ErrorKind::WouldBlock))
                }
            },
            &mut framer,
        )
        .unwrap();
        assert!(open);
        assert_eq!(reads, 1, "满格读取后必须先交回调用方检查帧上限");
        assert_eq!(framer.pending(), READ_CHUNK);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn connected_pair(tag: &str) -> (Stream, Stream) {
        let name = PathBuf::from(format!(
            r"\\.\pipe\nomo-transport-test-{tag}-{}",
            std::process::id()
        ));
        let mut listener = Listener::bind(&name).unwrap();
        let host = Stream::connect(&name, Duration::from_secs(5)).unwrap();
        let plugin = loop {
            if let Some(stream) = listener.accept().unwrap() {
                break stream;
            }
            thread::sleep(Duration::from_millis(2));
        };
        (plugin, host)
    }

    #[test]
    fn outbound_frame_limit_is_checked_before_platform_write() {
        assert!(check_outbound_len(MAX_FRAME_BYTES).is_ok());
        let error = check_outbound_len(MAX_FRAME_BYTES + 1).expect_err("超限帧必须被传输层拒绝");
        assert!(error.contains(&MAX_FRAME_BYTES.to_string()), "{error}");
    }

    #[test]
    fn an_oversized_outbound_frame_is_refused_before_writing() {
        let (mut plugin, mut host) = connected_pair("outbound-cap");
        let error = plugin
            .send_line_budget("x".repeat(MAX_FRAME_BYTES + 1), Duration::from_secs(1))
            .expect_err("超过线上上限的帧不许写出去");
        assert!(error.contains(&MAX_FRAME_BYTES.to_string()), "{error}");

        plugin
            .send_line_budget(r#"{"ok":true}"#.to_string(), Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            host.wait_msg(Duration::from_secs(1)).unwrap(),
            serde_json::json!({"ok": true})
        );
    }

    #[test]
    fn a_slow_peer_uses_one_budget_for_the_whole_write() {
        let (mut plugin, _host) = connected_pair("write-budget");
        let started = Instant::now();
        let error = plugin
            .send_line_budget("x".repeat(MAX_FRAME_BYTES), Duration::from_millis(30))
            .expect_err("对端不吸字节，发送必须按调用方预算收手");
        assert!(error.contains("没有吸完 pipe 字节"), "{error}");
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "整次发送用了 {:?}，不能按每次写各等 500ms",
            started.elapsed()
        );
    }

    /// 拨号那一侧带着自己那一位读侧格子（`None`＝走 `connect` 的缺省那一格）；交回的 `reader` 是拨进来
    /// 的那一半，`writer` 是被接住的那一半。
    fn pair_by_dial(tag: &str, read_max: Option<usize>) -> (Stream, Stream) {
        let name = PathBuf::from(format!(
            r"\\.\pipe\nomo-transport-test-cap-{tag}-{}",
            std::process::id()
        ));
        let mut listener = Listener::bind(&name).unwrap();
        let reader = match read_max {
            None => Stream::connect(&name, Duration::from_secs(5)).unwrap(),
            Some(read_max) => Stream::connect_with(&name, Duration::from_secs(5), read_max).unwrap(),
        };
        let writer = loop {
            if let Some(stream) = listener.accept().unwrap() {
                break stream;
            }
            thread::sleep(Duration::from_millis(2));
        };
        (reader, writer)
    }

    /// 轮询到那一格开口为止：Windows 的读是异步的，判据不能假定写完就读得到。
    fn read_until_quiet(reader: &mut Stream) -> Result<Option<String>, FrameError> {
        for _ in 0..2_500 {
            reader.poll().expect("读侧不该报管道错");
            match reader.next_line() {
                Ok(None) => thread::sleep(Duration::from_millis(2)),
                other => return other,
            }
        }
        panic!("轮询到预算用完还没等到那一格的判词");
    }

    /// 往只拨了一次的格子上灌"没有换行、比 1 MiB 大"的原始字节，交出读侧的判词。
    /// 写侧走 `write_raw` 绕开自家出站闸：宿主写回的答复不归那一位管，测的只能是读侧。
    fn verdict_of_raw_run(
        tag: &str,
        read_max: Option<usize>,
        blob: &[u8],
    ) -> Result<Option<String>, FrameError> {
        let (mut reader, writer) = pair_by_dial(tag, read_max);
        let sent = blob.to_vec();
        let pump = thread::spawn(move || {
            let mut writer = writer;
            let _ = writer.write_raw(&sent);
        });
        let verdict = read_until_quiet(&mut reader);
        // 先收读者：写侧那一路还在灌，管子断了它才立刻收尾，不然判据要等满写预算。
        drop(reader);
        pump.join().unwrap();
        verdict
    }

    #[test]
    fn a_dialed_connection_reads_with_the_cap_it_was_handed() {
        // 同一串线上字节，格子给 16：整帧到齐也照样判跑飞——报的是这一只自己的数。
        let (mut reader, mut writer) = pair_by_dial("narrow", Some(16));
        writer.send_line("x".repeat(64)).unwrap();
        assert_eq!(
            read_until_quiet(&mut reader),
            Err(FrameError::TooLong {
                got: 64,
                limit: 16
            }),
            "读侧没用自己那一格"
        );

        // 顶上那一格刚好够用，同一发就该完整交回来。
        let (mut ok_reader, mut ok_writer) = pair_by_dial("exact", Some(64));
        ok_writer.send_line("y".repeat(64)).unwrap();
        assert_eq!(
            read_until_quiet(&mut ok_reader).unwrap(),
            Some("y".repeat(64)),
            "刚好到格的帧不该被拒"
        );
    }

    #[test]
    fn a_wider_dial_keeps_a_frame_the_default_cap_would_refuse() {
        // 同一串没有换行的原始字节，比 1 MiB 大一点：缺省那一格与点名 1 MiB 的那一格都判跑飞。
        // 换构造写法时若把缺省那一格悄悄跟着放宽，头一臂就红。
        let blob = vec![b'z'; MAX_FRAME_BYTES + 4_096];
        for (tag, read_max) in [
            ("default-dial", None),
            ("one-mebibyte", Some(MAX_FRAME_BYTES)),
        ] {
            match verdict_of_raw_run(tag, read_max, &blob) {
                Err(FrameError::TooLong { got, limit }) => {
                    assert_eq!(limit, MAX_FRAME_BYTES, "{tag} 报的格子不是数据面那一格");
                    assert!(
                        got > limit,
                        "{tag} 没换行的那一段还没越过格子就判死了：{got}"
                    );
                }
                other => panic!("{tag} 该判这串字节跑飞，实际 {other:?}"),
            }
        }

        // 宽到 4 MiB 的那一格：同样的字节只当还没收完，换行到齐交出整帧。
        let (mut roomy, writer) = pair_by_dial("roomy", Some(4 << 20));
        let sent = blob.clone();
        let pump = thread::spawn(move || {
            let mut writer = writer;
            writer.write_raw(&sent).unwrap();
            writer.write_raw(b"\n").unwrap();
        });
        let line = read_until_quiet(&mut roomy)
            .expect("宽格子不该把合法长帧判死")
            .expect("换行到齐就该交出整帧");
        assert_eq!(line.len(), blob.len(), "交回来的不是完整那一帧");
        pump.join().unwrap();
    }

    #[test]
    fn wait_msg_delivers_the_last_frame_even_when_eof_arrives_with_it() {
        let name = PathBuf::from(format!(
            r"\\.\pipe\nomo-transport-test-last-{}",
            std::process::id()
        ));
        let mut listener = Listener::bind(&name).unwrap();
        let host_name = name.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_name, Duration::from_secs(5)).unwrap();
            host.send_line(r#"{"last":true}"#.to_string()).unwrap();
        });
        let mut plugin = loop {
            if let Some(stream) = listener.accept().unwrap() {
                break stream;
            }
            thread::sleep(Duration::from_millis(2));
        };
        let value = plugin.wait_msg(Duration::from_secs(5)).unwrap();
        assert_eq!(value, serde_json::json!({ "last": true }));
        host.join().unwrap();
    }
}
