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

impl Stream {
    /// 宿主侧拨进去（判据/自检里模拟宿主时才用；Nomo 作为插件走 [`Listener::accept`]）。
    pub fn connect(path: &Path, budget: Duration) -> Res<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;
            let sock = UnixStream::connect(path).map_err(|e| e.to_string())?;
            sock.set_nonblocking(true).map_err(|e| e.to_string())?;
            let _ = budget; // 连接预算：非阻塞 connect 已足够；保留签名与宿主对齐。
            Ok(Stream {
                sock,
                framer: Framer::default(),
                write_buf: Vec::new(),
            })
        }
        #[cfg(windows)]
        {
            let pipe = windows::Stream::connect(path, budget).map_err(|e| e.to_string())?;
            Ok(Stream {
                pipe,
                framer: Framer::default(),
            })
        }
    }

    /// 吸一格：把当前可读字节读进框帧缓冲。
    ///
    /// 交回 `false` **只有一种**含义——对端关闭（读到 EOF）。没数据（WouldBlock）交回 `true`。
    pub fn poll(&mut self) -> Res<bool> {
        #[cfg(unix)]
        {
            let mut buf = [0u8; READ_CHUNK];
            loop {
                match self.sock.read(&mut buf) {
                    Ok(0) => return Ok(false), // EOF。
                    Ok(n) => {
                        self.framer.push(&buf[..n]);
                        // 一次读尽量吸干净内核缓冲；再读若 WouldBlock 就收手（下一拍继续）。
                        if n < READ_CHUNK {
                            return Ok(true);
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(true),
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.to_string()),
                }
            }
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
