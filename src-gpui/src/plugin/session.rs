//! 数据面那一侧的服务端三步：绑上、握手、逐条答复（宿主 `pg-plugin-client::runtime` 的原生复刻）。
//!
//! [`Server`] 绑 `plugin-<id>.sock` 等宿主拨进来；[`Session`] 是一条握过手的连接，按 [`Session::pump`]
//! 一帧一帧地办、按 [`Session::request`] 向宿主发 `host.*` 请求。方向与 [`crate::plugin::transport`]
//! 头上钉的一致：绑这一条的是插件（Nomo），拨过来的是宿主。
//!
//! ⚠ **`pump` 不许阻塞 UI 线程**：它是常驻那一步，占住线程＝把窗口冻住。非阻塞靠 transport 的
//! `poll`（只吸当前可读字节）+ `next_msg`（只取已完整那一帧）。没活交回 [`Step::Idle`]。
//! 而 `accept` 里那次握手**允许**阻塞到 `hello_budget`：握手是「起来那一下」的事，宿主最多等 5 s 就判死
//! 这条连接，Slint/GPUI 循环还没转起来时它本来就该占着这根线程。
//!
//! ⚠ **`host_version` 由调用方给，本模块不猜**：运行时从宿主 spawn 的环境变量读取，缺位留空。

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::plugin::dispatch::{answer_of, Dispatch, HostReply, Step};
use crate::plugin::envelope::Envelope;
use crate::plugin::handshake::{Hello, HelloOk, PROTOCOL};
use crate::plugin::transport::{Listener, Stream, WaitError};

/// 这一颗插件自报的那三位（`Hello` 里除 `protocol`／`pid` 以外的全部来源）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub plugin_id: String,
    pub plugin_version: String,
    pub host_version: String,
}

/// 数据面那一侧的服务端：绑上 `plugin-<id>.sock`，等宿主进来。
pub struct Server {
    listener: Listener,
    identity: Identity,
    hello_budget: Duration,
}

impl Server {
    /// 绑上数据面那个名字。`hello_budget` 是等宿主回 `HelloOk` 的上限（宿主侧 `link::open` 给自己的
    /// 是 5000 ms，两边同一档才不会出现「宿主已经走了、插件还在等」）。
    pub fn bind(
        data_socket: &Path,
        identity: Identity,
        hello_budget: Duration,
    ) -> Result<Self, String> {
        let listener = Listener::bind(data_socket)
            .map_err(|e| format!("绑数据面 {} 失败：{e}", data_socket.display()))?;
        Ok(Self {
            listener,
            identity,
            hello_budget,
        })
    }

    /// 绑的是哪个名字（判据与诊断核对「env 给的那条与真绑上的那条是同一根」）。
    pub fn path(&self) -> &Path {
        self.listener.path()
    }

    /// 非阻塞接一条，并把握手走完。没有客户端在等 ⇒ `Ok(None)`（不是错）。
    ///
    /// 握手失败（对面不答、答的不是 `HelloOk`、预算用尽）交 `Err` —— 是「再等下一条连接」还是「退出」
    /// 由调用方定，本模块不替它选。
    pub fn accept(&mut self) -> Result<Option<Session>, String> {
        let Some(stream) = self.listener.accept()? else {
            return Ok(None);
        };
        Session::open(stream, &self.identity, self.hello_budget).map(Some)
    }
}

/// 插件向宿主发请求失败：超时只是这一趟通知落空，传输坏了则整条连接必须作废。
#[derive(Debug)]
pub enum RequestError {
    TimedOut(String),
    Broken(String),
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut(message) | Self::Broken(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RequestError {}

/// 一条握过手的连接。
pub struct Session {
    stream: Stream,
    accepted: HelloOk,
    identity: Identity,
    /// 下一发 `host.*` 用哪个关联号（从 1 起；同一连接上插件这一侧一次只有一趟在飞）。
    next_request: u64,
}

impl Session {
    fn open(
        mut stream: Stream,
        identity: &Identity,
        hello_budget: Duration,
    ) -> Result<Self, String> {
        let hello = Hello {
            protocol: PROTOCOL,
            plugin_id: identity.plugin_id.clone(),
            plugin_version: identity.plugin_version.clone(),
            host_version: identity.host_version.clone(),
            pid: std::process::id(),
        };
        let deadline = Instant::now() + hello_budget;
        stream
            .send_with_budget(&hello, hello_budget)
            .map_err(|e| format!("把 Hello 发给宿主时断了：{e}"))?;
        let value = stream
            .wait_msg(deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| format!("在预算内没等到宿主的 HelloOk：{e}"))?;
        let accepted: HelloOk =
            serde_json::from_value(value).map_err(|e| format!("宿主的 HelloOk 解不开：{e}"))?;
        // ⚠ 插件这一侧也判一次号：宿主先按它的 Expectation 判插件报的号，收到回应若不再判，
        // 「我认 1、你用 2」这件事就只有一头知道——而它的症状是后面每帧都解得开、每件事都办不对。
        if accepted.protocol != PROTOCOL {
            return Err(format!(
                "宿主用的协议号是 {}，与这一颗插件认的 {PROTOCOL} 不是同一档 ⇒ 这条连接上别办事",
                accepted.protocol
            ));
        }
        // Windows 的挂起读归属发起线程；交给泵线程之前先收净，已到的字节仍压进框帧器。
        stream.settle().map_err(|e| format!("握手后收净数据面读失败：{e}"))?;
        Ok(Self {
            stream,
            accepted,
            identity: identity.clone(),
            next_request: 1,
        })
    }

    /// 宿主在 `HelloOk` 里认可的那份能力（清单那一份原样）。
    pub fn capabilities(&self) -> &[String] {
        &self.accepted.capabilities
    }

    /// 宿主给的数据目录（与 spawn env 那格是同一层；从回应里读一次，让「宿主以为我在哪」有个读数）。
    pub fn data_dir(&self) -> &str {
        &self.accepted.data_dir
    }

    /// 宿主那一侧正在用的协议号（**不是**自己报出去的那一颗）。
    pub fn host_protocol(&self) -> u32 {
        self.accepted.protocol
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// 吸一格：读到的那一帧若是请求，交处理器并把答复发回去。非阻塞。
    ///
    /// ⚠ **解不回成 `Envelope` 的那一帧一律丢掉**（不是错、也不回话）：回话要有 `request` 号，而号就在
    /// 那一帧解不开的地方。能解出形来的坏帧由 `Envelope::from_value` 判成 `Err`，这里同样丢（回不了话）。
    pub fn pump<H>(&mut self, handler: &mut H) -> Result<Step, String>
    where
        H: FnMut(&str, Value) -> Dispatch,
    {
        let open = self.stream.poll()?;
        while let Some(value) = self.stream.next_msg()? {
            let Ok(envelope) = Envelope::from_value(&value) else {
                continue;
            };
            let Envelope::Request {
                request,
                op,
                payload,
            } = envelope
            else {
                continue;
            };
            let answer = answer_of(request, handler(&op, payload));
            self.stream
                .send(&answer.to_value())
                .map_err(|e| format!("第 {request} 号（{op}）的答复没发出去：{e}"))?;
            return Ok(Step::Served { request, op });
        }
        if open {
            Ok(Step::Idle)
        } else {
            Ok(Step::Closed)
        }
    }

    /// 插件这一侧发一条 `host.*` 请求并等它那一条答复。
    ///
    /// 等答复的空档里宿主若发来一条请求，照 [`Session::pump`] 那一形交给 `handler` 并当场回话 ——
    /// 宿主那一侧正卡在这趟往返上，不回话就是两边互等。预算用完只是这一趟落空；发送、读取或反向答复
    /// 发送失败都证明字节流不能再用，交 [`RequestError::Broken`]。配不回号的答复是上一趟留下的尾巴，丢掉。
    pub fn request<H>(
        &mut self,
        handler: &mut H,
        op: &str,
        payload: Value,
        budget: Duration,
    ) -> Result<HostReply, RequestError>
    where
        H: FnMut(&str, Value) -> Dispatch,
    {
        let request = self.next_request;
        self.next_request += 1;
        let deadline = Instant::now() + budget;
        self.stream
            .send_with_budget(
                &Envelope::Request {
                    request,
                    op: op.to_string(),
                    payload,
                }
                .to_value(),
                budget,
            )
            .map_err(|e| {
                RequestError::Broken(format!("第 {request} 号（{op}）请求没发出去：{e}"))
            })?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RequestError::TimedOut(format!(
                    "宿主无响应：{} ms 内没答第 {request} 号（{op}）",
                    budget.as_millis()
                )));
            }
            let value = self.stream.wait_msg_typed(remaining).map_err(|error| match error {
                WaitError::TimedOut => RequestError::TimedOut(format!(
                    "宿主无响应：{} ms 内没答第 {request} 号（{op}）",
                    remaining.as_millis()
                )),
                WaitError::Broken(error) => RequestError::Broken(format!(
                    "等第 {request} 号（{op}）的答复时连接或字节流坏了：{error}"
                )),
            })?;
            let Ok(envelope) = Envelope::from_value(&value) else {
                continue;
            };
            match envelope {
                Envelope::Reply {
                    request: got,
                    payload,
                } if got == request => return Ok(HostReply::Done(payload)),
                Envelope::Failure {
                    request: got,
                    code,
                    message,
                } if got == request => {
                    return Ok(HostReply::Failed { code, message })
                }
                Envelope::Request {
                    request: reverse,
                    op,
                    payload,
                } => {
                    let answer = answer_of(reverse, handler(&op, payload));
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    self.stream
                        .send_with_budget(&answer.to_value(), remaining)
                        .map_err(|e| {
                            RequestError::Broken(format!(
                                "第 {reverse} 号（{op}）的答复没发出去：{e}"
                            ))
                        })?;
                }
                // 上一趟超时/断掉留下的答复尾巴：号配不上，也不为它判连接错位。
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::thread;

    use serde_json::json;

    use super::*;
    use crate::plugin::handshake::Hello;

    fn identity() -> Identity {
        Identity {
            plugin_id: "notes".to_string(),
            plugin_version: "0.1.0".to_string(),
            host_version: "0.9.131".to_string(),
        }
    }

    /// 一条私有 socket 名字（判据专用，绝不指到用户桌面上真在跑的那颗）。
    fn site_socket(tag: &str) -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(format!(
                r"\\.\pipe\nomo-session-test-{tag}-{}",
                std::process::id()
            ))
        }
        #[cfg(not(windows))]
        {
            let dir = std::env::temp_dir().join(format!("nomo-plugin-{tag}-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            dir.join(format!("plugin-{tag}.sock"))
        }
    }

    fn cleanup_site(_path: &Path) {
        #[cfg(not(windows))]
        {
            let _ = std::fs::remove_file(_path);
            let _ = std::fs::remove_dir_all(_path.parent().unwrap());
        }
    }

    /// 把一颗正在听、且已经握完手的插件搬上另一条线程，办过的条数留在交件里。
    fn serve(server: Server) -> thread::JoinHandle<usize> {
        thread::spawn(move || {
            let mut server = server;
            let mut session = loop {
                match server.accept() {
                    Ok(Some(session)) => break session,
                    Ok(None) => thread::sleep(Duration::from_millis(5)),
                    Err(e) => panic!("夹具这一趟握手坏了：{e}"),
                }
            };
            let mut served = 0usize;
            loop {
                let step = session
                    .pump(&mut |_op, payload| Dispatch::Done(json!({ "echo": payload })))
                    .expect("夹具办事这一格坏了");
                match step {
                    Step::Served { .. } => served += 1,
                    Step::Idle => thread::sleep(Duration::from_millis(2)),
                    Step::Closed => return served,
                }
            }
        })
    }

    /// 端到端一跑：绑 socket → 插件先报 `Hello`、宿主答 `HelloOk`（两侧各判一次号）→ 宿主发一条
    /// `notes.open` 请求 → 插件 `pump` 办成并把答复发回 → 关联号对上、载荷逐字回来。
    #[test]
    fn loopback_handshake_then_serves_one_request() {
        let socket = site_socket("loop");
        let server = Server::bind(&socket, identity(), Duration::from_secs(5)).expect("夹具绑得上数据面");
        assert_eq!(server.path(), socket.as_path(), "绑上的名字该就是 env 会指的那条");
        let handle = serve(server);

        // 宿主侧：拨进去 → 读插件的 Hello（含它自己的 pid）→ 判号 → 答 HelloOk。
        let mut host = Stream::connect(&socket, Duration::from_secs(5)).expect("宿主拨不进数据面");
        let hello: Hello = serde_json::from_value(host.wait_msg(Duration::from_secs(5)).unwrap())
            .expect("Hello 那一帧该解得开");
        assert_eq!(hello.protocol, PROTOCOL);
        assert_eq!(hello.plugin_id, "notes");
        assert_eq!(hello.plugin_version, "0.1.0");
        assert_eq!(hello.host_version, "0.9.131");
        assert_eq!(hello.pid, std::process::id(), "pid 报的不是这一颗进程");

        let ok = HelloOk {
            protocol: PROTOCOL,
            capabilities: vec!["notes.open".into()],
            data_dir: "/data/plugins-data/notes".into(),
        };
        host.send(&ok).expect("HelloOk 没发出去");

        // 宿主发一条转发请求；插件 pump 回话；读回那帧答复。
        let request = Envelope::Request {
            request: 7,
            op: "notes.open".to_string(),
            payload: json!({ "note_id": 3 }),
        };
        host.send(&request.to_value()).expect("请求没发出去");
        let reply = Envelope::from_value(&host.wait_msg(Duration::from_secs(5)).unwrap())
            .expect("答复那一帧解不开");
        match reply {
            Envelope::Reply { request: got, payload } => {
                assert_eq!(got, 7, "答复的关联号要对上请求");
                assert_eq!(payload["echo"]["note_id"], json!(3), "载荷要逐字带回来");
            }
            other => panic!("解成了 {other:?}"),
        }

        drop(host);
        let served = handle.join().expect("夹具线程不该 panic");
        assert_eq!(served, 1, "正好办成一条");
        cleanup_site(&socket);
    }

    /// 宿主发完最后一请求就关线：先吸到 EOF，`pump` 仍要先办缓冲里的请求，不能提前报关闭。
    #[test]
    fn a_buffered_request_is_handled_even_when_eof_arrives_first() {
        let socket = site_socket("buffered-eof");
        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();
        let host_socket = socket.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            let _hello = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&HelloOk {
                protocol: PROTOCOL,
                capabilities: vec!["notes.open".into()],
                data_dir: "/data/plugins-data/notes".into(),
            })
            .unwrap();
            host.send(
                &(Envelope::Request {
                    request: 23,
                    op: "notes.open".to_string(),
                    payload: json!({ "note_id": 19 }),
                })
                .to_value(),
            )
            .unwrap();
        });

        let mut session = handshake_session(&mut server);
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.stream.poll().expect("收 EOF 前字节不该失败") {
            assert!(Instant::now() < deadline, "夹具没有及时关线");
            thread::sleep(Duration::from_millis(2));
        }
        host.join().unwrap();

        let mut handled = false;
        let result = session.pump(&mut |op, payload| {
            handled = true;
            assert_eq!(op, "notes.open");
            assert_eq!(payload["note_id"], json!(19));
            Dispatch::Done(json!({ "handled": true }))
        });
        assert!(handled, "EOF 不能吞掉关线前已经收进缓冲的请求");
        assert!(
            matches!(result, Ok(Step::Served { request: 23, .. }) | Err(_)),
            "请求要么办成，要么因对端已关而回不出答复：{result:?}"
        );
        cleanup_site(&socket);
    }

    /// 第一趟没回话只算通知超时；同一连接上的第二趟仍能正常往返，不能把超时误判成断连。
    #[test]
    fn request_timeout_keeps_the_session_usable() {
        let socket = site_socket("request-timeout");
        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();
        let host_socket = socket.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            let _hello = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&HelloOk {
                protocol: PROTOCOL,
                capabilities: vec![],
                data_dir: "/data/plugins-data/notes".into(),
            })
            .unwrap();
            let first = host.wait_msg(Duration::from_secs(5)).unwrap();
            assert_eq!(first["request"], json!(1));
            thread::sleep(Duration::from_millis(80));
            let second = host.wait_msg(Duration::from_secs(5)).unwrap();
            assert_eq!(second["request"], json!(2));
            host.send(&(Envelope::Reply {
                request: 2,
                payload: json!({ "ok": true }),
            }).to_value())
            .unwrap();
        });

        let mut session = handshake_session(&mut server);
        let first = session.request(
            &mut |_op, _payload| Dispatch::Done(json!({})),
            "host.notes_changed",
            json!({ "kind": "created" }),
            Duration::from_millis(30),
        );
        assert!(matches!(first, Err(RequestError::TimedOut(_))), "第一趟该只超时：{first:?}");
        let second = session.request(
            &mut |_op, _payload| Dispatch::Done(json!({})),
            "host.notes_changed",
            json!({ "kind": "renamed" }),
            Duration::from_secs(2),
        );
        assert_eq!(second.unwrap(), HostReply::Done(json!({ "ok": true })));
        host.join().unwrap();
        cleanup_site(&socket);
    }

    /// 宿主读完反向请求就关连接：等待答复时收到 EOF，必须归入连接损坏而不是通知超时。
    #[test]
    fn request_reports_peer_eof_as_broken() {
        let socket = site_socket("request-eof");
        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();
        let host_socket = socket.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            let _hello = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&HelloOk {
                protocol: PROTOCOL,
                capabilities: vec![],
                data_dir: "/data/plugins-data/notes".into(),
            })
            .unwrap();
            let _request = host.wait_msg(Duration::from_secs(5)).unwrap();
        });

        let mut session = handshake_session(&mut server);
        let result = session.request(
            &mut |_op, _payload| Dispatch::Done(json!({})),
            "host.notes_changed",
            json!({ "kind": "created" }),
            Duration::from_secs(2),
        );
        assert!(matches!(result, Err(RequestError::Broken(_))), "EOF 该判连接坏：{result:?}");
        host.join().unwrap();
        cleanup_site(&socket);
    }

    /// 宿主握手后先关：Windows 管道的首发写就该失败，不能把它误记成「通知超时」。
    #[cfg(windows)]
    #[test]
    fn request_reports_initial_send_failure_as_broken() {
        let socket = site_socket("request-send-eof");
        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();
        let host_socket = socket.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            let _hello = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&HelloOk {
                protocol: PROTOCOL,
                capabilities: vec![],
                data_dir: "/data/plugins-data/notes".into(),
            })
            .unwrap();
        });

        let mut session = handshake_session(&mut server);
        host.join().unwrap();
        let result = session.request(
            &mut |_op, _payload| Dispatch::Done(json!({})),
            "host.notes_changed",
            json!({ "kind": "created" }),
            Duration::from_secs(2),
        );
        assert!(matches!(result, Err(RequestError::Broken(_))), "首发失败该判连接坏：{result:?}");
        cleanup_site(&socket);
    }

    /// 宿主等插件发出反向请求后，回一条反向请求再关：反向答复写失败也必须判连接坏。
    #[cfg(windows)]
    #[test]
    fn request_reports_reverse_reply_failure_as_broken() {
        let socket = site_socket("request-reverse-eof");
        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();
        let host_socket = socket.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            let _hello = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&HelloOk {
                protocol: PROTOCOL,
                capabilities: vec![],
                data_dir: "/data/plugins-data/notes".into(),
            })
            .unwrap();
            let _request = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&(Envelope::Request {
                request: 99,
                op: "folders.snapshot".to_string(),
                payload: json!({}),
            })
            .to_value())
            .unwrap();
        });

        let mut session = handshake_session(&mut server);
        let mut handled = false;
        let result = session.request(
            &mut |_op, _payload| {
                handled = true;
                Dispatch::Done(json!({ "ok": true }))
            },
            "host.notes_changed",
            json!({ "kind": "created" }),
            Duration::from_secs(2),
        );
        host.join().unwrap();
        assert!(handled, "夹具的反向请求要先被办到");
        assert!(matches!(result, Err(RequestError::Broken(_))), "反向答复失败该判连接坏：{result:?}");
        cleanup_site(&socket);
    }

    fn handshake_session(server: &mut Server) -> Session {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(session) = server.accept().unwrap() {
                return session;
            }
            assert!(Instant::now() < deadline, "夹具握手超时");
            thread::sleep(Duration::from_millis(2));
        }
    }
}
