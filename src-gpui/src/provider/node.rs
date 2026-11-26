//! 答复里那一格 payload → Nomo 侧目录节点：只有过了这扇门的字段才进内存。
//!
//! 形状的唯一出处是宿主 `crates/pg-plugin/src/notes.rs` 的 `to_value`，不是文档转述：漂一位的失败形是
//! 整棵树空白或串台，而本机看得见的只有"连上了却没东西"。宿主对它自己的入站帧严格（那是两端同源的
//! 对称合同），Nomo 作为客户端反过来只读点名的那几位——宿主加一位不该让旧 Nomo 看不见目录，代价是
//! 投影类型里必须没有能装下路径的位子，因而未知字段丢得干净。
//!
//! 三页答复（roots／children／recent）与三种回执（open／remove／clear）都从这儿过；`triage` 只管
//! 一行是不是答复，"答复里装的是什么"归本模块。
use super::{NOTES_PROVIDER_ID, ProjectRef};
use serde_json::{Map, Value};

/// roots 那一页的协议位，与宿主 `PROTOCOL` 同值。⚠ 只在 roots 上门：children／recent 的答复不带它，
/// 把这道门铺到三页上会让本机能读、宿主从不发的那种形状变成必红。
const HOST_PROTOCOL: i64 = 1;

/// 锁定节点那枚 opaque 的长度上限，与宿主 `MAX_OPAQUE_BYTES` 同档。
const MAX_OPAQUE_BYTES: usize = 128;

/// 回带去翻页的游标上限，与宿主自家 `cursor()` 里那把尺同档：超界的游标发回去只会换一条拒收，
/// 留着它等于把"翻页到底"变成"翻页必败"。
const MAX_CURSOR_BYTES: usize = 1024;

/// 坏答复落在哪一位、为什么。一页只报第一处，不猜半页——半页投影出来的树比空树更难解释。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NodeError {
    pub(crate) field: &'static str,
    pub(crate) reason: &'static str,
}

impl NodeError {
    fn new(field: &'static str, reason: &'static str) -> Self {
        Self { field, reason }
    }
}

/// 一条目录节点。三臂不共享字段：私密行只有 `Locked`，它连名字都没有，只给一个锁位。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProviderNode {
    Folder {
        id: String,
        folder_id: i64,
        name: String,
        locked: bool,
    },
    Note {
        id: String,
        note_id: i64,
        folder_id: i64,
        name: String,
        updated_at_ms: i64,
    },
    Locked {
        id: String,
    },
}

impl ProviderNode {
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Folder { id, .. } | Self::Note { id, .. } | Self::Locked { id } => id,
        }
    }

    /// `Locked` 没有名字：宿主只交一枚 opaque，标题属于私密那侧，不能在这儿凭空补一个。
    pub(crate) fn name(&self) -> Option<&str> {
        match self {
            Self::Folder { name, .. } | Self::Note { name, .. } => Some(name),
            Self::Locked { .. } => None,
        }
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Folder { .. } => "folder",
            Self::Note { .. } => "note",
            Self::Locked { .. } => "locked",
        }
    }

    pub(crate) fn folder_id(&self) -> Option<i64> {
        match self {
            Self::Folder { folder_id, .. } | Self::Note { folder_id, .. } => Some(*folder_id),
            Self::Locked { .. } => None,
        }
    }

    pub(crate) fn note_id(&self) -> Option<i64> {
        match self {
            Self::Note { note_id, .. } => Some(*note_id),
            _ => None,
        }
    }

    /// 锁定位的读法：`Folder` 自己带，`Locked` 恒为真，公开的 `Note` 恒为假。
    pub(crate) fn locked(&self) -> bool {
        match self {
            Self::Folder { locked, .. } => *locked,
            Self::Note { .. } => false,
            Self::Locked { .. } => true,
        }
    }

    /// 能不能当一条可打开的笔记：`Locked` 永远不算，缺 `note_id` 也不算。
    pub(crate) fn is_openable_note(&self) -> bool {
        matches!(self, Self::Note { .. })
    }

    /// 目录身份就是缓存与去重用的那个 provider 引用；`ProjectRef::new` 那把尺是唯一出处。
    pub(crate) fn reference(&self) -> Option<ProjectRef> {
        ProjectRef::new(NOTES_PROVIDER_ID, self.id())
    }
}

/// 一页目录：节点加那一枚"还有下一段"的游标。
#[derive(Debug)]
pub(crate) struct Page {
    pub(crate) nodes: Vec<ProviderNode>,
    pub(crate) next_cursor: Option<String>,
}

/// 首页最近项目的一条：节点本体加宿主账本里那次真实打开时间。
#[derive(Debug)]
pub(crate) struct RecentEntry {
    pub(crate) node: ProviderNode,
    pub(crate) opened_at_ms: i64,
}

#[derive(Debug)]
pub(crate) struct RecentPage {
    pub(crate) as_of_ms: i64,
    pub(crate) items: Vec<RecentEntry>,
}

pub(crate) fn parse_roots(payload: &Value) -> Result<Page, NodeError> {
    let obj = match payload.as_object() {
        Some(obj) => obj,
        None => return Err(NodeError::new("payload", "目录答复的顶层不是对象")),
    };
    if integer(obj, "protocol") != Some(HOST_PROTOCOL) {
        return Err(NodeError::new("protocol", "协议位与本机不同，整页不猜"));
    }
    if string(obj, "provider_id") != Some(NOTES_PROVIDER_ID) {
        return Err(NodeError::new("provider_id", "答复来自别家 provider"));
    }
    if boolean(obj, "online") != Some(true) {
        return Err(NodeError::new("online", "在线答复标了自己不在线"));
    }
    // roots 只有文件夹：混进 note／locked 说明投影错位，展开时会拿笔记 id 当文件夹去查子项。
    let items = array(obj)?;
    let nodes = items
        .iter()
        .map(|item| match parse_node(item)? {
            node @ ProviderNode::Folder { .. } => Ok(node),
            _ => Err(NodeError::new(
                "kind",
                "根目录那一页出现了不是文件夹的节点",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page {
        nodes,
        next_cursor: cursor(obj)?,
    })
}

/// children 的 `folder_id` 回显与本机请求的那一格当场对账：对不上就不算这一页，
/// 否则宿主答了 A 夹、Nomo 会把节点挂到 B 夹下面，而这一屏看不出来。
pub(crate) fn parse_children(payload: &Value, requested_folder_id: i64) -> Result<Page, NodeError> {
    let obj = match payload.as_object() {
        Some(obj) => obj,
        None => return Err(NodeError::new("payload", "目录答复的顶层不是对象")),
    };
    match integer(obj, "folder_id") {
        Some(got) if got == requested_folder_id && got > 0 => {}
        Some(_) => {
            return Err(NodeError::new(
                "folder_id",
                "答复回显的文件夹与请求不是同一枚",
            ));
        }
        None => return Err(NodeError::new("folder_id", "答复没带文件夹位")),
    }
    let nodes = array(obj)?
        .iter()
        .map(parse_node)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page {
        nodes,
        next_cursor: cursor(obj)?,
    })
}

pub(crate) fn parse_recent(payload: &Value) -> Result<RecentPage, NodeError> {
    let obj = match payload.as_object() {
        Some(obj) => obj,
        None => return Err(NodeError::new("payload", "目录答复的顶层不是对象")),
    };
    let as_of_ms = non_negative(obj, "as_of_ms")?;
    let items = array(obj)?
        .iter()
        .map(|item| {
            let opened_at_ms = match item.as_object() {
                Some(obj) => non_negative(obj, "opened_at_ms")?,
                None => return Err(NodeError::new("items", "这一位必须是对象")),
            };
            // 节点那一位先按公开笔记投，再单独读打开时间：宿主就是把两位并成一行写的。
            let node = parse_node(item)?;
            Ok(RecentEntry {
                node,
                opened_at_ms,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RecentPage {
        as_of_ms,
        items,
    })
}

/// 打开回执：宿主只在该次真被接受时才写 `accepted:true`，两位回显各对一次本机问出去的那枚。
pub(crate) fn parse_open(payload: &Value, requested_note_id: i64) -> Result<(), NodeError> {
    let obj = match payload.as_object() {
        Some(obj) => obj,
        None => return Err(NodeError::new("payload", "目录答复的顶层不是对象")),
    };
    if boolean(obj, "accepted") != Some(true) {
        return Err(NodeError::new("accepted", "打开没被接受"));
    }
    match integer(obj, "note_id") {
        Some(got) if got == requested_note_id && got > 0 => {}
        Some(_) => {
            return Err(NodeError::new(
                "note_id",
                "打开回执答的不是本机问的那一条",
            ));
        }
        None => return Err(NodeError::new("note_id", "打开回执缺笔记位")),
    }
    match integer(obj, "folder_id") {
        Some(got) if got > 0 => Ok(()),
        Some(_) => Err(NodeError::new("folder_id", "打开回执的文件夹不是正整数")),
        None => Err(NodeError::new("folder_id", "打开回执缺文件夹位")),
    }
}

pub(crate) fn parse_removed(payload: &Value) -> Result<bool, NodeError> {
    let obj = match payload.as_object() {
        Some(obj) => obj,
        None => return Err(NodeError::new("payload", "目录答复的顶层不是对象")),
    };
    match boolean(obj, "removed") {
        Some(removed) => Ok(removed),
        None => Err(NodeError::new("removed", "移除回执的 `removed` 不是布尔值")),
    }
}

pub(crate) fn parse_cleared(payload: &Value) -> Result<usize, NodeError> {
    let obj = match payload.as_object() {
        Some(obj) => obj,
        None => return Err(NodeError::new("payload", "目录答复的顶层不是对象")),
    };
    match integer(obj, "removed") {
        Some(count) if count >= 0 => usize::try_from(count)
            .map_err(|_| NodeError::new("removed", "移除条数装不进本机指针宽度")),
        Some(_) => Err(NodeError::new("removed", "移除条数不能是负数")),
        None => Err(NodeError::new("removed", "清空回执缺条数位")),
    }
}

fn parse_node(value: &Value) -> Result<ProviderNode, NodeError> {
    let obj = match value.as_object() {
        Some(obj) => obj,
        None => return Err(NodeError::new("items", "这一位必须是对象")),
    };
    match string(obj, "kind") {
        Some("folder") => folder(obj),
        Some("note") => note(obj),
        Some("locked") => locked(obj),
        Some(_) => Err(NodeError::new("kind", "节点种类本机不认")),
        None => Err(NodeError::new("kind", "节点没带种类位")),
    }
}

fn folder(obj: &Map<String, Value>) -> Result<ProviderNode, NodeError> {
    let folder_id = positive(obj, "folder_id")?;
    let id = prefixed_id(obj, "folder", folder_id)?;
    Ok(ProviderNode::Folder {
        id,
        folder_id,
        name: text(obj, "name")?.to_string(),
        locked: boolean(obj, "locked")
            .ok_or_else(|| NodeError::new("locked", "文件夹的锁定位不是布尔值"))?,
    })
}

fn note(obj: &Map<String, Value>) -> Result<ProviderNode, NodeError> {
    let note_id = positive(obj, "note_id")?;
    let folder_id = positive(obj, "folder_id")?;
    let id = prefixed_id(obj, "note", note_id)?;
    // 公开笔记那一位恒为 false：谎报成 true 时既不能当公开行画，也不能当锁位丢名字。
    if boolean(obj, "locked") != Some(false) {
        return Err(NodeError::new(
            "locked",
            "公开笔记的锁定位必须为 false",
        ));
    }
    Ok(ProviderNode::Note {
        id,
        note_id,
        folder_id,
        name: text(obj, "name")?.to_string(),
        updated_at_ms: non_negative(obj, "updated_at_ms")?,
    })
}

fn locked(obj: &Map<String, Value>) -> Result<ProviderNode, NodeError> {
    if boolean(obj, "locked") != Some(true) {
        return Err(NodeError::new("locked", "锁定节点的锁定位必须为 true"));
    }
    let id = text(obj, "id")?;
    let Some(opaque) = id.strip_prefix("locked:") else {
        return Err(NodeError::new("id", "锁定节点的 id 不以 `locked:` 起头"));
    };
    // opaque 里塞控制字符会让 provider 身份在构造处被拒，那时症状是"这一条凭空消失"；
    // 在这儿当场判掉，一整页不会因为一条坏 id 少一行。
    if opaque.is_empty()
        || opaque.len() > MAX_OPAQUE_BYTES
        || opaque.chars().any(|ch| (ch as u32) < 0x20 || (ch as u32) == 0x7f)
    {
        return Err(NodeError::new("id", "锁定节点的 opaque 为空、过长或带控制字符"));
    }
    Ok(ProviderNode::Locked {
        id: id.to_string(),
    })
}

fn array(obj: &Map<String, Value>) -> Result<&Vec<Value>, NodeError> {
    obj.get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| NodeError::new("items", "这一位必须是数组"))
}

fn string<'a>(obj: &'a Map<String, Value>, field: &'static str) -> Option<&'a str> {
    obj.get(field).and_then(Value::as_str)
}

fn text<'a>(obj: &'a Map<String, Value>, field: &'static str) -> Result<&'a str, NodeError> {
    string(obj, field).ok_or_else(|| NodeError::new(field, "这一位必须是字符串"))
}

fn integer(obj: &Map<String, Value>, field: &'static str) -> Option<i64> {
    obj.get(field).and_then(Value::as_i64)
}

fn boolean(obj: &Map<String, Value>, field: &'static str) -> Option<bool> {
    obj.get(field).and_then(Value::as_bool)
}

fn positive(obj: &Map<String, Value>, field: &'static str) -> Result<i64, NodeError> {
    match integer(obj, field) {
        Some(value) if value > 0 => Ok(value),
        Some(_) => Err(NodeError::new(field, "这一位必须是正整数")),
        None => Err(NodeError::new(field, "这一位必须是整数")),
    }
}

fn non_negative(obj: &Map<String, Value>, field: &'static str) -> Result<i64, NodeError> {
    match integer(obj, field) {
        Some(value) if value >= 0 => Ok(value),
        Some(_) => Err(NodeError::new(field, "这一位不能是负数")),
        None => Err(NodeError::new(field, "这一位必须是整数")),
    }
}

fn prefixed_id(obj: &Map<String, Value>, prefix: &str, numeric: i64) -> Result<String, NodeError> {
    let got = text(obj, "id")?;
    // 字符串身份与数字身份必须自洽：只信一位的话，翻页与打开会各拿一套身份，缓存对不上号。
    if got != format!("{prefix}:{numeric}") {
        return Err(NodeError::new("id", "节点 id 与数字身份不符"));
    }
    Ok(got.to_string())
}

fn cursor(obj: &Map<String, Value>) -> Result<Option<String>, NodeError> {
    match obj.get("next_cursor") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value))
            if !value.is_empty() && value.len() <= MAX_CURSOR_BYTES =>
        {
            Ok(Some(value.clone()))
        }
        _ => Err(NodeError::new(
            "next_cursor",
            "游标必须是非空字符串或 null",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 夹具逐字照宿主 `to_value` 写：改了宿主那一侧的形状，这里的断言只会以"读不出来"暴露，
    /// 不会被本机自己拼的对称形状悄悄放过。
    fn folder(folder_id: i64, name: &str, locked: bool) -> Value {
        json!({
            "kind": "folder",
            "id": format!("folder:{folder_id}"),
            "folder_id": folder_id,
            "name": name,
            "locked": locked,
        })
    }

    fn note(note_id: i64, folder_id: i64, name: &str, updated_at_ms: i64) -> Value {
        json!({
            "kind": "note",
            "id": format!("note:{note_id}"),
            "note_id": note_id,
            "folder_id": folder_id,
            "name": name,
            "updated_at_ms": updated_at_ms,
            "locked": false,
        })
    }

    fn locked(id: &str) -> Value {
        json!({ "kind": "locked", "id": id, "locked": true })
    }

    fn roots(items: Vec<Value>) -> Value {
        json!({
            "protocol": HOST_PROTOCOL,
            "provider_id": NOTES_PROVIDER_ID,
            "online": true,
            "items": items,
            "next_cursor": Value::Null,
        })
    }

    fn children(folder_id: i64, items: Vec<Value>) -> Value {
        json!({ "folder_id": folder_id, "items": items, "next_cursor": Value::Null })
    }

    fn field(err: NodeError) -> &'static str {
        err.field
    }

    #[test]
    fn every_node_kind_projects_what_the_host_writes() {
        let page = parse_roots(&roots(vec![folder(3, "工作记录", true)])).expect("合法根目录页");
        assert_eq!(page.nodes.len(), 1);
        let node = &page.nodes[0];
        assert_eq!(
            (node.kind(), node.name(), node.folder_id(), node.locked()),
            ("folder", Some("工作记录"), Some(3), true)
        );
        assert!(page.next_cursor.is_none());

        let page = parse_children(&children(3, vec![note(7, 3, "标题", 42)]), 3).expect("合法子页");
        let node = &page.nodes[0];
        // 宿主把更新时间叫 `updated_at_ms`：这一枚单独按臂取，字段名漂了不会跟着绿。
        let updated_at_ms = match node {
            ProviderNode::Note { updated_at_ms, .. } => *updated_at_ms,
            other => panic!("笔记臂读成了 {other:?}"),
        };
        assert_eq!(
            (node.kind(), node.name(), node.note_id(), node.locked(), updated_at_ms),
            ("note", Some("标题"), Some(7), false, 42)
        );
        assert!(node.is_openable_note());

        let page = parse_children(&children(3, vec![locked("locked:note-9")]), 3).expect("锁位子页");
        let node = &page.nodes[0];
        assert_eq!((node.kind(), node.name(), node.note_id()), ("locked", None, None));
        assert!(node.locked() && !node.is_openable_note());
        assert_eq!(node.folder_id(), None);
    }

    #[test]
    fn a_node_whose_id_disagrees_with_its_number_is_refused() {
        let mut wrong = folder(3, "n", false);
        wrong["id"] = json!("folder:4");
        assert_eq!(
            field(parse_children(&children(1, vec![wrong]), 1).unwrap_err()),
            "id"
        );

        let mut swapped = note(7, 3, "n", 1);
        swapped["id"] = json!("folder:7");
        assert_eq!(
            field(parse_children(&children(3, vec![swapped]), 3).unwrap_err()),
            "id"
        );
    }

    #[test]
    fn a_note_claiming_locked_is_refused_not_silently_unlocked() {
        let mut lying = note(7, 3, "标题", 1);
        lying["locked"] = json!(true);
        assert_eq!(
            field(parse_children(&children(3, vec![lying]), 3).unwrap_err()),
            "locked"
        );

        let mut missing = note(7, 3, "标题", 1);
        missing.as_object_mut().unwrap().remove("locked");
        assert_eq!(
            field(parse_children(&children(3, vec![missing]), 3).unwrap_err()),
            "locked"
        );
    }

    #[test]
    fn a_locked_row_keeps_only_an_opaque_id() {
        let page = parse_children(&children(1, vec![locked("locked:abcdef")]), 1).expect("锁位");
        let node = &page.nodes[0];
        assert_eq!(node.name(), None);
        let reference = node.reference().expect("锁位也要有 provider 身份");
        assert_eq!((reference.provider_id(), reference.item_id()), ("notes", "locked:abcdef"));

        for id in [
            "locked:".to_string(),
            format!("locked:{}", "y".repeat(MAX_OPAQUE_BYTES + 1)),
            "note:1".to_string(),
            "locked:a\u{1}b".to_string(),
        ] {
            let err = parse_children(&children(1, vec![locked(&id)]), 1).unwrap_err();
            assert_eq!(err.field, "id", "{id} 被当成合法锁位");
        }
    }

    /// 未知字段丢掉而不是把整页判死：宿主加一位不该让旧 Nomo 看不见目录。
    /// 但投影出去的三臂里没有任何位子能装下它，所以下面那些字串一个都不该出现在节点上。
    #[test]
    fn unknown_fields_never_reach_the_projection() {
        let mut spicy = note(7, 3, "标题", 1);
        for (key, value) in [
            ("body_path", json!("D:\\private\\secret.md")),
            ("path", json!("/tmp/private")),
            ("snippet", json!("正文前 200 字")),
            ("title", json!("另一个标题")),
            ("future_flag", json!({"nested": true})),
        ] {
            spicy[key] = value;
        }
        let mut payload = children(3, vec![spicy]);
        payload["host_added_top_level"] = json!(1);
        let page = parse_children(&payload, 3).expect("多一位不该让整页读不回");
        let rendered = format!("{:?}", page.nodes);
        for absent in ["body_path", "secret", "/tmp/private", "snippet", "另一个标题", "future_flag"] {
            assert!(!rendered.contains(absent), "{absent} 被带进了投影：{rendered}");
        }
        assert_eq!(page.nodes[0].name(), Some("标题"));
    }

    #[test]
    fn the_roots_page_demands_the_protocol_and_provider_pair() {
        let cases = [
            ("protocol", json!(HOST_PROTOCOL + 1), "协议位与本机不同，整页不猜"),
            ("provider_id", json!("todo"), "答复来自别家 provider"),
            ("online", json!(false), "在线答复标了自己不在线"),
        ];
        for (key, value, reason) in cases {
            let mut payload = roots(vec![folder(1, "n", false)]);
            payload[key] = value;
            let err = parse_roots(&payload).unwrap_err();
            assert_eq!((err.field, err.reason), (key, reason), "{key} 那一位没被看");
        }
        assert_eq!(field(parse_roots(&json!({ "items": [] })).unwrap_err()), "protocol");
        // 空目录是合法答复（用户一条笔记都没建），缺 `items` 那一位才是坏形。
        assert!(parse_roots(&roots(vec![])).is_ok());
        let mut no_items = roots(vec![folder(1, "n", false)]);
        no_items.as_object_mut().unwrap().remove("items");
        assert_eq!(field(parse_roots(&no_items).unwrap_err()), "items");
        // children 那一份答复从不带协议位：把这道门铺到它身上会让合法页必红。
        assert!(parse_children(&children(1, vec![]), 1).is_ok());
        assert!(!children(1, vec![]).as_object().unwrap().contains_key("protocol"));
    }

    #[test]
    fn a_roots_page_that_is_not_all_folders_is_refused() {
        let err = parse_roots(&roots(vec![note(7, 3, "n", 1)])).unwrap_err();
        assert_eq!((err.field, err.reason), ("kind", "根目录那一页出现了不是文件夹的节点"));
        let err = parse_roots(&roots(vec![locked("locked:x")])).unwrap_err();
        assert_eq!(err.field, "kind");
    }

    #[test]
    fn a_cursor_too_long_to_send_back_is_not_kept() {
        let mut payload = children(1, vec![]);
        payload["next_cursor"] = json!("c".repeat(MAX_CURSOR_BYTES));
        assert_eq!(
            parse_children(&payload, 1).unwrap().next_cursor.unwrap().len(),
            MAX_CURSOR_BYTES
        );
        payload["next_cursor"] = json!("c".repeat(MAX_CURSOR_BYTES + 1));
        assert_eq!(field(parse_children(&payload, 1).unwrap_err()), "next_cursor");
        payload["next_cursor"] = json!("");
        assert_eq!(field(parse_children(&payload, 1).unwrap_err()), "next_cursor");
        payload["next_cursor"] = json!(7);
        assert_eq!(field(parse_children(&payload, 1).unwrap_err()), "next_cursor");
        payload.as_object_mut().unwrap().remove("next_cursor");
        assert_eq!(parse_children(&payload, 1).unwrap().next_cursor, None);
    }

    #[test]
    fn the_children_answer_is_matched_against_the_requested_folder() {
        assert_eq!(
            field(parse_children(&children(9, vec![]), 3).unwrap_err()),
            "folder_id"
        );
        let mut payload = children(3, vec![]);
        payload.as_object_mut().unwrap().remove("folder_id");
        assert_eq!(field(parse_children(&payload, 3).unwrap_err()), "folder_id");
        payload["folder_id"] = json!(0);
        assert_eq!(field(parse_children(&payload, 0).unwrap_err()), "folder_id");
    }

    #[test]
    fn recent_entries_carry_their_own_open_time() {
        let mut first = note(7, 3, "甲", 10);
        first["opened_at_ms"] = json!(900);
        let mut second = note(8, 3, "乙", 20);
        second["opened_at_ms"] = json!(800);
        let payload = json!({ "items": [first, second], "as_of_ms": 1000 });
        let page = parse_recent(&payload).expect("合法最近页");
        assert_eq!(page.as_of_ms, 1000);
        assert_eq!(
            page.items.iter().map(|e| (e.node.note_id().unwrap(), e.opened_at_ms)).collect::<Vec<_>>(),
            vec![(7, 900), (8, 800)]
        );

        let mut bad_time = note(7, 3, "甲", 10);
        bad_time["opened_at_ms"] = json!(-1);
        assert_eq!(
            field(parse_recent(&json!({ "items": [bad_time], "as_of_ms": 1 })).unwrap_err()),
            "opened_at_ms"
        );
        assert_eq!(
            field(parse_recent(&json!({ "items": [], "as_of_ms": -5 })).unwrap_err()),
            "as_of_ms"
        );
    }

    #[test]
    fn the_open_reply_must_agree_with_what_was_asked() {
        let ok = json!({ "accepted": true, "note_id": 7, "folder_id": 3 });
        assert_eq!(parse_open(&ok, 7), Ok(()));
        for (key, value) in [
            ("accepted", json!(false)),
            ("note_id", json!(8)),
            ("folder_id", json!(0)),
        ] {
            let mut payload = ok.clone();
            payload[key] = value;
            assert_eq!(field(parse_open(&payload, 7).unwrap_err()), key);
        }
        let mut missing = ok.clone();
        missing.as_object_mut().unwrap().remove("folder_id");
        assert_eq!(field(parse_open(&missing, 7).unwrap_err()), "folder_id");
    }

    #[test]
    fn remove_and_clear_read_their_own_counts() {
        assert_eq!(parse_removed(&json!({ "removed": true })), Ok(true));
        assert_eq!(parse_removed(&json!({ "removed": 1 })), Err(NodeError::new("removed", "移除回执的 `removed` 不是布尔值")));
        assert_eq!(parse_cleared(&json!({ "removed": 0 })), Ok(0));
        assert_eq!(parse_cleared(&json!({ "removed": 12 })), Ok(12));
        assert_eq!(field(parse_cleared(&json!({ "removed": -1 })).unwrap_err()), "removed");
        assert_eq!(field(parse_cleared(&json!({ "removed": true })).unwrap_err()), "removed");
    }

    /// 这一扇门外不该有第二条读 payload 的路：生产那一截只碰 `PathBuf` 之外的世界。
    #[test]
    fn the_projection_has_no_place_to_put_a_path() {
        let production = include_str!("node.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("测试模块之前是生产那一截");
        let scanned = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["PathBuf", "\"body_path\"", "\"path\"", "std::fs", "File::open"] {
            assert!(!scanned.contains(forbidden), "投影里出现了 {forbidden}");
        }
        assert!(scanned.contains("fn parse_roots") && scanned.contains("fn parse_children") && scanned.contains("fn parse_recent"));
    }
}
