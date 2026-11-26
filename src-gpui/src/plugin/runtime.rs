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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, Arc};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

use gpui::{
    div, px, size, App, AppContext, Bounds, Context, Entity, Focusable, FocusHandle,
    InteractiveElement, IntoElement, ParentElement, PromptButton, PromptLevel, Render,
    StatefulInteractiveElement, Styled, Subscription, Task, TitlebarOptions, UniformListScrollHandle,
    Window, WindowBounds, WindowOptions, uniform_list,
};

use crate::editor::ViewMode;
use crate::nomo_mark::{
    document_host::{DocumentEvent, DocumentHost},
    host::WindowHostAdapter,
    native_port::NativeWindowPort,
    protocol::ControlError,
    session_window::WindowRequest,
};
use crate::plugin::dispatch::Dispatch;
use crate::plugin::endpoints::Endpoints;
use crate::plugin::session::{Identity, RequestError, Server, Session};
use crate::plugin::storage::{derive_title, first_line, note_text, Db, NoteKind, NoteRow, Root};
use crate::text_document::markdown_session::{
    MarkdownDocumentSessionManager, RecoveryCandidate, RenameAttemptError, SessionLease,
};
use crate::ui::text_field::{FieldEvent, TextField};
use crate::ui::toolbar_i18n::ToolbarLocale;

/// 宿主数据库里一篇笔记的身份与正文路径。正文由正式会话按需读取。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenNote {
    pub note_id: i64,
    pub folder_id: i64,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FolderInfo {
    id: i64,
    name: String,
    dir_name: String,
    is_default: bool,
    locked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NoteListItem {
    note_id: i64,
    folder_id: i64,
    title: Option<String>,
    file_name: String,
    body_path: String,
    updated_at_ms: i64,
    locked: bool,
    excerpt: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LibrarySnapshot {
    folders: Vec<FolderInfo>,
    notes: Vec<NoteListItem>,
}

#[derive(Debug, PartialEq, Eq)]
struct DeleteOutcome {
    deleted: bool,
    warnings: Vec<String>,
}

/// 宿主随文件夹快照下发的颜色。插件窗不模拟宿主半透明圆角，只承接颜色与明暗档。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HostSkin {
    panel_bg: gpui::Hsla,
    accent: gpui::Hsla,
    text: gpui::Hsla,
    hint: gpui::Hsla,
}

impl HostSkin {
    fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            panel_bg: host_color(value.get("panel_bg")?)?,
            accent: host_color(value.get("theme_accent")?)?,
            text: host_color(value.get("text_color")?)?,
            hint: host_color(value.get("hint_color")?)?,
        })
    }

    fn theme(&self) -> crate::theme::Theme {
        use crate::theme::Theme;
        use crate::ui::motion;

        let panel = gpui::Rgba::from(self.panel_bg);
        let dark = 0.2126 * panel.r + 0.7152 * panel.g + 0.0722 * panel.b < 0.5;
        let mut theme = if dark { Theme::dark() } else { Theme::light() };
        let border = motion::color(self.panel_bg, self.text, if dark { 0.22 } else { 0.16 });
        let hover = motion::color(self.panel_bg, self.text, if dark { 0.09 } else { 0.06 });
        let surface = motion::color(
            self.panel_bg,
            if dark { self.text } else { gpui::white() },
            if dark { 0.07 } else { 0.35 },
        );
        let accent = gpui::Rgba::from(self.accent);
        let on_accent = if 0.2126 * accent.r + 0.7152 * accent.g + 0.0722 * accent.b > 0.62 {
            gpui::black()
        } else {
            gpui::white()
        };
        let accent_strong = motion::color(self.accent, self.text, if dark { 0.28 } else { 0.10 });
        let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
        let packed = |color: gpui::Hsla| {
            let rgba = gpui::Rgba::from(color);
            (byte(rgba.r) << 16) | (byte(rgba.g) << 8) | byte(rgba.b)
        };

        theme.id = "pingloss-host".into();
        theme.dark = dark;
        theme.document_bg = self.panel_bg;
        theme.titlebar_bg = surface;
        theme.titlebar_fg = self.text;
        theme.titlebar_border = border;
        theme.accent_fill = self.accent;
        theme.dropdown_bg = surface;
        theme.dropdown_border = border;
        theme.app_bg = self.panel_bg;
        theme.rail_bg = motion::color(self.panel_bg, self.text, if dark { 0.035 } else { 0.025 });
        theme.surface = surface;
        theme.fg = self.text;
        theme.heading = self.text;
        theme.muted = self.hint;
        theme.border = border;
        theme.workspace_edge = border.opacity(if dark { 0.9 } else { 0.76 });
        theme.hover = hover;
        theme.accent = self.accent;
        theme.accent_strong = accent_strong;
        theme.on_accent = on_accent;
        theme.sidebar_active = self.accent.opacity(if dark { 0.18 } else { 0.12 });
        theme.selection = self.accent.opacity(0.24);
        theme.caret = self.accent;
        theme.link = self.accent;
        theme.quote_border = self.accent.opacity(0.65);
        theme.quote_fg = self.text;
        theme.code_bg = hover;
        theme.code_border = border;
        theme.code_fg = self.text;
        theme.inline_code_bg = motion::color(self.panel_bg, self.accent, if dark { 0.14 } else { 0.09 });
        theme.inline_code_fg = accent_strong;
        theme.table_border = border;
        theme.table_header_bg = hover;
        theme.search_match = self.accent.opacity(0.30);
        theme.search_current = self.accent.opacity(0.62);
        theme.marker = self.hint.opacity(0.60);
        theme.scrollbar = self.hint.opacity(0.72);
        theme.mermaid = crate::embeds::MermaidPalette {
            mode: if dark { 1 } else { 0 },
            colors: [
                theme.document_bg, theme.surface, theme.fg, theme.border, theme.muted,
                theme.rail_bg, theme.surface, theme.accent, theme.heading,
            ].map(packed),
        };
        theme
    }
}

fn host_color(value: &Value) -> Option<gpui::Hsla> {
    let raw = value.as_str()?;
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(|rgb| gpui::rgb(rgb).into())
}

fn host_locale(value: &Value) -> Option<&'static str> {
    let raw = value.as_str()?.trim().replace('_', "-").to_ascii_lowercase();
    if raw == "zh-hant" || raw == "zh-tw" || raw == "zh-hk" || raw == "zh-mo" {
        Some("zh-TW")
    } else if matches!(raw.as_str(), "zh" | "zh-cn" | "zh-hans" | "zh-sg") {
        Some("zh-CN")
    } else if raw == "en" || raw.starts_with("en-") {
        Some("en-US")
    } else if raw == "ja" || raw.starts_with("ja-") {
        Some("ja-JP")
    } else {
        None
    }
}

fn plugin_text(cx: &App, key: &'static str) -> String {
    ToolbarLocale::current(cx).app_text(key).to_string()
}

fn plugin_format(cx: &App, key: &'static str, arguments: &[(&str, &str)]) -> String {
    ToolbarLocale::current(cx).app_format(key, arguments)
}

#[derive(Clone, Debug)]
enum StatusMessage {
    Localized {
        key: &'static str,
        arguments: Vec<(String, String)>,
    },
    Raw(String),
}

impl StatusMessage {
    fn text(key: &'static str) -> Self {
        Self::Localized { key, arguments: Vec::new() }
    }

    fn localized(key: &'static str, arguments: &[(&str, &str)]) -> Self {
        Self::Localized {
            key,
            arguments: arguments.iter().map(|(name, value)| ((*name).to_string(), (*value).to_string())).collect(),
        }
    }

    fn raw(value: impl Into<String>) -> Self {
        Self::Raw(value.into())
    }

    fn render(&self, cx: &App) -> String {
        match self {
            Self::Localized { key, arguments } => {
                let refs: Vec<_> = arguments.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
                plugin_format(cx, key, &refs)
            }
            Self::Raw(value) => value.clone(),
        }
    }
}

impl NoteListItem {
    fn to_json(&self, root: &Root) -> Value {
        json!({
            "id": self.note_id,
            "folder_id": self.folder_id,
            "title": self.title,
            "file_name": self.file_name,
            "updated_at_ms": self.updated_at_ms,
            "locked": self.locked,
            "excerpt": self.excerpt,
            "path": root.resolve(&self.body_path).ok().map(|path| path.to_string_lossy().into_owned()),
        })
    }
}

/// 泵线程递给界面那一格的东西。
#[derive(Clone, Debug, PartialEq)]
pub enum UiCommand {
    /// 握手成了：`data_dir` 是宿主在 `HelloOk` 里给的落点，`capabilities` 是它认可的那份清单（原样）。
    Connected {
        data_dir: String,
        capabilities: Vec<String>,
    },
    /// 收到 `folders.snapshot` ⇒ 界面那一侧的「文件夹 → 笔记」列表要重画。
    FoldersChanged,
    /// 宿主外观或语言变了；旧宿主不带这两格时不会递这一条。
    HostAppearanceChanged {
        skin: Option<HostSkin>,
        locale: Option<&'static str>,
    },
    /// `notes.open`／`note.new` 办成：把笔记身份与盘上路径递给正式编辑器。
    OpenNote { note: OpenNote },
    /// 宿主改了当前笔记的物理文件名。
    NoteRenamed { note_id: i64, path: PathBuf },
    /// 一篇笔记已从宿主数据库与磁盘删除。
    NoteDeleted { note_id: i64 },
    /// 数据面或会话给出的非阻断提示。
    Notice(String),
    /// `notes.open` 没点到具体号、且那目录里空空如也 ⇒ 留给宿主继续指定笔记。
    OpenBlank,
}

enum EngineCommand {
    Library {
        folder_id: Option<i64>,
        query: String,
        reply: oneshot::Sender<Result<LibrarySnapshot, String>>,
    },
    OpenLocal {
        note_id: i64,
        reply: oneshot::Sender<Result<OpenNote, String>>,
    },
    PrepareSave {
        note_id: i64,
        path: PathBuf,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SaveFinished {
        note_id: i64,
        path: PathBuf,
        reply: oneshot::Sender<Result<(), String>>,
    },
    NoteClosed {
        note_id: i64,
    },
    DeleteLocal {
        note_id: i64,
        reply: oneshot::Sender<Result<DeleteOutcome, String>>,
    },
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
    /// 每篇已打开笔记绑定的正文根，保存时按笔记身份取用。
    note_roots: HashMap<i64, PathBuf>,
    /// 已交给编辑器的笔记；删除请求遇到在飞会话时拒绝，避免正文被重新写回。
    open_notes: HashSet<i64>,
    /// `folders.snapshot` 里那排文件夹；保留宿主顺序，列表按原顺序展示。
    folders: Vec<FolderInfo>,
    /// 攒着待推给宿主的那一发 `host.notes_changed`（§4.3 P3b）：改了状态的 op（`note.new`／`notes.rename`／保存）
    /// 在 `handle` 里只**记账**、不直接发——发要靠 `serve` 那一侧的 `session`，而 `handle` 拿不到它。
    /// `serve` 的泵每办成一条就把这一格取走、`request` 出去；没接（旧宿主／缺权限）也不留，2 s TTL 兜底。
    pending_change: Option<Value>,
    /// 生产泵与界面共享同一会话服务，改名可在正式路径锁内更新已打开文档的基线。
    sessions: Option<MarkdownDocumentSessionManager>,
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
                        if let Some(id) = folder.get("id").and_then(Value::as_i64) {
                            let dir_name = folder.get("dir_name").and_then(Value::as_str).unwrap_or_default();
                            if dir_name.is_empty() { continue; }
                            let name = folder.get("name").and_then(Value::as_str)
                                .filter(|name| !name.is_empty()).unwrap_or(dir_name).to_string();
                            self.folders.push(FolderInfo {
                                id,
                                name,
                                dir_name: dir_name.to_string(),
                                is_default: folder.get("is_default").and_then(Value::as_bool).unwrap_or(false),
                                locked: folder.get("locked").and_then(Value::as_bool).unwrap_or(false),
                            });
                        }
                    }
                }
                let skin = payload.get("ui_skin").and_then(HostSkin::from_value);
                let locale = payload.get("locale").and_then(host_locale);
                if skin.is_some() || locale.is_some() {
                    sink(UiCommand::HostAppearanceChanged { skin, locale });
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
                    if let Some(item) = self.note_list_item(&root, &row, &needle) {
                        notes.push(item.to_json(&root));
                    }
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
                let Some(dir_name) = self.folders.iter().find(|entry| entry.id == folder)
                    .map(|entry| entry.dir_name.clone()) else {
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
                let path = match root.resolve(&row.body_path) {
                    Ok(path) => path,
                    Err(error) => return failed("create_failed", error),
                };
                self.note_roots.insert(row.id, root.path().to_path_buf());
                self.open_notes.insert(row.id);
                sink(UiCommand::OpenNote {
                    note: OpenNote { note_id: row.id, folder_id: row.folder_id, path },
                });
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
                let (old_abs, new_abs) = match self.db.rename_target(&root, &row, stem) {
                    Ok(paths) => paths,
                    Err(error) => return failed("rename_failed", error),
                };
                let renamed = match self.sessions.clone() {
                    Some(manager) => manager
                        .rename_path(&old_abs, &new_abs, || {
                            self.db.rename_body_for_session(&root, &row, stem)
                                .map(|_| ())
                                .map_err(|error| {
                                    let message = error.to_string();
                                    if error.rollback_failed() {
                                        RenameAttemptError::recovery_required(message)
                                    } else {
                                        RenameAttemptError::recovered(message)
                                    }
                                })
                        })
                        .map(|outcome| (new_abs.clone(), outcome.warning)),
                    None => self.db.rename_body(&root, &row, stem).map(|path| (path, None)),
                };
                match renamed {
                    Ok((new_abs, warning)) => {
                        self.note_roots.insert(id, root.path().to_path_buf());
                        sink(UiCommand::NoteRenamed { note_id: id, path: new_abs.clone() });
                        if let Some(warning) = warning {
                            sink(UiCommand::Notice(warning));
                        }
                        self.note_changed(row.folder_id, id, "renamed");
                        Dispatch::Done(json!({
                            "note_id": id,
                            "path": new_abs.to_string_lossy(),
                        }))
                    }
                    Err(error) => failed("rename_failed", error),
                }
            }
            "note.delete" => {
                let Some(id) = payload.get("note_id").and_then(Value::as_i64).filter(|id| *id > 0) else {
                    return failed("bad_request", "note.delete 需要正整数 `note_id`".to_string());
                };
                let root = match payload.get("root") {
                    None => self.root_for(&json!({})).map_err(|error| ("no_root", error)),
                    Some(Value::String(path)) if !path.trim().is_empty() => {
                        crate::plugin::storage::Root::open(Path::new(path))
                            .map_err(|error| ("bad_request", error))
                    }
                    Some(_) => return failed("bad_request", "note.delete 的 `root` 必须是非空路径".to_string()),
                };
                match root.and_then(|root| self.delete_note_at(id, &root)) {
                    Ok(outcome) => {
                        if outcome.deleted {
                            sink(UiCommand::NoteDeleted { note_id: id });
                        }
                        Dispatch::Done(json!({
                            "note_id": id,
                            "deleted": outcome.deleted,
                            "warnings": outcome.warnings,
                        }))
                    }
                    Err((code, message)) => failed(code, message),
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

    fn note_list_item(&self, root: &Root, row: &NoteRow, needle: &str) -> Option<NoteListItem> {
        let locked = row.enc.is_private();
        let body = if locked { None } else { self.read_markdown(root, &row.body_path) };
        let title = body.as_deref().map(derive_title).filter(|title| !title.is_empty());
        let excerpt = body.as_deref().map(first_line).filter(|line| !line.is_empty());
        let file_name = Path::new(&row.body_path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !needle.is_empty() {
            // 文件名、派生标题与正文首行前 30 个字共用搜索文本；私有行只读文件名。
            let head: String = excerpt.as_deref().unwrap_or_default().chars().take(30).collect();
            let title = title.as_deref().unwrap_or_default();
            if !format!("{file_name} {title} {head}").to_lowercase().contains(needle) {
                return None;
            }
        }
        Some(NoteListItem {
            note_id: row.id,
            folder_id: row.folder_id,
            title,
            file_name,
            body_path: row.body_path.clone(),
            updated_at_ms: row.updated_at_ms,
            locked,
            excerpt,
        })
    }

    fn library(&self, folder_id: Option<i64>, query: &str) -> crate::plugin::storage::Res<LibrarySnapshot> {
        let root = self.root_for(&json!({}))?;
        let rows = match folder_id {
            Some(folder) => self.db.notes_in_folder(folder)?,
            None => self.db.all_notes()?,
        };
        let needle = query.trim().to_lowercase();
        let notes = rows.into_iter()
            .filter(|row| row.kind == NoteKind::Note)
            .filter(|row| !self.folders.iter().any(|folder| folder.id == row.folder_id && folder.locked))
            .filter_map(|row| self.note_list_item(&root, &row, &needle))
            .collect();
        Ok(LibrarySnapshot { folders: self.folders.clone(), notes })
    }

    /// 本地列表点击与宿主 `notes.open` 共用同一道身份、私有状态、路径与大小检查。
    fn checked_open_note(&self, root: &Root, id: i64) -> crate::plugin::storage::Res<(NoteRow, PathBuf)> {
        let row = self.db.note(id)?.ok_or_else(|| format!("第 {id} 号不存在"))?;
        if row.kind != NoteKind::Note {
            return Err(format!("第 {id} 号不是笔记"));
        }
        if self.folders.iter().any(|folder| folder.id == row.folder_id && folder.locked) {
            return Err(format!("第 {id} 号在锁定文件夹里"));
        }
        if row.enc.is_private() {
            return Err(format!("第 {id} 号是私有的，私有正文今天没接线"));
        }
        let abs = root.resolve(&row.body_path)?;
        if !abs.is_file() {
            return Err(format!("正文 {} 不存在", abs.display()));
        }
        if std::fs::metadata(&abs)
            .map(|meta| meta.len() > crate::plugin::storage::MAX_DOC_BYTES as u64)
            .unwrap_or(false)
        {
            return Err(format!(
                "正文超过可编辑上限 {} B",
                crate::plugin::storage::MAX_DOC_BYTES
            ));
        }
        Ok((row, abs))
    }

    fn open_local(&mut self, note_id: i64) -> crate::plugin::storage::Res<OpenNote> {
        let root = self.root_for(&json!({}))?;
        let (row, path) = self.checked_open_note(&root, note_id)?;
        self.note_roots.insert(row.id, root.path().to_path_buf());
        self.open_notes.insert(row.id);
        Ok(OpenNote { note_id: row.id, folder_id: row.folder_id, path })
    }

    /// 打开某一号：正文由正式会话从路径读取，界面只接收笔记身份。
    fn open_one(&mut self, root: &Root, id: i64, sink: &mut impl FnMut(UiCommand)) -> Dispatch {
        let (row, abs) = match self.checked_open_note(root, id) {
            Ok(value) => value,
            Err(error) => return failed("open_failed", error),
        };
        self.note_roots.insert(row.id, root.path().to_path_buf());
        self.open_notes.insert(row.id);
        sink(UiCommand::OpenNote {
            note: OpenNote { note_id: row.id, folder_id: row.folder_id, path: abs },
        });
        Dispatch::Done(json!({ "queued": true }))
    }

    fn handle_command(&mut self, command: EngineCommand) {
        match command {
            EngineCommand::Library { folder_id, query, reply } => {
                let _ = reply.send(self.library(folder_id, &query));
            }
            EngineCommand::OpenLocal { note_id, reply } => {
                let _ = reply.send(self.open_local(note_id));
            }
            EngineCommand::PrepareSave { note_id, path, reply } => {
                let result = self.prepare_save(note_id, &path).map(|_| ());
                let _ = reply.send(result);
            }
            EngineCommand::SaveFinished { note_id, path, reply } => {
                let result = self.finish_save(note_id, &path);
                let _ = reply.send(result);
            }
            EngineCommand::NoteClosed { note_id } => {
                self.open_notes.remove(&note_id);
            }
            EngineCommand::DeleteLocal { note_id, reply } => {
                let result = self.delete_note(note_id).map_err(|(_, message)| message);
                let _ = reply.send(result);
            }
        }
    }

    fn note_for_save(&self, note_id: i64, path: &Path) -> crate::plugin::storage::Res<NoteRow> {
        let root_path = self.note_roots.get(&note_id)
            .map(PathBuf::as_path)
            .or(self.root_path.as_deref())
            .ok_or_else(|| "还没有绑定正文根，无法确认正文归属".to_string())?;
        let root = Root::open(root_path)?;
        let row = self.db.note(note_id)?.ok_or_else(|| format!("第 {note_id} 号笔记不存在"))?;
        if row.kind != NoteKind::Note {
            return Err(format!("第 {note_id} 号不是笔记"));
        }
        if row.enc.is_private() {
            return Err(format!("第 {note_id} 号是私有笔记"));
        }
        let expected = root.resolve(&row.body_path)?;
        let actual = std::fs::canonicalize(path).map_err(|error| {
            format!("正文路径 {} 无法确认：{error}", path.display())
        })?;
        let expected = std::fs::canonicalize(expected).map_err(|error| {
            format!("宿主登记的正文无法确认：{error}")
        })?;
        if actual != expected {
            return Err("宿主登记的正文路径已变化，请重新打开笔记".to_string());
        }
        Ok(row)
    }

    fn prepare_save(&self, note_id: i64, path: &Path) -> crate::plugin::storage::Res<()> {
        self.note_for_save(note_id, path)?;
        let bytes = std::fs::metadata(path)
            .map_err(|error| format!("读取正文大小失败：{error}"))?
            .len();
        if bytes > crate::plugin::storage::MAX_DOC_BYTES as u64 {
            return Err(format!(
                "正文 {bytes} B 超过可保存上限 {} B",
                crate::plugin::storage::MAX_DOC_BYTES
            ));
        }
        Ok(())
    }

    fn finish_save(&mut self, note_id: i64, path: &Path) -> crate::plugin::storage::Res<()> {
        let mut row = self.note_for_save(note_id, path)?;
        let bytes = std::fs::metadata(path)
            .map_err(|error| format!("读取正文大小失败：{error}"))?
            .len();
        if bytes > crate::plugin::storage::MAX_DOC_BYTES as u64 {
            return Err(format!(
                "正文 {bytes} B 超过可改上限 {} B",
                crate::plugin::storage::MAX_DOC_BYTES
            ));
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|error| format!("读取已保存正文失败：{error}"))?;
        self.db.save_note(&mut row, &note_text(&raw))?;
        self.note_changed(row.folder_id, row.id, "saved");
        Ok(())
    }

    fn delete_note(&mut self, note_id: i64) -> Result<DeleteOutcome, (&'static str, String)> {
        let root = self.root_for(&json!({})).map_err(|error| ("no_root", error))?;
        self.delete_note_at(note_id, &root)
    }

    fn delete_note_at(
        &mut self,
        note_id: i64,
        root: &crate::plugin::storage::Root,
    ) -> Result<DeleteOutcome, (&'static str, String)> {
        if self.open_notes.contains(&note_id) {
            return Err(("busy", "笔记仍在编辑器里打开，请先关闭再删除".to_string()));
        }
        let row = match self.db.note(note_id) {
            Ok(Some(row)) => row,
            Ok(None) => {
                self.note_roots.remove(&note_id);
                return Ok(DeleteOutcome { deleted: false, warnings: Vec::new() });
            }
            Err(error) => return Err(("delete_failed", error)),
        };
        if row.kind != NoteKind::Note {
            return Err(("bad_request", format!("第 {note_id} 号不是笔记")));
        }
        if self.folders.iter().any(|folder| folder.id == row.folder_id && folder.locked) {
            return Err(("locked", format!("第 {note_id} 号在锁定文件夹里")));
        }
        if row.enc.is_private() {
            return Err(("private", format!("第 {note_id} 号是私有笔记")));
        }
        let deleted = match self.db.delete_note(&root, &row) {
            Ok(Some(deleted)) => deleted,
            Ok(None) => {
                self.note_roots.remove(&note_id);
                return Ok(DeleteOutcome { deleted: false, warnings: Vec::new() });
            }
            Err(error) => return Err(("delete_failed", error)),
        };
        self.note_roots.remove(&note_id);
        self.open_notes.remove(&note_id);
        self.note_changed(row.folder_id, row.id, "deleted");
        Ok(DeleteOutcome { deleted: true, warnings: deleted.warnings })
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
pub fn serve(session: Session, sink: impl FnMut(UiCommand)) -> Result<(), String> {
    serve_loop(session, sink, None, None)
}

/// 生产泵同时接收界面发来的保存步骤，正文仍由正式 Markdown 会话写入。
fn serve_with_commands(
    session: Session,
    sink: impl FnMut(UiCommand),
    commands: mpsc::UnboundedReceiver<EngineCommand>,
    sessions: Option<MarkdownDocumentSessionManager>,
) -> Result<(), String> {
    serve_loop(session, sink, Some(commands), sessions)
}

fn serve_loop(
    mut session: Session,
    mut sink: impl FnMut(UiCommand),
    mut commands: Option<mpsc::UnboundedReceiver<EngineCommand>>,
    sessions: Option<MarkdownDocumentSessionManager>,
) -> Result<(), String> {
    let db_path = Path::new(session.data_dir()).join("notes.db");
    let mut engine = Engine {
        db: Db::open(&db_path)?,
        db_path,
        root_path: None,
        note_roots: HashMap::new(),
        open_notes: HashSet::new(),
        folders: Vec::new(),
        pending_change: None,
        sessions,
    };
    loop {
        if let Some(receiver) = commands.as_mut() {
            while let Ok(command) = receiver.try_recv() {
                engine.handle_command(command);
            }
        }
        let step = session.pump(&mut |op, payload| engine.handle(op, payload, &mut sink))?;
        let idle = matches!(step, crate::plugin::dispatch::Step::Idle);
        match step {
            crate::plugin::dispatch::Step::Served { .. }
            | crate::plugin::dispatch::Step::Idle => {
                if let Some(change) = engine.pending_change.take() {
                    match session.request(
                        &mut |op, payload| engine.handle(op, payload, &mut sink),
                        "host.notes_changed",
                        change,
                        Duration::from_secs(2),
                    ) {
                        Ok(_)
                        | Err(RequestError::TimedOut(_)) => {}
                        Err(RequestError::Broken(error)) => return Err(error),
                    }
                }
                if idle {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            crate::plugin::dispatch::Step::Closed => return Ok(()),
        }
    }
}

/// 数据面失败帧那一格（`code` 必须是过段名闸的那个词，`message` 直接进 `Failure.message`）。
fn failed(code: &str, message: impl Into<String>) -> Dispatch {
    Dispatch::Failed { code: code.to_string(), message: message.into() }
}

/// 插件模式入口：先绑数据面 + 握手（窗口之前），再进 GPUI 事件循环开正式编辑器。
pub fn run(endpoints: Endpoints) {
    run_plugin(endpoints);
}

fn session_root(data_dir: &str) -> PathBuf {
    let digest = Sha256::digest(data_dir.as_bytes());
    let key: String = digest.iter().take(12).map(|byte| format!("{byte:02x}")).collect();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir()
        .join("pingloss-notes-sessions")
        .join(key)
        .join(format!("{}-{nonce}", std::process::id()))
}

/// 绑定宿主数据面并初始化插件自己的编辑服务，随后进入 GPUI 事件循环。
fn run_plugin(endpoints: Endpoints) {
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
    let root = session_root(session.data_dir());
    let lease = match SessionLease::acquire(&root) {
        Ok(lease) => Arc::new(lease),
        Err(error) => {
            eprintln!("插件会话目录不可用：{error}");
            std::process::exit(2);
        }
    };
    let manager = match MarkdownDocumentSessionManager::new(root) {
        Ok(manager) => manager,
        Err(error) => {
            eprintln!("插件编辑会话起不来：{error}");
            std::process::exit(2);
        }
    };
    let pump_manager = manager.clone();

    #[cfg(not(windows))]
    let application = gpui_platform::application();
    #[cfg(windows)]
    let application = gpui::Application::with_platform(std::rc::Rc::new(
        gpui_windows::WindowsPlatform::new(false).expect("初始化 Windows 平台"),
    ));
    application.run(move |cx: &mut App| {
        cx.set_global(crate::theme::Theme::light());
        cx.set_global(crate::ui::motion::MotionPreference::default());
        let mut settings = crate::settings::Settings::default();
        settings.autosave = Some(Duration::from_millis(800));
        settings.snapshot_before_save = false;
        settings.show_outline = false;
        settings.stats_visible = false;
        cx.set_global(settings);
        crate::theme::catalog::init(cx);
        cx.set_global(crate::images::ImageStore::default());
        cx.set_global(crate::embeds::EmbedStore::default());
        crate::ui::reload_key_bindings(cx);
        crate::ui::dialogs::init(cx);
        let bounds = Bounds::centered(None, size(px(960.0), px(720.0)), cx);

        let (tx, mut rx) = mpsc::unbounded_channel::<UiCommand>();
        let _ = tx.send(connected);
        let (command_tx, command_rx) = mpsc::unbounded_channel::<EngineCommand>();

        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(plugin_text(cx, "plugin.appTitle").into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| PluginWindow::new(manager, lease, command_tx, cx));
                    let this = view.downgrade();
                    window.on_window_should_close(cx, move |window, cx| {
                        this.update(cx, |view, cx| view.request_close_window(window, cx))
                            .unwrap_or(true)
                    });
                    view
                },
            )
            .expect("插件模式开不出窗口");

        cx.spawn(async move |cx| {
            while let Some(command) = rx.recv().await {
                let _ = cx.update(|cx| {
                    window.update(cx, |view, window, cx| view.apply(command, window, cx))
                });
            }
        })
        .detach();

        std::thread::spawn(move || {
            if let Err(error) = serve_with_commands(
                session,
                move |command| {
                    let _ = tx.send(command);
                },
                command_rx,
                Some(pump_manager),
            ) {
                eprintln!("插件模式泵线程退了：{error}");
            }
        });
        cx.activate(true);
    });
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AfterSave {
    None,
    Close,
    Open(OpenNote),
    Delete(OpenNote),
}

impl Default for AfterSave {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Default)]
struct CloseIntent {
    pending: bool,
    after_save: AfterSave,
}

impl CloseIntent {
    fn queue(&mut self, after: AfterSave) {
        if matches!(after, AfterSave::Close) {
            self.pending = true;
            self.after_save = AfterSave::Close;
        } else if !self.pending && matches!(self.after_save, AfterSave::None) {
            self.after_save = after;
        }
    }

    fn take_after_save(&mut self) -> AfterSave {
        self.pending = false;
        std::mem::replace(&mut self.after_save, AfterSave::None)
    }

    fn cancel(&mut self) {
        self.pending = false;
        self.after_save = AfterSave::None;
    }
}

enum PreparedOpen {
    Document {
        port: NativeWindowPort,
        status: crate::text_document::markdown_session::MarkdownStatus,
    },
    Recovery(RecoveryCandidate),
}

struct PendingRecovery {
    note: OpenNote,
    candidate: RecoveryCandidate,
}

/// 插件模式的单文档窗。正文、历史和撤销只归正式 Markdown 会话，界面持有窗口、订阅与保存编排。
struct PluginWindow {
    manager: MarkdownDocumentSessionManager,
    _lease: Arc<SessionLease>,
    document: Option<Entity<DocumentHost>>,
    note: Option<OpenNote>,
    search: Entity<TextField>,
    _search_subscription: Subscription,
    folders: Vec<FolderInfo>,
    notes: Vec<NoteListItem>,
    selected_folder: Option<i64>,
    library_task: Option<Task<()>>,
    library_generation: u64,
    library_loading: bool,
    library_error: Option<StatusMessage>,
    library_scroll: UniformListScrollHandle,
    resolving_open: bool,
    open_request_generation: u64,
    subscription: Option<Subscription>,
    autosave: Option<Task<()>>,
    engine: mpsc::UnboundedSender<EngineCommand>,
    status: StatusMessage,
    failure: Option<StatusMessage>,
    focus: FocusHandle,
    saving: bool,
    waiting_toc: bool,
    close_intent: CloseIntent,
    index_pending: Option<OpenNote>,
    opening: bool,
    opening_note: Option<OpenNote>,
    open_generation: u64,
    open_cancel: Option<Arc<AtomicBool>>,
    pending_open: Option<OpenNote>,
    pending_recovery: Option<PendingRecovery>,
    dismiss_after_open: Option<RecoveryCandidate>,
    close_prompt: bool,
    deleting: bool,
    delete_prompt: bool,
}

impl PluginWindow {
    fn new(
        manager: MarkdownDocumentSessionManager,
        lease: Arc<SessionLease>,
        engine: mpsc::UnboundedSender<EngineCommand>,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            let mut field = TextField::new(&plugin_text(cx, "plugin.searchPlaceholder"), cx);
            field.set_settings_style();
            field.set_tab_stop();
            field
        });
        let search_subscription = cx.subscribe(&search, |view, field, event, cx| match event {
            FieldEvent::Changed if !field.read(cx).is_composing() => {
                let value = field.read(cx).content.clone();
                if value.len() > 256 {
                    let mut end = 256;
                    while !value.is_char_boundary(end) { end -= 1; }
                    field.update(cx, |field, cx| field.set_text(&value[..end], cx));
                    return;
                }
                view.schedule_library_refresh(cx);
            }
            _ => {}
        });
        Self {
            manager,
            _lease: lease,
            document: None,
            note: None,
            search,
            _search_subscription: search_subscription,
            folders: Vec::new(),
            notes: Vec::new(),
            selected_folder: None,
            library_task: None,
            library_generation: 0,
            library_loading: false,
            library_error: None,
            library_scroll: UniformListScrollHandle::new(),
            resolving_open: false,
            open_request_generation: 0,
            subscription: None,
            autosave: None,
            engine,
            status: StatusMessage::text("plugin.status.waiting"),
            failure: None,
            focus: cx.focus_handle(),
            saving: false,
            waiting_toc: false,
            close_intent: CloseIntent::default(),
            index_pending: None,
            opening: false,
            opening_note: None,
            open_generation: 0,
            open_cancel: None,
            pending_open: None,
            pending_recovery: None,
            dismiss_after_open: None,
            close_prompt: false,
            deleting: false,
            delete_prompt: false,
        }
    }

    fn schedule_library_refresh(&mut self, cx: &mut Context<Self>) {
        self.library_generation = self.library_generation.wrapping_add(1);
        let generation = self.library_generation;
        self.library_loading = true;
        self.library_error = None;
        self.library_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(Duration::from_millis(120)).await;
            let _ = view.update(cx, |view, cx| view.refresh_library_now(generation, cx));
        }));
        cx.notify();
    }

    fn refresh_library_now(&mut self, generation: u64, cx: &mut Context<Self>) {
        if generation != self.library_generation { return; }
        let query = self.search.read(cx).content.clone();
        let folder_id = self.selected_folder;
        let (reply, response) = oneshot::channel();
        if self.engine.send(EngineCommand::Library { folder_id, query, reply }).is_err() {
            self.library_loading = false;
            self.library_error = Some(StatusMessage::text("plugin.error.dataDisconnected"));
            cx.notify();
            return;
        }
        cx.spawn(async move |view, cx| {
            let result = match response.await {
                Ok(result) => result.map_err(StatusMessage::raw),
                Err(_) => Err(StatusMessage::text("plugin.error.noListReply")),
            };
            let _ = view.update(cx, |view, cx| {
                if generation != view.library_generation { return; }
                view.library_loading = false;
                match result {
                    Ok(snapshot) => {
                        view.folders = snapshot.folders;
                        view.notes = snapshot.notes;
                        view.library_error = None;
                        if view.selected_folder.is_some_and(|selected| {
                            !view.folders.iter().any(|folder| folder.id == selected)
                        }) {
                            view.selected_folder = None;
                            view.schedule_library_refresh(cx);
                            return;
                        }
                        view.library_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                    }
                    Err(error) => view.library_error = Some(error),
                }
                cx.notify();
            });
        }).detach();
    }

    fn request_open_from_library(&mut self, note_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        if self.resolving_open || self.opening || self.saving || self.waiting_toc || self.close_prompt || self.deleting {
            return;
        }
        if self.notes.iter().any(|note| note.note_id == note_id && note.locked) {
            self.status = StatusMessage::text("plugin.status.privateUnavailable");
            cx.notify();
            return;
        }
        self.resolving_open = true;
        self.open_request_generation = self.open_request_generation.wrapping_add(1);
        let generation = self.open_request_generation;
        self.failure = None;
        self.status = StatusMessage::text("plugin.status.confirming");
        let (reply, response) = oneshot::channel();
        if self.engine.send(EngineCommand::OpenLocal { note_id, reply }).is_err() {
            self.resolving_open = false;
            self.failure = Some(StatusMessage::text("plugin.error.openDisconnected"));
            cx.notify();
            return;
        }
        cx.spawn_in(window, async move |view, cx| {
            let result = match response.await {
                Ok(result) => result.map_err(StatusMessage::raw),
                Err(_) => Err(StatusMessage::text("plugin.error.noOpenReply")),
            };
            let _ = view.update_in(cx, |view, window, cx| {
                if generation != view.open_request_generation { return; }
                view.resolving_open = false;
                if view.close_intent.pending {
                    view.close_intent.cancel();
                    if let Ok(note) = &result {
                        view.release_open(note.note_id);
                    }
                    window.remove_window();
                    return;
                }
                match result {
                    Ok(note) => view.request_open(note, window, cx),
                    Err(error) => {
                        view.failure = Some(error);
                        view.status = StatusMessage::text("plugin.status.openFailed");
                    }
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn apply(&mut self, command: UiCommand, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            UiCommand::Connected {
                data_dir,
                capabilities,
            } => {
                self.status = StatusMessage::localized(
                    "plugin.status.connected",
                    &[("data_dir", &data_dir), ("count", &capabilities.len().to_string())],
                );
            }
            UiCommand::FoldersChanged => {
                if self.document.is_none() {
                    self.status = StatusMessage::text("plugin.status.snapshotReady");
                }
                self.schedule_library_refresh(cx);
            }
            UiCommand::HostAppearanceChanged { skin, locale } => {
                if let Some(skin) = skin {
                    let theme = skin.theme();
                    crate::layout::bump_generation();
                    cx.set_global(theme);
                }
                if let Some(locale) = locale {
                    crate::settings::Settings::set_with(
                        cx,
                        "interfaceLanguage",
                        json!(locale),
                        false,
                    );
                    let placeholder = plugin_text(cx, "plugin.searchPlaceholder");
                    self.search.update(cx, |field, cx| field.set_placeholder(&placeholder, cx));
                }
                self.update_title(window, cx);
            }
            UiCommand::OpenNote { note } => self.request_open(note, window, cx),
            UiCommand::NoteRenamed { note_id, path } => {
                retarget_queued_notes(
                    &mut self.note,
                    &mut self.index_pending,
                    &mut self.opening_note,
                    &mut self.pending_open,
                    &mut self.close_intent.after_save,
                    note_id,
                    &path,
                );
                if let Some(pending) = &mut self.pending_recovery {
                    retarget_note(&mut pending.note, note_id, &path);
                    if pending.note.note_id == note_id {
                        pending.candidate.path = Some(path.clone());
                    }
                }
                if self.note.as_ref().is_some_and(|note| note.note_id == note_id) {
                    if let Some(document) = self.document.clone() {
                        document.update(cx, |host, cx| host.refresh_renamed_path(cx));
                    }
                    self.status = StatusMessage::text("plugin.status.renamed");
                    self.update_title(window, cx);
                    self.schedule_library_refresh(cx);
                }
            }
            UiCommand::NoteDeleted { note_id } => {
                if let Some(cancel) = self.open_cancel.take() {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                }
                if self.opening_note.as_ref().is_some_and(|note| note.note_id == note_id) {
                    self.open_generation = self.open_generation.wrapping_add(1);
                    self.opening = false;
                    self.opening_note = None;
                }
                self.pending_open = self.pending_open.take().filter(|note| note.note_id != note_id);
                if self.note.as_ref().is_some_and(|note| note.note_id == note_id) {
                    let _ = self.close_current(false, cx);
                }
                if self.document.is_none() {
                    self.status = StatusMessage::text("plugin.status.deleted");
                }
                self.schedule_library_refresh(cx);
            }
            UiCommand::Notice(message) => self.status = StatusMessage::raw(message),
            UiCommand::OpenBlank => self.status = StatusMessage::text("plugin.status.noNote"),
        }
        cx.notify();
    }

    fn request_open(&mut self, note: OpenNote, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_intent.pending {
            self.release_open(note.note_id);
            return;
        }
        if self.resolving_open {
            self.pending_open = Some(note);
            return;
        }
        if self.note.as_ref() == Some(&note) && self.document.is_some() && !self.opening {
            if let Some(document) = self.document.clone() {
                window.focus(&document.focus_handle(cx), cx);
            }
            return;
        }
        if self.saving || self.waiting_toc {
            self.close_intent.queue(AfterSave::Open(note));
            return;
        }
        if self.opening || self.close_prompt {
            self.pending_open = Some(note);
            return;
        }
        if self.index_pending.is_some() {
            self.save(AfterSave::Open(note), window, cx);
            return;
        }
        let dirty = self.document.as_ref().is_some_and(|document| document.read(cx).is_dirty(cx));
        if dirty {
            self.close_prompt = true;
            let document = self.document.clone().expect("dirty document");
            let response = crate::ui::dialogs::confirm_document_close(
                document.read(cx).title_message(),
                document.read(cx).has_failed_input(cx),
                window,
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                let choice = response.await;
                let _ = this.update_in(cx, |view, window, cx| {
                    view.close_prompt = false;
                    match choice {
                        crate::ui::dialogs::SaveChoice::Save => {
                            view.save(AfterSave::Open(note), window, cx);
                        }
                        crate::ui::dialogs::SaveChoice::Discard => {
                            if view.close_current(true, cx).is_ok() {
                                view.open_note_now(note, window, cx);
                            }
                        }
                        crate::ui::dialogs::SaveChoice::Cancel => {
                            view.release_open(note.note_id);
                        }
                    }
                    cx.notify();
                });
            })
            .detach();
            return;
        }
        if self.close_current(false, cx).is_ok() {
            self.open_note_now(note, window, cx);
        }
    }

    fn release_open(&mut self, note_id: i64) {
        let _ = self.engine.send(EngineCommand::NoteClosed { note_id });
    }

    fn request_delete(&mut self, note: OpenNote, window: &mut Window, cx: &mut Context<Self>) {
        if self.deleting || self.delete_prompt || self.close_prompt || self.opening || self.resolving_open {
            return;
        }
        if self.saving || self.waiting_toc {
            self.close_intent.queue(AfterSave::Delete(note));
            return;
        }
        if self.index_pending.is_some() {
            self.save(AfterSave::Delete(note), window, cx);
            return;
        }
        self.delete_prompt = true;
        let name = file_name(&note.path);
        let response = window.prompt(
            PromptLevel::Warning,
            &plugin_text(cx, "plugin.delete.title"),
            Some(&plugin_format(cx, "plugin.delete.body", &[("name", &name)])),
            &[
                PromptButton::new(plugin_text(cx, "plugin.delete.confirm")),
                PromptButton::cancel(plugin_text(cx, "plugin.delete.cancel")),
            ],
            cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            let choice = response.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.delete_prompt = false;
                if matches!(choice, Ok(0)) {
                    view.confirm_delete(note, window, cx);
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn confirm_delete(&mut self, note: OpenNote, window: &mut Window, cx: &mut Context<Self>) {
        if self.note.as_ref().map(|current| current.note_id) != Some(note.note_id) {
            return;
        }
        let dirty = self.document.as_ref().is_some_and(|document| document.read(cx).is_dirty(cx));
        if !dirty {
            if self.close_current(false, cx).is_ok() {
                self.request_delete_now(note, window, cx);
            }
            return;
        }
        self.close_prompt = true;
        let document = self.document.clone().expect("dirty document");
        let response = crate::ui::dialogs::confirm_document_close(
            document.read(cx).title_message(),
            document.read(cx).has_failed_input(cx),
            window,
            cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            let choice = response.await;
            let _ = view.update_in(cx, |view, window, cx| {
                view.close_prompt = false;
                match choice {
                    crate::ui::dialogs::SaveChoice::Save => {
                        view.save(AfterSave::Delete(note), window, cx);
                    }
                    crate::ui::dialogs::SaveChoice::Discard => {
                        if view.close_current(true, cx).is_ok() {
                            view.request_delete_now(note, window, cx);
                        }
                    }
                    crate::ui::dialogs::SaveChoice::Cancel => {}
                }
                cx.notify();
            });
        }).detach();
    }

    fn request_delete_now(&mut self, note: OpenNote, window: &mut Window, cx: &mut Context<Self>) {
        self.deleting = true;
        self.failure = None;
        self.status = StatusMessage::text("plugin.status.deleting");
        let (reply, response) = oneshot::channel();
        if self.engine.send(EngineCommand::DeleteLocal { note_id: note.note_id, reply }).is_err() {
            self.deleting = false;
            self.failure = Some(StatusMessage::text("plugin.error.deleteDisconnected"));
            self.status = StatusMessage::text("plugin.status.deleteFailed");
            cx.notify();
            return;
        }
        cx.spawn_in(window, async move |view, cx| {
            let result = match response.await {
                Ok(result) => result.map_err(StatusMessage::raw),
                Err(_) => Err(StatusMessage::text("plugin.error.noDeleteReply")),
            };
            let _ = view.update_in(cx, |view, _, cx| {
                view.deleting = false;
                match result {
                    Ok(outcome) => {
                        if outcome.deleted {
                            view.status = if outcome.warnings.is_empty() {
                                StatusMessage::text("plugin.status.deleted")
                            } else {
                                StatusMessage::localized(
                                    "plugin.status.deletedWithWarning",
                                    &[("warning", &outcome.warnings.join("；"))],
                                )
                            };
                        } else {
                            view.status = StatusMessage::text("plugin.status.deleteMissing");
                        }
                    }
                    Err(error) => {
                        view.failure = Some(error);
                        view.status = StatusMessage::text("plugin.status.deleteFailed");
                    }
                }
                view.schedule_library_refresh(cx);
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn open_note_now(&mut self, note: OpenNote, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_open(note, true, None, window, cx);
    }

    fn begin_open(
        &mut self,
        note: OpenNote,
        allow_recovery: bool,
        dismiss_after_open: Option<RecoveryCandidate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(cancel) = self.open_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        self.open_generation = self.open_generation.wrapping_add(1);
        let generation = self.open_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.open_cancel = Some(cancel.clone());
        self.opening = true;
        self.opening_note = Some(note.clone());
        self.failure = None;
        self.status = StatusMessage::localized(
            "plugin.status.opening",
            &[("name", &file_name(&note.path))],
        );
        self.dismiss_after_open = dismiss_after_open;
        let manager = self.manager.clone();
        let note_for_open = note.clone();
        let task = cx.background_executor().spawn(async move {
            if allow_recovery {
                let (candidates, warning) = manager.recovery_candidates(&cancel)
                    .map_err(|error| error.to_string())?;
                if let Some(candidate) = candidates.into_iter().find(|candidate| {
                    candidate.path.as_ref().is_some_and(|path| same_path(path, &note_for_open.path))
                }) {
                    return Ok::<_, String>(PreparedOpen::Recovery(candidate));
                }
                if let Some(warning) = warning {
                    eprintln!("插件恢复扫描提示：{warning}");
                }
            }
            let owner = format!("pingloss-notes-{}", note_for_open.note_id);
            let port = NativeWindowPort::open_in(manager, owner, Some(note_for_open.path.clone()), None)
                .map_err(|error| error.to_string())?;
            let status = port.status().map_err(|error| error.to_string())?;
            Ok(PreparedOpen::Document { port, status })
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.complete_open(note, generation, result, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn complete_open(
        &mut self,
        note: OpenNote,
        generation: u64,
        result: Result<PreparedOpen, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.open_generation {
            self.release_open(note.note_id);
            if let Ok(PreparedOpen::Document { port, .. }) = result {
                cx.background_executor()
                    .spawn(async move { let _ = port.release_unmounted(); })
                    .detach();
            }
            return;
        }
        self.opening = false;
        self.open_cancel = None;
        let requested_path = note.path.clone();
        let note = self.opening_note.take()
            .filter(|current| current.note_id == note.note_id)
            .unwrap_or(note);
        if self.close_intent.pending {
            self.close_intent.cancel();
            self.release_open(note.note_id);
            if let Ok(PreparedOpen::Document { port, .. }) = result {
                cx.background_executor()
                    .spawn(async move { let _ = port.release_unmounted(); })
                    .detach();
            }
            window.remove_window();
            return;
        }
        match result {
            Ok(PreparedOpen::Recovery(candidate)) => {
                self.pending_recovery = Some(PendingRecovery { note, candidate });
                self.prompt_recovery(window, cx);
            }
            Ok(PreparedOpen::Document { port, status }) => {
                self.install_document(note, port, status, window, cx);
            }
            Err(error) => {
                if note.path != requested_path {
                    self.begin_open(note, true, None, window, cx);
                } else {
                    self.fail_open(note.note_id, error, cx);
                }
            }
        }
        cx.notify();
    }

    fn install_document(
        &mut self,
        note: OpenNote,
        port: NativeWindowPort,
        status: crate::text_document::markdown_session::MarkdownStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if status.byte_length > crate::plugin::storage::MAX_DOC_BYTES as u64 {
            cx.background_executor()
                .spawn(async move { let _ = port.release_unmounted(); })
                .detach();
            self.fail_open(note.note_id, format!(
                "正文 {} B 超过可编辑上限 {} B",
                status.byte_length,
                crate::plugin::storage::MAX_DOC_BYTES
            ), cx);
            return;
        }
        let path_changed_during_open = status.path.as_deref()
            != Some(note.path.to_string_lossy().as_ref());
        let loaded = match port.load_initial_group(status.revision, &WindowRequest::default()) {
            Ok(loaded) => loaded,
            Err(error) => {
                cx.background_executor()
                    .spawn(async move { let _ = port.release_unmounted(); })
                    .detach();
                self.fail_open(note.note_id, error.to_string(), cx);
                return;
            }
        };
        let adapter = match WindowHostAdapter::new(
            loaded.input,
            format!("pingloss-notes-window-{}-{}", note.note_id, self.open_generation),
            0,
            loaded.context,
        ) {
            Ok(adapter) => adapter,
            Err(error) => {
                cx.background_executor()
                    .spawn(async move { let _ = port.release_unmounted(); })
                    .detach();
                self.fail_open(note.note_id, error.to_string(), cx);
                return;
            }
        };
        let document = cx.new(|cx| {
            DocumentHost::workspace(port, adapter, status, ViewMode::Semantic, None, cx)
        });
        if path_changed_during_open {
            document.update(cx, |host, cx| host.refresh_renamed_path(cx));
        }
        self.subscription = Some(cx.subscribe_in(
            &document,
            window,
            |view, document, event, window, cx| {
                view.on_document_event(document, event, window, cx);
            },
        ));
        self.note = Some(note);
        self.document = Some(document.clone());
        self.failure = None;
        self.status = StatusMessage::text("plugin.status.opened");
        self.update_title(window, cx);
        self.schedule_library_refresh(cx);
        window.focus(&document.focus_handle(cx), cx);
        if let Some(candidate) = self.dismiss_after_open.take() {
            let manager = self.manager.clone();
            cx.background_executor()
                .spawn(async move {
                    if let Err(error) = manager.dismiss_recovery(&candidate) {
                        eprintln!("插件清理已放弃的恢复草稿失败：{error}");
                    }
                })
                .detach();
        }
        if let Some(note) = self.pending_open.take() {
            self.request_open(note, window, cx);
        }
    }

    fn prompt_recovery(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_recovery.take() else { return; };
        self.close_prompt = true;
        let label = pending.candidate.label();
        let response = window.prompt(
            PromptLevel::Warning,
            &plugin_text(cx, "plugin.recovery.title"),
            Some(&plugin_format(
                cx,
                "plugin.recovery.body",
                &[("label", &label)],
            )),
            &[
                PromptButton::new(plugin_text(cx, "plugin.recovery.restore")),
                PromptButton::new(plugin_text(cx, "plugin.recovery.disk")),
                PromptButton::cancel(plugin_text(cx, "plugin.recovery.cancel")),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let choice = response.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.close_prompt = false;
                match choice {
                    Ok(0) => view.recover_note(pending.note, pending.candidate, window, cx),
                    Ok(1) => view.begin_open(pending.note, false, Some(pending.candidate), window, cx),
                    _ => {
                        view.release_open(pending.note.note_id);
                        view.status = StatusMessage::text("plugin.status.recoveryKept");
                        if let Some(note) = view.pending_open.take() {
                            view.request_open(note, window, cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn recover_note(
        &mut self,
        note: OpenNote,
        candidate: RecoveryCandidate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(cancel) = self.open_cancel.take() {
            cancel.store(true, std::sync::atomic::Ordering::Release);
        }
        self.open_generation = self.open_generation.wrapping_add(1);
        let generation = self.open_generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.open_cancel = Some(cancel.clone());
        self.opening = true;
        self.opening_note = Some(note.clone());
        self.status = StatusMessage::text("plugin.status.restoring");
        let manager = self.manager.clone();
        let owner = format!("pingloss-notes-{}", note.note_id);
        let task = cx.background_executor().spawn(async move {
            let status = manager.adopt_recovery(&owner, &candidate, false, || {
                cancel.load(std::sync::atomic::Ordering::Acquire)
            }).map_err(|error| error.to_string())?;
            let port = NativeWindowPort {
                manager,
                owner,
                document_id: status.document_id.clone(),
            };
            Ok::<_, String>(PreparedOpen::Document { port, status })
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.complete_open(note, generation, result, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn fail_open(&mut self, note_id: i64, error: String, cx: &mut Context<Self>) {
        self.opening = false;
        self.opening_note = None;
        self.dismiss_after_open = None;
        self.release_open(note_id);
        self.failure = Some(StatusMessage::raw(error));
        self.status = StatusMessage::text("plugin.status.openFailed");
        cx.notify();
    }

    fn on_document_event(
        &mut self,
        document: &Entity<DocumentHost>,
        event: &DocumentEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.document.as_ref() != Some(document) {
            return;
        }
        match event {
            DocumentEvent::Changed => self.schedule_autosave(document.clone(), window, cx),
            DocumentEvent::Failure(message) => self.failure = Some(StatusMessage::raw(message.clone())),
            DocumentEvent::Notice(message) => self.status = StatusMessage::raw(message.clone()),
            DocumentEvent::TocSettled(warning) => {
                if let Some(warning) = warning {
                    self.status = StatusMessage::localized(
                        "plugin.status.tocWarning",
                        &[("warning", warning)],
                    );
                }
                if self.waiting_toc {
                    self.waiting_toc = false;
                    let after = self.close_intent.take_after_save();
                    self.save(after, window, cx);
                }
            }
            DocumentEvent::Saved => self.status = StatusMessage::text("plugin.status.saved"),
            DocumentEvent::OpenMarkdownLink { path, .. } => {
                self.status = StatusMessage::localized(
                    "plugin.status.openFromBodyUnsupported",
                    &[("path", &path.display().to_string())],
                );
            }
            DocumentEvent::FileStateChanged
            | DocumentEvent::Status
            | DocumentEvent::ToolbarState => {}
        }
        cx.notify();
    }

    fn schedule_autosave(
        &mut self,
        document: Entity<DocumentHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving || self.waiting_toc || self.index_pending.is_some() {
            return;
        }
        if document.read(cx).has_composition(cx) {
            return;
        }
        let Some(delay) = cx.global::<crate::settings::Settings>().autosave else { return; };
        self.autosave = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.document.as_ref() != Some(&document) {
                    return;
                }
                if document.read(cx).has_composition(cx) {
                    view.schedule_autosave(document, window, cx);
                    return;
                }
                if !view.saving
                    && !view.waiting_toc
                    && document.read(cx).is_dirty(cx)
                {
                    view.save(AfterSave::None, window, cx);
                }
            });
        }));
    }

    fn save(&mut self, after: AfterSave, window: &mut Window, cx: &mut Context<Self>) {
        if self.opening || self.close_prompt {
            return;
        }
        if self.saving || self.waiting_toc {
            self.close_intent.queue(after);
            return;
        }
        let (Some(document), Some(note)) = (self.document.clone(), self.note.clone()) else {
            match after {
                AfterSave::Close => window.remove_window(),
                AfterSave::Delete(note) => self.request_delete_now(note, window, cx),
                AfterSave::None | AfterSave::Open(_) => {}
            }
            return;
        };
        if document.read(cx).status().byte_length > crate::plugin::storage::MAX_DOC_BYTES as u64 {
            self.fail_save(format!(
                "正文 {} B 超过可保存上限 {} B",
                document.read(cx).status().byte_length,
                crate::plugin::storage::MAX_DOC_BYTES
            ), cx);
            return;
        }
        self.close_intent.queue(after);
        self.failure = None;
        if document.read(cx).has_composition(cx) {
            self.schedule_autosave(document, window, cx);
            self.status = StatusMessage::text("plugin.status.waitingComposition");
            cx.notify();
            return;
        }
        match document.update(cx, |host, cx| host.finish_input(window, cx)) {
            Ok(true) => {}
            Ok(false) => {
                self.schedule_autosave(document, window, cx);
                self.status = StatusMessage::text("plugin.status.waitingEditor");
                cx.notify();
                return;
            }
            Err(error) => {
                self.fail_save(error.to_string(), cx);
                return;
            }
        }
        if self.index_pending.as_ref() == Some(&note) && !document.read(cx).is_dirty(cx) {
            self.finish_index(document, note, window, cx);
            return;
        }
        if !document.update(cx, |host, cx| host.toc_ready_for_save(cx)) {
            self.waiting_toc = true;
            self.status = StatusMessage::text("plugin.status.waitingToc");
            cx.notify();
            return;
        }
        self.prepare_save(document, note, window, cx);
    }

    fn prepare_save(
        &mut self,
        document: Entity<DocumentHost>,
        note: OpenNote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.saving = true;
        self.status = StatusMessage::text("plugin.status.confirmingOwnership");
        let (reply, response) = oneshot::channel();
        if self.engine.send(EngineCommand::PrepareSave {
            note_id: note.note_id,
            path: note.path.clone(),
            reply,
        }).is_err() {
            self.fail_save("宿主数据面已断开，无法确认笔记归属".to_string(), cx);
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let result = match response.await {
                Ok(result) => result,
                Err(_) => Err("宿主数据面没有返回保存确认".to_string()),
            };
            let _ = this.update_in(cx, |view, window, cx| {
                if view.document.as_ref() != Some(&document)
                    || view.note.as_ref().map(|current| current.note_id) != Some(note.note_id)
                {
                    return;
                }
                match result {
                    Ok(()) => view.save_body(document, window, cx),
                    Err(error) => view.fail_save(error, cx),
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn save_body(
        &mut self,
        document: Entity<DocumentHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !document.update(cx, |host, cx| host.toc_ready_for_save(cx)) {
            self.saving = false;
            self.waiting_toc = true;
            self.status = StatusMessage::text("plugin.status.waitingToc");
            cx.notify();
            return;
        }
        let (port, captured) = match document.update(cx, |host, cx| host.begin_save(cx)) {
            Ok(value) => value,
            Err(error) => {
                self.fail_save(error.to_string(), cx);
                return;
            }
        };
        self.status = StatusMessage::text("plugin.status.savingBody");
        let task = cx.background_executor().spawn(async move {
            // 让会话在路径锁内解析当前文件名，避免保存与宿主重命名交错时写回旧路径。
            port.save_captured(captured, None, false)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.document.as_ref() != Some(&document) {
                    return;
                }
                let success = document.update(cx, |host, cx| host.finish_save(result, cx));
                if !success {
                    view.fail_save("正文保存失败，未更新宿主索引".to_string(), cx);
                    return;
                }
                let Some(mut note) = view.note.clone() else {
                    view.fail_save("保存成功后笔记身份丢失，未更新宿主索引".to_string(), cx);
                    return;
                };
                if let Some(path) = document.read(cx).path() {
                    note.path = path;
                    view.note = Some(note.clone());
                }
                view.finish_index(document, note, window, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn finish_index(
        &mut self,
        document: Entity<DocumentHost>,
        note: OpenNote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.saving = true;
        self.status = StatusMessage::text("plugin.status.updatingIndex");
        let (reply, response) = oneshot::channel();
        if self.engine.send(EngineCommand::SaveFinished {
            note_id: note.note_id,
            path: note.path.clone(),
            reply,
        }).is_err() {
            self.index_pending = Some(note);
            self.fail_save("正文已保存，但宿主数据面已断开，索引尚未更新".to_string(), cx);
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            let result = match response.await {
                Ok(result) => result,
                Err(_) => Err("宿主数据面没有返回索引更新结果".to_string()),
            };
            let _ = this.update_in(cx, |view, window, cx| {
                if view.document.as_ref() != Some(&document)
                    || view.note.as_ref().map(|current| current.note_id) != Some(note.note_id)
                {
                    return;
                }
                match result {
                    Ok(()) => {
                        view.index_pending = None;
                        view.saving = false;
                        view.failure = None;
                        view.status = StatusMessage::text("plugin.status.saved");
                        view.update_title(window, cx);
                        view.schedule_library_refresh(cx);
                        let after = view.close_intent.take_after_save();
                        view.complete_after_save(after, window, cx);
                    }
                    Err(error) => {
                        view.index_pending = Some(note);
                        view.fail_save(format!("正文已保存，但宿主索引更新失败：{error}"), cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn fail_save(&mut self, error: String, cx: &mut Context<Self>) {
        self.saving = false;
        self.waiting_toc = false;
        self.close_intent.cancel();
        self.failure = Some(StatusMessage::raw(error));
        self.status = StatusMessage::text("plugin.status.saveFailed");
        cx.notify();
    }

    fn complete_after_save(
        &mut self,
        after: AfterSave,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match after {
            AfterSave::None => {}
            AfterSave::Close => {
                if self.close_current(false, cx).is_ok() {
                    window.remove_window();
                }
            }
            AfterSave::Open(note) => {
                if self.close_current(false, cx).is_ok() {
                    self.open_note_now(note, window, cx);
                }
            }
            AfterSave::Delete(note) => {
                if self.close_current(false, cx).is_ok() {
                    self.request_delete_now(note, window, cx);
                }
            }
        }
    }

    fn close_current(
        &mut self,
        discard: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), ControlError> {
        let result = self.document.clone().map(|document| {
            document.update(cx, |host, cx| host.close_document(discard, cx))
        }).transpose().map(|_| ());
        if let Err(error) = &result {
            self.failure = Some(StatusMessage::raw(error.to_string()));
            self.status = StatusMessage::text("plugin.status.closeFailed");
            cx.notify();
            return result;
        }
        self.autosave = None;
        self.subscription = None;
        if let Some(note) = self.note.clone() {
            self.release_open(note.note_id);
        }
        self.document = None;
        self.note = None;
        self.index_pending = None;
        self.resolving_open = false;
        self.open_request_generation = self.open_request_generation.wrapping_add(1);
        self.status = StatusMessage::text("plugin.status.chooseNote");
        Ok(())
    }

    fn update_title(&self, window: &mut Window, cx: &App) {
        let title = self.note.as_ref().map(|note| file_name(&note.path))
            .unwrap_or_else(|| plugin_text(cx, "plugin.appTitle"));
        window.set_window_title(&format!("{title} — Nomo"));
    }

    fn request_close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if window.has_active_prompt() || self.close_prompt {
            return false;
        }
        if self.opening {
            self.close_intent.queue(AfterSave::Close);
            return false;
        }
        if self.resolving_open {
            self.close_intent.queue(AfterSave::Close);
            return false;
        }
        if self.saving || self.waiting_toc {
            self.close_intent.queue(AfterSave::Close);
            return false;
        }
        if self.index_pending.is_some() {
            self.save(AfterSave::Close, window, cx);
            return false;
        }
        let Some(document) = self.document.clone() else { return true; };
        if !document.read(cx).is_dirty(cx) {
            return self.close_current(false, cx).is_ok();
        }
        self.close_prompt = true;
        let response = crate::ui::dialogs::confirm_document_close(
            document.read(cx).title_message(),
            document.read(cx).has_failed_input(cx),
            window,
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let choice = response.await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.close_prompt = false;
                match choice {
                    crate::ui::dialogs::SaveChoice::Save => {
                        view.save(AfterSave::Close, window, cx);
                    }
                    crate::ui::dialogs::SaveChoice::Discard => {
                        if view.close_current(true, cx).is_ok() {
                            window.remove_window();
                        }
                    }
                    crate::ui::dialogs::SaveChoice::Cancel => {}
                }
                cx.notify();
            });
        })
        .detach();
        false
    }
}

fn retarget_note(note: &mut OpenNote, note_id: i64, path: &Path) {
    if note.note_id == note_id {
        note.path = path.to_path_buf();
    }
}

fn retarget_open_note(slot: &mut Option<OpenNote>, note_id: i64, path: &Path) {
    if let Some(note) = slot {
        retarget_note(note, note_id, path);
    }
}

fn retarget_queued_notes(
    current: &mut Option<OpenNote>,
    index_pending: &mut Option<OpenNote>,
    opening_note: &mut Option<OpenNote>,
    pending_open: &mut Option<OpenNote>,
    after_save: &mut AfterSave,
    note_id: i64,
    path: &Path,
) {
    retarget_open_note(current, note_id, path);
    retarget_open_note(index_pending, note_id, path);
    retarget_open_note(opening_note, note_id, path);
    retarget_open_note(pending_open, note_id, path);
    match after_save {
        AfterSave::Open(note) | AfterSave::Delete(note) => retarget_note(note, note_id, path),
        AfterSave::None | AfterSave::Close => {}
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "未命名笔记".to_string())
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

impl Render for PluginWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<crate::theme::Theme>().clone();
        let title = self.note.as_ref().map(|note| file_name(&note.path))
            .unwrap_or_else(|| plugin_text(cx, "plugin.appTitle"));
        let dirty = self.document.as_ref().is_some_and(|document| document.read(cx).is_dirty(cx));
        let save_label = if self.saving {
            plugin_text(cx, "plugin.save.saving")
        } else if dirty {
            plugin_text(cx, "plugin.save.dirty")
        } else {
            plugin_text(cx, "plugin.save.saved")
        };
        let save_kind = if self.saving {
            crate::ui::components::ButtonKind::Disabled
        } else {
            crate::ui::components::ButtonKind::Secondary
        };
        let delete_button = self.note.clone().map(|note| {
            crate::ui::components::button(
                "plugin-delete-note",
                plugin_text(cx, "plugin.delete.label"),
                Some(crate::ui::components::IconName::Trash2),
                if self.deleting { crate::ui::components::ButtonKind::Disabled } else { crate::ui::components::ButtonKind::Secondary },
                &theme,
            ).on_click(cx.listener(move |view, _, window, cx| {
                view.request_delete(note.clone(), window, cx);
            }))
        });

        let mut all_row = div().id("plugin-library-all").h(px(32.0)).mx(px(8.0)).px(px(10.0))
            .flex().items_center().gap(px(8.0)).rounded(px(6.0)).cursor_pointer()
            .text_size(px(13.0)).text_color(theme.fg)
            .child(crate::ui::components::icon(crate::ui::components::IconName::BookOpenText, 15.0, theme.muted))
            .child(plugin_text(cx, "plugin.library.allNotes"))
            .on_click(cx.listener(|view, _, _, cx| {
                view.selected_folder = None;
                view.schedule_library_refresh(cx);
            }));
        if self.selected_folder.is_none() {
            all_row = all_row.bg(theme.sidebar_active);
        }
        let mut folder_rows = vec![all_row.into_any_element()];
        for folder in &self.folders {
            let folder_id = folder.id;
            let selected = self.selected_folder == Some(folder_id);
            let icon = if folder.locked {
                crate::ui::components::IconName::Lock
            } else if selected {
                crate::ui::components::IconName::FolderOpen
            } else {
                crate::ui::components::IconName::Folder
            };
            let mut row = div().id(format!("plugin-library-folder-{folder_id}"))
                .h(px(32.0)).mx(px(8.0)).px(px(10.0)).flex().items_center().gap(px(8.0))
                .rounded(px(6.0)).text_size(px(13.0)).text_color(theme.fg)
                .child(crate::ui::components::icon(icon, 15.0, theme.muted))
                .child(div().flex_1().min_w_0().truncate().child(folder.name.clone()))
                .on_click(cx.listener(move |view, _, _, cx| {
                    if view.folders.iter().find(|entry| entry.id == folder_id).is_some_and(|entry| entry.locked) {
                        view.status = StatusMessage::text("plugin.status.lockedFolderUnavailable");
                        cx.notify();
                        return;
                    }
                    view.selected_folder = Some(folder_id);
                    view.schedule_library_refresh(cx);
                }));
            if selected {
                row = row.bg(theme.sidebar_active);
            } else {
                row = row.hover(|style| style.bg(theme.hover)).cursor_pointer();
            }
            folder_rows.push(row.into_any_element());
        }

        let notes = self.notes.clone();
        let note_count = notes.len();
        let note_list = uniform_list("plugin-library-notes", note_count, cx.processor(move |view, range: std::ops::Range<usize>, _, cx| {
            range.map(|index| {
                let note = &notes[index];
                let note_id = note.note_id;
                let active = view.note.as_ref().map(|current| current.note_id) == Some(note_id)
                    || view.opening_note.as_ref().map(|current| current.note_id) == Some(note_id);
                let label = note.title.clone().unwrap_or_else(|| note.file_name.clone());
                let excerpt = note.excerpt.clone().unwrap_or_else(|| note.file_name.clone());
                let mut row = div().id(format!("plugin-library-note-{note_id}"))
                    .h(px(60.0)).mx(px(8.0)).px(px(10.0)).flex().flex_col().justify_center()
                    .gap(px(3.0)).rounded(px(6.0)).text_color(theme.fg)
                    .child(div().flex().items_center().gap(px(6.0)).min_w_0()
                        .child(crate::ui::components::icon(
                            if note.locked { crate::ui::components::IconName::Lock } else { crate::ui::components::IconName::FileText },
                            14.0, if note.locked { theme.muted } else { theme.accent },
                        ))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(13.0)).child(label)))
                    .child(div().pl(px(20.0)).min_w_0().truncate().text_size(px(11.0)).text_color(theme.muted).child(excerpt))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.request_open_from_library(note_id, window, cx);
                    }));
                if active {
                    row = row.bg(theme.sidebar_active);
                } else {
                    row = row.hover(|style| style.bg(theme.hover)).cursor_pointer();
                }
                row.into_any_element()
            }).collect::<Vec<_>>()
        }));
        let empty_notes = (note_count == 0).then(|| {
            div().h(px(72.0)).px(px(14.0)).flex().items_center().text_size(px(12.0))
                .text_color(theme.muted).child(if self.library_loading {
                    plugin_text(cx, "plugin.library.loading")
                } else {
                    plugin_text(cx, "plugin.library.empty")
                })
        });
        let library_status = self.library_error.as_ref().map(|message| {
            div().flex_none().px_3().py_2().text_size(px(11.0)).text_color(theme.danger)
                .whitespace_normal().child(message.render(cx))
        });
        let sidebar = div().w(px(248.0)).flex_none().flex().flex_col().min_h_0()
            .bg(theme.rail_bg).border_r_1().border_color(theme.border)
            .child(div().h(px(40.0)).flex_none().px_4().flex().items_center().justify_between()
                .text_size(px(13.0)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme.fg)
                .child(plugin_text(cx, "plugin.library.notes"))
                .child(if self.library_loading { plugin_text(cx, "plugin.library.refreshing") } else { String::new() }))
            .child(div().h(px(34.0)).mx_3().mb_2().px_2().flex_none().flex().items_center().gap_2()
                .rounded(px(6.0)).bg(theme.surface).border_1().border_color(theme.border)
                .child(crate::ui::components::icon(crate::ui::components::IconName::Search, 14.0, theme.muted))
                .child(self.search.clone()))
            .child(div().id("plugin-library-folders").flex_none().max_h(px(196.0))
                .overflow_y_scroll().py_1().children(folder_rows))
            .child(div().h(px(1.0)).flex_none().bg(theme.border))
            .child(div().h(px(34.0)).flex_none().px_4().flex().items_center().text_size(px(11.0))
                .font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme.muted)
                .child(plugin_text(cx, "plugin.library.noteList")))
            .child(div().flex_1().min_h_0().overflow_hidden().child(note_list).children(empty_notes))
            .children(library_status);

        let body = div().flex_1().min_h_0().min_w_0().flex().flex_col().children(self.document.clone());
        let empty = self.document.is_none().then(|| {
            div().flex_1().size_full().flex().items_center().justify_center().text_color(theme.muted)
                .child(self.status.render(cx))
        });
        let error = self.failure.as_ref().map(|message| {
            div().w_full().flex_none().px_4().py_2().border_t_1().border_color(theme.border)
                .text_size(px(12.0)).text_color(theme.danger).whitespace_normal()
                .child(message.render(cx))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.app_bg)
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(|view, _: &crate::ui::workspace::Save, window, cx| {
                view.save(AfterSave::None, window, cx);
            }))
            .child(
                div().h(px(44.0)).flex_none().flex().items_center().justify_between()
                    .px_4().border_b_1().border_color(theme.border).bg(theme.surface)
                    .child(div().min_w_0().truncate().text_size(px(14.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme.fg).child(title))
                    .child(div().flex().items_center().gap_2().children(delete_button)
                        .child(crate::ui::components::button(
                            "plugin-save-note", save_label, Some(crate::ui::components::IconName::Save),
                            save_kind, &theme,
                        ).on_click(cx.listener(|view, _, window, cx| {
                            view.save(AfterSave::None, window, cx);
                        }))))
            )
            .child(div().flex_1().min_h_0().flex().child(sidebar).child(body).children(empty))
            .children(error)
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
        #[cfg(windows)]
        {
            PathBuf::from(format!(
                r"\\.\pipe\nomo-runtime-test-{tag}-{}",
                std::process::id()
            ))
        }
        #[cfg(not(windows))]
        {
            let dir = std::env::temp_dir().join(format!("nomo-runtime-{tag}-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&dir);
            dir.join(format!("plugin-{tag}.sock"))
        }
    }

    fn cleanup_socket(_path: &Path) {
        #[cfg(not(windows))]
        {
            let _ = std::fs::remove_file(_path);
            let _ = std::fs::remove_dir_all(_path.parent().unwrap());
        }
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

        cleanup_socket(&socket);
    }

    /// 握手那道闸：预算内没人拨进来 ⇒ `Err`（不是无限等，更不是建一扇连不上的窗）。
    #[test]
    fn handshake_before_window_times_out_without_a_host() {
        let socket = socket("nohost");
        let mut server = Server::bind(&socket, identity(), Duration::from_millis(50)).unwrap();
        let got = handshake_before_window(&mut server, Duration::from_millis(120));
        assert!(got.is_err(), "没宿主拨进来时先握手后建窗的闸要判死，不能放行建窗");
        cleanup_socket(&socket);
    }

    /// 直接造一台引擎（不绕 socket、不碰 GPUI）：临时 `notes.db` + 临时正文根；`folders.snapshot` 之前根是空的。
    fn engine(db_path: &Path) -> Engine {
        Engine {
            db: Db::open(db_path).unwrap(),
            db_path: db_path.to_path_buf(),
            root_path: None,
            note_roots: HashMap::new(),
            open_notes: HashSet::new(),
            folders: Vec::new(),
            pending_change: None,
            sessions: None,
        }
    }

    fn appearance_payload(root: &Path) -> Value {
        json!({
            "root": root.to_string_lossy(),
            "folders": [{ "id": 7, "name": "收件箱", "dir_name": "inbox" }],
            "ui_skin": {
                "panel_bg": "#f4f1ea",
                "panel_opacity": 0.8,
                "panel_font_scale": 1.0,
                "theme_accent": "#c25436",
                "text_color": "#24211d",
                "hint_color": "#746e65"
            },
            "locale": "zh_Hant"
        })
    }

    #[test]
    fn host_snapshot_emits_appearance_before_the_folder_refresh() {
        let data = tempfile::Builder::new().prefix("nomo-rt-skin-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-skin-root").tempdir().unwrap();
        let mut engine = engine(&data.path().join("notes.db"));
        let mut seen = Vec::new();

        let done = engine.handle("folders.snapshot", appearance_payload(root.path()), &mut |command| seen.push(command));

        assert_eq!(done, Dispatch::Done(json!({ "ok": true })));
        assert_eq!(seen.len(), 2, "外观与目录刷新是两格，不能互相顶掉：{seen:?}");
        let UiCommand::HostAppearanceChanged { skin, locale } = &seen[0] else {
            panic!("第一格该是宿主外观：{seen:?}");
        };
        assert_eq!(*locale, Some("zh-TW"));
        let skin = skin.as_ref().expect("完整 ui_skin 该被接住");
        assert_eq!(skin.panel_bg, gpui::rgb(0xf4f1ea).into());
        assert_eq!(skin.accent, gpui::rgb(0xc25436).into());
        assert_eq!(skin.text, gpui::rgb(0x24211d).into());
        assert_eq!(skin.hint, gpui::rgb(0x746e65).into());
        assert_eq!(seen[1], UiCommand::FoldersChanged);

        let theme = skin.theme();
        assert_eq!(theme.id, "pingloss-host");
        assert!(!theme.dark, "浅色宿主背景该落到亮色基础主题");
        assert_eq!(theme.document_bg, skin.panel_bg);
        assert_eq!(theme.fg, skin.text);
        assert_eq!(theme.muted, skin.hint);
        assert_eq!(theme.accent, skin.accent);
        assert_eq!(theme.mermaid.mode, 0);
        assert_eq!(theme.mermaid.colors[0], 0xf4f1ea, "Mermaid 背景必须取叠加后的宿主色");
        assert_eq!(theme.mermaid.colors[7], 0xc25436, "Mermaid 强调色必须取宿主色");
        assert_eq!(theme.code_function, crate::theme::Theme::light().code_function, "语法高亮仍沿用基础主题");
    }

    #[test]
    fn dark_host_skin_keeps_mermaid_and_contrast_in_the_same_palette() {
        let skin = HostSkin::from_value(&json!({
            "panel_bg": "171b21",
            "theme_accent": "7bd6c2",
            "text_color": "f1eee8",
            "hint_color": "a39d94"
        })).unwrap();
        let theme = skin.theme();

        assert!(theme.dark);
        assert_eq!(theme.document_bg, gpui::rgb(0x171b21).into());
        assert_eq!(theme.mermaid.mode, 1);
        assert_eq!(theme.mermaid.colors[0], 0x171b21);
        assert_eq!(theme.mermaid.colors[2], 0xf1eee8);
        assert_eq!(theme.on_accent, gpui::black().into(), "明亮强调色上用黑字");
    }

    #[test]
    fn incomplete_or_legacy_appearance_does_not_break_folder_snapshots() {
        let data = tempfile::Builder::new().prefix("nomo-rt-skin-fallback-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-skin-fallback-root").tempdir().unwrap();
        let mut engine = engine(&data.path().join("notes.db"));
        let mut seen = Vec::new();
        let done = engine.handle(
            "folders.snapshot",
            json!({
                "root": root.path().to_string_lossy(),
                "ui_skin": { "panel_bg": "#ffffff", "theme_accent": "bad" },
                "locale": "en_US"
            }),
            &mut |command| seen.push(command),
        );
        assert_eq!(done, Dispatch::Done(json!({ "ok": true })));
        assert_eq!(seen, vec![
            UiCommand::HostAppearanceChanged { skin: None, locale: Some("en-US") },
            UiCommand::FoldersChanged,
        ], "坏皮肤不能拖住同一条快照里的好语言");

        seen.clear();
        let mut skin_only = appearance_payload(root.path());
        skin_only.as_object_mut().unwrap().remove("locale");
        engine.handle("folders.snapshot", skin_only, &mut |command| seen.push(command));
        assert_eq!(seen.len(), 2, "只有皮肤时也要先应用外观再刷目录");
        let UiCommand::HostAppearanceChanged { skin, locale } = &seen[0] else {
            panic!("只有皮肤时第一格仍是宿主外观：{seen:?}");
        };
        assert!(skin.is_some(), "完整皮肤不能因为没有 locale 被丢掉");
        assert_eq!(*locale, None);
        assert_eq!(seen[1], UiCommand::FoldersChanged);

        seen.clear();
        engine.handle(
            "folders.snapshot",
            json!({ "root": root.path().to_string_lossy(), "ui_skin": null, "locale": "unknown" }),
            &mut |command| seen.push(command),
        );
        assert_eq!(seen, vec![UiCommand::FoldersChanged], "旧宿主与坏外观都该只刷新目录");
    }

    #[test]
    fn host_colors_require_six_hex_digits() {
        for value in [json!("#fff"), json!("#ffffffff"), json!("#gg0000"), json!(123456), Value::Null] {
            assert!(host_color(&value).is_none(), "不该接受 {value}");
        }
        assert_eq!(host_color(&json!("F4F1EA")), Some(gpui::rgb(0xf4f1ea).into()));
        assert_eq!(host_color(&json!("#c25436")), Some(gpui::rgb(0xc25436).into()));
    }

    #[test]
    fn host_skin_rejects_each_missing_color() {
        let root = tempfile::Builder::new().prefix("nomo-rt-skin-fields-root").tempdir().unwrap();
        let full = appearance_payload(root.path());
        for field in ["panel_bg", "theme_accent", "text_color", "hint_color"] {
            let mut payload = full.clone();
            payload["ui_skin"].as_object_mut().unwrap().remove(field);
            assert!(HostSkin::from_value(payload.get("ui_skin").unwrap()).is_none(), "缺 {field} 时不能拼一只半截皮肤");
        }
    }

    #[test]
    fn host_locales_normalize_to_the_four_supported_ids() {
        for (input, expected) in [
            ("zh", "zh-CN"), ("zh_CN", "zh-CN"), ("zh-Hans", "zh-CN"),
            ("zh-Hant", "zh-TW"), ("zh_TW", "zh-TW"), ("zh-HK", "zh-TW"), ("zh-MO", "zh-TW"),
            ("en", "en-US"), ("en_US", "en-US"), ("ja", "ja-JP"), ("ja_JP", "ja-JP"),
        ] {
            assert_eq!(host_locale(&json!(input)), Some(expected), "{input}");
        }
        for input in ["", "fr-FR", "zh-CN-extra", "de"] {
            assert_eq!(host_locale(&json!(input)), None, "{input}");
        }
        assert_eq!(host_locale(&json!(42)), None);
    }

    #[test]
    fn every_plugin_translation_has_all_languages_and_matching_placeholders() {
        let source = include_str!("runtime.rs");
        let source = source.split_once("#[cfg(test)]").expect("测试段标记").0;
        let keys: std::collections::BTreeSet<_> = source.match_indices("\"plugin.").filter_map(|(at, _)| {
            let rest = &source[at + 1..];
            Some(rest[..rest.find('"')?].to_string())
        }).collect();
        assert!(keys.len() >= 40, "插件文案键少得反常：{keys:?}");
        let values: Value = serde_json::from_str(include_str!("../../resources/app-locales.json")).unwrap();
        let placeholders = |text: &str| {
            let mut names: Vec<_> = text.split('{').skip(1).filter_map(|part| part.split_once('}').map(|(name, _)| name.to_string())).collect();
            names.sort();
            names
        };
        for locale in ["zh-CN", "zh-TW", "en-US", "ja-JP"] {
            let table = &values[locale];
            for key in &keys {
                let text = table[key].as_str().unwrap_or_else(|| panic!("{locale} 缺 {key}"));
                assert!(!text.trim().is_empty(), "{locale} 的 {key} 是空串");
                assert_eq!(placeholders(text), placeholders(values["zh-CN"][key].as_str().unwrap()), "{locale} 的 {key} 占位符漂了");
            }
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
        let UiCommand::OpenNote { note } = sink.into_iter().next().unwrap() else {
            panic!("note.new 该递 OpenNote");
        };
        assert_eq!(note.folder_id, 7, "新建笔记归到快照指定的夹");
        assert!(note.path.to_string_lossy().contains("inbox"), "正文落在快照那个夹：{:?}", note.path);

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

    fn create_note(engine: &mut Engine, body_root: &str) -> OpenNote {
        let mut sink = Vec::new();
        let Dispatch::Done(_) = engine.handle(
            "folders.snapshot",
            json!({ "root": body_root, "folders": [{ "id": 7, "dir_name": "inbox" }] }),
            &mut |command| sink.push(command),
        ) else {
            panic!("folders.snapshot 该办成");
        };
        let Dispatch::Done(reply) =
            engine.handle("note.new", json!({ "folder_id": 7 }), &mut |command| sink.push(command))
        else {
            panic!("note.new 该办成");
        };
        let UiCommand::OpenNote { note } = sink.pop().unwrap() else {
            panic!("note.new 该递 OpenNote");
        };
        assert!(reply["id"].as_i64() == Some(note.note_id));
        note
    }

    fn replace_body(port: &NativeWindowPort, text: &str) {
        let status = port.status().unwrap();
        port.submit(crate::nomo_mark::protocol::DocumentTransaction {
            base_revision: status.revision,
            operation_id: format!("plugin-save-test-{}", status.revision + 1),
            history_group_id: "plugin-save-test".into(),
            edits: vec![crate::nomo_mark::protocol::SourceEdit {
                from_byte: 0,
                to_byte: status.byte_length,
                inserted_text: text.to_string(),
            }],
            selection_before: crate::nomo_mark::protocol::DocumentSelection::caret(0),
            selection_after: crate::nomo_mark::protocol::DocumentSelection::caret(text.len() as u64),
            template_update: None,
        })
        .unwrap();
    }

    fn library_command(
        engine: &mut Engine,
        folder_id: Option<i64>,
        query: &str,
    ) -> Result<LibrarySnapshot, String> {
        let (reply, response) = oneshot::channel();
        engine.handle_command(EngineCommand::Library { folder_id, query: query.to_string(), reply });
        response.blocking_recv().unwrap()
    }

    fn open_local_command(engine: &mut Engine, note_id: i64) -> Result<OpenNote, String> {
        let (reply, response) = oneshot::channel();
        engine.handle_command(EngineCommand::OpenLocal { note_id, reply });
        response.blocking_recv().unwrap()
    }

    fn update_note_for_test(engine: &Engine, sql: &str, note_id: i64) {
        rusqlite::Connection::open(&engine.db_path)
            .unwrap()
            .execute(sql, [note_id])
            .unwrap();
    }

    #[test]
    fn library_snapshot_keeps_folder_order_and_hides_locked_folders() {
        let data = tempfile::Builder::new().prefix("nomo-rt-library-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-library-root").tempdir().unwrap();
        let mut engine = engine(&data.path().join("notes.db"));
        let mut sink = Vec::new();
        let Dispatch::Done(_) = engine.handle(
            "folders.snapshot",
            json!({
                "root": root.path().to_string_lossy(),
                "folders": [
                    { "id": 9, "name": "空夹", "dir_name": "empty", "is_default": false, "locked": false },
                    { "id": 7, "name": "收件箱", "dir_name": "inbox", "is_default": true, "locked": false },
                    { "id": 8, "name": "锁定夹", "dir_name": "locked", "is_default": false, "locked": true }
                ]
            }),
            &mut |command| sink.push(command),
        ) else { panic!("folders.snapshot 该办成") };
        let Dispatch::Done(public) = engine.handle("note.new", json!({ "folder_id": 7 }), &mut |_| {}) else {
            panic!("公开笔记该建成")
        };
        let Dispatch::Done(locked) = engine.handle("note.new", json!({ "folder_id": 8 }), &mut |_| {}) else {
            panic!("锁定夹笔记该建成")
        };
        let public_id = public["id"].as_i64().unwrap();
        let locked_id = locked["id"].as_i64().unwrap();
        let public_path = Root::open(root.path()).unwrap()
            .resolve(&format!("folders/inbox/notes/{public_id}.md")).unwrap();
        std::fs::write(public_path, "导语先行。\n\n# 列表标题\n\n列表摘要。\n").unwrap();

        let snapshot = library_command(&mut engine, None, "").unwrap();
        assert_eq!(snapshot.folders.iter().map(|folder| folder.id).collect::<Vec<_>>(), vec![9, 7, 8]);
        assert_eq!(snapshot.folders[1].name, "收件箱");
        assert!(snapshot.folders[1].is_default);
        assert!(snapshot.folders[2].locked);
        assert_eq!(snapshot.notes.len(), 1, "锁定夹里的行不能进插件列表：{:?}", snapshot.notes);
        assert_eq!(snapshot.notes[0].note_id, public_id);
        assert_eq!(snapshot.notes[0].title.as_deref(), Some("列表标题"));
        assert_eq!(snapshot.notes[0].excerpt.as_deref(), Some("导语先行。"));
        assert!(open_local_command(&mut engine, locked_id).unwrap_err().contains("锁定文件夹"));

        let filtered = library_command(&mut engine, Some(7), "列表标题").unwrap();
        assert_eq!(filtered.notes.len(), 1, "H1 不在首行时也必须按标题命中");
        let by_file = library_command(&mut engine, Some(7), &format!("{public_id}.md")).unwrap();
        assert_eq!(by_file.notes.len(), 1);
        let empty = library_command(&mut engine, Some(9), "").unwrap();
        assert!(empty.notes.is_empty());
    }

    #[test]
    fn library_search_never_reads_private_note_body() {
        let data = tempfile::Builder::new().prefix("nomo-rt-library-private-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-library-private-root").tempdir().unwrap();
        let mut engine = engine(&data.path().join("notes.db"));
        let note = create_note(&mut engine, &root.path().to_string_lossy());
        std::fs::write(&note.path, "# 私密标题\n\n私密摘要。\n").unwrap();
        update_note_for_test(&engine, "UPDATE notes SET enc=1 WHERE id=?1", note.note_id);

        let snapshot = library_command(&mut engine, None, "").unwrap();
        assert_eq!(snapshot.notes.len(), 1);
        assert!(snapshot.notes[0].locked);
        assert_eq!(snapshot.notes[0].title, None);
        assert_eq!(snapshot.notes[0].excerpt, None);
        assert!(library_command(&mut engine, None, "私密标题").unwrap().notes.is_empty());
        assert!(library_command(&mut engine, None, "私密摘要").unwrap().notes.is_empty());
        let by_file = library_command(&mut engine, None, &format!("{}.md", note.note_id)).unwrap();
        assert_eq!(by_file.notes.len(), 1);
        assert!(by_file.notes[0].locked);
    }

    #[test]
    fn local_open_rechecks_identity_privacy_and_size() {
        let data = tempfile::Builder::new().prefix("nomo-rt-library-open-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-library-open-root").tempdir().unwrap();
        let mut engine = engine(&data.path().join("notes.db"));
        let note = create_note(&mut engine, &root.path().to_string_lossy());
        engine.note_roots.clear();

        let opened = open_local_command(&mut engine, note.note_id).unwrap();
        assert_eq!(opened, note);
        assert_eq!(engine.note_roots.get(&note.note_id), Some(&Root::open(root.path()).unwrap().path().to_path_buf()));
        assert!(open_local_command(&mut engine, note.note_id + 100).unwrap_err().contains("不存在"));

        std::fs::remove_file(&note.path).unwrap();
        assert!(open_local_command(&mut engine, note.note_id).unwrap_err().contains("正文"));
        std::fs::write(&note.path, "").unwrap();

        update_note_for_test(&engine, "UPDATE notes SET kind='todo' WHERE id=?1", note.note_id);
        assert!(open_local_command(&mut engine, note.note_id).unwrap_err().contains("不是笔记"));
        update_note_for_test(&engine, "UPDATE notes SET kind='note' WHERE id=?1", note.note_id);

        update_note_for_test(&engine, "UPDATE notes SET enc=1 WHERE id=?1", note.note_id);
        assert!(open_local_command(&mut engine, note.note_id).unwrap_err().contains("私有"));

        update_note_for_test(&engine, "UPDATE notes SET enc=0 WHERE id=?1", note.note_id);
        std::fs::write(&note.path, "x".repeat(crate::plugin::storage::MAX_DOC_BYTES + 1)).unwrap();
        assert!(open_local_command(&mut engine, note.note_id).unwrap_err().contains("超过可编辑上限"));
    }

    #[test]
    fn delete_op_validates_identity_root_and_missing_rows() {
        let data = tempfile::Builder::new().prefix("nomo-rt-delete-validate-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-delete-validate-root").tempdir().unwrap();
        let mut engine = engine(&data.path().join("notes.db"));

        assert_eq!(
            engine.handle("note.delete", json!({ "note_id": 0 }), &mut |_| {}),
            Dispatch::Failed { code: "bad_request".into(), message: "note.delete 需要正整数 `note_id`".into() }
        );
        assert_eq!(
            engine.handle("note.delete", json!({ "note_id": 1, "root": 42 }), &mut |_| {}),
            Dispatch::Failed { code: "bad_request".into(), message: "note.delete 的 `root` 必须是非空路径".into() }
        );
        assert_eq!(
            engine.handle("note.delete", json!({ "note_id": 1 }), &mut |_| {}),
            Dispatch::Failed { code: "no_root".into(), message: "还没收到 root（`folders.snapshot` 里那一格），这一发办不了".into() }
        );

        let mut sink = Vec::new();
        let done = engine.handle(
            "note.delete",
            json!({ "note_id": 999, "root": root.path().to_string_lossy() }),
            &mut |command| sink.push(command),
        );
        assert_eq!(done, Dispatch::Done(json!({
            "note_id": 999,
            "deleted": false,
            "warnings": [],
        })));
        assert!(sink.is_empty(), "幂等空删不发删除通知：{sink:?}");
    }

    #[test]
    fn delete_op_refuses_an_open_note_until_the_session_closes() {
        let data = tempfile::Builder::new().prefix("nomo-rt-delete-open-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-delete-open-root").tempdir().unwrap();
        let mut engine = engine(&data.path().join("notes.db"));
        let note = create_note(&mut engine, &root.path().to_string_lossy());
        std::fs::write(&note.path, "# 打开中\n").unwrap();
        engine.pending_change = None;
        let mut sink = Vec::new();

        let busy = engine.handle("note.delete", json!({ "note_id": note.note_id }), &mut |c| sink.push(c));
        assert_eq!(busy, Dispatch::Failed {
            code: "busy".into(),
            message: "笔记仍在编辑器里打开，请先关闭再删除".into(),
        });
        assert!(note.path.exists());
        assert!(engine.db.note(note.note_id).unwrap().is_some());
        assert!(sink.is_empty());

        engine.handle_command(EngineCommand::NoteClosed { note_id: note.note_id });
        assert!(!engine.open_notes.contains(&note.note_id));
        let done = engine.handle("note.delete", json!({ "note_id": note.note_id }), &mut |c| sink.push(c));
        assert_eq!(done, Dispatch::Done(json!({
            "note_id": note.note_id,
            "deleted": true,
            "warnings": [],
        })));
        assert!(!note.path.exists());
        assert!(engine.db.note(note.note_id).unwrap().is_none());
        assert_eq!(sink, vec![UiCommand::NoteDeleted { note_id: note.note_id }]);
        assert_eq!(engine.pending_change, Some(json!({
            "folder": 7,
            "note_id": note.note_id,
            "kind": "deleted",
        })));

        sink.clear();
        let again = engine.handle("note.delete", json!({ "note_id": note.note_id }), &mut |c| sink.push(c));
        assert_eq!(again, Dispatch::Done(json!({
            "note_id": note.note_id,
            "deleted": false,
            "warnings": [],
        })));
        assert!(sink.is_empty());
    }

    #[test]
    fn delete_op_rejects_private_locked_and_non_note_rows() {
        let data = tempfile::Builder::new().prefix("nomo-rt-delete-guards-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-delete-guards-root").tempdir().unwrap();
        let mut engine = engine(&data.path().join("notes.db"));
        let note = create_note(&mut engine, &root.path().to_string_lossy());
        engine.handle_command(EngineCommand::NoteClosed { note_id: note.note_id });

        update_note_for_test(&engine, "UPDATE notes SET enc=1 WHERE id=?1", note.note_id);
        assert_eq!(
            engine.handle("note.delete", json!({ "note_id": note.note_id }), &mut |_| {}),
            Dispatch::Failed { code: "private".into(), message: format!("第 {} 号是私有笔记", note.note_id) }
        );
        assert!(engine.db.note(note.note_id).unwrap().is_some());

        update_note_for_test(&engine, "UPDATE notes SET enc=0, kind='todo' WHERE id=?1", note.note_id);
        assert_eq!(
            engine.handle("note.delete", json!({ "note_id": note.note_id }), &mut |_| {}),
            Dispatch::Failed { code: "bad_request".into(), message: format!("第 {} 号不是笔记", note.note_id) }
        );
        assert!(engine.db.note(note.note_id).unwrap().is_some());

        update_note_for_test(&engine, "UPDATE notes SET kind='note' WHERE id=?1", note.note_id);
        engine.handle(
            "folders.snapshot",
            json!({
                "root": root.path().to_string_lossy(),
                "folders": [{ "id": 7, "dir_name": "inbox", "locked": true }]
            }),
            &mut |_| {},
        );
        assert_eq!(
            engine.handle("note.delete", json!({ "note_id": note.note_id }), &mut |_| {}),
            Dispatch::Failed { code: "locked".into(), message: format!("第 {} 号在锁定文件夹里", note.note_id) }
        );
        assert!(engine.db.note(note.note_id).unwrap().is_some());
        assert!(note.path.exists());
    }

    #[test]
    fn delete_local_uses_the_same_guards_as_the_host_op() {
        let data = tempfile::Builder::new().prefix("nomo-rt-delete-local-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-delete-local-root").tempdir().unwrap();
        let mut engine = engine(&data.path().join("notes.db"));
        let note = create_note(&mut engine, &root.path().to_string_lossy());
        let (reply, response) = oneshot::channel();
        engine.handle_command(EngineCommand::DeleteLocal { note_id: note.note_id, reply });
        assert!(response.blocking_recv().unwrap().unwrap_err().contains("仍在编辑器里打开"));

        engine.handle_command(EngineCommand::NoteClosed { note_id: note.note_id });
        let (reply, response) = oneshot::channel();
        engine.handle_command(EngineCommand::DeleteLocal { note_id: note.note_id, reply });
        assert_eq!(response.blocking_recv().unwrap().unwrap(), DeleteOutcome {
            deleted: true,
            warnings: Vec::new(),
        });
        assert!(!note.path.exists());
    }

    #[test]
    fn queued_delete_follows_renames_and_yields_to_window_close() {
        let note = OpenNote { note_id: 7, folder_id: 3, path: PathBuf::from("old.md") };
        let mut current = Some(note.clone());
        let mut index_pending = Some(note.clone());
        let mut opening_note = Some(note.clone());
        let mut pending_open = Some(note.clone());
        let mut after_save = AfterSave::Delete(note.clone());
        let renamed = PathBuf::from("new.md");

        retarget_queued_notes(
            &mut current,
            &mut index_pending,
            &mut opening_note,
            &mut pending_open,
            &mut after_save,
            7,
            &renamed,
        );
        let expected = OpenNote { note_id: 7, folder_id: 3, path: renamed };
        assert_eq!(after_save, AfterSave::Delete(expected));

        let mut intent = CloseIntent::default();
        intent.queue(AfterSave::Delete(note.clone()));
        intent.queue(AfterSave::Close);
        intent.queue(AfterSave::Delete(note));
        assert!(intent.pending);
        assert_eq!(intent.take_after_save(), AfterSave::Close);
    }

    #[test]
    fn formal_session_save_updates_renamed_body_db_and_saved_notice() {
        let data = tempfile::Builder::new().prefix("nomo-rt-save-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-save-root").tempdir().unwrap();
        let sessions = tempfile::Builder::new().prefix("nomo-rt-save-sessions").tempdir().unwrap();
        let db_path = data.path().join("notes.db");
        let manager = MarkdownDocumentSessionManager::new(sessions.path().to_path_buf()).unwrap();
        let mut engine = engine(&db_path);
        engine.sessions = Some(manager.clone());
        let note = create_note(&mut engine, &root.path().to_string_lossy());
        let port = NativeWindowPort::open_in(
            manager.clone(),
            format!("pingloss-notes-{}", note.note_id),
            Some(note.path.clone()),
            None,
        )
        .unwrap();
        let body = "# 正式保存标题\n\n插件正文。\n";
        replace_body(&port, body);

        let mut sink = Vec::new();
        let Dispatch::Done(reply) = engine.handle(
            "notes.rename",
            json!({ "note_id": note.note_id, "stem": "renamed" }),
            &mut |command| sink.push(command),
        ) else {
            panic!("已打开笔记改名该办成");
        };
        let new_path = PathBuf::from(reply["path"].as_str().unwrap());
        assert!(!note.path.exists(), "旧正文路径应已移走");
        assert_eq!(port.status().unwrap().path.as_deref(), Some(new_path.to_string_lossy().as_ref()));
        assert!(sink.contains(&UiCommand::NoteRenamed { note_id: note.note_id, path: new_path.clone() }));

        engine.prepare_save(note.note_id, &new_path).unwrap();
        let captured = port.capture_for_save(port.status().unwrap().revision).unwrap();
        port.save_captured(captured, None, false).unwrap();
        assert_eq!(std::fs::read_to_string(&new_path).unwrap(), body);
        engine.finish_save(note.note_id, &new_path).unwrap();

        let row = engine.db.note(note.note_id).unwrap().unwrap();
        assert_eq!(row.title.as_deref(), Some("正式保存标题"));
        assert_eq!(row.bytes, body.len() as i64);
        assert!(row.updated_at_ms > 0);
        assert_eq!(engine.pending_change, Some(json!({
            "folder": 7,
            "note_id": note.note_id,
            "kind": "saved",
        })));
        assert!(!port.status().unwrap().dirty);
        port.close(false).unwrap();
    }

    #[test]
    fn oversized_plugin_body_is_rejected_before_save_and_index_update() {
        let data = tempfile::Builder::new().prefix("nomo-rt-limit-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-limit-root").tempdir().unwrap();
        let db_path = data.path().join("notes.db");
        let mut engine = engine(&db_path);
        let note = create_note(&mut engine, &root.path().to_string_lossy());
        engine.pending_change = None;
        let oversized = "x".repeat(crate::plugin::storage::MAX_DOC_BYTES + 1);
        std::fs::write(&note.path, oversized).unwrap();

        let prepare = engine.prepare_save(note.note_id, &note.path).unwrap_err();
        assert!(prepare.contains("超过可保存上限"), "{prepare}");
        let finish = engine.finish_save(note.note_id, &note.path).unwrap_err();
        assert!(finish.contains("超过可改上限"), "{finish}");
        assert!(engine.pending_change.is_none(), "超限正文不能更新索引或推 saved");
        let row = engine.db.note(note.note_id).unwrap().unwrap();
        assert_eq!(row.bytes, 0);
        assert_eq!(row.title, None);
    }

    #[test]
    fn explicit_open_root_is_kept_for_save_without_a_snapshot() {
        let data = tempfile::Builder::new().prefix("nomo-rt-root-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-root-body").tempdir().unwrap();
        let body_root = Root::open(root.path()).unwrap();
        let mut db = Db::open(&data.path().join("notes.db")).unwrap();
        let row = db.create_note(&body_root, "inbox", 7).unwrap();
        let mut engine = engine(&data.path().join("notes.db"));
        let mut sink = Vec::new();

        let Dispatch::Done(reply) = engine.handle(
            "notes.open",
            json!({ "note_id": row.id, "root": root.path().to_string_lossy() }),
            &mut |command| sink.push(command),
        ) else {
            panic!("显式 root 的 notes.open 该办成");
        };
        assert_eq!(reply, json!({ "queued": true }));
        assert!(engine.root_path.is_none(), "这个用例故意不发 folders.snapshot");
        let UiCommand::OpenNote { note } = sink.pop().unwrap() else {
            panic!("notes.open 该递 OpenNote");
        };
        engine.prepare_save(note.note_id, &note.path).unwrap();
        std::fs::write(&note.path, "# 显式正文根\n").unwrap();
        engine.finish_save(note.note_id, &note.path).unwrap();
        let saved = engine.db.note(note.note_id).unwrap().unwrap();
        assert_eq!(saved.title.as_deref(), Some("显式正文根"));
    }

    #[test]
    fn queued_open_paths_follow_host_renames() {
        let note = OpenNote { note_id: 7, folder_id: 3, path: PathBuf::from("old.md") };
        let other = OpenNote { note_id: 8, folder_id: 4, path: PathBuf::from("other.md") };
        let mut current = Some(note.clone());
        let mut index_pending = Some(note.clone());
        let mut opening_note = Some(note.clone());
        let mut pending_open = Some(note.clone());
        let mut after_save = AfterSave::Open(note.clone());
        let renamed = PathBuf::from("new.md");

        retarget_queued_notes(
            &mut current,
            &mut index_pending,
            &mut opening_note,
            &mut pending_open,
            &mut after_save,
            7,
            &renamed,
        );

        let expected = OpenNote { note_id: 7, folder_id: 3, path: renamed.clone() };
        assert_eq!(current, Some(expected.clone()));
        assert_eq!(index_pending, Some(expected.clone()));
        assert_eq!(opening_note, Some(expected.clone()));
        assert_eq!(pending_open, Some(expected.clone()));
        assert_eq!(after_save, AfterSave::Open(expected));

        let mut untouched = Some(other);
        retarget_open_note(&mut untouched, 7, &renamed);
        assert_eq!(untouched.unwrap().path, PathBuf::from("other.md"));
    }

    #[test]
    fn close_intent_wins_over_a_queued_open() {
        let note = OpenNote { note_id: 7, folder_id: 3, path: PathBuf::from("note.md") };
        let mut intent = CloseIntent::default();
        intent.queue(AfterSave::Open(note.clone()));
        intent.queue(AfterSave::Close);
        intent.queue(AfterSave::Open(note));

        assert!(intent.pending);
        assert_eq!(intent.take_after_save(), AfterSave::Close);
        assert!(!intent.pending);
        assert_eq!(intent.take_after_save(), AfterSave::None);
    }

    #[test]
    fn failed_rename_with_failed_rollback_keeps_recovery_intent() {
        let files = tempfile::Builder::new().prefix("nomo-rt-rename-intent").tempdir().unwrap();
        let sessions = tempfile::Builder::new().prefix("nomo-rt-rename-intent-sessions").tempdir().unwrap();
        let files = files.path().canonicalize().unwrap();
        let source = files.join("old.md");
        let destination = files.join("new.md");
        std::fs::write(&source, "# 原名\n").unwrap();
        let manager = MarkdownDocumentSessionManager::new(sessions.path().to_path_buf()).unwrap();
        let port = NativeWindowPort::open_in(
            manager.clone(),
            "pingloss-notes-rename-intent".into(),
            Some(source.clone()),
            None,
        ).unwrap();

        let error = manager.rename_path(&source, &destination, || {
            std::fs::rename(&source, &destination).unwrap();
            Err(RenameAttemptError::recovery_required("数据库与回滚都失败"))
        }).unwrap_err();

        assert_eq!(error, "数据库与回滚都失败");
        assert!(destination.exists());
        assert!(!source.exists());
        let status = port.status().unwrap();
        assert_eq!(status.path.as_deref(), Some(source.to_string_lossy().as_ref()));
        assert!(
            sessions.path().join(status.document_id).join("pending-rename.json").exists(),
            "回滚失败时必须留下重命名意图"
        );
        port.close(false).unwrap();
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

        cleanup_socket(&socket);
    }

    #[test]
    fn a_formally_saved_note_pushes_saved_back_to_the_host() {
        let data = tempfile::Builder::new().prefix("nomo-rt-saved-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-saved-root").tempdir().unwrap();
        let sessions = tempfile::Builder::new().prefix("nomo-rt-saved-sessions").tempdir().unwrap();
        let socket = socket("push-saved");
        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();
        let data_dir = data.path().to_string_lossy().to_string();
        let body_root = root.path().to_string_lossy().to_string();
        let host_socket = socket.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            greet(&mut host, data_dir);
            host.send(&(Envelope::Request {
                request: 1,
                op: "folders.snapshot".to_string(),
                payload: json!({ "root": body_root, "folders": [{ "id": 7, "dir_name": "inbox" }] }),
            }).to_value()).unwrap();
            let _snapshot = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&(Envelope::Request {
                request: 2,
                op: "note.new".to_string(),
                payload: json!({ "folder_id": 7 }),
            }).to_value()).unwrap();
            let _created_reply = host.wait_msg(Duration::from_secs(5)).unwrap();
            for expected in ["created", "saved"] {
                let pushed = host.wait_msg(Duration::from_secs(5)).unwrap();
                assert_eq!(pushed["op"], json!("host.notes_changed"));
                assert_eq!(pushed["payload"]["kind"], json!(expected));
                host.send(&(Envelope::Reply {
                    request: pushed["request"].as_u64().unwrap(),
                    payload: json!({ "ok": true }),
                }).to_value()).unwrap();
            }
            drop(host);
        });

        let session = handshake_before_window(&mut server, Duration::from_secs(5)).unwrap();
        let manager = MarkdownDocumentSessionManager::new(sessions.path().to_path_buf()).unwrap();
        let pump_manager = manager.clone();
        let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<EngineCommand>();
        let (ui_tx, ui_rx) = mpsc::channel::<UiCommand>();
        let pump = thread::spawn(move || {
            serve_with_commands(
                session,
                move |command| { let _ = ui_tx.send(command); },
                command_rx,
                Some(pump_manager),
            )
        });
        let note = loop {
            match ui_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
                UiCommand::OpenNote { note } => break note,
                _ => {}
            }
        };
        let port = NativeWindowPort::open_in(
            manager.clone(),
            format!("pingloss-notes-{}", note.note_id),
            Some(note.path.clone()),
            None,
        )
        .unwrap();
        let body = "# 已通知宿主\n\n保存闭环。\n";
        replace_body(&port, body);
        let (prepare_reply, prepare_response) = oneshot::channel();
        command_tx.send(EngineCommand::PrepareSave {
            note_id: note.note_id,
            path: note.path.clone(),
            reply: prepare_reply,
        }).unwrap();
        prepare_response.blocking_recv().unwrap().unwrap();
        let captured = port.capture_for_save(port.status().unwrap().revision).unwrap();
        port.save_captured(captured, None, false).unwrap();
        let (finish_reply, finish_response) = oneshot::channel();
        command_tx.send(EngineCommand::SaveFinished {
            note_id: note.note_id,
            path: note.path.clone(),
            reply: finish_reply,
        }).unwrap();
        finish_response.blocking_recv().unwrap().unwrap();

        host.join().unwrap();
        pump.join().unwrap().unwrap();
        let row = Db::open(&data.path().join("notes.db")).unwrap().note(note.note_id).unwrap().unwrap();
        assert_eq!(row.title.as_deref(), Some("已通知宿主"));
        assert_eq!(row.bytes, body.len() as i64);
        assert_eq!(std::fs::read_to_string(&note.path).unwrap(), body);
        port.close(false).unwrap();
        cleanup_socket(&socket);
    }

    /// 通知超时后假宿主仍在连接上发下一快照：插件应继续办，而不是把 2 s 无回复当成断连。
    #[test]
    fn a_timed_out_change_notification_does_not_stop_the_pump() {
        let data = tempfile::Builder::new().prefix("nomo-rt-timeout-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-timeout-root").tempdir().unwrap();
        let socket = socket("push-timeout");
        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();
        let data_dir = data.path().to_string_lossy().to_string();
        let body_root = root.path().to_string_lossy().to_string();
        let next_root = body_root.clone();
        let host_socket = socket.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            greet(&mut host, data_dir);
            host.send(&(Envelope::Request {
                request: 1,
                op: "folders.snapshot".to_string(),
                payload: json!({ "root": body_root, "folders": [{ "id": 7, "dir_name": "inbox" }] }),
            }).to_value())
            .unwrap();
            let _snapshot = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&(Envelope::Request {
                request: 2,
                op: "note.new".to_string(),
                payload: json!({ "folder_id": 7 }),
            }).to_value())
            .unwrap();
            let _created = host.wait_msg(Duration::from_secs(5)).unwrap();
            let pushed = host.wait_msg(Duration::from_secs(5)).unwrap();
            assert_eq!(pushed["op"], json!("host.notes_changed"));
            thread::sleep(Duration::from_millis(2_100));
            host.send(&(Envelope::Request {
                request: 3,
                op: "folders.snapshot".to_string(),
                payload: json!({ "root": next_root }),
            }).to_value())
            .unwrap();
            host.wait_msg(Duration::from_secs(5)).unwrap()
        });

        let session = handshake_before_window(&mut server, Duration::from_secs(5)).unwrap();
        serve(session, |_| {}).unwrap();
        let reply = host.join().unwrap();
        assert_eq!(reply["ok"], json!(true), "超时后的下一快照仍要办成");
        cleanup_socket(&socket);
    }

    /// 宿主明确回 Failure 是「这一条通知不认」，不是字节流坏；回完后下一快照仍应照常办。
    #[test]
    fn a_rejected_change_notification_does_not_stop_the_pump() {
        let data = tempfile::Builder::new().prefix("nomo-rt-reject-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-reject-root").tempdir().unwrap();
        let socket = socket("push-reject");
        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();
        let data_dir = data.path().to_string_lossy().to_string();
        let body_root = root.path().to_string_lossy().to_string();
        let next_root = body_root.clone();
        let host_socket = socket.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            greet(&mut host, data_dir);
            host.send(&(Envelope::Request {
                request: 1,
                op: "folders.snapshot".to_string(),
                payload: json!({ "root": body_root, "folders": [{ "id": 7, "dir_name": "inbox" }] }),
            }).to_value())
            .unwrap();
            let _snapshot = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&(Envelope::Request {
                request: 2,
                op: "note.new".to_string(),
                payload: json!({ "folder_id": 7 }),
            }).to_value())
            .unwrap();
            let _created = host.wait_msg(Duration::from_secs(5)).unwrap();
            let pushed = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&(Envelope::Failure {
                request: pushed["request"].as_u64().unwrap(),
                code: "unknown_op".to_string(),
                message: "old host".to_string(),
            }).to_value())
            .unwrap();
            host.send(&(Envelope::Request {
                request: 3,
                op: "folders.snapshot".to_string(),
                payload: json!({ "root": next_root }),
            }).to_value())
            .unwrap();
            host.wait_msg(Duration::from_secs(5)).unwrap()
        });

        let session = handshake_before_window(&mut server, Duration::from_secs(5)).unwrap();
        serve(session, |_| {}).unwrap();
        let reply = host.join().unwrap();
        assert_eq!(reply["ok"], json!(true), "宿主拒绝通知后下一快照仍要办成");
        cleanup_socket(&socket);
    }

    /// 宿主读完通知就断：读取答复撞 EOF，serve 必须把这条连接交给调用方收掉。
    #[test]
    fn a_broken_change_notification_stops_the_pump() {
        let data = tempfile::Builder::new().prefix("nomo-rt-broken-db").tempdir().unwrap();
        let root = tempfile::Builder::new().prefix("nomo-rt-broken-root").tempdir().unwrap();
        let socket = socket("push-broken");
        let mut server = Server::bind(&socket, identity(), Duration::from_secs(5)).unwrap();
        let data_dir = data.path().to_string_lossy().to_string();
        let body_root = root.path().to_string_lossy().to_string();
        let host_socket = socket.clone();
        let host = thread::spawn(move || {
            let mut host = Stream::connect(&host_socket, Duration::from_secs(5)).unwrap();
            greet(&mut host, data_dir);
            host.send(&(Envelope::Request {
                request: 1,
                op: "folders.snapshot".to_string(),
                payload: json!({ "root": body_root, "folders": [{ "id": 7, "dir_name": "inbox" }] }),
            }).to_value())
            .unwrap();
            let _snapshot = host.wait_msg(Duration::from_secs(5)).unwrap();
            host.send(&(Envelope::Request {
                request: 2,
                op: "note.new".to_string(),
                payload: json!({ "folder_id": 7 }),
            }).to_value())
            .unwrap();
            let _created = host.wait_msg(Duration::from_secs(5)).unwrap();
            let pushed = host.wait_msg(Duration::from_secs(5)).unwrap();
            assert_eq!(pushed["op"], json!("host.notes_changed"));
        });

        let session = handshake_before_window(&mut server, Duration::from_secs(5)).unwrap();
        let result = serve(session, |_| {});
        host.join().unwrap();
        assert!(result.is_err(), "通知读写撞断时 serve 必须交 Err：{result:?}");
        cleanup_socket(&socket);
    }

    fn greet(host: &mut Stream, data_dir: String) {
        let _hello = host.wait_msg(Duration::from_secs(5)).unwrap();
        host.send(&HelloOk {
            protocol: PROTOCOL,
            capabilities: vec!["folders.snapshot".into(), "note.new".into()],
            data_dir,
        })
        .unwrap();
    }
}
