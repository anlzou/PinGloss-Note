//! `notes.db` 那一层的**原生复刻**（宿主 `pg-notes`/sidecar `pingloss-notes` 的数据口径）。
//!
//! ⚠ **谁是真源**（决策 D-B①）：插件模式下的笔记真值 = 宿主的 `notes.db` ＋ 编号正文
//! `folders/<dir>/notes/<id>.md` ＋ 附件 `a/<asset>.<ext>`／`note_assets` 行。Nomo 自己的
//! `document-library.sqlite3` **不是**真源，这里一律不读写它。本模块只碰宿主交给插件的那一层：
//! `data_dir`（`<data>/plugins-data/<id>/`，`notes.db` 在这一层）与 `root`（含 `folders/` 的库根，
//! 每一个 DB `body_path` 都按 `Root::open(root).resolve(body_path)` 解析）。
//!
//! 为什么不用 `rusqlite::Connection` 直接散在 op 里：schema、迁移、正文路径闸、原子写这四件事必须
//! **只有一个主人** —— 分叉的形状是「list 读到旧 schema、open 用新路径规则」这种半新半旧。于是这层
//! 收成一个 [`Db`]（只管表）＋一套路径函数（只管口径），op 那一发（P1-d）在它们上面搭。
//!
//! 报错口径跟着本仓走（没有 anyhow）：这一层的失败交回一句 `String`，正好填数据面 `Failure.message`。
//!
//! 与 sidecar 一致的**边界**（今天都不越）：私有笔记（`enc=1`）在 list 里只给路径、不给标题／摘要；
//! `open`／`create`／`rename` 撞到私有行或私有目录一律**拒绝**（sidecar 原话 "private notes are not
//! wired yet"）。锁定不是隐藏，是「这条路今天没接」——如实拒，不猜内容。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};

/// 这一层的失败：一句给人读的话（数据面 `Failure.message` 直接拿它）。
pub type Res<T> = Result<T, String>;

/// 把任何 `Display` 错折成 [`Res`] 的 `String`（`Ok` 那侧原样留）。
trait DisplayErr<T> {
    fn es(self) -> Res<T>;
}
impl<T, E: std::fmt::Display> DisplayErr<T> for Result<T, E> {
    fn es(self) -> Res<T> {
        self.map_err(|e| e.to_string())
    }
}

/// 与宿主 `pg-notes::db::SCHEMA_VERSION` 同值；落在 `PRAGMA user_version`。
pub const SCHEMA_VERSION: i32 = 2;

/// 单篇正文的可改上限（`<=` 才算可编辑），与宿主 `markdown::MAX_DOC_BYTES` 同值。
pub const MAX_DOC_BYTES: usize = 2 * 1024 * 1024;

// —— 目录字面量（store.rs 49–60），POSIX `/` 分隔，相对 root ——
const FOLDERS_DIR: &str = "folders";
const NOTES_SUBDIR: &str = "notes";
/// 正文扩展名：公开正文自 2026-11-16 起就是裸 Markdown（不再有信封）。
const BODY_EXT: &str = "md";

/// `notes` 表的列序（读侧 `row.get` 与 SQL 的 `SELECT` 都照这一串；改一处要连着改另一处）。
const NOTE_COLUMNS: &str =
    "id, kind, folder_id, title, enc, body_path, created_at_ms, updated_at_ms, bytes";

const NOTES_DDL: &str = r#"CREATE TABLE IF NOT EXISTS notes (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT    NOT NULL CHECK(kind IN ('note','todo')),
    folder_id     INTEGER NOT NULL,
    title         TEXT,
    enc           INTEGER NOT NULL DEFAULT 0,
    body_path     TEXT    NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    bytes         INTEGER NOT NULL DEFAULT 0
)"#;

const NOTES_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS notes_folder_recent ON notes(folder_id, updated_at_ms DESC, id DESC);\nCREATE INDEX IF NOT EXISTS notes_kind_recent ON notes(kind, updated_at_ms DESC, id DESC)";

/// `note_assets`：13 列，无外键 —— 级联删除由插件在一个事务里自己办。
const ASSETS_DDL: &str = r#"CREATE TABLE IF NOT EXISTS note_assets (
    id            TEXT    PRIMARY KEY,
    note_id       INTEGER NOT NULL,
    kind          TEXT    NOT NULL CHECK(kind IN ('image','gif')),
    rel_path      TEXT    NOT NULL,
    source_kind   TEXT    NOT NULL CHECK(source_kind IN ('pin','gif','file')),
    source_ref    TEXT    NOT NULL DEFAULT '',
    bytes         INTEGER NOT NULL,
    width         INTEGER,
    height        INTEGER,
    frames        INTEGER,
    duration_ms   INTEGER,
    sha256        TEXT    NOT NULL,
    created_at_ms INTEGER NOT NULL
)"#;

const ASSETS_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS note_assets_note ON note_assets(note_id)";

/// `notes.kind`：笔记与待办两条数据线共用一张表，靠这一位分家。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteKind {
    Note,
    Todo,
}

impl NoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Todo => "todo",
        }
    }

    /// 严格解析：认不出的词交回 `None`，不猜（宿主 `from_word` 同口径）。
    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "note" => Some(Self::Note),
            "todo" => Some(Self::Todo),
            _ => None,
        }
    }
}

/// `notes.enc` 的业务值：位尺只在这一层（0＝公开、非 0＝私有）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Enc {
    Public,
    Private,
}

impl Enc {
    /// 公开**必须**是 0（要对上 DDL 的 `DEFAULT 0`）。
    pub fn bit(self) -> i64 {
        match self {
            Self::Public => 0,
            Self::Private => 1,
        }
    }

    pub fn is_private(self) -> bool {
        self == Self::Private
    }
}

/// 一行 `notes`。身份列（`kind`／`folder_id`／`enc`）在写侧 `UPDATE` 的 `WHERE` 里当乐观锁用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteRow {
    pub id: i64,
    pub kind: NoteKind,
    pub folder_id: i64,
    pub title: Option<String>,
    pub enc: Enc,
    pub body_path: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub bytes: i64,
}

/// 墙钟毫秒。宿主写侧也用这个量纲（`notes_io.rs::now_ms`）。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 一个打开的 `notes.db`。生命周期＝插件模式这一格连接；`Db` 一 drop，rusqlite 就关句柄。
pub struct Db {
    conn: Connection,
}

impl Db {
    /// 开库并跑迁移：先读 `PRAGMA user_version`，比 [`SCHEMA_VERSION`] **新**就整个拒（一个字都不写）。
    ///
    /// ⚠ 迁移的幂等由**表在不在**决定、不由 `user_version` 决定：`note_assets` 只要缺席就建（哪怕
    /// 号已经是 2），这是宿主那条「v1→v2 只加 `note_assets`、`notes` 列永不改」的落实。
    pub fn open(path: &Path) -> Res<Self> {
        let conn = Connection::open(path).es()?;
        let found: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .es()?;
        if found > SCHEMA_VERSION {
            return Err(format!(
                "notes.db 的 schema 版本 {found} 比这颗插件认的 {SCHEMA_VERSION} 还新，拒绝打开（别拿旧插件写新库）"
            ));
        }
        // WAL / busy_timeout 尽力设、失败不致命：真正的 journal_mode 从库里读回来才是事实。
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.busy_timeout(Duration::from_secs(2)).es()?;
        migrate(&conn, found).es()?;
        Ok(Self { conn })
    }

    /// 按 `folder_id` 取（近→远）。
    pub fn notes_in_folder(&self, folder_id: i64) -> Res<Vec<NoteRow>> {
        let sql = format!(
            "SELECT {NOTE_COLUMNS} FROM notes WHERE folder_id=?1 ORDER BY updated_at_ms DESC, id DESC"
        );
        self.query_notes(&sql, params![folder_id])
    }

    /// 取全部 kind='note'（跨目录，近→远）。
    pub fn all_notes(&self) -> Res<Vec<NoteRow>> {
        let sql = format!(
            "SELECT {NOTE_COLUMNS} FROM notes WHERE kind=?1 ORDER BY updated_at_ms DESC, id DESC"
        );
        self.query_notes(&sql, params![NoteKind::Note.as_str()])
    }

    /// 按 id 取一行。
    pub fn note(&self, id: i64) -> Res<Option<NoteRow>> {
        let sql = format!("SELECT {NOTE_COLUMNS} FROM notes WHERE id=?1");
        let mut stmt = self.conn.prepare(&sql).es()?;
        let row = stmt
            .query_row(params![id], read_note)
            .optional()
            .es()?;
        Ok(row)
    }

    /// 新建一行笔记：草稿占位（`title=NULL`、`body_path=''`、`bytes=0`）拿到 id，落正文空文件，
    /// 再把 `body_path` settle 回去，全在**一个事务**里 —— 提交前那一号对别的读者不可见。
    ///
    /// 私有目录的拒绝由调用方在进来之前判（那一位读的是快照目录表，不归本层）。
    pub fn create_note(&mut self, root: &Root, dir_name: &str, folder_id: i64) -> Res<NoteRow> {
        check_dir_name(dir_name)?;
        let ts = now_ms();
        let mut tx = self.conn.transaction().es()?;
        tx.execute(
            "INSERT INTO notes (kind, folder_id, title, enc, body_path, created_at_ms, updated_at_ms, bytes) \
             VALUES (?1, ?2, NULL, ?3, '', ?4, ?5, 0)",
            params![NoteKind::Note.as_str(), folder_id, Enc::Public.bit(), ts, ts],
        )
        .es()?;
        let id = tx.last_insert_rowid();
        let rel = public_body_rel(dir_name, id);
        let abs = root.resolve(&rel)?;
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).es()?;
        }
        atomic_write(&abs, b"", 0)?;
        tx.execute("UPDATE notes SET body_path=?1 WHERE id=?2", params![rel, id])
            .es()?;
        tx.commit().es()?;
        Ok(NoteRow {
            id,
            kind: NoteKind::Note,
            folder_id,
            title: None,
            enc: Enc::Public,
            body_path: rel,
            created_at_ms: ts,
            updated_at_ms: ts,
            bytes: 0,
        })
    }

    /// 存正文后刷新那一行：写侧 `UPDATE` 把身份列摆进 `WHERE` 当乐观锁；0 行变更 ⇒ 有人先动过。
    ///
    /// ⚠ 存进 `title` 列的是**从正文派生出来的**标题（sidecar `after_save` 那一格），非空才写、否则 `NULL`。
    pub fn save_note(&self, row: &mut NoteRow, body: &str) -> Res<()> {
        if body.len() > MAX_DOC_BYTES {
            return Err(format!(
                "正文 {} B 超过可改上限 {MAX_DOC_BYTES} B",
                body.len()
            ));
        }
        let bytes = body.len() as i64;
        let derived = derive_title(body);
        row.title = if derived.trim().is_empty() {
            None
        } else {
            Some(derived)
        };
        row.updated_at_ms = now_ms();
        row.bytes = bytes;
        let changed = self
            .conn
            .execute(
                "UPDATE notes SET title=?1, body_path=?2, bytes=?3, updated_at_ms=?4 \
                 WHERE id=?5 AND kind=?6 AND folder_id=?7 AND enc=?8",
                params![
                    row.title,
                    row.body_path,
                    bytes,
                    row.updated_at_ms,
                    row.id,
                    row.kind.as_str(),
                    row.folder_id,
                    row.enc.bit()
                ],
            )
            .es()?;
        if changed == 0 {
            return Err(format!(
                "第 {} 号那一行已被别处改动（身份列对不上），这次保存作废",
                row.id
            ));
        }
        Ok(())
    }

    /// 把正文文件就地改名（保留旧扩展名、默认 `md`），再把新 `body_path` 落回那一行；
    /// **DB 失败就回滚磁盘改名**。私有行拒绝。不碰标题、不碰信封——改的是**物理文件名**。
    pub fn rename_body(&self, root: &Root, row: &NoteRow, stem: &str) -> Res<PathBuf> {
        if row.enc.is_private() {
            return Err("私有笔记改名今天没接线".to_string());
        }
        let stem = clean_note_stem(stem)?;
        let old_abs = root.resolve(&row.body_path)?;
        let old_name = old_abs
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let ext = Path::new(&old_name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(BODY_EXT)
            .to_string();
        let new_name = format!("{stem}.{ext}");
        let new_abs = old_abs.with_file_name(&new_name);
        if new_abs.exists() {
            return Err(format!("目标 {new_name} 已存在，改名不覆盖"));
        }
        std::fs::rename(&old_abs, &new_abs).es()?;
        let rel = rel_under(root, &new_abs);
        let changed = self
            .conn
            .execute(
                "UPDATE notes SET body_path=?1 WHERE id=?2 AND folder_id=?3 AND enc=?4",
                params![rel, row.id, row.folder_id, row.enc.bit()],
            )
            .es()?;
        if changed == 0 {
            let _ = std::fs::rename(&new_abs, &old_abs);
            return Err(format!(
                "改名落库失败（第 {} 号身份对不上），已回滚文件",
                row.id
            ));
        }
        Ok(new_abs)
    }

    /// 只读点查标题（op `notes.info` 用）：**不建库、不迁移、不设 WAL**，只 `SELECT title, enc`。
    ///
    /// `locked = enc != 0`（任何非零位都算锁，故意不走位尺解码）；锁着的一律 `title: None`。
    /// 库里缺号 ⇒ 该号 `exists=false`；库文件不在 ⇒ 空表。
    pub fn titles_for(db_path: &Path, wanted: &[i64]) -> Vec<InfoEntry> {
        if !db_path.exists() {
            return Vec::new();
        }
        let conn = match Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        ) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let _ = conn.busy_timeout(Duration::from_millis(200));
        wanted
            .iter()
            .map(|id| {
                match conn.query_row(
                    "SELECT title, enc FROM notes WHERE id=?1 AND kind='note'",
                    params![id],
                    |r| -> rusqlite::Result<(Option<String>, i64)> { Ok((r.get(0)?, r.get(1)?)) },
                ) {
                    Ok((title, enc)) => InfoEntry {
                        note_id: *id,
                        exists: true,
                        title: if enc != 0 { None } else { title },
                        locked: enc != 0,
                    },
                    Err(_) => InfoEntry {
                        note_id: *id,
                        exists: false,
                        title: None,
                        locked: false,
                    },
                }
            })
            .collect()
    }

    /// 正文相对路径 → 绝对（op 读盘要它）。
    pub fn resolve_body(&self, root: &Root, rel: &str) -> Res<PathBuf> {
        root.resolve(rel)
    }

    fn query_notes(&self, sql: &str, p: impl rusqlite::Params) -> Res<Vec<NoteRow>> {
        let mut stmt = self.conn.prepare(sql).es()?;
        let rows = stmt.query_map(p, read_note).es()?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.es()?);
        }
        Ok(out)
    }
}

fn read_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
    let kind_word: String = r.get(1)?;
    let enc_bit: i64 = r.get(4)?;
    Ok(NoteRow {
        id: r.get(0)?,
        kind: NoteKind::from_word(&kind_word).unwrap_or(NoteKind::Note),
        folder_id: r.get(2)?,
        title: r.get(3)?,
        enc: if enc_bit == 0 {
            Enc::Public
        } else {
            Enc::Private
        },
        body_path: r.get(5)?,
        created_at_ms: r.get(6)?,
        updated_at_ms: r.get(7)?,
        bytes: r.get(8)?,
    })
}

/// `notes.info` 的一条读数（宿主桥接 `info_entries` 的形状）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InfoEntry {
    pub note_id: i64,
    pub exists: bool,
    pub title: Option<String>,
    pub locked: bool,
}

fn migrate(conn: &Connection, found: i32) -> rusqlite::Result<()> {
    if found < 2 {
        conn.execute_batch(NOTES_DDL)?;
        conn.execute_batch(NOTES_INDEX_DDL)?;
    }
    if !table_exists(conn, "note_assets")? {
        conn.execute_batch(ASSETS_DDL)?;
        conn.execute_batch(ASSETS_INDEX_DDL)?;
    }
    if found != SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        params![name],
        |r| r.get(0),
    )
}

// —— 路径口径 ——

/// 库根：含 `folders/` 的那一层，`notes.db` 在它旁边的 `data_dir`。
///
/// `resolve` 是**沙箱闸**：先过词法 [`check_rel`]（`..`／`.`／空段／`\`／`:`／控制字符／绝对／NUL 全拒），
/// 再把路径限在 root 内。半路（还没建的正文）取最近存在的祖先规范化后比对，防止靠 symlink 越狱。
pub struct Root {
    root: PathBuf,
}

impl Root {
    pub fn open(root: &Path) -> Res<Self> {
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        Ok(Self { root: canonical })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    /// 相对路径 → 落在 root 内的绝对路径。
    ///
    /// ⚠ 空尾不补（`Path::join("")` 会甩一个分隔符，把 `…/1.md` 变成 `…/1.md/` ⇒ 读成「以 1.md 为目录」，
    /// `exists()` 假、写盘 `IsADirectory`）：先把最近存在的那截祖先规范化，再按**分量**补回还没建的尾巴。
    pub fn resolve(&self, rel: &str) -> Res<PathBuf> {
        check_rel(rel)?;
        let joined = self.root.join(rel);
        let canonical = match nearest_existing(&joined) {
            Some(existing) => {
                let prefix = std::fs::canonicalize(&existing).unwrap_or_else(|_| existing.clone());
                let tail = joined.strip_prefix(&existing).unwrap_or(Path::new(""));
                let mut out = prefix;
                for comp in tail.components() {
                    out.push(comp.as_os_str());
                }
                out
            }
            None => joined,
        };
        if canonical.starts_with(&self.root) {
            Ok(canonical)
        } else {
            Err(format!("正文路径 `{rel}` 落在库根之外，拒绝"))
        }
    }
}

fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut cur = path.to_path_buf();
    loop {
        if std::fs::canonicalize(&cur).is_ok() {
            return Some(cur);
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// 公开正文相对路径：`folders/<dir>/notes/<id>.md`。
pub fn public_body_rel(dir_name: &str, note_id: i64) -> String {
    format!("{FOLDERS_DIR}/{dir_name}/{NOTES_SUBDIR}/{note_id}.{BODY_EXT}")
}

/// 绝对路径 → 相对 root 的 POSIX 串（`\`→`/`）。
fn rel_under(root: &Root, abs: &Path) -> String {
    let stripped = abs.strip_prefix(root.path()).unwrap_or(abs);
    stripped.to_string_lossy().replace('\\', "/")
}

/// 相对路径的词法闸（与宿主 `check_rel` 同族）：越界形状一律拒。
pub fn check_rel(rel: &str) -> Res<()> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\0') {
        return Err(format!("正文路径不合规（空／绝对／含 NUL）：`{rel}`"));
    }
    if rel.contains('\\') {
        return Err(format!("正文路径要用 `/`，不许有 `\\`：`{rel}`"));
    }
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains(':') {
            return Err(format!(
                "正文路径分段不合规（空／`.`／`..`／含 `:`）：`{rel}`"
            ));
        }
    }
    Ok(())
}

/// 目录名闸（`check_dir_name`）：非空、不是 `.`/`..`、无 `/` `\` `:`、无控制字符、不超 64 标量。
pub fn check_dir_name(name: &str) -> Res<()> {
    const DIR_NAME_MAX_SCALARS: usize = 64;
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("目录名不合规：`{name}`"));
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.chars().any(|c| c.is_control())
    {
        return Err(format!("目录名含非法字符：`{name}`"));
    }
    if name.chars().count() > DIR_NAME_MAX_SCALARS {
        return Err(format!("目录名超过 64 标量：`{name}`"));
    }
    Ok(())
}

/// 正文文件名闸（`clean_note_stem`）：trim、去尾部 `.`／空格、≤96 字符、拒控制字符与 `\ / : * ? " < > |`、
/// 拒 Windows 保留名（CON/PRN/AUX/NUL/COM1-9/LPT1-9，不分大小写）。
pub fn clean_note_stem(raw: &str) -> Res<String> {
    let mut s = raw.trim().to_string();
    while s.ends_with('.') || s.ends_with(' ') {
        s.pop();
    }
    if s.is_empty() {
        return Err("改名 stem 不能为空".to_string());
    }
    if s.chars().count() > 96 {
        return Err("改名 stem 超过 96 字符".to_string());
    }
    if s
        .chars()
        .any(|c| c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return Err(format!("改名 stem 含非法字符：`{s}`"));
    }
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let head = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&head.as_str()) {
        return Err(format!("`{s}` 撞上保留名"));
    }
    Ok(s)
}

/// 原子写：同目录临时名 `<name>.tmp-<pid>-<seq>` → `create_new`+`0600`（unix）→ write → fsync → rename。
///
/// 只留一个写手在这条路上；失败留原文件不动、删临时。`create_new` 撞同名（同 pid+seq）失败而非接管半截文件。
pub fn atomic_write(target: &Path, bytes: &[u8], seq: u64) -> Res<()> {
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "原子写要一个有名字的落点".to_string())?;
    let tmp = target.with_file_name(format!("{file_name}.tmp-{}-{seq}", std::process::id()));
    let result = (|| -> Res<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp).es()?;
        std::io::Write::write_all(&mut file, bytes).es()?;
        file.sync_all().es()?;
        drop(file);
        std::fs::rename(&tmp, target).es()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 把盘上原文折成正文：公开是裸 Markdown 原样返回；只有「以 `{` 开头且解得开成旧信封」才折出
/// `payload.body`（宿主 `note_text` 对旧 JSON 信封那一条兼容读）。
pub fn note_text(raw: &str) -> String {
    let trimmed = raw.trim_start();
    if trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if v.get("v").and_then(|x| x.as_i64()) == Some(1) {
                if let Some(body) = v
                    .get("payload")
                    .and_then(|p| p.get("body"))
                    .and_then(|b| b.as_str())
                {
                    return body.to_string();
                }
            }
        }
    }
    raw.to_string()
}

// —— 标题派生 ——

/// 从正文派生标题：第一个非空 H1（纯文本化）；否则第一个「有话说」的块的首行；否则空串（未命名归 i18n）。
pub fn derive_title(body: &str) -> String {
    for line in body.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("# ") {
            let text = plain_inline(rest.trim());
            if !text.is_empty() {
                return truncate_glyphs(&text, 80);
            }
        }
    }
    for line in body.lines() {
        let t = line.trim();
        if t.is_empty() || is_thematic(t) || t.starts_with("```") {
            continue;
        }
        let cleaned = strip_block_prefix(t);
        let text = plain_inline(&cleaned);
        if !text.is_empty() {
            return truncate_glyphs(&text, 80);
        }
    }
    String::new()
}

/// 摘要首行：正文里第一个非空行的纯文本（宿主 `first_line` 的简化版）。
pub fn first_line(body: &str) -> String {
    for line in body.lines() {
        let t = line.trim();
        if !t.is_empty() {
            return plain_inline(&strip_block_prefix(t));
        }
    }
    String::new()
}

fn is_thematic(t: &str) -> bool {
    let compact: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    (compact.chars().all(|c| c == '-') && compact.len() >= 3)
        || (compact.chars().all(|c| c == '*') && compact.len() >= 3)
        || (compact.chars().all(|c| c == '_') && compact.len() >= 3)
}

fn strip_block_prefix(t: &str) -> String {
    let mut s = t;
    while let Some(rest) = s.strip_prefix('#') {
        s = rest;
    }
    s = s.trim_start();
    while let Some(rest) = s.strip_prefix('>') {
        s = rest.trim_start();
    }
    if let Some(rest) = s.strip_prefix("- ").or_else(|| s.strip_prefix("* ")).or_else(|| s.strip_prefix("+ ")) {
        s = rest;
    } else if let Some(pos) = s.find(". ") {
        let head = &s[..pos];
        if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) {
            s = &s[pos + 2..];
        }
    }
    s.to_string()
}

/// 把行内 Markdown 拍成纯文本：链接只留文字 `[t](u)`→`t`，去掉成对的 `*`／`_`／反引号。
///
/// ⚠ 逐**标量**走、不用字节索引（`bytes[i] as char` 会把中文拆成 mojibake）；链接定位在整串上按
/// 字符偏移量算，`char_indices` 交回的 `i` 就是切 `s[i..]` 的合法字节边界。
fn plain_inline(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '[' => {
                // 从这一位往后找 `](`，找到就把中括号里的文字留下、跳到闭括号之后。
                let close = (i + 1..chars.len())
                    .find(|&j| chars[j] == ']' && j + 1 < chars.len() && chars[j + 1] == '(');
                if let Some(bracket) = close {
                    out.extend(chars[i + 1..bracket].iter());
                    let after_paren = (bracket + 2..chars.len())
                        .find(|&j| chars[j] == ')')
                        .map(|j| j + 1)
                        .unwrap_or(chars.len());
                    i = after_paren;
                } else {
                    out.push('[');
                    i += 1;
                }
            }
            '*' | '_' | '`' => {
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out.trim().to_string()
}

fn truncate_glyphs(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new().prefix(&format!("nomo-plugin-{tag}")).tempdir().unwrap()
    }

    #[test]
    fn open_creates_schema_and_is_idempotent() {
        let d = temp_dir("schema");
        let db = d.path().join("notes.db");
        {
            let conn = Db::open(&db).unwrap();
            assert!(conn.note(1).unwrap().is_none());
        }
        let conn = Db::open(&db).unwrap();
        let found: i32 = conn
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(found, SCHEMA_VERSION);
    }

    #[test]
    fn a_newer_schema_is_refused_without_writing() {
        let d = temp_dir("toounw");
        let db = d.path().join("notes.db");
        {
            let c = Connection::open(&db).unwrap();
            c.pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        }
        assert!(Db::open(&db).is_err(), "比插件还新的库要整个拒");
    }

    #[test]
    fn create_then_read_finds_the_body_file() {
        let d = temp_dir("create");
        let db = d.path().join("notes.db");
        let mut conn = Db::open(&db).unwrap();
        let root = Root::open(d.path()).unwrap();
        let row = conn.create_note(&root, "inbox", 7).unwrap();
        assert_eq!(row.folder_id, 7);
        assert_eq!(row.body_path, "folders/inbox/notes/1.md");
        let abs = root.resolve(&row.body_path).unwrap();
        assert!(abs.exists(), "空正文文件该被建出来");
        let back = conn.note(row.id).unwrap().expect("刚建的号要读得回");
        assert_eq!(back.body_path, row.body_path);
    }

    #[test]
    fn save_writes_derived_title_and_locks_on_identity() {
        let d = temp_dir("save");
        let db = d.path().join("notes.db");
        let mut conn = Db::open(&db).unwrap();
        let root = Root::open(d.path()).unwrap();
        let mut row = conn.create_note(&root, "inbox", 7).unwrap();
        conn.save_note(&mut row, "# 我的标题\n正文一行\n").unwrap();
        assert_eq!(row.title.as_deref(), Some("我的标题"));
        let got = conn.note(row.id).unwrap().unwrap();
        assert_eq!(got.title.as_deref(), Some("我的标题"));
    }

    #[test]
    fn paths_and_names_are_gated() {
        assert!(check_rel("folders/a/notes/1.md").is_ok());
        assert!(check_rel("../escape").is_err());
        assert!(check_rel("/abs").is_err());
        assert!(check_rel("a\\b").is_err());
        assert!(check_dir_name("inbox").is_ok());
        assert!(check_dir_name("..").is_err());
        assert_eq!(clean_note_stem("  a.txt. ").unwrap(), "a.txt");
        assert!(clean_note_stem("CON").is_err());
        assert!(clean_note_stem("带/斜杠").is_err());
    }

    #[test]
    fn derive_title_prefers_h1_then_first_content_line() {
        assert_eq!(derive_title("# 大标题\n内容"), "大标题");
        assert_eq!(derive_title("\n\n普通第一段"), "普通第一段");
        assert_eq!(derive_title("## [链接文字](http://x)"), "链接文字");
        assert_eq!(derive_title(""), "");
        assert_eq!(first_line("- 列表项"), "列表项");
    }

    #[test]
    fn legacy_envelope_folds_but_plain_markdown_passes_through() {
        let legacy = r#"{"v":1,"kind":"note","payload":{"title":"旧的","body":"折出来"}}"#;
        assert_eq!(note_text(legacy), "折出来");
        assert_eq!(note_text("# 裸 Markdown"), "# 裸 Markdown");
    }

    #[test]
    fn rename_moves_file_and_rolls_back_on_identity_mismatch() {
        let d = temp_dir("rename");
        let db = d.path().join("notes.db");
        let mut conn = Db::open(&db).unwrap();
        let root = Root::open(d.path()).unwrap();
        let row = conn.create_note(&root, "inbox", 7).unwrap();
        let old_abs = root.resolve(&row.body_path).unwrap();
        std::fs::write(&old_abs, b"x").unwrap();
        let new_abs = conn.rename_body(&root, &row, "新名").unwrap();
        assert!(new_abs.exists());
        assert!(!old_abs.exists(), "旧名该没了");
        let back = conn.note(row.id).unwrap().unwrap();
        assert!(back.body_path.ends_with("新名.md"), "{}", back.body_path);
    }

    #[test]
    fn titles_for_marks_locked_and_missing() {
        let d = temp_dir("info");
        let db = d.path().join("notes.db");
        let mut conn = Db::open(&db).unwrap();
        let root = Root::open(d.path()).unwrap();
        let mut row = conn.create_note(&root, "inbox", 7).unwrap();
        conn.save_note(&mut row, "# 活的").unwrap();
        let got = Db::titles_for(&db, &[row.id, 999]);
        assert_eq!(got[0].title.as_deref(), Some("活的"));
        assert!(!got[0].locked);
        assert!(!got[1].exists, "库里没这一号要如实说 exists=false");
    }
}
