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

use std::collections::HashMap;
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
/// 插件自己的附件宽限账不升这个号，避免宿主把仍兼容的核心库判成新 schema。
pub const SCHEMA_VERSION: i32 = 2;

/// 插件自有附件失去引用后的最短保留时间。
pub const ASSET_ORPHAN_GRACE_MS: i64 = 24 * 60 * 60 * 1000;

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

const ASSET_COLUMNS: &str = "id, note_id, kind, rel_path, source_kind, source_ref, bytes, width, height, frames, duration_ms, sha256, created_at_ms";

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

/// 插件自有的延迟回收账：路径唯一，时间记录首次失去引用的时刻。
const ASSET_ORPHANS_DDL: &str = r#"CREATE TABLE IF NOT EXISTS nomo_asset_orphans (
    rel_path       TEXT    PRIMARY KEY,
    orphaned_at_ms INTEGER NOT NULL
)"#;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    Image,
    Gif,
}

impl AssetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Gif => "gif",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Pin,
    Gif,
    File,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pin => "pin",
            Self::Gif => "gif",
            Self::File => "file",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetRow {
    pub id: String,
    pub note_id: i64,
    pub kind: AssetKind,
    pub rel_path: String,
    pub source_kind: SourceKind,
    pub source_ref: String,
    pub bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frames: Option<u32>,
    pub duration_ms: Option<u32>,
    pub sha256: String,
    pub created_at_ms: i64,
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

/// 正文改名失败时，调用方据此决定是否保留正式会话的恢复意图。
#[derive(Debug)]
pub struct RenameBodyError {
    message: String,
    rollback_failed: bool,
}

impl RenameBodyError {
    fn recovered(message: impl Into<String>) -> Self {
        Self { message: message.into(), rollback_failed: false }
    }

    fn recovery_required(message: impl Into<String>) -> Self {
        Self { message: message.into(), rollback_failed: true }
    }

    pub fn rollback_failed(&self) -> bool {
        self.rollback_failed
    }
}

impl std::fmt::Display for RenameBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RenameBodyError {}

/// 数据库提交后待清理的文件；附件只登记宽限账，正文仍在提交后立即清理。
#[derive(Debug)]
pub struct DeletedNote {
    pub body_path: Option<PathBuf>,
    pub asset_paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

/// 一次启动附件扫收的读数；未知文件只计数，不携带内容或无限路径列表。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssetSweepReport {
    pub removed: usize,
    pub kept_young: usize,
    pub referenced: usize,
    pub unrecognized: usize,
    pub refused: usize,
    pub failed: usize,
}

impl Db {
    /// 开库并跑迁移：先读 `PRAGMA user_version`，比 [`SCHEMA_VERSION`] **新**就整个拒（一个字都不写）。
    ///
    /// ⚠ 迁移的幂等由**表在不在**决定、不由 `user_version` 决定：`note_assets` 只要缺席就建（哪怕
    /// 号已经是 2），这是宿主那条「v1→v2 只加 `note_assets`、`notes` 列永不改」的落实。
    /// `nomo_asset_orphans` 是插件扩展表，不改变宿主核心 schema 版本。
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
        let tx = self.conn.transaction().es()?;
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
        save_note_on(&self.conn, row, body)
    }

    /// 正文与仍被引用的附件同事务落账；失去引用的旧行退成孤儿账，文件至少保留 24 h。
    pub fn save_note_with_assets(
        &mut self,
        row: &mut NoteRow,
        body: &str,
        root: &Root,
        assets: &[AssetRow],
    ) -> Res<()> {
        let tx = self.conn.transaction().es()?;
        save_note_on(&tx, row, body)?;
        let orphaned_at = now_ms();
        let existing = asset_rows_on(&tx, row.id)?;
        let mut incoming = std::collections::BTreeMap::new();
        for asset in assets.iter().filter(|asset| asset.note_id == row.id) {
            if let Some(previous) = incoming.insert(asset.id.as_str(), asset) {
                if previous != asset {
                    return Err(format!("附件 {} 在同一批待办里形状不一致", asset.id));
                }
            }
        }
        let mut referenced = std::collections::HashSet::new();
        for asset in incoming.values() {
            if !body_references_asset(body, asset) {
                continue;
            }
            validate_asset_file(root, row, asset)?;
            put_asset_on(&tx, asset)?;
            tx.execute("DELETE FROM nomo_asset_orphans WHERE rel_path=?1", params![asset.rel_path]).es()?;
            referenced.insert(asset.id.as_str());
        }
        for asset in existing {
            if referenced.contains(asset.id.as_str()) || body_references_asset(body, &asset) {
                tx.execute("DELETE FROM nomo_asset_orphans WHERE rel_path=?1", params![asset.rel_path]).es()?;
                continue;
            }
            tx.execute("DELETE FROM note_assets WHERE id=?1 AND note_id=?2", params![asset.id, row.id]).es()?;
            if owned_asset_rel(row, &asset).is_some() {
                register_orphan_on(&tx, &asset.rel_path, orphaned_at)?;
            }
        }
        for asset in incoming.values().filter(|asset| !referenced.contains(asset.id.as_str())) {
            if owned_asset_rel(row, asset).is_some() {
                register_orphan_on(&tx, &asset.rel_path, orphaned_at)?;
            }
        }
        tx.commit().es()?;
        Ok(())
    }

    /// 先算好改名的两端，供已打开文档的会话在同一把路径锁内更新基线。
    pub fn rename_target(&self, root: &Root, row: &NoteRow, stem: &str) -> Res<(PathBuf, PathBuf)> {
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
        let new_abs = old_abs.with_file_name(format!("{stem}.{ext}"));
        if new_abs.exists() {
            return Err(format!("目标 {}.{ext} 已存在，改名不覆盖", stem));
        }
        Ok((old_abs, new_abs))
    }

    /// 把正文文件就地改名（保留旧扩展名、默认 `md`），再把新 `body_path` 落回那一行；
    /// **DB 失败就回滚磁盘改名**。私有行拒绝。不碰标题、不碰信封——改的是**物理文件名**。
    pub fn rename_body(&self, root: &Root, row: &NoteRow, stem: &str) -> Res<PathBuf> {
        self.rename_body_for_session(root, row, stem)
            .map_err(|error| error.to_string())
    }

    /// 与 [`Db::rename_body`] 同一趟改名，同时把回滚结果交给已打开文档的恢复流程。
    pub fn rename_body_for_session(
        &self,
        root: &Root,
        row: &NoteRow,
        stem: &str,
    ) -> Result<PathBuf, RenameBodyError> {
        let (old_abs, new_abs) = self.rename_target(root, row, stem)
            .map_err(RenameBodyError::recovered)?;
        std::fs::rename(&old_abs, &new_abs)
            .map_err(|error| RenameBodyError::recovered(format!("移动正文失败：{error}")))?;
        let rel = rel_under(root, &new_abs);
        let rollback = |message: String| match std::fs::rename(&new_abs, &old_abs) {
            Ok(()) => RenameBodyError::recovered(format!("{message}，已回滚文件")),
            Err(error) => RenameBodyError::recovery_required(format!(
                "{message}；回滚文件也失败：{error}，恢复意图需保留"
            )),
        };
        let changed = match self.conn.execute(
            "UPDATE notes SET body_path=?1 WHERE id=?2 AND folder_id=?3 AND enc=?4",
            params![rel, row.id, row.folder_id, row.enc.bit()],
        ) {
            Ok(changed) => changed,
            Err(error) => return Err(rollback(format!("改名落库失败：{error}"))),
        };
        if changed == 0 {
            return Err(rollback(format!(
                "改名落库失败（第 {} 号身份对不上）",
                row.id
            )));
        }
        Ok(new_abs)
    }

    /// 事务删除笔记行与全部附件行，提交后只清正文；附件退成宽限账，由扫收在 24 h 后回收。
    /// 身份列或正文路径在事务里漂移则整趟回滚；附件路径越界在提交前就拒（不拿半截账换一篇删不掉的笔记）。
    pub fn delete_note(&mut self, root: &Root, expected: &NoteRow) -> Res<Option<DeletedNote>> {
        let tx = self.conn.transaction().es()?;
        let sql = format!("SELECT {NOTE_COLUMNS} FROM notes WHERE id=?1");
        let current = tx.query_row(&sql, params![expected.id], read_note)
            .optional()
            .es()?;
        let Some(current) = current else { return Ok(None) };
        if current.kind != expected.kind
            || current.folder_id != expected.folder_id
            || current.enc != expected.enc
            || current.body_path != expected.body_path
        {
            return Err(format!("第 {} 号笔记已被别处改动，这次删除作废", expected.id));
        }
        let body_path = if current.body_path.is_empty() {
            None
        } else {
            Some(root.resolve(&current.body_path)?)
        };
        let assets = asset_rows_on(&tx, current.id)?;
        let mut asset_paths = Vec::new();
        for asset in &assets {
            asset_paths.push(root.resolve(&asset.rel_path)?);
        }
        let orphaned_at = now_ms();
        for asset in &assets {
            if owned_asset_rel(&current, asset).is_some() {
                register_orphan_on(&tx, &asset.rel_path, orphaned_at)?;
            }
        }
        tx.execute("DELETE FROM note_assets WHERE note_id=?1", params![current.id]).es()?;
        let changed = tx.execute(
            "DELETE FROM notes WHERE id=?1 AND kind=?2 AND folder_id=?3 AND enc=?4 AND body_path=?5",
            params![current.id, current.kind.as_str(), current.folder_id, current.enc.bit(), current.body_path],
        ).es()?;
        if changed != 1 {
            return Err(format!("第 {} 号笔记删除时身份对不上", current.id));
        }
        tx.commit().es()?;

        let mut warnings = Vec::new();
        if let Some(path) = &body_path {
            remove_deleted_file(path, "正文", &mut warnings);
        }
        Ok(Some(DeletedNote { body_path, asset_paths, warnings }))
    }

    /// 一次启动附件扫收（P2）：只清「形状确认为插件自有 ＋ 确认无引用 ＋ 已过 [`ASSET_ORPHAN_GRACE_MS`]」的副本。
    /// 未知名字、临时文件、目录／符号链接、判不出年龄的一律留盘；删除失败**保留宽限账**，下一趟再试。
    pub fn sweep_expired_assets(&self, root: &Root, now: i64) -> Res<AssetSweepReport> {
        let mut report = AssetSweepReport::default();
        // 候选 = 宽限账 ∪ 盘上合法但没落账的旧副本（账里那条时间优先，盘上时间只兜底）。
        let mut candidates: std::collections::BTreeMap<String, Option<i64>> = std::collections::BTreeMap::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT rel_path, orphaned_at_ms FROM nomo_asset_orphans")
                .es()?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .es()?;
            for row in rows {
                let (rel, at) = row.es()?;
                candidates.insert(rel, Some(at));
            }
        }
        let live = self.live_asset_paths()?;
        for rel in self.disk_asset_candidates(root, &mut report) {
            candidates.entry(rel).or_insert(None);
        }
        let mut bodies: HashMap<String, Vec<String>> = HashMap::new();
        for (rel, ledger_at) in candidates {
            if !plugin_asset_rel(&rel) {
                // 账里躺着一格不认识的形状：既不删文件也不销账，留给人看。
                report.refused += 1;
                continue;
            }
            if live.contains(rel.as_str()) {
                report.referenced += 1;
                self.drop_orphan_row(&rel)?;
                continue;
            }
            let path = match root.resolve(&rel) {
                Ok(path) => path,
                Err(_) => {
                    report.refused += 1;
                    continue;
                }
            };
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // 盘上本就没有：这格账结了，顺带清掉指向它的残留索引行。
                    report.removed += 1;
                    self.drop_orphan_row(&rel)?;
                    self.forget_orphaned_index_rows(&rel)?;
                    continue;
                }
                Err(_) => {
                    report.refused += 1;
                    continue;
                }
            };
            if !metadata.is_file() {
                report.unrecognized += 1;
                continue;
            }
            let dir = rel.split('/').nth(1).unwrap_or_default();
            if !bodies.contains_key(dir) {
                match self.folder_bodies(root, dir) {
                    Ok(rows) => {
                        bodies.insert(dir.to_string(), rows);
                    }
                    // 读不动那一夹的正文就没法确认引用 ⇒ 宁可不删。
                    Err(_) => {
                        report.refused += 1;
                        continue;
                    }
                }
            }
            let name = rel.rsplit('/').next().unwrap_or_default();
            let asset = candidate_asset_row(name.split('.').next().unwrap_or_default(), &rel);
            if bodies[dir].iter().any(|body| body_references_asset(body, &asset)) {
                report.referenced += 1;
                self.drop_orphan_row(&rel)?;
                continue;
            }
            let Some(age) = ledger_at.or_else(|| modified_ms(&metadata)) else {
                report.refused += 1;
                continue;
            };
            if now - age < ASSET_ORPHAN_GRACE_MS {
                report.kept_young += 1;
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    report.failed += 1;
                    continue;
                }
            }
            report.removed += 1;
            self.drop_orphan_row(&rel)?;
            self.forget_orphaned_index_rows(&rel)?;
        }
        Ok(report)
    }

    /// 仍挂在活笔记上的附件路径（索引在＝有人在用，私密那一格也靠它护着，不去读正文）。
    fn live_asset_paths(&self) -> Res<std::collections::HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT rel_path FROM note_assets WHERE note_id IN (SELECT id FROM notes)")
            .es()?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).es()?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.insert(row.es()?);
        }
        Ok(out)
    }

    /// 那个文件夹里**公开**正文的当前内容（对引用用；私密正文不读，其附件由 `note_assets` 行保护）。
    fn folder_bodies(&self, root: &Root, dir: &str) -> Res<Vec<String>> {
        let prefix = format!("{FOLDERS_DIR}/{dir}/{NOTES_SUBDIR}/");
        let mut paths = Vec::new();
        {
            let mut stmt = self
                .conn
                .prepare("SELECT body_path FROM notes WHERE kind='note' AND enc=0")
                .es()?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).es()?;
            for row in rows {
                let body_path = row.es()?;
                if let Some(name) = body_path.strip_prefix(&prefix) {
                    if !name.is_empty() && !name.contains('/') {
                        paths.push(body_path);
                    }
                }
            }
        }
        let mut out = Vec::new();
        for body_path in paths {
            let abs = root.resolve(&body_path)?;
            if let Ok(raw) = std::fs::read_to_string(&abs) {
                out.push(note_text(&raw));
            }
        }
        Ok(out)
    }

    /// 盘上扫 `folders/<dir>/notes/a/`：认不出的名字（临时文件、别家副本、目录）只计数、不碰。
    fn disk_asset_candidates(&self, root: &Root, report: &mut AssetSweepReport) -> Vec<String> {
        let mut rels = Vec::new();
        let Ok(entries) = std::fs::read_dir(root.path().join(FOLDERS_DIR)) else {
            return rels;
        };
        for entry in entries.flatten() {
            let dir = entry.file_name().to_string_lossy().to_string();
            if !entry.path().is_dir() || check_dir_name(&dir).is_err() {
                continue;
            }
            let assets = root
                .path()
                .join(FOLDERS_DIR)
                .join(&dir)
                .join(NOTES_SUBDIR)
                .join("a");
            let Ok(files) = std::fs::read_dir(assets) else { continue };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().to_string();
                if name.contains('/') || name.contains('\\') {
                    report.unrecognized += 1;
                    continue;
                }
                let rel = format!("{FOLDERS_DIR}/{dir}/{NOTES_SUBDIR}/a/{name}");
                if plugin_asset_rel(&rel) {
                    rels.push(rel);
                } else {
                    report.unrecognized += 1;
                }
            }
        }
        rels
    }

    fn drop_orphan_row(&self, rel: &str) -> Res<()> {
        self.conn
            .execute("DELETE FROM nomo_asset_orphans WHERE rel_path=?1", params![rel])
            .es()?;
        Ok(())
    }

    /// 文件已回收，指向它的残留索引行（笔记早没了那种旧形状）一并清掉；活笔记的行不在这里动。
    fn forget_orphaned_index_rows(&self, rel: &str) -> Res<()> {
        self.conn
            .execute(
                "DELETE FROM note_assets WHERE rel_path=?1 \
                 AND note_id NOT IN (SELECT id FROM notes)",
                params![rel],
            )
            .es()?;
        Ok(())
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

fn save_note_on(conn: &Connection, row: &mut NoteRow, body: &str) -> Res<()> {
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
    let changed = conn
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

fn put_asset_on(conn: &Connection, asset: &AssetRow) -> Res<()> {
    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT note_id, rel_path FROM note_assets WHERE id=?1",
            params![asset.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .es()?;
    if let Some((note_id, rel_path)) = existing {
        if note_id != asset.note_id || rel_path != asset.rel_path {
            return Err(format!(
                "附件 {} 已属于另一篇笔记或另一路径，拒绝覆盖",
                asset.id
            ));
        }
        let changed = conn
            .execute(
                "UPDATE note_assets SET kind=?1, source_kind=?2, source_ref=?3, bytes=?4, \
                 width=?5, height=?6, frames=?7, duration_ms=?8, sha256=?9, created_at_ms=?10 \
                 WHERE id=?11 AND note_id=?12 AND rel_path=?13",
                params![
                    asset.kind.as_str(),
                    asset.source_kind.as_str(),
                    asset.source_ref,
                    asset.bytes as i64,
                    asset.width,
                    asset.height,
                    asset.frames,
                    asset.duration_ms,
                    asset.sha256,
                    asset.created_at_ms,
                    asset.id,
                    asset.note_id,
                    asset.rel_path
                ],
            )
            .es()?;
        if changed != 1 {
            return Err(format!("附件 {} 重试时身份对不上", asset.id));
        }
        return Ok(());
    }
    conn.execute(
        "INSERT INTO note_assets \
         (id, note_id, kind, rel_path, source_kind, source_ref, bytes, width, height, frames, duration_ms, sha256, created_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            asset.id,
            asset.note_id,
            asset.kind.as_str(),
            asset.rel_path,
            asset.source_kind.as_str(),
            asset.source_ref,
            asset.bytes as i64,
            asset.width,
            asset.height,
            asset.frames,
            asset.duration_ms,
            asset.sha256,
            asset.created_at_ms
        ],
    )
    .es()?;
    Ok(())
}

fn read_asset(r: &rusqlite::Row<'_>) -> rusqlite::Result<AssetRow> {
    let kind_word: String = r.get(2)?;
    let source_word: String = r.get(4)?;
    Ok(AssetRow {
        id: r.get(0)?,
        note_id: r.get(1)?,
        kind: if kind_word == AssetKind::Gif.as_str() { AssetKind::Gif } else { AssetKind::Image },
        rel_path: r.get(3)?,
        source_kind: match source_word.as_str() {
            "pin" => SourceKind::Pin,
            "gif" => SourceKind::Gif,
            _ => SourceKind::File,
        },
        source_ref: r.get(5)?,
        bytes: r.get::<_, i64>(6)?.max(0) as u64,
        width: r.get(7)?,
        height: r.get(8)?,
        frames: r.get(9)?,
        duration_ms: r.get(10)?,
        sha256: r.get(11)?,
        created_at_ms: r.get(12)?,
    })
}

/// 某篇笔记已登记的附件行（列序照 [`ASSET_COLUMNS`]，`ORDER BY rowid` 保宿主写入序）。
fn asset_rows_on(conn: &Connection, note_id: i64) -> Res<Vec<AssetRow>> {
    let sql = format!("SELECT {ASSET_COLUMNS} FROM note_assets WHERE note_id=?1 ORDER BY rowid");
    let mut stmt = conn.prepare(&sql).es()?;
    let rows = stmt.query_map(params![note_id], read_asset).es()?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.es()?);
    }
    Ok(out)
}

/// 记一条宽限账：同一格只认**首次**失引时刻，重复登记不续期（否则反复保存会把该收的副本一直养着）。
fn register_orphan_on(conn: &Connection, rel: &str, orphaned_at_ms: i64) -> Res<()> {
    conn.execute(
        "INSERT INTO nomo_asset_orphans (rel_path, orphaned_at_ms) VALUES (?1, ?2) \
         ON CONFLICT(rel_path) DO NOTHING",
        params![rel, orphaned_at_ms],
    )
    .es()?;
    Ok(())
}

/// 附件行的词法归属闸：只有「当前正文旁的 `a/<id>.<扩展名>`、文件名就是那颗 id」才算**插件自有**副本。
/// 认不出一律 `None`——不 canonicalize、不猜归属，别家的落点不进回收账。
fn owned_asset_rel(note: &NoteRow, asset: &AssetRow) -> Option<String> {
    if asset.note_id != note.id {
        return None;
    }
    let parent = note.body_path.rsplit_once('/')?.0;
    let name = asset.rel_path.strip_prefix(&format!("{parent}/a/"))?;
    let (id, extension) = name.split_once('.')?;
    if id != asset.id || !is_lower_hex(id, 16) || !valid_asset_extension(extension) {
        return None;
    }
    Some(asset.rel_path.clone())
}

/// 扫收用的归属闸（手上只有相对路径时）：`folders/<dir>/notes/a/<32 位 hex>.<扩展名>`。
fn plugin_asset_rel(rel: &str) -> bool {
    let segments: Vec<&str> = rel.split('/').collect();
    if segments.len() != 5
        || segments[0] != FOLDERS_DIR
        || segments[2] != NOTES_SUBDIR
        || segments[3] != "a"
    {
        return false;
    }
    if check_dir_name(segments[1]).is_err() {
        return false;
    }
    let Some((id, extension)) = segments[4].split_once('.') else {
        return false;
    };
    is_lower_hex(id, 16) && valid_asset_extension(extension)
}

fn valid_asset_extension(extension: &str) -> bool {
    !extension.is_empty()
        && extension.len() <= 8
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

/// 扫收候选的年龄：没落账的旧副本回落到盘上时间（读不出就如实当「判不了年龄」）。
fn modified_ms(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|when| when.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as i64)
}

/// 拿附件 id 拼一条只为对引用用的假行：[`body_references_asset`] 只读 `id` 与 `rel_path` 的扩展名。
fn candidate_asset_row(id: &str, rel: &str) -> AssetRow {
    AssetRow {
        id: id.to_string(),
        note_id: 0,
        kind: AssetKind::Image,
        rel_path: rel.to_string(),
        source_kind: SourceKind::File,
        source_ref: String::new(),
        bytes: 0,
        width: None,
        height: None,
        frames: None,
        duration_ms: None,
        sha256: String::new(),
        created_at_ms: 0,
    }
}

fn is_lower_hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes * 2
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_asset_file(root: &Root, note: &NoteRow, asset: &AssetRow) -> Res<()> {
    if asset.note_id <= 0 || asset.note_id != note.id {
        return Err("附件归属的笔记身份不合规".to_string());
    }
    if !is_lower_hex(&asset.id, 16) {
        return Err(format!("附件 id `{}` 不是 32 位小写 hex", asset.id));
    }
    if !is_lower_hex(&asset.sha256, 32) {
        return Err(format!("附件 {} 的 sha256 不是 64 位小写 hex", asset.id));
    }
    if asset.source_kind != SourceKind::File || !asset.source_ref.is_empty() {
        return Err(format!("附件 {} 不是本地文件来源", asset.id));
    }
    if asset.kind == AssetKind::Image && (asset.frames.is_some() || asset.duration_ms.is_some()) {
        return Err(format!("静态图片 {} 不能带 GIF 帧度量", asset.id));
    }
    if asset.bytes == 0 || asset.bytes > i64::MAX as u64 || asset.created_at_ms < 0 {
        return Err(format!("附件 {} 的大小或创建时间不合规", asset.id));
    }
    let (parent, file_name) = note.body_path
        .rsplit_once('/')
        .ok_or_else(|| "正文相对路径没有父目录".to_string())?;
    let extension = Path::new(&asset.rel_path)
        .extension()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("附件 {} 缺少扩展名", asset.id))?;
    if extension.is_empty()
        || extension.len() > 8
        || !extension.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(format!("附件 {} 的扩展名不合规", asset.id));
    }
    let expected_name = format!("{}.{}", asset.id, extension);
    if file_name.is_empty() || asset.rel_path != format!("{parent}/a/{expected_name}") {
        return Err(format!("附件 {} 的落点不是当前正文旁的 a/", asset.id));
    }
    let path = root.resolve(&asset.rel_path)?;
    let canonical = std::fs::canonicalize(&path)
        .map_err(|error| format!("附件 {} 无法确认：{error}", asset.id))?;
    let expected_dir = root.resolve(&format!("{parent}/a"))?;
    let expected_dir = std::fs::canonicalize(&expected_dir)
        .map_err(|error| format!("附件目录无法确认：{error}"))?;
    if canonical.parent() != Some(expected_dir.as_path()) || !canonical.starts_with(root.path()) {
        return Err(format!("附件 {} 落在当前正文附件目录之外", asset.id));
    }
    let metadata = std::fs::metadata(&canonical)
        .map_err(|error| format!("读取附件 {} 失败：{error}", asset.id))?;
    if !metadata.is_file() || metadata.len() != asset.bytes {
        return Err(format!("附件 {} 的字节数与登记值对不上", asset.id));
    }
    let digest = crate::image_import::fingerprint(&canonical, &Default::default())?;
    if hex_lower(&digest) != asset.sha256 {
        return Err(format!("附件 {} 的 SHA-256 与登记值对不上", asset.id));
    }
    Ok(())
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 15) as usize] as char);
    }
    out
}

fn asset_target_matches(target: &str, asset: &AssetRow) -> bool {
    let target = html_escape::decode_html_entities(target.trim());
    let target = target.strip_prefix('<').and_then(|value| value.strip_suffix('>')).unwrap_or(&target);
    let target = target.split(['?', '#']).next().unwrap_or("");
    let mut target = target;
    while let Some(rest) = target.strip_prefix("./") {
        target = rest;
    }
    let extension = Path::new(&asset.rel_path).extension().and_then(|value| value.to_str()).unwrap_or_default();
    !target.contains('\\') && target == format!("a/{}.{}", asset.id, extension)
}

/// 只认解析出来的图片目标，避免正文别处的裸 asset id 或相似路径误触发落账。
pub fn body_references_asset(body: &str, asset: &AssetRow) -> bool {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let mut events = Parser::new(body).into_offset_iter();
    while let Some((event, range)) = events.next() {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                if asset_target_matches(&dest_url, asset) {
                    return true;
                }
                while let Some((event, _)) = events.next() {
                    if matches!(event, Event::End(TagEnd::Image)) {
                        break;
                    }
                }
            }
            Event::Html(raw) | Event::InlineHtml(raw) => {
                let mut offset = 0;
                let bytes = raw.as_bytes();
                while let Some(found) = bytes[offset..]
                    .windows(4)
                    .position(|value| value.eq_ignore_ascii_case(b"<img"))
                {
                    let start = offset + found;
                    let Some(tag) = crate::nomo_mark::nomo_syntax::html_tag(&raw, start) else {
                        offset = start + 4;
                        continue;
                    };
                    if tag.name == "img" && !tag.closing {
                        if let Some(src) = tag.attrs.get("src") {
                            if asset_target_matches(src, asset) {
                                return true;
                            }
                        }
                    }
                    offset = tag.end.max(start + 4);
                }
            }
            _ => {}
        }
        let _ = range;
    }
    false
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
    // 宽限账是插件自己的表：建不建都不动 `user_version`，宿主那边看到的仍是核心 schema 2。
    if !table_exists(conn, "nomo_asset_orphans")? {
        conn.execute_batch(ASSET_ORPHANS_DDL)?;
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
#[derive(Clone, Debug)]
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
    if rel.chars().any(char::is_control) {
        return Err(format!("正文路径含控制字符：`{rel}`"));
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

fn remove_deleted_file(path: &Path, label: &str, warnings: &mut Vec<String>) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warnings.push(format!("{label} {} 删除失败：{error}", path.display())),
    }
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

    fn insert_asset(db: &Db, note_id: i64, id: &str, rel_path: &str) {
        db.conn.execute(
            "INSERT INTO note_assets \
             (id, note_id, kind, rel_path, source_kind, source_ref, bytes, width, height, frames, duration_ms, sha256, created_at_ms) \
             VALUES (?1, ?2, 'image', ?3, 'file', '', 1, NULL, NULL, NULL, NULL, 'x', 1)",
            params![id, note_id, rel_path],
        ).unwrap();
    }

    fn orphan_count(db: &Db) -> i64 {
        db.conn.query_row("SELECT COUNT(*) FROM nomo_asset_orphans", [], |r| r.get(0)).unwrap()
    }

    fn orphan_ledger(db: &Db, rel: &str) -> Option<i64> {
        db.conn.query_row(
            "SELECT orphaned_at_ms FROM nomo_asset_orphans WHERE rel_path=?1",
            [rel],
            |r| r.get(0),
        ).ok()
    }

    fn book_orphan(db: &Db, rel: &str, at: i64) {
        db.conn.execute(
            "INSERT INTO nomo_asset_orphans (rel_path, orphaned_at_ms) VALUES (?1, ?2)",
            params![rel, at],
        ).unwrap();
    }

    /// 造一颗形状合法的附件行（id 是 32 位小写 hex，落点就在正文旁边的 `a/`）。
    fn written_asset(root: &Root, dir: &str, note_id: i64, seed: u8) -> (String, AssetRow) {
        let id = format!("{:02x}", seed).repeat(16);
        let rel = format!("folders/{dir}/notes/a/{id}.png");
        let path = root.resolve(&rel).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"png").unwrap();
        let row = AssetRow {
            id: id.clone(),
            note_id,
            kind: AssetKind::Image,
            rel_path: rel.clone(),
            source_kind: SourceKind::File,
            source_ref: String::new(),
            bytes: 3,
            width: None,
            height: None,
            frames: None,
            duration_ms: None,
            sha256: "c".repeat(64),
            created_at_ms: 1,
        };
        (rel, row)
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
        assert!(table_exists(&conn.conn, "nomo_asset_orphans").unwrap(), "宽限账要随开库建出来");
        assert_eq!(found, 2, "插件扩展表不升宿主核心 schema 版本");
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
    fn rename_rolls_back_when_the_sql_update_fails() {
        let d = temp_dir("rename-sql-fail");
        let db = d.path().join("notes.db");
        let mut conn = Db::open(&db).unwrap();
        let root = Root::open(d.path()).unwrap();
        let row = conn.create_note(&root, "inbox", 7).unwrap();
        let old_abs = root.resolve(&row.body_path).unwrap();
        conn.conn.execute_batch(
            "CREATE TRIGGER reject_body_path BEFORE UPDATE OF body_path ON notes \
             BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
        ).unwrap();

        let error = conn.rename_body(&root, &row, "新名").unwrap_err();
        assert!(error.contains("已回滚文件"), "{error}");
        assert!(old_abs.exists(), "SQL 失败后旧文件必须回来");
        assert!(!old_abs.with_file_name("新名.md").exists(), "SQL 失败后新文件不能留下");
        let back = conn.note(row.id).unwrap().unwrap();
        assert_eq!(back.body_path, row.body_path, "SQL 失败后数据库路径必须保持旧值");
    }

    #[test]
    fn delete_note_keeps_assets_for_the_grace_window_and_sweep_finishes_the_job() {
        let d = temp_dir("delete");
        let db_path = d.path().join("notes.db");
        let mut db = Db::open(&db_path).unwrap();
        let root = Root::open(d.path()).unwrap();
        let row = db.create_note(&root, "inbox", 7).unwrap();
        let body = root.resolve(&row.body_path).unwrap();
        std::fs::write(&body, "# 待删\n").unwrap();
        let (asset_rel, asset) = written_asset(&root, "inbox", row.id, 0xa1);
        insert_asset(&db, row.id, &asset.id, &asset.rel_path);

        let deleted = db.delete_note(&root, &row).unwrap().expect("第一次删除该命中");
        assert!(deleted.warnings.is_empty(), "{:?}", deleted.warnings);
        assert!(!body.exists(), "正文仍在数据库提交后立即删");
        assert!(root.resolve(&asset_rel).unwrap().exists(), "附件留在盘上等宽限期，不当场删");
        assert!(db.note(row.id).unwrap().is_none());
        assert_eq!(asset_count(&db, row.id), 0, "索引行仍随笔记一并清掉");
        assert!(orphan_ledger(&db, &asset_rel).is_some(), "删除要把附件退成宽限账");
        assert!(db.delete_note(&root, &row).unwrap().is_none(), "第二次删除应幂等");
        assert!(body.parent().unwrap().is_dir(), "删除单篇笔记不裁剪目录");

        let young = db.sweep_expired_assets(&root, now_ms()).unwrap();
        assert_eq!((young.kept_young, young.removed), (1, 0), "{young:?}");
        assert!(root.resolve(&asset_rel).unwrap().exists(), "宽限期内一颗都不许少");

        let aged = db.sweep_expired_assets(&root, now_ms() + ASSET_ORPHAN_GRACE_MS).unwrap();
        assert_eq!((aged.removed, aged.kept_young), (1, 0), "{aged:?}");
        assert!(root.resolve(&asset_rel).unwrap().symlink_metadata().is_err(), "过期无引用的副本该被回收");
        assert_eq!(orphan_count(&db), 0, "回收成功的账要一并结掉");
    }

    #[test]
    fn delete_note_rolls_back_db_and_keeps_files_when_sql_fails() {
        let d = temp_dir("delete-rollback");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let row = db.create_note(&root, "inbox", 7).unwrap();
        let body = root.resolve(&row.body_path).unwrap();
        std::fs::write(&body, "keep").unwrap();
        let (asset_rel, asset) = written_asset(&root, "inbox", row.id, 0xb2);
        insert_asset(&db, row.id, &asset.id, &asset.rel_path);
        db.conn.execute_batch(
            "CREATE TRIGGER reject_note_delete BEFORE DELETE ON notes \
             BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
        ).unwrap();

        assert!(db.delete_note(&root, &row).is_err());
        assert!(db.note(row.id).unwrap().is_some());
        let assets: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM note_assets WHERE note_id=?1", [row.id], |r| r.get(0),
        ).unwrap();
        assert_eq!(assets, 1);
        assert_eq!(orphan_count(&db), 0, "整趟回滚不能留下半截宽限账");
        assert!(body.exists());
        assert!(root.resolve(&asset_rel).unwrap().exists());
    }

    #[test]
    fn delete_note_rejects_asset_escape_before_committing() {
        let d = temp_dir("delete-escape");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let row = db.create_note(&root, "inbox", 7).unwrap();
        let body = root.resolve(&row.body_path).unwrap();
        insert_asset(&db, row.id, &"c3".repeat(16), "../outside.png");

        assert!(db.delete_note(&root, &row).is_err());
        assert!(db.note(row.id).unwrap().is_some());
        assert_eq!(orphan_count(&db), 0, "越界路径不能先进账再回滚");
        assert!(body.exists());
    }

    #[test]
    fn delete_note_rejects_changed_identity_before_removing_files() {
        let d = temp_dir("delete-identity");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let row = db.create_note(&root, "inbox", 7).unwrap();
        let body = root.resolve(&row.body_path).unwrap();
        std::fs::write(&body, "keep").unwrap();
        db.conn.execute("UPDATE notes SET folder_id=8 WHERE id=?1", [row.id]).unwrap();

        assert!(db.delete_note(&root, &row).unwrap_err().contains("已被别处改动"));
        assert!(body.exists());
        let current = db.note(row.id).unwrap().unwrap();
        assert_eq!(current.folder_id, 8);
    }

    #[test]
    fn delete_note_reports_file_cleanup_failures_without_restoring_rows() {
        let d = temp_dir("delete-warning");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let row = db.create_note(&root, "inbox", 7).unwrap();
        let body = root.resolve(&row.body_path).unwrap();
        std::fs::remove_file(&body).unwrap();
        std::fs::create_dir(&body).unwrap();

        let deleted = db.delete_note(&root, &row).unwrap().expect("数据库删除该提交");
        assert!(deleted.warnings.iter().any(|warning| warning.contains("正文")), "{:?}", deleted.warnings);
        assert!(db.note(row.id).unwrap().is_none());
        assert!(body.is_dir(), "删不掉的文件留作孤儿，不伪造清理成功");
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

    fn asset_file(root: &Root, note: &NoteRow, id: &str, contents: &[u8]) -> AssetRow {
        use sha2::{Digest, Sha256};
        let parent = note.body_path.rsplit_once('/').unwrap().0;
        let rel_path = format!("{parent}/a/{id}.png");
        let path = root.resolve(&rel_path).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        AssetRow {
            id: id.into(),
            note_id: note.id,
            kind: AssetKind::Image,
            rel_path,
            source_kind: SourceKind::File,
            source_ref: String::new(),
            bytes: contents.len() as u64,
            width: None,
            height: None,
            frames: None,
            duration_ms: None,
            sha256: hex_lower(&Sha256::digest(contents)),
            created_at_ms: 1,
        }
    }

    fn asset_count(db: &Db, note_id: i64) -> i64 {
        db.conn.query_row(
            "SELECT COUNT(*) FROM note_assets WHERE note_id=?1",
            [note_id],
            |row| row.get(0),
        ).unwrap()
    }

    #[test]
    fn save_records_only_assets_referenced_by_real_image_targets() {
        let d = temp_dir("asset-references");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let mut row = db.create_note(&root, "inbox", 7).unwrap();
        let referenced = asset_file(&root, &row, &"a1".repeat(16), b"referenced");
        let bare = asset_file(&root, &row, &"b2".repeat(16), b"bare");
        let lookalike = asset_file(&root, &row, &"c3".repeat(16), b"lookalike");
        std::fs::remove_file(root.resolve(&bare.rel_path).unwrap()).unwrap();
        std::fs::remove_file(root.resolve(&lookalike.rel_path).unwrap()).unwrap();
        let body = format!(
            "# 保存\n\n![图](a/{}.png)\n\n{} 和 ![像](ba/a/{}.png)",
            referenced.id, bare.id, lookalike.id,
        );

        db.save_note_with_assets(&mut row, &body, &root, &[referenced.clone(), bare, lookalike]).unwrap();
        assert_eq!(asset_count(&db, row.id), 1);
        assert_eq!(orphan_count(&db), 2, "正文没引用的那两颗要落宽限账，等扫收回收");
        let saved: String = db.conn.query_row(
            "SELECT id FROM note_assets WHERE note_id=?1",
            [row.id],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(saved, referenced.id);
        assert_eq!(db.note(row.id).unwrap().unwrap().title.as_deref(), Some("保存"));
    }

    #[test]
    fn asset_validation_failure_rolls_back_the_note_update() {
        let d = temp_dir("asset-rollback");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let mut row = db.create_note(&root, "inbox", 7).unwrap();
        let old_body = "# 旧标题";
        db.save_note(&mut row, old_body).unwrap();
        let asset = asset_file(&root, &row, &"d4".repeat(16), b"original");
        std::fs::write(root.resolve(&asset.rel_path).unwrap(), b"changed").unwrap();
        let body = format!("# 新标题\n\n![图](a/{}.png)", asset.id);

        assert!(db.save_note_with_assets(&mut row, &body, &root, &[asset]).is_err());
        let saved = db.note(row.id).unwrap().unwrap();
        assert_eq!(saved.title.as_deref(), Some("旧标题"));
        assert_eq!(saved.bytes, old_body.len() as i64);
        assert_eq!(asset_count(&db, row.id), 0);
    }

    #[test]
    fn asset_retry_is_idempotent_and_foreign_ownership_is_rejected() {
        let d = temp_dir("asset-owner");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let mut row = db.create_note(&root, "inbox", 7).unwrap();
        let asset = asset_file(&root, &row, &"e5".repeat(16), b"image");
        let body = format!("# 图\n\n![图](a/{}.png)", asset.id);
        db.save_note_with_assets(&mut row, &body, &root, &[asset.clone()]).unwrap();
        db.save_note_with_assets(&mut row, &body, &root, &[asset.clone()]).unwrap();
        assert_eq!(asset_count(&db, row.id), 1);

        let mut other = db.create_note(&root, "inbox", 8).unwrap();
        let mut foreign = asset.clone();
        foreign.note_id = other.id;
        assert!(db.save_note_with_assets(&mut other, &body, &root, &[foreign]).is_err());
        let unchanged = db.note(other.id).unwrap().unwrap();
        assert_eq!(unchanged.title, None);
        assert_eq!(unchanged.bytes, 0);
        assert_eq!(asset_count(&db, row.id), 1);
        assert_eq!(asset_count(&db, other.id), 0);
    }

    #[test]
    fn asset_path_must_stay_beside_the_saved_note() {
        let d = temp_dir("asset-path");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let mut row = db.create_note(&root, "inbox", 7).unwrap();
        let mut asset = asset_file(&root, &row, &"f6".repeat(16), b"image");
        let escaped = format!("folders/other/notes/a/{}.png", asset.id);
        let path = root.resolve(&escaped).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"image").unwrap();
        asset.rel_path = escaped;
        let body = format!("# 图\n\n![图](a/{}.png)", asset.id);

        assert!(db.save_note_with_assets(&mut row, &body, &root, &[asset]).is_err());
        assert_eq!(asset_count(&db, row.id), 0);
        assert_eq!(db.note(row.id).unwrap().unwrap().title, None);
    }

    #[test]
    fn save_books_a_dereferenced_asset_without_touching_the_file() {
        let d = temp_dir("asset-orphan");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let mut row = db.create_note(&root, "inbox", 7).unwrap();
        let asset = asset_file(&root, &row, &"a7".repeat(16), b"bytes");
        db.save_note_with_assets(
            &mut row,
            &format!("# 图\n\n![图](a/{}.png)", asset.id),
            &root,
            &[asset.clone()],
        ).unwrap();
        assert_eq!(asset_count(&db, row.id), 1);
        assert_eq!(orphan_count(&db), 0, "还在用的那颗不该进宽限账");

        db.save_note_with_assets(&mut row, "# 图\n\n正文里没图了", &root, &[asset.clone()]).unwrap();
        assert_eq!(asset_count(&db, row.id), 0, "失去引用的索引行要退账");
        assert!(orphan_ledger(&db, &asset.rel_path).is_some(), "退账不等于删文件");
        assert!(root.resolve(&asset.rel_path).unwrap().exists());

        // 首次失引时刻只认一次：反复保存不许给这一格续期。
        let first = orphan_ledger(&db, &asset.rel_path).unwrap();
        db.save_note_with_assets(&mut row, "# 图\n\n正文里没图了", &root, &[asset.clone()]).unwrap();
        assert_eq!(orphan_ledger(&db, &asset.rel_path).unwrap(), first, "重复失引不续期");
    }

    #[test]
    fn sweep_only_removes_expired_unreferenced_plugin_copies() {
        let d = temp_dir("sweep");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let mut live = db.create_note(&root, "inbox", 7).unwrap();
        let referenced = asset_file(&root, &live, &"11".repeat(16), b"in-use");
        // 正文引用它、索引行却没有：只有"读那一夹公开正文"这一条护得住。
        let body_only = asset_file(&root, &live, &"99".repeat(16), b"body-only");
        // 扫收读的是盘上正文（正式链路里会话先写盘，适配层再落索引）。
        let disk_body = format!("# 活的\n\n![图](a/{}.png)\n![二](a/{}.png)", referenced.id, body_only.id);
        std::fs::write(root.resolve(&live.body_path).unwrap(), &disk_body).unwrap();
        db.save_note_with_assets(&mut live, &disk_body, &root, &[referenced.clone()]).unwrap();
        // 索引行还在、正文已经不再引用：只有 `note_assets` 那一条护得住。
        let indexed_only = asset_file(&root, &live, &"88".repeat(16), b"indexed-only");
        insert_asset(&db, live.id, &indexed_only.id, &indexed_only.rel_path);
        let now = now_ms();
        let later = now + ASSET_ORPHAN_GRACE_MS + 60_000;

        // ① 过期、无引用、形状合法 ⇒ 该回收。
        let (expired_rel, _) = written_asset(&root, "inbox", live.id, 0x22);
        book_orphan(&db, &expired_rel, now - ASSET_ORPHAN_GRACE_MS - 1_000);
        // ② 宽限期没过 ⇒ 一颗都不许少。
        let (young_rel, _) = written_asset(&root, "inbox", live.id, 0x33);
        book_orphan(&db, &young_rel, now + ASSET_ORPHAN_GRACE_MS / 2);
        // ③ 旧版本遗留：盘上有、账上没有，年龄回落到盘上时间。
        let (bare_rel, _) = written_asset(&root, "inbox", live.id, 0x44);
        // ④ 认不出的名字与占着那个位置的目录：只计数、不碰。
        let stray = root.resolve("folders/inbox/notes/a/stray.txt").unwrap();
        std::fs::write(&stray, b"stray").unwrap();
        let boxed = root.resolve(&format!("folders/inbox/notes/a/{}.png", "55".repeat(16))).unwrap();
        std::fs::create_dir_all(&boxed).unwrap();
        // ⑤ 账里躺着一格形状不合法的：既不删文件也不销账。
        let ragged_rel = "folders/inbox/notes/a/not-hex.png".to_string();
        let ragged = root.resolve(&ragged_rel).unwrap();
        std::fs::write(&ragged, b"ragged").unwrap();
        book_orphan(&db, &ragged_rel, now - ASSET_ORPHAN_GRACE_MS - 1_000);

        let report = db.sweep_expired_assets(&root, later).unwrap();
        assert_eq!(
            report,
            AssetSweepReport {
                removed: 2,
                kept_young: 1,
                referenced: 3,
                unrecognized: 3,
                refused: 1,
                failed: 0,
            },
            "{report:?}"
        );
        let gone = |rel: &str| root.resolve(rel).unwrap().symlink_metadata().is_err();
        assert!(gone(&expired_rel), "过期无引用的副本该被回收");
        assert!(gone(&bare_rel), "盘上时间也算得出年龄");
        assert!(!gone(&young_rel), "宽限期内的副本必须还在");
        assert!(!gone(&referenced.rel_path), "仍被正文与索引引用的副本不能动");
        assert!(!gone(&body_only.rel_path), "正文引用、索引没落行的那颗也得护住");
        assert!(!gone(&indexed_only.rel_path), "索引有行、正文已不引用的那颗也得护住");
        assert!(stray.exists(), "认不出的名字只计数");
        assert!(boxed.is_dir(), "占着附件名的目录不是我们的副本");
        assert!(ragged.exists(), "坏形状既不删也不销账");
        assert_eq!(orphan_ledger(&db, &young_rel), Some(now + ASSET_ORPHAN_GRACE_MS / 2));
        assert!(orphan_ledger(&db, &ragged_rel).is_some());
        assert_eq!(orphan_count(&db), 2, "回收成功与仍年轻的结了账，剩下的留着");

        let second = db.sweep_expired_assets(&root, later).unwrap();
        assert_eq!(second.removed, 0, "同一趟扫收不该重复计数");
        assert_eq!(second.refused, 1, "坏形状那一格每趟都会如实报");
    }

    #[test]
    fn sweep_clears_the_ledger_row_and_a_stale_index_row_together() {
        let d = temp_dir("sweep-stale");
        let mut db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let row = db.create_note(&root, "inbox", 7).unwrap();
        let (rel, asset) = written_asset(&root, "inbox", row.id, 0x66);
        // 旧版本形状：笔记行早没了，索引行与文件都还留着，也没落账。
        db.conn.execute("DELETE FROM notes WHERE id=?1", [row.id]).unwrap();
        insert_asset(&db, asset.note_id, &asset.id, &asset.rel_path);

        let report = db.sweep_expired_assets(&root, now_ms() + ASSET_ORPHAN_GRACE_MS + 60_000).unwrap();
        assert_eq!((report.removed, report.referenced), (1, 0), "{report:?}");
        assert!(root.resolve(&rel).unwrap().symlink_metadata().is_err());
        assert_eq!(asset_count(&db, asset.note_id), 0, "文件回收后残留索引行一并清掉");
        assert_eq!(orphan_count(&db), 0);
    }

    #[cfg(windows)]
    #[test]
    fn sweep_keeps_the_ledger_when_the_file_refuses_to_go_away() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = temp_dir("sweep-failed");
        let db = Db::open(&d.path().join("notes.db")).unwrap();
        let root = Root::open(d.path()).unwrap();
        let (rel, _) = written_asset(&root, "inbox", 7, 0x77);
        book_orphan(&db, &rel, now_ms() - ASSET_ORPHAN_GRACE_MS - 1_000);
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(root.resolve(&rel).unwrap())
            .unwrap();

        let report = db.sweep_expired_assets(&root, now_ms()).unwrap();
        assert_eq!((report.failed, report.removed), (1, 0), "{report:?}");
        assert!(orphan_ledger(&db, &rel).is_some(), "删不掉要留账，下一趟再试");
        assert!(root.resolve(&rel).unwrap().exists());

        drop(held);
        let retry = db.sweep_expired_assets(&root, now_ms()).unwrap();
        assert_eq!((retry.removed, retry.failed), (1, 0), "{retry:?}");
        assert_eq!(orphan_count(&db), 0);
    }

    #[test]
    fn image_reference_parser_accepts_exact_targets_and_rejects_lookalikes() {
        let asset = AssetRow {
            id: "ab".repeat(16),
            note_id: 1,
            kind: AssetKind::Image,
            rel_path: format!("folders/inbox/notes/a/{}.png", "ab".repeat(16)),
            source_kind: SourceKind::File,
            source_ref: String::new(),
            bytes: 1,
            width: None,
            height: None,
            frames: None,
            duration_ms: None,
            sha256: "cd".repeat(32),
            created_at_ms: 1,
        };
        let target = format!("a/{}.png", asset.id);
        for body in [
            format!("![图]({target})"),
            format!("![图](<{target}>)"),
            format!("![图](./{target}?size=2#preview)"),
            format!("<img src=\"./{target}?x=1&amp;y=2\">"),
        ] {
            assert!(body_references_asset(&body, &asset), "{body}");
        }
        for body in [
            asset.id.clone(),
            format!("`![图]({target})`"),
            format!("[链接]({target})"),
            format!("![图](ba/{target})"),
            format!("![图](foo/{target})"),
            format!("![图](a/{}.jpg)", asset.id),
        ] {
            assert!(!body_references_asset(&body, &asset), "{body}");
        }
    }
}
