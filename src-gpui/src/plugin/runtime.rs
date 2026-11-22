//! 插件模式那一格的**运行**：先绑数据面、把握手走完，再建窗（重构文档 §9 P1「先握手后建窗」）。
//!
//! 三件事各管一段、边界清清楚楚：
//! - [`handshake_before_window`]：在**开窗口之前**把 `accept`+`Hello`/`HelloOk` 走完。窗口没连上宿主
//!   就建起来＝一个连数据面都没有的孤窗，所以这一步挡在前面；预算用尽交 `Err`（宿主 5 s 判死，两边同档）。
//! - [`serve`]：一条连接一个泵线程的常驻循环。`pump` 不许阻塞 UI ⇒ 它跑在**自己的线程**上，办到的每一格
//!   交 `sink`（`FnMut(UiCommand)`）递给界面。把 `sink` 收成闭包而不是塞进结构体，是为了让这一格**不碰 GPUI
//!   也能测**（见 `tests` 里的环回：假宿主拨进来、发一条 `folders.snapshot`、看答复与递出的格）。
//! - [`run`]：唯一碰 GPUI 的一档——把上面两块拼进 `application.run`，并在**桌面壳之外**自成一路。
//!
//! ⚠ **桌面壳一律不进**：插件模式不持 `com.nomo.desktop` 数据租约、不起托盘／更新器／单实例，也不写 Nomo
//! 自有的库与恢复快照（D-B①）。`notes.db` 建在宿主给的 `data_dir` 那一层，正文根 `folders/` 从 `folders.snapshot`
//! 里绑——真源是宿主的，不是 Nomo 的（[`crate::plugin::storage`] 头上那条 ⚠）。
//!
//! ⚠ **Windows 数据面今天没接**（[`crate::plugin::transport`] 头上同一条）：非 unix 平台上 [`run`] 在碰
//! GPUI 之前就如实报错退 2，不假装能连、不静默成功。P1 是 Linux-first。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

#[cfg(unix)]
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, IntoElement, ParentElement, Render, Styled,
    TitlebarOptions, Window, WindowBounds, WindowOptions,
};

use crate::plugin::dispatch::Dispatch;
use crate::plugin::endpoints::Endpoints;
use crate::plugin::session::{Identity, Server, Session};
use crate::plugin::storage::{derive_title, first_line, note_text, Db, NoteKind, Root};

/// 泵线程递给界面那一格的东西。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiCommand {
    /// 握手成了：`data_dir` 是宿主在 `HelloOk` 里给的落点，`capabilities` 是它认可的那份清单（原样）。
    Connected {
        data_dir: String,
        capabilities: Vec<String>,
    },
    /// 收到 `folders.snapshot` ⇒ 界面那一侧的「文件夹 → 笔记」列表要重画。
    FoldersChanged,
    /// `notes.open`／`note.new` 办成：把那一格的正文与盘上路径递给编辑器（宿主侧车 `UiCommand::OpenNote` 同形）。
    OpenNote {
        markdown: String,
        path: PathBuf,
    },
    /// `notes.open` 没点到具体号、且那目录里空空如也 ⇒ 开一张空白草稿（答复里那句 `blank:true`）。
    OpenBlank,
}

/// 这一颗插件自报的那三位。`host_version` 从 env 读、**不猜**（宿主 spawn 那一发塞不塞是宿主的决定，
/// 见 [`crate::plugin::session`] 头上那条 ⚠）；读不到就留空，握手合同里那一格允许空串。
pub fn identity() -> Identity {
    Identity {
        plugin_id: "notes".to_string(),
        plugin_version: env!("CARGO_PKG_VERSION").to_string(),
        host_version: std::env::var("PINGLOSS_HOST_VERSION").unwrap_or_default(),
    }
}

/// `--probe` 那一发：把这一颗插件的自述原样打到 stdout 就退，**一个字都不报错**（口径同 `pingloss-todo`）。
///
/// ⚠ 运行时那一路 stdout 被宿主 spawn 置空（见宿主 `plugins::spawner` 头上那条），所以这一发只在**命令行手起**
/// 时给装机／排查的人看：他们分不清「这一颗没装好」与「这一发是自己起的」，就把两格落点也从 env 如实报回去——
/// 独立手起时没有宿主 spawn，那两位自然是 `null`，不是错。
pub fn probe() {
    let endpoints = Endpoints::from_env();
    let descriptor = json!({
        "app": "nomo",
        "plugin_id": "notes",
        "version": env!("CARGO_PKG_VERSION"),
        "mode": "probe",
        "protocol": crate::plugin::handshake::PROTOCOL,
        "data_socket": endpoints
            .as_ref()
            .map(|e| e.data_socket.to_string_lossy().into_owned()),
        "data_dir": endpoints
            .as_ref()
            .map(|e| e.data_dir.to_string_lossy().into_owned()),
    });
    println!("{descriptor}");
}

/// 先握手后建窗：在预算内等宿主**拨进来并走完握手**。
///
/// `accept` 本身非阻塞（没客户端 ⇒ `Ok(None)`），所以外面套一层有界轮询——这一段是「起来那一下」的事，
/// 窗口还没建、UI 线程还没进事件循环，占着线程等握手是对的。超预算交 `Err`，让 [`run`] 当场退、而不是
/// 开一个连不上宿主的窗。
pub fn handshake_before_window(server: &mut Server, budget: Duration) -> Result<Session, String> {
    let deadline = Instant::now() + budget;
    loop {
        match server.accept()? {
            Some(session) => return Ok(session),
            None => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "在 {} ms 预算内宿主没拨进数据面（先握手后建窗的那道闸）",
                        budget.as_millis()
                    ));
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// 一条握过手的连接上的数据面状态：`notes.db`（在宿主给的 `data_dir` 那一层）＋ 从 `folders.snapshot`
/// 绑来的正文根与「folder_id → 目录名」那张表（`note.new` 靠它把新正文落到对的 `folders/<dir>/notes/`）。
///
/// ⚠ **真源是宿主的**：这里的 `db`／正文读的都是宿主 `notes.db` 与 `folders/<dir>/notes/<id>.md`，
/// 不碰 Nomo 自有的 `document-library.sqlite3`（[`crate::plugin::storage`] 头上那条 ⚠）。
struct Engine {
    db: Db,
    /// `<data_dir>/notes.db`——`notes.info` 那一发另开一条只读连接要它（不建库、不迁移）。
    db_path: PathBuf,
    /// 含 `folders/` 的那一层正文根；`folders.snapshot` 之前是 `None`。
    root_path: Option<PathBuf>,
    /// `folders.snapshot` 里那排 `{id, dir_name}`；`note.new` 据此定新正文落在哪个夹。
    folders: HashMap<i64, String>,
    /// 攒着待推给宿主的那一发 `host.notes_changed`（§4.3 P3b）：改了状态的 op（`note.new`／`notes.rename`）
    /// 在 `handle` 里只**记账**、不直接发——发要靠 `serve` 那一侧的 `session`，而 `handle` 拿不到它。
    /// `serve` 的泵每办成一条就把这一格取走、`request` 出去；没接（旧宿主／缺权限）也不留，2 s TTL 兜底。
    pending_change: Option<Value>,
}

impl Engine {
    /// 这一发用哪个正文根：op 自己带的 `root` 优先（宿主 `notes.open`／`notes.list` 的载荷里就带），
    /// 否则回落到上一次 `folders.snapshot` 绑的那一份。两处都没有 ⇒ 拒（`note.new` 只带 `folder_id`，
    /// 没先收到快照就是这一格起晚了，如实拒、不拿默认根去猜）。
    fn root_for(&self, payload: &Value) -> crate::plugin::storage::Res<Root> {
        let path = payload
            .get("root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .or_else(|| self.root_path.clone());
        let path =
            path.ok_or_else(|| "还没收到 root（`folders.snapshot` 里那一格），这一发办不了".to_string())?;
        Root::open(&path)
    }

    /// 读那一格正文并折旧信封：路径解不开、文件读不动都交 `None`（列表把这一格当「没有可读正文」，
    /// 标题／摘要留空，但文件名／路径照给——私密那一份只是正文不给读，路径不是秘密）。
    fn read_markdown(&self, root: &Root, rel: &str) -> Option<String> {
        let abs = root.resolve(rel).ok()?;
        std::fs::read_to_string(&abs).ok().map(|raw| note_text(&raw))
    }

    /// 一条 op 的答复。载荷字段名与宿主 `panels::notes::bridge`／`plugins::host` 发出去的那几份**逐字对齐**
    /// （`root`／`note_id`／`folder_id`／`folder`／`query`／`stem`／`ids`），答复形状与侧车 `plugin.rs` 一致。
    fn handle(&mut self, op: &str, payload: Value, sink: &mut impl FnMut(UiCommand)) -> Dispatch {
        match op {
            "folders.snapshot" => {
                if let Some(root) = payload.get("root").and_then(Value::as_str) {
                    self.root_path = Some(PathBuf::from(root));
                }
                self.folders.clear();
                if let Some(list) = payload.get("folders").and_then(Value::as_array) {
                    for folder in list {
                        if let (Some(id), Some(dir)) = (
                            folder.get("id").and_then(Value::as_i64),
                            folder.get("dir_name").and_then(Value::as_str),
                        ) {
                            self.folders.insert(id, dir.to_string());
                        }
                    }
                }
                sink(UiCommand::FoldersChanged);
                Dispatch::Done(json!({ "ok": true }))
            }
            "notes.list" => {
                let root = match self.root_for(&payload) {
                    Ok(root) => root,
                    Err(error) => return failed("list_failed", error),
                };
                let rows = match payload.get("folder").and_then(Value::as_i64) {
                    Some(folder) => self.db.notes_in_folder(folder),
                    None => self.db.all_notes(),
                };
                let rows = match rows {
                    Ok(rows) => rows,
                    Err(error) => return failed("list_failed", error),
                };
                let needle = payload
                    .get("query")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                let mut notes = Vec::new();
                for row in rows {
                    // 一张表两类线：列表只认 `note`，待办另有一条线（宿主那边分着）。
                    if row.kind != NoteKind::Note {
                        continue;
                    }
                    let locked = row.enc.is_private();
                    let body = if locked {
                        None
                    } else {
                        self.read_markdown(&root, &row.body_path)
                    };
                    let title = body.as_deref().map(derive_title).filter(|t| !t.is_empty());
                    let excerpt = body.as_deref().map(first_line).filter(|l| !l.is_empty());
                    let file_name = Path::new(&row.body_path)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if !needle.is_empty() {
                        // 搜索只看文件名与正文首行前 30 个字（与侧车同尺，按 `char` 计）；锁定只搜文件名。
                        let head: String = excerpt.as_deref().unwrap_or_default().chars().take(30).collect();
                        if !format!("{file_name} {head}").to_lowercase().contains(&needle) {
                            continue;
                        }
                    }
                    notes.push(json!({
                        "id": row.id,
                        "folder_id": row.folder_id,
                        "title": title,
                        "file_name": file_name,
                        "updated_at_ms": row.updated_at_ms,
                        "locked": locked,
                        "excerpt": excerpt,
                        "path": root.resolve(&row.body_path).ok().map(|p| p.to_string_lossy().into_owned()),
                    }));
                }
                Dispatch::Done(json!({ "notes": notes }))
            }
            "notes.open" => {
                let root = match self.root_for(&payload) {
                    Ok(root) => root,
                    Err(error) => return failed("no_root", error),
                };
                match payload.get("note_id").and_then(Value::as_i64) {
                    Some(id) => self.open_one(&root, id, sink),
                    // 没点到具体号：给了夹就取那夹最近那一篇，两样都没有才开空白草稿。
                    None => {
                        let folder = payload
                            .get("folder_id")
                            .or_else(|| payload.get("folder"))
                            .and_then(Value::as_i64);
                        let first = match folder {
                            Some(folder) => self.db.notes_in_folder(folder),
                            None => self.db.all_notes(),
                        }
                        .ok()
                        .and_then(|rows| {
                            rows.into_iter().find(|row| row.kind == NoteKind::Note).map(|row| row.id)
                        });
                        match first {
                            Some(id) => self.open_one(&root, id, sink),
                            None => {
                                sink(UiCommand::OpenBlank);
                                Dispatch::Done(json!({ "queued": true, "blank": true }))
                            }
                        }
                    }
                }
            }
            "note.new" => {
                let Some(folder) = payload.get("folder_id").and_then(Value::as_i64) else {
                    return failed("unknown_folder", "note.new 需要宿主的 `folder_id`".to_string());
                };
                let Some(dir_name) = self.folders.get(&folder).cloned() else {
                    return failed(
                        "unknown_folder",
                        format!("快照里没有 {folder} 号那一格的目录名，正文无处可落"),
                    );
                };
                let root = match self.root_for(&payload) {
                    Ok(root) => root,
                    Err(error) => return failed("no_root", error),
                };
                let row = match self.db.create_note(&root, &dir_name, folder) {
                    Ok(row) => row,
                    Err(error) => return failed("create_failed", error),
                };
                let path = root.resolve(&row.body_path).unwrap_or_default();
                sink(UiCommand::OpenNote { markdown: String::new(), path });
                self.note_changed(folder, row.id, "created");
                Dispatch::Done(json!({ "queued": true, "id": row.id }))
            }
            "notes.rename" => {
                let Some(id) = payload.get("note_id").and_then(Value::as_i64) else {
                    return failed("bad_request", "notes.rename 需要 `note_id`".to_string());
                };
                let stem = payload.get("stem").and_then(Value::as_str).unwrap_or("");
                let root = match self.root_for(&payload) {
                    Ok(root) => root,
                    Err(error) => return failed("rename_failed", error),
                };
                let row = match self.db.note(id) {
                    Ok(Some(row)) => row,
                    Ok(None) => return failed("rename_failed", format!("第 {id} 号不存在")),
                    Err(error) => return failed("rename_failed", error),
                };
                match self.db.rename_body(&root, &row, stem) {
                    Ok(new_abs) => {
                        self.note_changed(row.folder_id, id, "renamed");
                        Dispatch::Done(json!({
                            "note_id": id,
                            "path": new_abs.to_string_lossy(),
                        }))
                    }
                    Err(error) => failed("rename_failed", error),
                }
            }
            "notes.info" => {
                let ids: Vec<i64> = payload
                    .get("ids")
                    .and_then(Value::as_array)
                    .map(|list| list.iter().filter_map(Value::as_i64).collect())
                    .unwrap_or_default();
                let entries = Db::titles_for(&self.db_path, &ids);
                let arr: Vec<Value> = entries
                    .into_iter()
                    .map(|e| json!({
                        "note_id": e.note_id,
                        "exists": e.exists,
                        "title": e.title,
                        "locked": e.locked,
                    }))
                    .collect();
                Dispatch::Done(json!({ "entries": arr }))
            }
            _ => Dispatch::unknown_op(op, "notes"),
        }
    }

    /// 打开某一号：私有拒（口径同侧车 "private notes are not wired yet"）、路径或读盘坏了交 `open_failed`。
    fn open_one(&self, root: &Root, id: i64, sink: &mut impl FnMut(UiCommand)) -> Dispatch {
        let row = match self.db.note(id) {
            Ok(Some(row)) => row,
            Ok(None) => return failed("open_failed", format!("第 {id} 号不存在")),
            Err(error) => return failed("open_failed", error),
        };
        if row.enc.is_private() {
            return failed("open_failed", format!("第 {id} 号是私有的，私有正文今天没接线"));
        }
        let abs = match root.resolve(&row.body_path) {
            Ok(abs) => abs,
            Err(error) => return failed("open_failed", error),
        };
        let markdown = match std::fs::read_to_string(&abs) {
            Ok(raw) => note_text(&raw),
            Err(error) => {
                return failed("open_failed", format!("读 {} 失败：{error}", abs.display()))
            }
        };
        sink(UiCommand::OpenNote { markdown, path: abs });
        Dispatch::Done(json!({ "queued": true }))
    }

    /// 攒一条待推的 `host.notes_changed`（§4.3 P3b）：载荷 `{folder, note_id, kind, title?}`，字段名照文档那一格。
    /// 只**记账**、不在 `handle` 里发（那里拿不到 `session`）——发由 [`serve`] 的泵在 `handle` 返回后做。
    /// 一条接一个：后一条盖过前一条（2 s TTL 还在兜底，推漏一条只是退化成今天的时延）。
    fn note_changed(&mut self, folder: i64, note_id: i64, kind: &str) {
        self.pending_change = Some(json!({
            "folder": folder,
            "note_id": note_id,
            "kind": kind,
        }));
    }
}

/// 一条连接的常驻泵：开 `notes.db` → 一帧一帧 `pump`，办到的每一格交 `sink`，对端关闭就返回。
///
/// ⚠ **不在 UI 线程上跑**：`serve` 会一直循环到连接断，调用它的是 [`run`] 起的那根泵线程。`sink` 由调用方
/// 决定「递出去之后界面怎么办」——测里收进一个 `Vec`，[`run`] 里转成一条通道 hop 回主线程。
pub fn serve(mut session: Session, mut sink: impl FnMut(UiCommand)) -> Result<(), String> {
    let db_path = Path::new(session.data_dir()).join("notes.db");
    let mut engine = Engine {
        db: Db::open(&db_path)?,
        db_path,
        root_path: None,
        folders: HashMap::new(),
        pending_change: None,
    };
    loop {
        let step = session.pump(&mut |op, payload| engine.handle(op, payload, &mut sink))?;
        match step {
            crate::plugin::dispatch::Step::Served { .. } => {
                // 改了状态那一发（`note.new`／`notes.rename`）攒下了变更账，趁这条连接还活着尽力推给宿主
                // （§4.3 P3b）。`request` 里若宿主又发来一条请求，照常就地办掉。
                // ⚠ 这一发的成败**不致命也不重试**：旧宿主没把 `host.notes_changed` 接进代理表、缺 `history.write`
                // 权限、或它卡在上一趟往返上没在预算内回话，都只让这次即时刷新落空——2 s 轮询 TTL 还在兜底，
                // 事件丢失最多退化成今天「等一个轮询」的时延，不能因为推不动就把这跟泵线程判死。
                if let Some(change) = engine.pending_change.take() {
                    let _ = session.request(
                        &mut |op, payload| engine.handle(op, payload, &mut sink),
                        "host.notes_changed",
                        change,
                        Duration::from_secs(2),
                    );
                }
            }
            crate::plugin::dispatch::Step::Idle => std::thread::sleep(Duration::from_millis(2)),
            crate::plugin::dispatch::Step::Closed => return Ok(()),
        }
    }
}

/// 数据面失败帧那一格（`code` 必须是过段名闸的那个词，`message` 直接进 `Failure.message`）。
fn failed(code: &str, message: impl Into<String>) -> Dispatch {
    Dispatch::Failed { code: code.to_string(), message: message.into() }
}

/// 插件模式入口：先绑数据面 + 握手（窗口之前），再进 GPUI 事件循环开那扇窄窗，泵线程跑 [`serve`]。
///
/// 非 unix：数据面 [`crate::plugin::transport`] 没接，碰 GPUI 之前就退 2。
pub fn run(endpoints: Endpoints) {
    #[cfg(not(unix))]
    {
        let _ = endpoints;
        eprintln!("插件模式的数据面今天只在 Linux/Unix 接线；这一格以桌面模式以外退出");
        std::process::exit(2);
    }
    #[cfg(unix)]
    {
        run_unix(endpoints);
    }
}

/// 唯一的 GPUI 那一档。绑 socket → 握手 → 进 `application.run`：窗口先带着 `Connected` 起来，泵线程把
/// 后续每一格经 `async_cx` 转成窗口更新。**桌面壳的租约／托盘／单实例／更新器一行都不碰。**
#[cfg(unix)]
fn run_unix(endpoints: Endpoints) {
    const HELLO_BUDGET: Duration = Duration::from_secs(5);

    let mut server = match Server::bind(&endpoints.data_socket, identity(), HELLO_BUDGET) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("插件模式起不来：{error}");
            std::process::exit(2);
        }
    };
    // 先握手后建窗：握不完手，下面这行不返回，窗口也就不存在。
    let session = match handshake_before_window(&mut server, HELLO_BUDGET) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("插件模式起不来：{error}");
            std::process::exit(2);
        }
    };
    let connected = UiCommand::Connected {
        data_dir: session.data_dir().to_string(),
        capabilities: session.capabilities().to_vec(),
    };

    let application = gpui_platform::application();
    application.run(move |cx: &mut App| {
        cx.set_global(crate::theme::Theme::light());
        let bounds = Bounds::centered(None, size(px(720.0), px(520.0)), cx);

        // 泵线程递出来的每一格先落进这条通道，下面那个 GPUI 任务在主执行器上等它、再 hop 回主线程更新窗口。
        // ⚠ 为什么非要这一跳：`AsyncApp` 装的是 `Weak<AppCell>`、不是 `Send`，泵线程拿不到它、更没法直接
        // 更新窗口（这正是 `updates/install_ui.rs` 下载进度那条通道存在的理由）。窗口初始那格「连上了」
        // 也走同一条通道，免得为它单开一路。
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UiCommand>();
        let _ = tx.send(connected);

        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Nomo · 笔记".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |_window, cx| cx.new(|_cx| PluginWindow::new()),
            )
            .expect("插件模式开不出窗口");

        // 主执行器上这一路：等通道 → 更新那扇窗（`WindowHandle::update` 要 `&mut App`，正好在 `cx.update` 里）。
        cx.spawn(async move |cx| {
            while let Some(command) = rx.recv().await {
                let _ = cx.update(|cx| {
                    window.update(cx, |view, _window, _cx| view.apply(command))
                });
            }
        })
        .detach();

        // 泵线程拥有 session，一帧一帧办到连接关闭；办到的每一格投进上面那条通道。session 是 Send。
        std::thread::spawn(move || {
            if let Err(error) = serve(session, move |command| {
                let _ = tx.send(command);
            }) {
                eprintln!("插件模式泵线程退了：{error}");
            }
        });
        cx.activate(true);
    });
}

/// 插件模式那扇**窄窗**：P1-c 先只把连接态摆出来（数据目录／认可的能力），把桌面壳的首页／文档库／多标签
/// 全部关在门外。真正的「文件夹 → 笔记」列表与编辑器接进来是 P1-d／P2 的事——这一档的存在就是证明
/// 「插件模式建的是另一扇窗，不是 `com.nomo.desktop` 那一套」。
#[cfg(unix)]
struct PluginWindow {
    status: String,
}

#[cfg(unix)]
impl PluginWindow {
    fn new() -> Self {
        Self {
            status: "等待宿主接入…".to_string(),
        }
    }

    fn apply(&mut self, command: UiCommand) {
        match command {
            UiCommand::Connected {
                data_dir,
                capabilities,
            } => {
                self.status = format!(
                    "已连上宿主 · 数据目录 {data_dir} · 认可能力 {} 条",
                    capabilities.len()
                );
            }
            UiCommand::FoldersChanged => self.status = "目录快照已更新".to_string(),
            // P1-c 的窄窗只把「打开了哪一格」摆出来；真正的编辑器把 `markdown` 渲进去是 P2 的事。
            UiCommand::OpenNote { markdown, path } => {
                self.status = format!(
                    "已打开 {}（正文 {} 字）",
                    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    markdown.chars().count()
                );
            }
            UiCommand::OpenBlank => self.status = "已开一张空白草稿".to_string(),
        }
    }
}

#[cfg(unix)]
impl Render for PluginWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<crate::theme::Theme>().clone();
        div()
            .size_full()
            .bg(theme.app_bg)
            .p(px(24.0))
            .child(div().text_size(px(14.0)).text_color(theme.fg).child(self.status.clone()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::plugin::envelope::Envelope;
    use crate::plugin::handshake::{HelloOk, PROTOCOL};
    use crate::plugin::transport::Stream;

    fn socket(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nomo-runtime-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("plugin-{tag}.sock"))
    }

    fn identity() -> Identity {
        Identity {
            plugin_id: "notes".to_string(),
            plugin_version: "0.7.3".to_string(),
            host_version: "0.9.131".to_string(),
        }
    }

    /// 环回验 P1-c 那两步（先握手、后泵）：假宿主拨进来 → 读完插件的 `Hello` 答 `HelloOk`（带 `data_dir`）
    /// → 发一条 `folders.snapshot` → 插件 `serve` 把正文根绑上、`sink` 收到 `FoldersChanged`、答复 `ok:true`
    /// → `notes.db` 落在 `data_dir` 那一层。全程不碰 GPUI。
    #[test]
    fn handshake_then_snapshot_reaches_the_ui_sink() {
        let data = tempfile::Builder::new()
            .prefix("nomo-rt-data")
            .tempdir()
            .unwrap();
        let root = tempfile::Builder::new()
            .prefix("nomo-rt-root")
            .tempdir()
            .unwrap();
        let socket = socket("snap");

        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();

        // 假宿主那一头：把 data_dir / 正文根 交出去，等一条答复。
        let data_dir = data.path().to_string_lossy().to_string();
        let body_root = root.path().to_string_lossy().to_string();
        let host_socket = socket.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            let _hello = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&HelloOk {
                protocol: PROTOCOL,
                capabilities: vec!["folders.snapshot".into(), "notes.open".into()],
                data_dir: data_dir.clone(),
            })
            .unwrap();
            host.send(
                &Envelope::Request {
                    request: 1,
                    op: "folders.snapshot".to_string(),
                    payload: json!({ "root": body_root }),
                }
                .to_value(),
            )
            .unwrap();
            let reply = host.wait_msg(Duration::from_secs(5)).unwrap();
            drop(host); // 关掉连接，让插件那侧的泵走到 Step::Closed 收工。
            reply
        });

        // 插件这一头：先握手后建窗的那一步，再进泵，收 sink。
        let session = handshake_before_window(&mut server, Duration::from_secs(5)).unwrap();
        assert_eq!(session.data_dir(), data.path().to_string_lossy());
        assert_eq!(session.capabilities().len(), 2);

        let (tx, rx) = mpsc::channel::<UiCommand>();
        serve(session, move |command| {
            let _ = tx.send(command);
        })
        .unwrap();

        let reply = host.join().unwrap();
        match Envelope::from_value(&reply).unwrap() {
            Envelope::Reply { request, payload } => {
                assert_eq!(request, 1);
                assert_eq!(payload["ok"], json!(true), "folders.snapshot 该回 ok:true");
            }
            other => panic!("答复解成了 {other:?}"),
        }
        let seen: Vec<UiCommand> = rx.try_iter().collect();
        assert_eq!(seen, vec![UiCommand::FoldersChanged], "泵该把快照那一格递给 sink");
        assert!(
            data.path().join("notes.db").exists(),
            "serve 起来就在宿主给的 data_dir 里把 notes.db 建好"
        );

        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    /// 握手那道闸：预算内没人拨进来 ⇒ `Err`（不是无限等，更不是建一扇连不上的窗）。
    #[test]
    fn handshake_before_window_times_out_without_a_host() {
        let socket = socket("nohost");
        let mut server = Server::bind(&socket, identity(), Duration::from_millis(50)).unwrap();
        let got = handshake_before_window(&mut server, Duration::from_millis(120));
        assert!(got.is_err(), "没宿主拨进来时先握手后建窗的闸要判死，不能放行建窗");
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    /// 直接造一台引擎（不绕 socket、不碰 GPUI）：临时 `notes.db` + 临时正文根；`folders.snapshot` 之前根是空的。
    fn engine(db_path: &Path) -> Engine {
        Engine {
            db: Db::open(db_path).unwrap(),
            db_path: db_path.to_path_buf(),
            root_path: None,
            folders: HashMap::new(),
            pending_change: None,
        }
    }

    /// 六条 op 的真闭环（P1-d）：在一颗真的 `notes.db` 与正文根上，按宿主 `bridge`／`host.rs` 发出去的来路
    /// 顺序走一遍——快照绑根与夹 → `note.new` 造一篇并递 `OpenNote` → `notes.list` 认到它 → `notes.open`
    /// 再开一次 → 往正文写一行后 `notes.rename` 改物理文件名 → `notes.info` 点查那一号。全程不碰 GPUI、不碰 socket。
    #[test]
    fn six_ops_close_the_loop_on_a_real_db() {
        let data = tempfile::Builder::new()
            .prefix("nomo-rt-six-db")
            .tempdir()
            .unwrap();
        let root = tempfile::Builder::new()
            .prefix("nomo-rt-six-root")
            .tempdir()
            .unwrap();
        let db_path = data.path().join("notes.db");
        let body_root = root.path().to_string_lossy().to_string();
        let mut engine = engine(&db_path);

        // ① `folders.snapshot`：把正文根与「夹号 → 目录名」绑上，递给界面一句「快照已更新」。
        let mut sink = Vec::new();
        let done = engine.handle(
            "folders.snapshot",
            json!({ "root": body_root, "folders": [{ "id": 7, "dir_name": "inbox" }] }),
            &mut |command| sink.push(command),
        );
        assert_eq!(done, Dispatch::Done(json!({ "ok": true })));
        assert_eq!(sink, vec![UiCommand::FoldersChanged]);

        // ② `note.new`：只带 `folder_id`，靠快照里那个 `dir_name` 把正文落到 `folders/inbox/notes/`。
        let mut sink = Vec::new();
        let Dispatch::Done(reply) =
            engine.handle("note.new", json!({ "folder_id": 7 }), &mut |c| sink.push(c))
        else {
            panic!("note.new 该办成");
        };
        let id = reply["id"].as_i64().unwrap();
        assert_eq!(reply["queued"], json!(true));
        let UiCommand::OpenNote { markdown, path } = sink.into_iter().next().unwrap() else {
            panic!("note.new 该递 OpenNote");
        };
        assert_eq!(markdown, "", "新建的正文是空的");
        assert!(path.to_string_lossy().contains("inbox"), "正文落在快照那个夹：{path:?}");

        // ③ `notes.list`：不带夹走全表，认到刚造那一篇（正文空 ⇒ 标题／摘要留空，文件名照给）。
        let Dispatch::Done(reply) =
            engine.handle("notes.list", json!({ "folder": Value::Null, "query": "" }), &mut |_| {})
        else {
            panic!("notes.list 该办成");
        };
        let notes = reply["notes"].as_array().unwrap();
        assert_eq!(notes.len(), 1, "只该有一篇：{notes:?}");
        assert_eq!(notes[0]["id"].as_i64().unwrap(), id);
        assert_eq!(notes[0]["file_name"].as_str().unwrap(), format!("{id}.md"));

        // ④ `notes.open`：点到那一号，把正文再递一次给界面，答 `queued:true`。
        let mut sink = Vec::new();
        let Dispatch::Done(reply) =
            engine.handle("notes.open", json!({ "note_id": id }), &mut |c| sink.push(c))
        else {
            panic!("notes.open 该办成");
        };
        assert_eq!(reply, json!({ "queued": true }));
        assert!(
            matches!(sink.first(), Some(UiCommand::OpenNote { .. })),
            "notes.open 该递 OpenNote：{sink:?}"
        );

        // ⑤ 往正文写一行标题，再 `notes.rename` 改物理文件名；答复点名改后的绝对路径。
        let rel = format!("folders/inbox/notes/{id}.md");
        let abs = Root::open(root.path())
            .unwrap()
            .resolve(&rel)
            .unwrap();
        std::fs::write(&abs, "# 重构笔记\n").unwrap();
        let Dispatch::Done(reply) =
            engine.handle("notes.rename", json!({ "note_id": id, "stem": "pingloss" }), &mut |_| {})
        else {
            panic!("notes.rename 该办成");
        };
        assert_eq!(reply["note_id"].as_i64().unwrap(), id);
        assert!(
            reply["path"].as_str().unwrap().ends_with("pingloss.md"),
            "改完该落在 pingloss.md：{reply}"
        );

        // ⑥ `notes.info`：另开一条只读连接点查那一号（行还在 ⇒ exists；标题列没走过存盘那条 ⇒ 留空，只证在）。
        let Dispatch::Done(reply) =
            engine.handle("notes.info", json!({ "ids": [id] }), &mut |_| {})
        else {
            panic!("notes.info 该办成");
        };
        let entries = reply["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["note_id"].as_i64().unwrap(), id);
        assert_eq!(entries[0]["exists"], json!(true));
    }

    /// P3b 推送的环回验：假宿主拨进来、握完手、先 `folders.snapshot` 绑根，再发一条 `note.new`；
    /// 插件这一头 [`serve`] 的泵办完 `note.new` 后，**反过来**向宿主推一发 `host.notes_changed`
    /// （载荷 `{folder, note_id, kind:"created"}`）。宿主读那一帧、回一句 `ok:true`，插件就地收账继续。
    /// 全程不碰 GPUI：验的就是 `handle` 记账 → `serve` 用 `Session::request` 发出去这一段接线。
    #[test]
    fn a_new_note_pushes_host_notes_changed_back_to_the_host() {
        let data = tempfile::Builder::new()
            .prefix("nomo-rt-push-db")
            .tempdir()
            .unwrap();
        let root = tempfile::Builder::new()
            .prefix("nomo-rt-push-root")
            .tempdir()
            .unwrap();
        let socket = socket("push");

        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();

        let data_dir = data.path().to_string_lossy().to_string();
        let body_root = root.path().to_string_lossy().to_string();
        let host_socket = socket.clone();
        // 假宿主：握手 → 绑根 → note.new（各等一帧答复）→ 读插件反向推来的那一帧、回 ok。交回那一帧原文供断言。
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            let _hello = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&HelloOk {
                protocol: PROTOCOL,
                capabilities: vec!["folders.snapshot".into(), "note.new".into()],
                data_dir: data_dir.clone(),
            })
            .unwrap();
            host.send(
                &Envelope::Request {
                    request: 1,
                    op: "folders.snapshot".to_string(),
                    payload: json!({ "root": body_root, "folders": [{ "id": 7, "dir_name": "inbox" }] }),
                }
                .to_value(),
            )
            .unwrap();
            let _snapshot_reply = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(
                &Envelope::Request {
                    request: 2,
                    op: "note.new".to_string(),
                    payload: json!({ "folder_id": 7 }),
                }
                .to_value(),
            )
            .unwrap();
            let _new_reply = host.wait_msg(Duration::from_secs(5)).unwrap();
            // 反向那一帧：插件用 `host.notes_changed` 把「刚新建了哪一篇」推回来。
            let pushed = host.wait_msg(Duration::from_secs(5)).unwrap();
            if let Ok(Envelope::Request { request, .. }) = Envelope::from_value(&pushed) {
                host.send(
                    &Envelope::Reply {
                        request,
                        payload: json!({ "ok": true }),
                    }
                    .to_value(),
                )
                .unwrap();
            }
            drop(host); // 关掉连接，让插件泵走到 Step::Closed 收工。
            pushed
        });

        let session = handshake_before_window(&mut server, Duration::from_secs(5)).unwrap();
        let (tx, rx) = mpsc::channel::<UiCommand>();
        serve(session, move |command| {
            let _ = tx.send(command);
        })
        .unwrap();

        let pushed = host.join().unwrap();
        match Envelope::from_value(&pushed).unwrap() {
            Envelope::Request { op, payload, .. } => {
                assert_eq!(op, "host.notes_changed", "泵该用 §4.3 那条名字反推");
                assert_eq!(payload["kind"], json!("created"));
                assert_eq!(payload["folder"], json!(7), "folder 用宿主给的 folder_id");
                assert!(payload["note_id"].is_i64(), "note_id 要给出刚造那号：{payload}");
            }
            other => panic!("插件反推那一帧解成了 {other:?}"),
        }
        // sink 那边照常收到绑根与开新篇两格——推送这一步不该把递给界面的格挤掉。
        let seen: Vec<UiCommand> = rx.try_iter().collect();
        assert!(
            matches!(
                seen.as_slice(),
                [UiCommand::FoldersChanged, UiCommand::OpenNote { .. }]
            ),
            "sink 收格不符：{seen:?}"
        );

        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }
}
