//! 数据面传输：JSON Lines over Unix socket（Linux）。宿主 `pg_core::ipc` 的原生复刻。
//!
//! 方向再钉一次：数据面上**插件是 server、宿主是主客户端** —— 绑这条 socket 的是 Nomo，拨进来的是宿主。
//! 控制面 `plugin.sock` 上宿主才是 server，那颗不归本模块。
//!
//! 这一层只管「字节从哪只句柄进出、怎么攒成一行」，**不解析消息内容**（那在 [`crate::plugin::envelope`]）。
//! 半条消息在这里只能是「还没收完」，不能被拼成一个默认合法的帧。
//!
//! ⚠ Windows named pipe 那一格今天**没接**：P1 是 Linux-first（宿主那侧 M8-2 已经做了 pipe 双形，但
//! Nomo 这发不假装支持）。非 unix 的 [`Stream`]／[`Listener`] 方法一律交回一句明确的 `Err`，不静默成功、
//! 不谎报——症状「插件起来了、宿主永远连不上」正是靠「宁可当场报错」挡住的。

// 两个 trait 只在 unix 那几臂的 socket 读写上被点兵；非 unix 构建整臂编掉，import 跟着走。
#[cfg(unix)]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::plugin::frame::{FrameError, Framer};

/// 传输层的失败。
pub type Res<T> = Result<T, String>;

/// 单帧缓冲：一次 socket 读最多吸这么多字节（读循环靠 [`crate::plugin::frame`] 的 1 MiB 闸兜底无界增长）。
const READ_CHUNK: usize = 64 * 1024;

/// 一条握过手的连接上的字节管道。
pub struct Stream {
    #[cfg(unix)]
    sock: std::os::unix::net::UnixStream,
    framer: Framer,
    #[allow(dead_code)]
    write_buf: Vec<u8>,
}

/// 数据面服务端句柄：绑住 `plugin-<id>.sock` 等宿主进来。
pub struct Listener {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixListener,
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
        #[cfg(not(unix))]
        {
            Err(format!(
                "Windows named pipe 数据面今天没接线，无法绑 `{}`",
                path.display()
            ))
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
        #[cfg(not(unix))]
        {
            Err("Windows named pipe 数据面今天没接线".to_string())
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
        #[cfg(not(unix))]
        {
            let _ = (path, budget);
            Err("Windows named pipe 数据面今天没接线".to_string())
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
        #[cfg(not(unix))]
        {
            Err("Windows named pipe 数据面今天没接线".to_string())
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
        #[cfg(unix)]
        {
            // mut 只归这一臂（append 换行的是它）；挂在签名上非 unix 构建就没人改它。
            let mut line = line;
            line.push('\n');
            let bytes = line.into_bytes();
            self.write_buf.extend_from_slice(&bytes);
            let mut written = 0usize;
            loop {
                let remaining = &self.write_buf[written..];
                if remaining.is_empty() {
                    self.write_buf.clear();
                    return Ok(());
                }
                match self.sock.write(remaining) {
                    Ok(n) => written += n,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if written > 0 {
                            self.write_buf.drain(..written);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        self.write_buf.clear();
                        return Err(e.to_string());
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = line;
            Err("Windows named pipe 数据面今天没接线".to_string())
        }
    }

    /// 发一个 `serde` 可序列化对象编成的那一帧。
    pub fn send<T: serde::Serialize>(&mut self, value: &T) -> Res<()> {
        let line = serde_json::to_string(value).map_err(|e| e.to_string())?;
        self.send_line(line)
    }

    /// 在预算内等**下一帧 JSON**（握手那一步宿主回 `HelloOk` 用的就是它；"没有事件循环的那一侧"才允许阻塞）。
    pub fn wait_msg(&mut self, budget: Duration) -> Res<serde_json::Value> {
        let deadline = Instant::now() + budget;
        loop {
            // 先取缓冲里已经攒好的完整帧。
            if let Some(value) = self.next_msg()? {
                return Ok(value);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "在 {} ms 预算内没等到一帧完整消息",
                    budget.as_millis()
                ));
            }
            // 再吸一次；对端关了判死（poll=false 只在这一格出现）。
            if !self.poll()? {
                return Err("等答复时连接被对端关了".to_string());
            }
            // 没吸到完整帧就让出一点，别把 CPU 烧满（下一圈回到顶部再取）。
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
