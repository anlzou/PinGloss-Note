//! 控制面那一趟的字节形状：落点怎么定、一行请求长什么样、一行答复怎么分诊。
//!
//! 本模块不起线程、不握句柄，也不拨号——连接、代次与缓存归连接那一层。它只保证一件事：
//! **从 Nomo 发出去的每一行、和对收回来的每一行的读法，都和宿主实际认的那一份是同一套。**
//!
//! ⚠ 默认落点的拼法逐字照抄宿主 `pg_core::ipc::socket_path_named`：漂一位的失败形不是报错，
//! 而是"永远连不上、屏上只显示离线"，那种失败在本机判据里看不见。

use std::ffi::OsStr;
use std::path::PathBuf;

use serde_json::{Map, Value};

/// Nomo 侧私有覆盖那一位。宿主自己从不读它，它只是这一侧的私有落点约定，用来把判据指到
/// 一颗私有管子上。
pub(crate) const ENV_CONTROL_SOCKET: &str = "PINGLOSS_CONTROL_SOCKET";

/// 宿主控制面默认文件名，与宿主 `PLUGIN_SOCKET_FILE` 同一个串。
pub(crate) const SOCKET_FILE: &str = "plugin.sock";

/// Windows 默认管名里的词干：宿主拿 `SOCKET_FILE` 去掉 `.sock` 拼名，这里跟着同一位。
const PIPE_STEM: &str = "plugin";

/// Windows named pipe 的前缀。
const PIPE_PREFIX: &str = r"\\.\pipe\pingloss-";

/// unix 默认名里那一层会话目录的段名。
const RUNTIME_DIR: &str = "pingloss";

/// 读回答复的字节上限。⚠ 它与数据面 `plugin::frame::MAX_FRAME_BYTES` 的 1 MiB **不是一个数**：
/// 1 MiB 是宿主对**它自己入站**那一向 enforced 的位，而它写回的答复可以到 4 MiB（宿主自家客户端
/// 就按 4 MiB 读）。拿数据面那把尺读控制面，症状是合法长答复被当成对端跑飞而断线。
pub(crate) const REPLY_MAX_BYTES: usize = 4 << 20;

/// `notes_roots`／`notes_children` 的页长上限，取宿主 `MAX_NOTES_LIMIT`。
pub(crate) const MAX_PAGE_LIMIT: u32 = 500;

/// `notes_recent` 的页长上限，取宿主 `MAX_NOTES_RECENT_LIMIT`；比页的那一位低一档。
pub(crate) const MAX_RECENT_LIMIT: u32 = 200;

/// 宿主 `request_id_at` 的那一把尺。
const MAX_REQUEST_ID_BYTES: usize = 128;

/// 宿主 `cursor_at` 的那一把尺：游标对 Nomo 不透明，只许原样带回去。
const MAX_CURSOR_BYTES: usize = 1024;

/// 宿主收下的六条 notes 命令的线上名，顺序与宿主命令名单里那一段一致。
pub(crate) const NOTES_COMMANDS: [&str; 6] = [
    "notes_roots",
    "notes_children",
    "notes_recent",
    "notes_open",
    "notes_remove_recent",
    "notes_clear_recent",
];

/// 一条 notes 命令。位名与宿主 `Command` 那六个变体一一对应：`cmd`＋`request_id` 两位必带。
/// ⚠ 线上没有 `provider_id` 这一位——宿主只有一家 notes，多带顶层字段是整行拒收。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NotesRequest {
    Roots {
        request_id: String,
        cursor: Option<String>,
        limit: Option<u32>,
    },
    Children {
        request_id: String,
        folder_id: i64,
        cursor: Option<String>,
        limit: Option<u32>,
    },
    Recent {
        request_id: String,
        limit: Option<u32>,
    },
    Open {
        request_id: String,
        note_id: i64,
        folder_id: i64,
    },
    RemoveRecent {
        request_id: String,
        note_id: i64,
    },
    ClearRecent {
        request_id: String,
    },
}

/// 发出去之前在自己家门口挡下的坏形状。宿主对这些是**整行拒收**——先报出来才能把"屏幕上什么都没
/// 发生"归到自己这一侧，而不是等一条永远不会来的答复。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestError {
    RequestId,
    Cursor,
    Limit,
    Id,
}

impl NotesRequest {
    pub(crate) fn cmd(&self) -> &'static str {
        match self {
            Self::Roots { .. } => NOTES_COMMANDS[0],
            Self::Children { .. } => NOTES_COMMANDS[1],
            Self::Recent { .. } => NOTES_COMMANDS[2],
            Self::Open { .. } => NOTES_COMMANDS[3],
            Self::RemoveRecent { .. } => NOTES_COMMANDS[4],
            Self::ClearRecent { .. } => NOTES_COMMANDS[5],
        }
    }

    pub(crate) fn request_id(&self) -> &str {
        match self {
            Self::Roots { request_id, .. }
            | Self::Children { request_id, .. }
            | Self::Recent { request_id, .. }
            | Self::Open { request_id, .. }
            | Self::RemoveRecent { request_id, .. }
            | Self::ClearRecent { request_id } => request_id,
        }
    }

    fn cursor(&self) -> Option<&String> {
        match self {
            Self::Roots { cursor, .. } | Self::Children { cursor, .. } => cursor.as_ref(),
            _ => None,
        }
    }

    fn limit(&self) -> Option<u32> {
        match self {
            Self::Roots { limit, .. } | Self::Children { limit, .. } => *limit,
            Self::Recent { limit, .. } => *limit,
            _ => None,
        }
    }

    fn folder_id(&self) -> Option<i64> {
        match self {
            Self::Children { folder_id, .. } | Self::Open { folder_id, .. } => Some(*folder_id),
            _ => None,
        }
    }

    fn note_id(&self) -> Option<i64> {
        match self {
            Self::Open { note_id, .. } | Self::RemoveRecent { note_id, .. } => Some(*note_id),
            _ => None,
        }
    }

    /// 这一条命令带的页长上限：`notes_recent` 是 200，带页长的其余三条是 500。
    fn limit_ceiling(&self) -> u32 {
        match self {
            Self::Recent { .. } => MAX_RECENT_LIMIT,
            _ => MAX_PAGE_LIMIT,
        }
    }

    /// 一行请求的字节，**含结尾换行**：宿主按行分帧，一行一条是它的合同。
    pub(crate) fn to_line(&self) -> Result<String, RequestError> {
        if !request_id_ok(self.request_id()) {
            return Err(RequestError::RequestId);
        }
        let mut obj = Map::new();
        obj.insert("cmd".to_string(), Value::String(self.cmd().to_string()));
        obj.insert(
            "request_id".to_string(),
            Value::String(self.request_id().to_string()),
        );
        if let Some(cursor) = self.cursor() {
            if !cursor_ok(cursor) {
                return Err(RequestError::Cursor);
            }
            obj.insert("cursor".to_string(), Value::String(cursor.clone()));
        }
        if let Some(limit) = self.limit() {
            if limit == 0 || limit > self.limit_ceiling() {
                return Err(RequestError::Limit);
            }
            obj.insert("limit".to_string(), Value::from(limit));
        }
        for (field, id) in [("folder_id", self.folder_id()), ("note_id", self.note_id())] {
            match id {
                Some(id) if id > 0 => {
                    obj.insert(field.to_string(), Value::from(id));
                }
                Some(_) => return Err(RequestError::Id),
                None => {}
            }
        }
        let mut line = serde_json::to_string(&Value::Object(obj)).expect("自家拼的 Map 序列化不会失败");
        line.push('\n');
        Ok(line)
    }
}

/// 与宿主同一把尺：非空、≤128 字节、只允许 ASCII 字母数字与 `-`／`_`。
fn request_id_ok(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_REQUEST_ID_BYTES
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn cursor_ok(raw: &str) -> bool {
    !raw.is_empty() && raw.len() <= MAX_CURSOR_BYTES
}

/// 一行答复的分诊结果。读不出形状的行只交 `Garbage`，不猜：连接去留归连接那一层判。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Triage {
    /// 某条命令办成了：带着回显的 `request_id` 与那一格 payload。
    Answer { request_id: String, payload: Value },
    /// 宿主明确说这条没办成：`field`／`code`／`message` 三位原文。
    Refused {
        request_id: String,
        field: String,
        code: String,
        message: String,
    },
    /// 宿主主动推的一行（`status`／`notes_changed`）：它不是任何请求的答复，不能拿去配对。
    /// `folder`／`kind` 只在变更那一发上有值——宿主把载荷三格与标签同层写，缺位或不是正整数就交
    /// `None`。`status` 那一发压根不带这两位。
    Announce {
        event: String,
        folder: Option<i64>,
        kind: Option<String>,
    },
    Garbage(&'static str),
}

pub(crate) fn triage(line: &str) -> Triage {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Triage::Garbage("不是合法 JSON");
    };
    let Value::Object(obj) = value else {
        return Triage::Garbage("顶层不是对象");
    };
    if let Some(event) = obj.get("event") {
        let Some(event) = event.as_str() else {
            return Triage::Garbage("event 位不是字符串");
        };
        return Triage::Announce {
            event: event.to_string(),
            folder: obj.get("folder").and_then(Value::as_i64).filter(|id| *id > 0),
            kind: obj.get("kind").and_then(Value::as_str).map(str::to_string),
        };
    }
    let Some(request_id) = obj.get("request_id").and_then(Value::as_str) else {
        return Triage::Garbage("答复没回显 request_id");
    };
    if request_id.is_empty() {
        return Triage::Garbage("回显的 request_id 是空串");
    }
    let request_id = request_id.to_string();
    match obj.get("ok") {
        Some(Value::Bool(true)) => match obj.get("payload") {
            Some(payload @ Value::Object(_)) => Triage::Answer {
                request_id,
                payload: payload.clone(),
            },
            _ => Triage::Garbage("办成那一支缺 payload 对象"),
        },
        Some(Value::Bool(false)) => {
            let Some(Value::Object(error)) = obj.get("error") else {
                return Triage::Garbage("没办成那一支缺 error 对象");
            };
            let text = |key: &str| {
                error
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            Triage::Refused {
                request_id,
                field: text("field"),
                code: text("code"),
                message: text("message"),
            }
        }
        Some(_) => Triage::Garbage("ok 位不是布尔"),
        None => Triage::Garbage("缺 ok 位"),
    }
}

/// 覆盖值：非空就原样用。空串在宿主那一侧是报错而不是"回落默认"，这里同一口径——
/// "我明明设了却没生效"最省钱的读法就是当场报出来。
pub(crate) fn override_endpoint(raw: &OsStr) -> Result<PathBuf, EndpointError> {
    if raw.is_empty() {
        return Err(EndpointError::EmptyOverride);
    }
    Ok(PathBuf::from(raw))
}

/// unix 默认名：`$XDG_RUNTIME_DIR/pingloss/plugin.sock`，那位没有就退 `/tmp/pingloss-<uid>/plugin.sock`。
pub(crate) fn unix_default(xdg: Option<&OsStr>, uid: u32) -> PathBuf {
    match xdg.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join(RUNTIME_DIR).join(SOCKET_FILE),
        None => PathBuf::from(format!("/tmp/{RUNTIME_DIR}-{uid}")).join(SOCKET_FILE),
    }
}

/// Windows 默认管名：`\\.\pipe\pingloss-plugin-<SID>`。SID 拿不到就是定不出落点，不猜一个名字。
pub(crate) fn windows_default(sid: Result<&str, String>) -> Result<PathBuf, EndpointError> {
    match sid {
        Ok(sid) => Ok(PathBuf::from(format!("{PIPE_PREFIX}{PIPE_STEM}-{sid}"))),
        Err(reason) => Err(EndpointError::NoSid(reason)),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EndpointError {
    /// 覆盖位被设成空串：要么给绝对路径，要么别设。
    EmptyOverride,
    /// 拿不到当前用户的 SID，因而定不出默认管名。
    NoSid(String),
    /// 这个平台上宿主控制面没有默认落点。
    NoDefault(&'static str),
}

impl EndpointError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::EmptyOverride => {
                format!("{ENV_CONTROL_SOCKET} 被设成空串：要么给绝对路径，要么别设")
            }
            Self::NoSid(reason) => format!("取当前用户的 SID 失败，定不出控制面默认名：{reason}"),
            Self::NoDefault(reason) => (*reason).to_string(),
        }
    }
}

/// 这一趟要拨的那颗：覆盖位有值就用它，否则按宿主同一套拼法定默认名。
pub(crate) fn control_endpoint() -> Result<PathBuf, EndpointError> {
    match std::env::var_os(ENV_CONTROL_SOCKET) {
        Some(raw) => override_endpoint(&raw),
        None => default_endpoint(),
    }
}

fn default_endpoint() -> Result<PathBuf, EndpointError> {
    #[cfg(unix)]
    {
        Ok(unix_default(
            std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            owner_uid(),
        ))
    }
    #[cfg(windows)]
    {
        match current_sid() {
            Ok(sid) => windows_default(Ok(sid.as_str())),
            Err(reason) => windows_default(Err(reason)),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(EndpointError::NoDefault(
            "这个平台没有宿主控制面的默认落点，请指定覆盖位那一颗",
        ))
    }
}

/// unix 侧那个 uid：与宿主同一取法（拿 `$HOME` 的属主），拿不到退 0。
#[cfg(unix)]
fn owner_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::env::var_os("HOME")
        .and_then(|home| std::fs::metadata(home).ok())
        .map(|meta| meta.uid())
        .unwrap_or(0)
}

/// Windows 侧那个 SID 字面串。⚠ 与宿主同一趟系统调用：令牌先问长度、再按长度取、最后翻成字面串；
/// `GetCurrentProcess` 就是 `-1` 那个宏，不是导入表里的函数。
#[cfg(windows)]
fn current_sid() -> Result<String, String> {
    use std::ffi::c_void;

    const TOKEN_QUERY: u32 = 0x0008;
    const TOKEN_USER: u32 = 1;
    const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

    #[repr(C)]
    struct TokenUser {
        sid: *mut c_void,
        _statistics: [usize; 2],
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
        fn GetTokenInformation(
            token: *mut c_void,
            class: u32,
            buffer: *mut c_void,
            length: u32,
            return_length: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn LocalFree(ptr: *mut c_void) -> *mut c_void;
    }

    let me = -1isize as *mut c_void;
    let mut token: *mut c_void = core::ptr::null_mut();
    if unsafe { OpenProcessToken(me, TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut need = 0u32;
    let probe =
        unsafe { GetTokenInformation(token, TOKEN_USER, core::ptr::null_mut(), 0, &mut need) };
    if probe == 0
        && std::io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER)
    {
        unsafe { CloseHandle(token) };
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut raw = vec![0u8; need as usize];
    let taken = unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER,
            raw.as_mut_ptr() as *mut c_void,
            need,
            &mut need,
        )
    };
    unsafe { CloseHandle(token) };
    if taken == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let sid_pointer = unsafe { &*(raw.as_ptr() as *const TokenUser) }.sid;
    let mut text: *mut u16 = core::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid_pointer, &mut text) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut len = 0usize;
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    let units = unsafe { std::slice::from_raw_parts(text, len) };
    let sid = String::from_utf16_lossy(units);
    unsafe { LocalFree(text as *mut c_void) };
    Ok(sid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(line: &str) -> Value {
        serde_json::from_str(line.trim_end()).expect("测试自己拼的那一行要能读回")
    }

    fn names(line: &str) -> String {
        let parsed = value(line);
        let Some(obj) = parsed.as_object() else {
            panic!("顶层不是对象");
        };
        let mut names: Vec<&str> = obj.keys().map(String::as_str).collect();
        names.sort();
        names.join(",")
    }

    fn root(request_id: &str) -> NotesRequest {
        NotesRequest::Roots {
            request_id: request_id.to_string(),
            cursor: None,
            limit: None,
        }
    }

    /// 默认落点必须是宿主那套拼法逐字重演：漂一位的失败形是"永远连不上"，本机判据看不见。
    #[test]
    fn the_default_endpoint_replays_the_host_socket_recipe() {
        assert_eq!(
            unix_default(Some(OsStr::new("/run/user/1000")), 1000),
            PathBuf::from("/run/user/1000/pingloss/plugin.sock")
        );
        assert_eq!(
            unix_default(None, 1000),
            PathBuf::from("/tmp/pingloss-1000/plugin.sock")
        );
        assert_eq!(
            unix_default(Some(OsStr::new("")), 7),
            PathBuf::from("/tmp/pingloss-7/plugin.sock"),
            "空的 XDG 那位是'没有'，不是覆盖"
        );
        assert_eq!(
            windows_default(Ok("S-1-5-21-1-2-3-1001")).expect("有 SID 就该定得出名字"),
            PathBuf::from(r"\\.\pipe\pingloss-plugin-S-1-5-21-1-2-3-1001")
        );
        // 词干由那一个文件名派生，不是第二份字面量：两处漂移就是一条连不上的默认名。
        assert_eq!(SOCKET_FILE, format!("{PIPE_STEM}.sock"));
        assert!(windows_default(Err("拿不到".to_string())).is_err());
    }

    #[test]
    fn the_override_is_verbatim_and_an_empty_one_is_not_a_default() {
        assert_eq!(
            override_endpoint(OsStr::new("/tmp/private/control.sock")).expect("非空就该原样用"),
            PathBuf::from("/tmp/private/control.sock")
        );
        assert_eq!(
            override_endpoint(OsStr::new("")),
            Err(EndpointError::EmptyOverride),
            "空串被当成'用默认值'就会让人明明设了却没生效"
        );
    }

    /// 这一侧只认自己那一位私有覆盖名。宿主 spawn 插件时塞进来的那一位装的是数据面那颗的名字，
    /// 读来当控制面就会拨到自己家的管子上——那条线永远不会有答复。
    ///
    /// 扫的是产品那一截：判据自己写的靶串不算证据，也不算污染。
    #[test]
    fn only_the_private_control_variable_names_the_endpoint() {
        let code = include_str!("control.rs")
            .split_once("#[cfg(test)]")
            .expect("测试那一截还在原位")
            .0
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            code.contains(ENV_CONTROL_SOCKET)
                && code.contains(&format!("\"{ENV_CONTROL_SOCKET}\"")),
            "覆盖位不叫这个名字了"
        );
        assert!(
            !code.contains("PINGLOSS_PLUGIN_SOCKET"),
            "控制面读了数据面那一位覆盖名"
        );
        assert!(code.contains("\"plugin.sock\""), "默认名不再由那一个文件名拼出");
    }

    #[test]
    fn each_request_line_carries_exactly_the_fields_the_host_reads() {
        let cases = [
            (
                NotesRequest::Roots {
                    request_id: "r1".into(),
                    cursor: None,
                    limit: Some(20),
                },
                "cmd,limit,request_id",
            ),
            (
                NotesRequest::Children {
                    request_id: "r2".into(),
                    folder_id: 7,
                    cursor: Some("abc".into()),
                    limit: None,
                },
                "cmd,cursor,folder_id,request_id",
            ),
            (
                NotesRequest::Recent {
                    request_id: "r3".into(),
                    limit: None,
                },
                "cmd,request_id",
            ),
            (
                NotesRequest::Open {
                    request_id: "r4".into(),
                    note_id: 11,
                    folder_id: 7,
                },
                "cmd,folder_id,note_id,request_id",
            ),
            (
                NotesRequest::RemoveRecent {
                    request_id: "r5".into(),
                    note_id: 11,
                },
                "cmd,note_id,request_id",
            ),
            (
                NotesRequest::ClearRecent {
                    request_id: "r6".into(),
                },
                "cmd,request_id",
            ),
        ];
        assert_eq!(cases.len(), NOTES_COMMANDS.len());
        for (index, (request, expected)) in cases.into_iter().enumerate() {
            let line = request.to_line().expect("这几颗都是合法形状");
            assert!(line.ends_with('\n'), "一行一条是宿主的分帧合同");
            assert_eq!(names(&line), expected, "{} 的位不合", request.cmd());
            assert_eq!(request.cmd(), NOTES_COMMANDS[index]);
            assert_eq!(value(&line)["request_id"], request.request_id());
        }
    }

    #[test]
    fn the_host_never_sees_a_body_or_provider_field_on_the_wire() {
        // 宿主对未知顶层字段整行拒收；正文与私密标题则压根不该出这一侧的门。
        for request in [
            root("r"),
            NotesRequest::Open {
                request_id: "r".into(),
                note_id: 1,
                folder_id: 1,
            },
            NotesRequest::Recent {
                request_id: "r".into(),
                limit: Some(MAX_RECENT_LIMIT),
            },
        ] {
            let text = request.to_line().expect("形状对");
            for absent in ["provider", "body_path", "path", "title", "locked"] {
                assert!(!text.contains(absent), "{absent} 被带上了控制面");
            }
        }
    }

    #[test]
    fn request_ids_are_gated_before_the_line_leaves() {
        let bad: [String; 5] = [
            String::new(),
            "a".repeat(MAX_REQUEST_ID_BYTES + 1),
            "has space".into(),
            "dot.dot".into(),
            "中文".into(),
        ];
        for raw in bad {
            assert_eq!(
                NotesRequest::ClearRecent { request_id: raw.clone() }.to_line(),
                Err(RequestError::RequestId),
                "这一形宿主会整行拒收：{raw:?}"
            );
        }
        let good = vec![
            "a".to_string(),
            "A1-b_2".to_string(),
            "x".repeat(MAX_REQUEST_ID_BYTES),
        ];
        for raw in good {
            assert!(
                NotesRequest::ClearRecent {
                    request_id: raw.clone(),
                }
                .to_line()
                .is_ok(),
                "{raw:?} 是宿主认得的形状"
            );
        }
    }

    #[test]
    fn limits_and_cursors_stay_inside_each_ones_own_ceiling() {
        let page = |limit: u32| NotesRequest::Roots {
            request_id: "r".into(),
            cursor: None,
            limit: Some(limit),
        };
        assert!(page(MAX_PAGE_LIMIT).to_line().is_ok());
        assert_eq!(page(MAX_PAGE_LIMIT + 1).to_line(), Err(RequestError::Limit));
        assert_eq!(page(0).to_line(), Err(RequestError::Limit));

        // recent 的上限比页的那一位低一档：拿页那把尺放过去就会被宿主拒收。
        let recent = |limit: u32| NotesRequest::Recent {
            request_id: "r".into(),
            limit: Some(limit),
        };
        assert!(recent(MAX_RECENT_LIMIT).to_line().is_ok());
        assert_eq!(recent(MAX_RECENT_LIMIT + 1).to_line(), Err(RequestError::Limit));
        assert!(MAX_RECENT_LIMIT < MAX_PAGE_LIMIT);

        let children = |cursor: String| NotesRequest::Children {
            request_id: "r".into(),
            folder_id: 1,
            cursor: Some(cursor),
            limit: None,
        };
        assert!(children("c".repeat(MAX_CURSOR_BYTES)).to_line().is_ok());
        assert_eq!(
            children("c".repeat(MAX_CURSOR_BYTES + 1)).to_line(),
            Err(RequestError::Cursor)
        );
        assert_eq!(children(String::new()).to_line(), Err(RequestError::Cursor));
    }

    #[test]
    fn non_positive_ids_are_refused_before_send() {
        for id in [0, -1] {
            assert_eq!(
                NotesRequest::Open {
                    request_id: "r".into(),
                    note_id: id,
                    folder_id: 3,
                }
                .to_line(),
                Err(RequestError::Id),
                "note_id={id} 宿主会整行拒收"
            );
            assert_eq!(
                NotesRequest::Children {
                    request_id: "r".into(),
                    folder_id: id,
                    cursor: None,
                    limit: None,
                }
                .to_line(),
                Err(RequestError::Id),
                "folder_id={id} 宿主会整行拒收"
            );
        }
    }

    #[test]
    fn an_answer_is_read_by_its_echoed_request_id() {
        match triage(r#"{"ok":true,"payload":{"items":[]},"request_id":"r9"}"#) {
            Triage::Answer { request_id, payload } => {
                assert_eq!(request_id, "r9");
                assert_eq!(payload["items"], Value::Array(Vec::new()));
            }
            other => panic!("办成那一支读成了 {other:?}"),
        }
        // 宿主只在 notes 系命令上回显 request_id；缺位就是配不上号，不能当"随便一条答复"。
        assert_eq!(
            triage(r#"{"ok":true,"payload":{}}"#),
            Triage::Garbage("答复没回显 request_id")
        );
        assert_eq!(
            triage(r#"{"ok":true,"payload":{},"request_id":""}"#),
            Triage::Garbage("回显的 request_id 是空串")
        );
        // 不带 payload 的那一支是宿主管理窗用的 ok，Nomo 没发过那种命令，收到就当形状不对。
        assert_eq!(
            triage(r#"{"ok":true,"request_id":"r9"}"#),
            Triage::Garbage("办成那一支缺 payload 对象")
        );
    }

    #[test]
    fn a_refusal_keeps_the_three_fields_the_host_writes() {
        match triage(
            r#"{"ok":false,"request_id":"r8","error":{"field":"cursor","message":"游标坏了","code":"bad_request"}}"#,
        ) {
            Triage::Refused {
                request_id,
                field,
                code,
                message,
            } => {
                assert_eq!((request_id.as_str(), field.as_str()), ("r8", "cursor"));
                assert_eq!(code, "bad_request");
                assert_eq!(message, "游标坏了");
            }
            other => panic!("没办成那一支读成了 {other:?}"),
        }
        // error 里多出来的位忽略即可：宿主加一位不该让整行读不回。
        match triage(
            r#"{"ok":false,"request_id":"r8","error":{"field":"limit","message":"m","code":"busy","extra":1}}"#,
        ) {
            Triage::Refused { code, .. } => assert_eq!(code, "busy"),
            other => panic!("多一位把整行读坏了：{other:?}"),
        }
        assert_eq!(
            triage(r#"{"ok":false,"request_id":"r8"}"#),
            Triage::Garbage("没办成那一支缺 error 对象")
        );
    }

    #[test]
    fn an_event_line_is_never_an_answer() {
        match triage(r#"{"event":"notes_changed","folder":3,"note_id":9,"kind":"saved"}"#) {
            Triage::Announce { event, folder, kind } => {
                assert_eq!(event, "notes_changed");
                assert_eq!(folder, Some(3));
                assert_eq!(kind.as_deref(), Some("saved"));
            }
            other => panic!("变更那一发读成了 {other:?}"),
        }
        // status 那一发压根不带这两位：把它们当成"整屏作废"会白重问一遍目录。
        match triage(r#"{"event":"status","plugins":[]}"#) {
            Triage::Announce { event, folder, kind } => {
                assert_eq!(event, "status");
                assert_eq!(folder, None);
                assert_eq!(kind, None);
            }
            other => panic!("状态那一发读成了 {other:?}"),
        }
        assert_eq!(
            triage(r#"{"event":7,"ok":true,"request_id":"r","payload":{}}"#),
            Triage::Garbage("event 位不是字符串")
        );
    }

    /// `folder` 只认正整数：宿主对字符串那一位是整条拒收的（`plugins/proxy.rs` 的变更闸），
    /// 读成 `None` 比猜一枚号安全——猜中的症状是把别家目录那一格作废掉。
    #[test]
    fn an_announce_only_believes_a_positive_numeric_folder() {
        let read = |line: &str| match triage(line) {
            Triage::Announce { folder, .. } => folder,
            other => panic!("这一行该是推送：{other:?}"),
        };
        for stray in [
            r#"{"event":"notes_changed"}"#,
            r#"{"event":"notes_changed","folder":0,"kind":"saved"}"#,
            r#"{"event":"notes_changed","folder":-3,"kind":"saved"}"#,
            r#"{"event":"notes_changed","folder":"7","kind":"saved"}"#,
        ] {
            assert_eq!(read(stray), None, "{stray} 里那一位不该被当成文件夹");
        }
        assert_eq!(
            read(r#"{"event":"notes_changed","folder":7,"kind":"deleted"}"#),
            Some(7)
        );
        // 种类缺位照样交 None：种类只用来多作废首页那一格，缺它不影响目录。
        match triage(r#"{"event":"notes_changed","folder":7}"#) {
            Triage::Announce { kind, .. } => assert_eq!(kind, None),
            other => panic!("缺种类那一发行读成了 {other:?}"),
        }
    }

    #[test]
    fn a_line_that_is_not_a_reply_shape_is_garbage_not_a_guess() {
        for (line, reason) in [
            ("", "不是合法 JSON"),
            ("hello", "不是合法 JSON"),
            ("[1,2]", "顶层不是对象"),
            (r#"{"request_id":"r"}"#, "缺 ok 位"),
            (r#"{"ok":"yes","request_id":"r","payload":{}}"#, "ok 位不是布尔"),
            (r#"{"ok":true,"payload":7,"request_id":"r"}"#, "办成那一支缺 payload 对象"),
        ] {
            assert_eq!(triage(line), Triage::Garbage(reason), "{line:?} 读成了别的形状");
        }
    }

    /// 读侧那一格与宿主自家客户端同值，且比宿主**入站**那一向宽一档：拿 1 MiB 去读控制面，合法长答复
    /// 会被判成对端跑飞。攒行与越界的实况由 `plugin::frame`、`plugin::transport` 各测一次。
    #[test]
    fn the_reply_cap_is_wider_than_the_frame_the_host_reads() {
        assert_eq!(REPLY_MAX_BYTES, 4 << 20);
        assert!(REPLY_MAX_BYTES > crate::plugin::frame::MAX_FRAME_BYTES);
    }

    /// 六位命令名是本模块拼行的那一份出处：名单漂一位，症状是宿主整行拒收而屏上只显示离线。
    #[test]
    fn the_six_command_names_are_the_hosts_own_list() {
        let code = include_str!("control.rs")
            .split_once("#[cfg(test)]")
            .expect("测试那一截还在原位")
            .0;
        for name in NOTES_COMMANDS {
            assert!(code.contains(&format!("\"{name}\"")), "{name} 不在本模块的名单里");
        }
        assert_eq!(NOTES_COMMANDS.len(), 6);
        for stray in ["notes_list", "notes_create", "notes_delete", "notes_info"] {
            assert!(!code.contains(stray), "{stray} 不是宿主控制面认得的命令");
        }
    }
}
