//! Windows 数据面：本机 named pipe 上的 JSON Lines 字节流。
//!
//! 插件是 server，宿主是 client。每只句柄都以 `FILE_FLAG_OVERLAPPED` 打开，接与读发出后按完成事件
//! 非阻塞收取；写与取消收净共用调用方预算，对端不吸字节时及时收尾。

use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_OPERATION_ABORTED, ERROR_PATH_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::plugin::frame::{Framer, MAX_FRAME_BYTES};

const BUFSIZE: usize = 8 * 1024;
const WRITE_WAIT_MS: u32 = 500;
const CANCEL_WAIT_MS: u32 = 2_000;

struct Handle(HANDLE);

// 管道句柄是进程级内核对象，跨线程读写与 CloseHandle 都合法。
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct Event(HANDLE);

impl Event {
    fn new() -> io::Result<Self> {
        let raw = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(raw))
    }

    fn done_now(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }

    fn wait_within(&self, ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// 内核持有其地址的异步操作。`OVERLAPPED` 与读写缓冲都放堆上，外层结构移动不会改动地址。
/// 缓冲归操作自己所有：取消超时只能泄掉整块，不能把仍在内核手里的缓冲先释放。
struct Op {
    ov: Box<OVERLAPPED>,
    done: Event,
    buffer: Option<Box<[u8]>>,
}

// OVERLAPPED、完成事件与缓冲都在堆上；内核只认稳定地址，不要求发起线程亲自回收。
unsafe impl Send for Op {}

impl Op {
    fn new(buffer: Option<Box<[u8]>>) -> io::Result<Self> {
        let done = Event::new()?;
        let mut ov = Box::new(OVERLAPPED::default());
        ov.hEvent = done.0;
        Ok(Self { ov, done, buffer })
    }

    fn ptr(&mut self) -> *mut OVERLAPPED {
        std::ptr::addr_of_mut!(*self.ov)
    }

    fn take_buffer(&mut self) -> Option<Box<[u8]>> {
        self.buffer.take()
    }

    fn result(&mut self, handle: HANDLE) -> io::Result<usize> {
        let mut transferred = 0u32;
        if unsafe { GetOverlappedResult(handle, self.ptr(), &mut transferred, 0) == 0 } {
            Err(io::Error::last_os_error())
        } else {
            Ok(transferred as usize)
        }
    }
}

/// 取消并收净一条异步操作，之后内核不再引用它的堆块。
fn reap(op: Op, handle: HANDLE) {
    reap_until(
        op,
        handle,
        Instant::now() + Duration::from_millis(CANCEL_WAIT_MS as u64),
    );
}

fn reap_until(mut op: Op, handle: HANDLE, deadline: Instant) {
    unsafe {
        CancelIoEx(handle, std::ptr::addr_of!(*op.ov));
        let wait_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(CANCEL_WAIT_MS as u128) as u32;
        if op.done.wait_within(wait_ms) {
            let _ = op.result(handle);
        } else {
            std::mem::forget(op);
        }
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn check_pipe_name(path: &Path) -> io::Result<()> {
    let name = path.to_string_lossy().to_ascii_lowercase();
    if !(name.starts_with(r"\\.\pipe\") || name.starts_with(r"\\?\pipe\")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Windows 数据面只认 named pipe 名：{}", path.display()),
        ));
    }
    Ok(())
}

fn create_instance(path: &Path) -> io::Result<Handle> {
    create_instance_with(path, false)
}

fn create_instance_first(path: &Path) -> io::Result<Handle> {
    create_instance_with(path, true)
}

fn create_instance_with(path: &Path, first_instance: bool) -> io::Result<Handle> {
    let name = wide(path);
    let open_mode = PIPE_ACCESS_DUPLEX
        | FILE_FLAG_OVERLAPPED
        | if first_instance {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    let raw = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFSIZE as u32,
            BUFSIZE as u32,
            0,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(Handle(raw))
    }
}

fn eof_code(code: u32) -> bool {
    matches!(
        code,
        ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED
    )
}

fn os_code(error: &io::Error) -> Option<u32> {
    error.raw_os_error().map(|code| code as u32)
}

pub struct Listener {
    path: PathBuf,
    waiting: Option<Handle>,
    connected: Option<Handle>,
    connect: Option<Op>,
}

impl Listener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        check_pipe_name(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            waiting: Some(create_instance_first(path)?),
            connected: None,
            connect: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn accept(&mut self) -> io::Result<Option<Stream>> {
        // 补等待实例失败后，下一次调用先重试；已经连上的实例留在 connected 里。
        if self.connect.is_none() && self.waiting.is_none() {
            if self.connected.is_some() {
                return self.hand_off().map(Some);
            }
            self.waiting = Some(create_instance(&self.path)?);
        }
        if let Some(mut op) = self.connect.take() {
            if !op.done.done_now() {
                self.connect = Some(op);
                return Ok(None);
            }
            let waiting = self.waiting.as_ref().ok_or_else(|| {
                io::Error::other("监听实例已关闭，不能继续收取连接")
            })?;
            let result = op.result(waiting.0);
            drop(op);
            if let Err(error) = result {
                return Err(self.rebuild_waiting(error));
            }
            self.connected = self.waiting.take();
            return self.hand_off().map(Some);
        }

        let waiting = self.waiting.as_ref().ok_or_else(|| {
            io::Error::other("监听实例已关闭，不能继续收取连接")
        })?;
        let mut op = Op::new(None)?;
        if unsafe { ConnectNamedPipe(waiting.0, op.ptr()) } != 0 {
            self.connected = self.waiting.take();
            return self.hand_off().map(Some);
        }
        let error = io::Error::last_os_error();
        match os_code(&error) {
            Some(ERROR_PIPE_CONNECTED) => {
                self.connected = self.waiting.take();
                self.hand_off().map(Some)
            }
            Some(ERROR_IO_PENDING) => {
                self.connect = Some(op);
                Ok(None)
            }
            _ => Err(self.rebuild_waiting(error)),
        }
    }

    fn rebuild_waiting(&mut self, error: io::Error) -> io::Error {
        let fresh = create_instance(&self.path);
        self.rebuild_waiting_with(error, fresh)
    }

    fn rebuild_waiting_with(
        &mut self,
        error: io::Error,
        fresh: io::Result<Handle>,
    ) -> io::Error {
        match fresh {
            Ok(fresh) => {
                self.waiting = Some(fresh);
                io::Error::new(error.kind(), format!("{error}；已补上等待实例"))
            }
            Err(fresh_error) => io::Error::new(
                error.kind(),
                format!("{error}；补等待实例也失败：{fresh_error}"),
            ),
        }
    }

    fn hand_off(&mut self) -> io::Result<Stream> {
        self.hand_off_with(create_instance(&self.path))
    }

    fn hand_off_with(&mut self, fresh: io::Result<Handle>) -> io::Result<Stream> {
        let accepted = self
            .connected
            .take()
            .ok_or_else(|| io::Error::other("没有可交出的已连接实例"))?;
        if self.waiting.is_none() {
            if let Ok(fresh) = fresh {
                self.waiting = Some(fresh);
            }
        }
        Ok(Stream::new(accepted))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(op) = self.connect.take() {
            if let Some(waiting) = self.waiting.as_ref() {
                reap(op, waiting.0);
            }
        }
    }
}

pub struct Stream {
    inner: Handle,
    pending: Option<Op>,
}

impl Stream {
    fn new(inner: Handle) -> Self {
        Self {
            inner,
            pending: None,
        }
    }

    pub fn connect(path: &Path, budget: Duration) -> io::Result<Self> {
        check_pipe_name(path)?;
        let name = wide(path);
        let deadline = Instant::now() + budget;
        loop {
            let raw = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if raw != INVALID_HANDLE_VALUE {
                return Ok(Self::new(Handle(raw)));
            }
            let error = io::Error::last_os_error();
            if matches!(
                os_code(&error),
                Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND | ERROR_PIPE_BUSY)
            ) {
                if Instant::now() >= deadline {
                    return Err(error);
                }
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            return Err(error);
        }
    }

    pub fn poll(&mut self, framer: &mut Framer) -> io::Result<bool> {
        if let Some(mut op) = self.pending.take() {
            if !op.done.done_now() {
                self.pending = Some(op);
                return Ok(true);
            }
            let result = op.result(self.inner.0);
            let buffer = op.take_buffer().expect("在飞读必须带缓冲");
            match result {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    framer.push(&buffer[..n]);
                    // 每次有进展就交回调用方，让它及时检查帧上限与等待预算。
                    return Ok(true);
                }
                Err(error) => {
                    return if eof_code(os_code(&error).unwrap_or(0)) {
                        Ok(false)
                    } else {
                        Err(error)
                    };
                }
            }
        }

        let buffer: Box<[u8]> = vec![0u8; BUFSIZE].into_boxed_slice();
        let mut op = Op::new(Some(buffer))?;
        let (ptr, len) = {
            let buffer = op.buffer.as_mut().expect("新读必须带缓冲");
            (buffer.as_mut_ptr(), buffer.len() as u32)
        };
        let mut got = 0u32;
        if unsafe { ReadFile(self.inner.0, ptr, len, &mut got, op.ptr()) } != 0 {
            let buffer = op.take_buffer().expect("同步读必须带缓冲");
            if got == 0 {
                return Ok(false);
            }
            framer.push(&buffer[..got as usize]);
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match os_code(&error) {
            Some(ERROR_IO_PENDING) => {
                self.pending = Some(op);
                Ok(true)
            }
            Some(code) if eof_code(code) => Ok(false),
            _ => Err(error),
        }
    }

    pub fn send_line(&mut self, line: String) -> io::Result<()> {
        self.send_line_until(line, Instant::now() + Duration::from_secs(5))
    }

    pub fn send_line_until(&mut self, line: String, deadline: Instant) -> io::Result<()> {
        if line.len() > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("帧超过 {MAX_FRAME_BYTES} 字节上限：{}", line.len()),
            ));
        }
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        self.send_bytes_until(&bytes, deadline)
    }

    #[cfg(test)]
    fn send_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send_bytes_until(bytes, Instant::now() + Duration::from_secs(5))
    }

    fn send_bytes_until(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<()> {
        let mut sent = 0usize;
        while sent < bytes.len() {
            let wait_ms = deadline.saturating_duration_since(Instant::now());
            if wait_ms.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "发送一整帧的预算用完了",
                ));
            }
            let buffer: Box<[u8]> = bytes[sent..].to_vec().into_boxed_slice();
            let mut op = Op::new(Some(buffer))?;
            let (ptr, len) = {
                let buffer = op.buffer.as_ref().expect("新写必须带缓冲");
                (buffer.as_ptr(), buffer.len() as u32)
            };
            let mut written = 0u32;
            let done = unsafe {
                WriteFile(self.inner.0, ptr, len, &mut written, op.ptr())
            };
            if done != 0 {
                if written == 0 {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "pipe 写入 0 字节"));
                }
                sent += written as usize;
                continue;
            }
            let error = io::Error::last_os_error();
            if os_code(&error) != Some(ERROR_IO_PENDING) {
                return Err(error);
            }
            let this_wait = wait_ms.min(Duration::from_millis(WRITE_WAIT_MS as u64));
            let wait_ms = this_wait.as_millis().clamp(1, WRITE_WAIT_MS as u128) as u32;
            if !op.done.wait_within(wait_ms) {
                reap_until(op, self.inner.0, deadline);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("对端在 {wait_ms}ms 内没有吸完 pipe 字节"),
                ));
            }
            let written = op.result(self.inner.0)?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pipe 异步写入 0 字节",
                ));
            }
            sent += written;
        }
        Ok(())
    }

    /// 移交线程前收净挂起读，使下一条读由接手线程发起。
    pub fn settle(&mut self, framer: &mut Framer) -> io::Result<()> {
        if let Some(mut op) = self.pending.take() {
            if !op.done.done_now() {
                unsafe { CancelIoEx(self.inner.0, op.ptr()) };
                if !op.done.wait_within(CANCEL_WAIT_MS) {
                    std::mem::forget(op);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("取消在飞读后 {CANCEL_WAIT_MS}ms 内没有落地"),
                    ));
                }
            }
            match op.result(self.inner.0) {
                Ok(0) => {}
                Ok(n) => {
                    let buffer = op.take_buffer().expect("在飞读必须带缓冲");
                    framer.push(&buffer[..n]);
                }
                Err(error) => {
                    let code = os_code(&error).unwrap_or(0);
                    if code != ERROR_OPERATION_ABORTED && !eof_code(code) {
                        return Err(error);
                    }
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send_bytes(bytes)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Some(op) = self.pending.take() {
            reap(op, self.inner.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::plugin::frame::FrameError;

    fn pipe(tag: &str) -> PathBuf {
        PathBuf::from(format!(
            r"\\.\pipe\nomo-plugin-test-{tag}-{}",
            std::process::id()
        ))
    }

    fn accept_ready(listener: &mut Listener) -> Stream {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(stream) = listener.accept().unwrap() {
                return stream;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for pipe client"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_connected_instance_survives_a_failed_replenishment() {
        let name = pipe("replenish");
        let mut listener = Listener::bind(&name).unwrap();
        let accepted = listener.waiting.take().unwrap();
        let accepted_handle = accepted.0;
        listener.connected = Some(accepted);

        let stream = listener
            .hand_off_with(Err(io::Error::other("补实例失败")))
            .expect("补等待实例失败时仍要交出已经连上的实例");
        assert_eq!(stream.inner.0, accepted_handle);
        assert!(listener.waiting.is_none());
        assert!(listener.connected.is_none());

        assert!(
            matches!(listener.accept(), Ok(None)),
            "下一次 accept 要补上等待实例并继续监听"
        );
    }

    #[test]
    fn a_second_listener_cannot_share_the_same_pipe_name() {
        let name = pipe("duplicate-bind");
        let mut first = Listener::bind(&name).unwrap();
        let Err(error) = Listener::bind(&name) else {
            panic!("同名 pipe 已有监听时第二个 server 必须被拒绝");
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");

        let mut host = Stream::connect(&name, Duration::from_secs(5)).unwrap();
        let mut plugin = accept_ready(&mut first);
        plugin.send_line("first still listening".to_string()).unwrap();
        assert_eq!(host.wait_for_test(5), "first still listening");
    }

    #[test]
    fn a_failed_rebuild_keeps_the_existing_waiting_instance() {
        let name = pipe("rebuild-keeps-old");
        let mut listener = Listener::bind(&name).unwrap();
        let before = listener.waiting.as_ref().unwrap().0;
        let error = listener.rebuild_waiting_with(
            io::Error::other("ConnectNamedPipe 失败"),
            Err(io::Error::other("补实例失败")),
        );
        assert!(error.to_string().contains("补等待实例也失败"), "{error}");
        assert_eq!(
            listener.waiting.as_ref().unwrap().0,
            before,
            "新实例没建出来时不能先关掉旧监听"
        );

        let mut host = Stream::connect(&name, Duration::from_secs(5)).unwrap();
        let mut plugin = accept_ready(&mut listener);
        plugin.send_line("old instance still listening".to_string()).unwrap();
        assert_eq!(host.wait_for_test(5), "old instance still listening");
    }

    #[test]
    fn accepts_byte_pipe_roundtrip_and_eof() {
        let name = pipe("roundtrip");
        let mut listener = Listener::bind(&name).unwrap();
        let connect_name = name.clone();
        let host = thread::spawn(move || {
            let mut stream = Stream::connect(&connect_name, Duration::from_secs(5)).unwrap();
            stream.send_line(r#"{"hello":"世界"}"#.to_string()).unwrap();
            let reply = stream.wait_for_test(5);
            drop(stream);
            reply
        });
        let mut plugin = accept_ready(&mut listener);
        let line = plugin.wait_for_test(5);
        plugin.send_line(line).unwrap();
        assert_eq!(host.join().unwrap(), r#"{"hello":"世界"}"#);
    }

    #[test]
    fn handoff_keeps_listening_and_last_frame_survives_eof() {
        let name = pipe("last-frame");
        let mut listener = Listener::bind(&name).unwrap();
        let connect_name = name.clone();
        let host = thread::spawn(move || {
            let mut stream = Stream::connect(&connect_name, Duration::from_secs(5)).unwrap();
            stream.send_line("last".to_string()).unwrap();
        });
        let mut plugin = accept_ready(&mut listener);
        assert_eq!(plugin.wait_for_test(5), "last");
        host.join().unwrap();

        let connect_name = name.clone();
        let host = thread::spawn(move || {
            let mut stream = Stream::connect(&connect_name, Duration::from_secs(5)).unwrap();
            stream.write_raw(b"second\n").unwrap();
        });
        let mut plugin = accept_ready(&mut listener);
        let mut framer = Framer::default();
        loop {
            if !plugin.poll(&mut framer).unwrap() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        host.join().unwrap();
        assert_eq!(framer.next_line().unwrap().as_deref(), Some("second"));
    }

    #[test]
    fn half_frame_is_not_delivered_until_newline_arrives() {
        let name = pipe("half");
        let mut listener = Listener::bind(&name).unwrap();
        let connect_name = name.clone();
        let host = thread::spawn(move || {
            let mut stream = Stream::connect(&connect_name, Duration::from_secs(5)).unwrap();
            stream.write_raw(b"half").unwrap();
            thread::sleep(Duration::from_millis(30));
            stream.write_raw(b" done\n").unwrap();
        });
        let mut plugin = accept_ready(&mut listener);
        let mut framer = Framer::default();
        assert!(plugin.poll(&mut framer).unwrap());
        assert_eq!(framer.next_line().unwrap(), None);
        thread::sleep(Duration::from_millis(60));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let open = plugin.poll(&mut framer).unwrap();
            if framer.next_line().unwrap().as_deref() == Some("half done") {
                break;
            }
            assert!(open, "夹具在补齐半帧前关了");
            assert!(Instant::now() < deadline, "补齐半帧超时");
            thread::sleep(Duration::from_millis(2));
        }
        host.join().unwrap();
    }

    #[test]
    fn an_oversized_outbound_frame_is_refused_before_writing() {
        let name = pipe("outbound-cap");
        let mut listener = Listener::bind(&name).unwrap();
        let mut host = Stream::connect(&name, Duration::from_secs(5)).unwrap();
        let mut plugin = accept_ready(&mut listener);
        let deadline = Instant::now() + Duration::from_secs(1);
        let error = plugin
            .send_line_until(
                "x".repeat(MAX_FRAME_BYTES + 1),
                deadline,
            )
            .expect_err("Windows 底层也要在写之前拒掉超限帧");
        assert!(error.to_string().contains(&MAX_FRAME_BYTES.to_string()), "{error}");

        plugin
            .send_line_until(r#"{"ok":true}"#.to_string(), deadline)
            .unwrap();
        assert_eq!(host.wait_for_test(5), r#"{"ok":true}"#);
    }

    #[test]
    fn a_cancellation_that_never_completes_stays_inside_the_deadline() {
        let op = Op::new(None).unwrap();
        let started = Instant::now();
        reap_until(op, INVALID_HANDLE_VALUE, started + Duration::from_millis(30));
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "取消收净用了 {:?}，不能越过整帧写的截止时间",
            started.elapsed()
        );
    }

    #[test]
    fn a_peer_that_never_reads_times_out_instead_of_blocking_forever() {
        let name = pipe("write-timeout");
        let mut listener = Listener::bind(&name).unwrap();
        let connect_name = name.clone();
        let host =
            thread::spawn(move || Stream::connect(&connect_name, Duration::from_secs(5)).unwrap());
        let mut plugin = accept_ready(&mut listener);
        let peer = host.join().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = plugin.send_bytes(&vec![0u8; 1024 * 1024]);
            let _ = tx.send(result.map(|_| ()));
        });
        let result = rx
            .recv_timeout(Duration::from_secs(4))
            .expect("对端不读时写侧必须自己收尾，不能永久挂住");
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        drop(peer);
    }

    #[test]
    fn a_continuous_unterminated_stream_stops_at_the_frame_cap() {
        let name = pipe("frame-cap");
        let mut listener = Listener::bind(&name).unwrap();
        let connect_name = name.clone();
        let host = thread::spawn(move || {
            let mut stream = Stream::connect(&connect_name, Duration::from_secs(5)).unwrap();
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..128 {
                if stream.write_raw(&chunk).is_err() {
                    return;
                }
            }
        });
        let plugin = accept_ready(&mut listener);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut plugin = plugin;
            let mut framer = Framer::default();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(plugin.poll(&mut framer).unwrap(), "夹具提前关了");
                match framer.next_line() {
                    Err(FrameError::TooLong { got, limit }) => {
                        let _ = tx.send((got, limit));
                        return;
                    }
                    Ok(Some(_)) => panic!("没有换行不该交帧"),
                    Ok(None) => {}
                    Err(error) => panic!("unexpected frame error: {error:?}"),
                }
                assert!(Instant::now() < deadline, "持续读取没有及时交回帧闸");
                thread::sleep(Duration::from_millis(1));
            }
        });
        let (got, limit) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("持续无换行字节流必须及时触发帧上限，不能挂在 poll 里");
        assert_eq!(limit, MAX_FRAME_BYTES);
        assert!(
            got > MAX_FRAME_BYTES && got <= MAX_FRAME_BYTES + BUFSIZE,
            "一次 poll 最多多读一格，实际读到 {got} 字节"
        );
        host.join().unwrap();
    }

    #[test]
    fn rejects_non_pipe_path() {
        assert!(Listener::bind(Path::new(r"C:\temp\plugin.sock")).is_err());
        assert!(Stream::connect(Path::new("relative.sock"), Duration::ZERO).is_err());
    }

    impl Stream {
        fn wait_for_test(&mut self, seconds: u64) -> String {
            let deadline = Instant::now() + Duration::from_secs(seconds);
            let mut framer = Framer::default();
            loop {
                if let Some(line) = framer.next_line().unwrap() {
                    return line;
                }
                let open = self.poll(&mut framer).unwrap();
                if let Some(line) = framer.next_line().unwrap() {
                    return line;
                }
                assert!(open, "fixture closed too early");
                if Instant::now() >= deadline {
                    panic!("timed out");
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
}
