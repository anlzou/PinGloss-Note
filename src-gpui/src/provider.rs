//! provider 的打开身份：外部条目不是本地路径，也不进本地那条以路径为主键的链路。
//!
//! PingGloss 宿主投影出来的笔记条目只有 `provider_id` ＋ `item_id` 两位（§3.7.3／§9.8.3）：物理正文
//! 路径只留在宿主与 `PinGloss-Note x Nomo` 侧车内部，普通 Nomo 拿不到，也不能拿它当 `PathBuf` 用。
//! 而标签去重、跨窗复用、`canonicalize` 与恢复快照这四条都以路径为主键，所以"是本地还是 provider"
//! 必须在类型上先分开，而不是在每个调用点再判一次 `Option`——漏判的症状是在本地长出一个假标签。
//!
//! 这几件事各住一处：控制面的帧形与落点住在 `provider::control`，答复到目录节点的投影住在
//! `provider::node`，一屏留多少页住在 `provider::cache`，哪一趟答复还作数住在 `provider::session`，
//! 号与来路的对应住在 `provider::pending`，把这四层串成"问一格、收一行、报一事"的泵住在
//! `provider::link`，拨号、发帧与按预算收行那一根管子住在 `provider::pipe`，把管子搬到线程上、
//! 只让屏递意图收事件的通道住在 `provider::worker`；屏侧只读快照与订阅住在 `provider::snapshot`，
//! 文件树与首页还没来接，provider 目标仍到不了屏。
use std::path::PathBuf;

#[allow(dead_code)] // 接线之前，端点推导与分诊表只有测试在读。
pub(crate) mod control;
#[allow(dead_code)] // 同上：树与首页还没来读投影。
pub(crate) mod node;
#[allow(dead_code)] // 同上：还没有读者按这两把尺留页。
pub(crate) mod cache;
#[allow(dead_code)] // 同上：调度两头都还只在测试里走。
pub(crate) mod session;
#[allow(dead_code)] // 同上：那根线程还没接上，号与来路的对应只有测试在走。
pub(crate) mod pending;
#[allow(dead_code)] // 同上：泵的两头（句柄与屏）都还没接，进出两侧只有测试在喂。
pub(crate) mod link;
#[allow(dead_code)] // 同上：管子已经能拨通，但没有屏那一侧的读者去递意图。
pub(crate) mod pipe;
#[allow(dead_code)] // 同上：线程已经能拨号回信，文件树与首页还没来递意图。
pub(crate) mod worker;
#[allow(dead_code)] // 快照与订阅已备好，文件树与首页还没来读。
pub(crate) mod snapshot;

/// `item_id` 的长度上限：它既当缓存键，也要原样进控制帧，超界的那条按坏值拒收而不是截断。
const MAX_ITEM_BYTES: usize = 512;

/// Notes 这一家的 provider 名，与宿主清单里的 `providerId` 同一个串。比较与构造都从这里取，
/// 调用点各拼一份字面量的话，拼错的那一条只会安静地变成离线态。
pub(crate) const NOTES_PROVIDER_ID: &str = "notes";

/// 一条 provider 条目的稳定身份。`item_id` 由该 provider 自己生成（Notes 用 `note:<note_id>`），
/// 对普通 Nomo 不透明：只作去重键、缓存键与命令参数，不解析、不当路径、不进正文日志。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ProjectRef {
    provider_id: &'static str,
    item_id: String,
}

impl ProjectRef {
    /// 唯一的合法入口：空 provider、空或超界的 id、带控制字符的 id 都拒收。
    ///
    /// ⚠ 长度与形状那两把尺只立在这一处。`OpenTarget::Provider` 里的那一枚因此恒为可用形状，
    /// 排队闸与缓存都不必再量一遍；N-02 解析宿主帧时也从这儿过，别在别处补第二把尺。
    pub(crate) fn new(provider_id: &'static str, item_id: &str) -> Option<Self> {
        if provider_id.is_empty() || item_id.is_empty() || item_id.len() > MAX_ITEM_BYTES { return None; }
        if item_id.chars().any(|ch| (ch as u32) < 0x20 || (ch as u32) == 0x7f) { return None; }
        Some(Self { provider_id, item_id: item_id.to_owned() })
    }

    pub(crate) fn provider_id(&self) -> &'static str { self.provider_id }
    pub(crate) fn item_id(&self) -> &str { &self.item_id }
}

/// 一条打开意图的目标身份。`Blank` 是"本窗就地新建"（旧 `path: None`），`Path` 是本地文件／目录，
/// `Provider` 是外部条目——三位互斥，旧形状里"旗标齐了却没有路径"那类没定义的组合就此编不出来。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum OpenTarget {
    #[default]
    Blank,
    Path(PathBuf),
    Provider(ProjectRef),
}

impl OpenTarget {
    /// 只有本地路径那一臂交得出路径：预览取消、跨窗复用、标签去重与改名映射都从这一处取。
    pub(crate) fn path(&self) -> Option<&PathBuf> {
        match self { Self::Path(path) => Some(path), _ => None }
    }

    pub(crate) fn path_mut(&mut self) -> Option<&mut PathBuf> {
        match self { Self::Path(path) => Some(path), _ => None }
    }

    /// 取走路径并把目标降回 `Blank`（调用点要么立刻用完，要么显式放回延迟队列）。
    ///
    /// ⚠ 非路径臂原样留着：provider 身份不能被这一次取值顺手丢掉。
    pub(crate) fn take_path(&mut self) -> Option<PathBuf> {
        if !matches!(self, Self::Path(_)) { return None; }
        match std::mem::replace(self, Self::Blank) { Self::Path(path) => Some(path), _ => None }
    }
}

/// 路由那一趟的分类结果：先按身份分，本地旗标只参与 `Path` 那一臂的决定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpenDispatch {
    /// 本窗新建空白文档。
    NewDocument,
    /// 收藏夹那一路：按路径切工作区。
    FavoriteFolder(PathBuf),
    /// 本地文件／目录：`canonicalize`、目录判断、跨窗复用与窗口选择都在这条臂里。
    Local(PathBuf),
    /// 外部 provider 条目：交 provider 自己的打开流程，不建本地标签。
    Provider(ProjectRef),
}

/// provider 优先于一切本地旗标：它压根没有路径，一旦借道 `canonicalize`／目录判断／路径复用，
/// 那一条笔记就会以本地文件的身份在本地标签、恢复快照与改名映射里留下洗不掉的假条目。
pub(crate) fn dispatch_open_target(target: &OpenTarget, favorite_folder: bool) -> OpenDispatch {
    match target {
        OpenTarget::Provider(reference) => OpenDispatch::Provider(reference.clone()),
        OpenTarget::Blank => OpenDispatch::NewDocument,
        OpenTarget::Path(path) if favorite_folder => OpenDispatch::FavoriteFolder(path.clone()),
        OpenTarget::Path(path) => OpenDispatch::Local(path.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference() -> ProjectRef { ProjectRef::new("notes", "note:7").expect("测试用的合法身份") }

    /// 出口条件那两条"provider 目标不进本地链"在这一发里分别落在身份闸与路径闸上。
    #[test]
    fn provider_identity_outranks_every_local_flag() {
        let target = OpenTarget::Provider(reference());
        // 收藏夹旗标只作用在路径那一臂；带旗标的 provider 目标仍然走 provider。
        assert_eq!(dispatch_open_target(&target, true), OpenDispatch::Provider(reference()));
        assert_eq!(dispatch_open_target(&target, false), OpenDispatch::Provider(reference()));
    }

    #[test]
    fn local_targets_keep_the_three_routes_they_had_before_the_split() {
        let path = PathBuf::from("/tmp/note.md");
        assert_eq!(dispatch_open_target(&OpenTarget::Blank, true), OpenDispatch::NewDocument);
        assert_eq!(dispatch_open_target(&OpenTarget::Blank, false), OpenDispatch::NewDocument);
        assert_eq!(
            dispatch_open_target(&OpenTarget::Path(path.clone()), false),
            OpenDispatch::Local(path.clone())
        );
        assert_eq!(
            dispatch_open_target(&OpenTarget::Path(path.clone()), true),
            OpenDispatch::FavoriteFolder(path)
        );
    }

    #[test]
    fn provider_targets_never_yield_a_reusable_local_path() {
        let mut target = OpenTarget::Provider(reference());
        assert_eq!(target.path(), None);
        assert_eq!(target.path_mut().map(|_| ()), None);
        // 取路径不吞身份：被误调一次之后，那一枚 provider 引用还在原处，还能走 provider 流程。
        assert_eq!(target.take_path(), None);
        assert_eq!(target, OpenTarget::Provider(reference()));
        // 本地臂照旧：取得到路径、取过之后降回 `Blank`。
        let mut local = OpenTarget::Path(path());
        assert_eq!(local.path(), Some(&path()));
        assert_eq!(local.take_path(), Some(path()));
        assert_eq!(local, OpenTarget::Blank);
    }

    #[test]
    fn project_refs_are_validated_once_at_construction() {
        let accepted = ProjectRef::new("notes", "folder:12").expect("合法身份");
        assert_eq!((accepted.provider_id(), accepted.item_id()), ("notes", "folder:12"));
        assert_eq!(ProjectRef::new("notes", &"x".repeat(MAX_ITEM_BYTES)).map(|_| ()), Some(()));
        assert!(ProjectRef::new("", "note:1").is_none(), "空 provider 当不了缓存键的一半");
        assert!(ProjectRef::new("notes", "").is_none());
        assert!(ProjectRef::new("notes", "   ").is_some(), "空白形状由 provider 自己负责，不是长度闸的事");
        assert!(ProjectRef::new("notes", &"x".repeat(MAX_ITEM_BYTES + 1)).is_none());
        assert!(ProjectRef::new("notes", "note:1\nnote:2").is_none(), "换行会把身份带成两行日志");
        assert!(ProjectRef::new("notes", "note:1\u{0}").is_none());
    }

    /// 分流必须排在任何本地路径工作之前——这是"provider 目标不进 `canonicalize`"那条出口的钉。
    #[test]
    fn the_provider_split_precedes_every_local_path_call() {
        let source = include_str!("ui/workspace/application_dialogs.rs");
        let body = source
            .split_once("fn route_open_request")
            .expect("路由函数还在原名")
            .1
            .split_once("fn check_external_change")
            .expect("路由函数的尾巴")
            .0;
        let split = body.find("dispatch_open_target").expect("先匹配身份那一步不见了");
        for call in ["canonicalize()", "is_dir()", "reuse_open_document("] {
            let at = body.find(call).unwrap_or(usize::MAX);
            assert!(split < at, "{call} 排到了身份分流之前，provider 目标会借道本地链");
        }
    }

    /// Windows 单实例转发那条线只收路径：provider 身份不进 `WM_COPYDATA` 载荷。
    #[test]
    fn instance_forwarding_still_carries_only_paths() {
        let source = include_str!("open_instance.rs");
        for token in ["OpenTarget", "ProjectRef", "provider"] {
            assert!(!source.contains(token), "转发那条线里出现了 {token}：provider 身份不该进载荷");
        }
        assert!(source.contains("Vec<PathBuf>"), "转发只收路径这一形状被改掉了");
    }

    /// 旧的 `path: Option<PathBuf>` 一位不能与 `target` 并存：留着它，provider 请求就能伪装成本地路径
    /// 请求，从下游任何一把以路径为主键的尺里混过去。
    #[test]
    fn open_requests_carry_one_target_field_instead_of_a_bare_path() {
        let body = include_str!("ui/workspace/open_requests.rs")
            .split_once("struct OpenRequest")
            .expect("打开意图还在原名")
            .1
            .split_once("pub generation: u64")
            .expect("意图字段尾巴")
            .0;
        assert!(body.contains("pub target: OpenTarget,"), "打开意图不再持有单一 target 位");
        assert!(!body.contains("pub path:"), "本地路径字段还与 target 并存，provider 身份能被绕过去");
    }

    /// 跨窗复用只从本地臂取路径：换任何别的来源，provider 条目都会以"路径"身份落进标签表。
    #[test]
    fn document_reuse_borrows_its_path_from_the_local_accessor_only() {
        let body = include_str!("ui/workspace/open_requests.rs")
            .split_once("fn reuse_open_document")
            .expect("复用函数还在原名")
            .1;
        let guard = body.split_once("let current = window.window_handle()").expect("复用守卫的尾巴").0;
        assert!(guard.contains("request.local_path() else { return false; }"), "复用不再只看本地臂那一把尺");
        for leak in [".item_id()", "OpenTarget::Provider", "take_local_path"] {
            assert!(!body.contains(leak), "复用里出现了 {leak}：provider 身份被当成路径用了");
        }
    }

    /// 排队闸那把 32 KiB 尺只量本地路径；provider 身份另有构造时的 512 字节闸，两把不合用。
    #[test]
    fn the_queue_length_gate_only_measures_local_paths() {
        let gate = include_str!("ui/workspace/open_requests.rs")
            .split_once("fn enqueue_open_request")
            .expect("入队函数还在原名")
            .1
            .split_once("open.queueFull")
            .expect("入队闸的尾巴")
            .0;
        assert!(gate.contains("request.local_path().is_some_and"), "长度闸不再只量本地臂");
        assert!(!gate.contains("request.target"), "长度闸直接摸 target：provider 身份会被当成路径拒收");
    }

    /// provider 臂在路由处当场收束：不建本地标签，也不排本地会话。
    #[test]
    fn the_provider_arm_ends_at_the_notice_before_any_local_open() {
        let arm = include_str!("ui/workspace/application_dialogs.rs")
            .split_once("fn route_open_request")
            .expect("路由函数还在原名")
            .1
            .split_once("OpenDispatch::Provider")
            .expect("provider 那一臂不见了")
            .1
            .split_once('}')
            .expect("那一臂的尾巴")
            .0;
        assert!(arm.contains("refuse_provider_open"), "provider 臂没有止步于提示");
        assert!(arm.trim_end().ends_with("return;"), "provider 臂没有当场收束，会继续走本地链");
        for leak in ["open_request_here", "open_path_now", "open_document_from", "reuse_open", "enqueue_open", "PathBuf::from", "canonicalize"] {
            assert!(!arm.contains(leak), "provider 臂里出现了 {leak}：条目被当成本地路径或本地标签处理了");
        }
    }

    /// 提示的两端对账：四语模板都得在场，模板里的占位符名与调用点实际交出去的参数名必须是同一批。
    /// `app_text` 缺键时回落到键名，只查调用点会看见一句拼不上字的提示。
    #[test]
    fn the_provider_notice_template_and_argument_agree() {
        let values: serde_json::Value =
            serde_json::from_str(include_str!("../resources/app-locales.json")).unwrap();
        let array = include_str!("ui/workspace/open_requests.rs")
            .split_once("fn refuse_provider_open")
            .expect("提示函数还在原名")
            .1
            .split_once("app_format")
            .expect("提示不再走带参模板")
            .1
            .split_once("&[")
            .expect("参数数组不见了")
            .1
            .split_once(']')
            .expect("参数数组尾巴")
            .0;
        let passed: std::collections::BTreeSet<String> = array
            .match_indices("(\"")
            .filter_map(|(at, _)| {
                let rest = &array[at + 2..];
                rest.find("\",").map(|end| rest[..end].to_owned())
            })
            .collect();
        for locale in ["zh-CN", "zh-TW", "en-US", "ja-JP"] {
            let text = values[locale]["open.providerUnsupported"]
                .as_str()
                .unwrap_or_else(|| panic!("{locale} 缺 provider 提示"));
            assert!(!text.trim().is_empty(), "{locale} 的 provider 提示是空串");
            assert!(!text.contains("open.providerUnsupported"), "{locale} 的提示写成了键名");
            let wanted: std::collections::BTreeSet<String> = text
                .split('{')
                .skip(1)
                .filter_map(|part| part.split_once('}').map(|(name, _)| name.to_owned()))
                .collect();
            assert_eq!(wanted, passed, "{locale} 的 provider 提示占位符与调用点参数不合");
        }
    }

    fn path() -> PathBuf { PathBuf::from("/tmp/note.md") }
}
