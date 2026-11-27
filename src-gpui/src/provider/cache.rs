//! provider 元数据的有界缓存：只留投出来的页，两把尺先到者淘汰。
//!
//! 目录树与首页都要"这一屏别再去问宿主"，而一次答复最大可以到 4 MiB——只限条数会让几枚大页把常驻
//! 钉死，只限字节又让 512 枚小页无限长下去，所以两把尺共用同一个淘汰循环，谁先到听谁的。
//! 一夹的每一页各占一格，所以翻页不会把刚看过的上一页挤掉；作废却按整夹找，因为宿主推的变更只点名
//! 文件夹。
//! 缓存里住的是 `provider::node` 的投影，正文、附件路径与私密明文从类型上就进不来。
use super::node::{Page, ProviderNode, RecentEntry, RecentPage};

/// 项数上限，与 §3.7.9 那格预算同值。
pub(crate) const MAX_CACHE_ENTRIES: usize = 512;

/// 字节上限（约 1 MiB）。⚠ 它比 `control::REPLY_MAX_BYTES` 小一档是有意的：单格比整缓存还大的答复
/// 根本留不住，那一格当场不存，而不是把别人挤光只留下它。
pub(crate) const MAX_CACHE_BYTES: usize = 1 << 20;

/// 缓存的键。三形，没有路径那一形。
///
/// 目录那两形各带一位游标：一夹超过一页时每一页各占一格，`cursor: None` 就是首次展开那一格。
/// ⚠ 游标对 Nomo 不透明——它只当键与请求参数原样带回宿主，不解析、不进日志、不拼给用户看。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CacheKey {
    Roots { cursor: Option<String> },
    Children { folder_id: i64, cursor: Option<String> },
    Recent,
}

impl CacheKey {
    /// 键自己那一段的堆成本：目录两形各带一位可选游标，`Recent` 不带堆上字段。
    /// ⚠ 这一位只进字节账——格子认的是 `CacheKey` 相等，不是拼出来的位名串。
    fn key_bytes(&self) -> usize {
        size_of::<CacheKey>() + self.cursor().map_or(0, |cursor| cursor.len())
    }

    fn cursor(&self) -> Option<&String> {
        match self {
            Self::Roots { cursor } | Self::Children { cursor, .. } => cursor.as_ref(),
            Self::Recent => None,
        }
    }

    /// 这一格属于哪一夹：变更那一发只点名文件夹，不点名第几页，作废要按这一位整批找。
    fn folder(&self) -> Option<i64> {
        match self {
            Self::Children { folder_id, .. } => Some(*folder_id),
            _ => None,
        }
    }
}

/// 存下来的那一格。两种形状共用一枚 enum，字节账因此只有一处。
#[derive(Clone, Debug)]
pub(crate) enum Cached {
    Directory(Page),
    Recent(RecentPage),
}

impl Cached {
    pub(crate) fn as_directory(&self) -> Option<&Page> {
        match self {
            Self::Directory(page) => Some(page),
            Self::Recent(_) => None,
        }
    }

    pub(crate) fn as_recent(&self) -> Option<&RecentPage> {
        match self {
            Self::Recent(page) => Some(page),
            Self::Directory(_) => None,
        }
    }

    fn estimated_bytes(&self) -> usize {
        match self {
            Self::Directory(page) => {
                let nodes = page.nodes.iter().map(heap_bytes).sum::<usize>();
                nodes
                    + page.nodes.len() * size_of::<ProviderNode>()
                    + cursor_bytes(&page.next_cursor)
            }
            Self::Recent(page) => {
                let items = page.items.iter().map(|entry| heap_bytes(&entry.node)).sum::<usize>();
                items + page.items.len() * size_of::<RecentEntry>()
            }
        }
    }
}

/// 一条节点堆上那两段字串：enum 本体由调用处按 `size_of` 计。`Locked` 没有名字，只带 opaque。
fn heap_bytes(node: &ProviderNode) -> usize {
    node.id().len() + node.name().map_or(0, |name| name.len())
}

fn cursor_bytes(cursor: &Option<String>) -> usize {
    match cursor {
        Some(cursor) => size_of::<String>() + cursor.len(),
        None => 0,
    }
}

#[derive(Clone, Default, Debug)]
pub(crate) struct ProviderCache {
    /// 队首是最先写进来的那一位，也就是最先被赶走的那一位。
    entries: Vec<(CacheKey, Cached)>,
    bytes: usize,
    revision: u64,
}

impl ProviderCache {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn get(&self, key: &CacheKey) -> Option<&Cached> {
        self.entries.iter().find(|(at, _)| at == key).map(|(_, page)| page)
    }

    /// `false`＝这一格比整个缓存还大，当场不存。已经留着的那些一位都不动：
    /// 一次翻页失败不该把用户已经看得见的目录清空。
    pub(crate) fn insert(&mut self, key: &CacheKey, page: Cached) -> bool {
        let cost = key.key_bytes() + size_of::<Cached>() + page.estimated_bytes();
        if cost > MAX_CACHE_BYTES {
            return false;
        }
        self.drop(key);
        self.entries.push((key.clone(), page));
        self.bytes += cost;
        self.trim();
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub(crate) fn invalidate(&mut self, key: &CacheKey) -> bool {
        let had = self.get(key).is_some();
        self.drop(key);
        if had {
            self.revision = self.revision.wrapping_add(1);
        }
        had
    }

    /// 一夹的每一页整批作废，交出被赶走的那几枚键让调用方逐格报回——宿主推的变更只点名文件夹，
    /// 第几页是这一侧的事。返回的键按写入先后排。
    pub(crate) fn invalidate_folder(&mut self, folder_id: i64) -> Vec<CacheKey> {
        let dropped: Vec<CacheKey> = self
            .entries
            .iter()
            .filter(|(key, _)| key.folder() == Some(folder_id))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &dropped {
            self.drop(key);
        }
        if !dropped.is_empty() {
            self.revision = self.revision.wrapping_add(1);
        }
        dropped
    }

    pub(crate) fn clear(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.entries.clear();
        self.bytes = 0;
        self.revision = self.revision.wrapping_add(1);
    }

    fn drop(&mut self, key: &CacheKey) {
        let Some(at) = self.entries.iter().position(|(at, _)| at == key) else {
            return;
        };
        self.bytes -=
            key.key_bytes() + size_of::<Cached>() + self.entries[at].1.estimated_bytes();
        self.entries.remove(at);
    }

    fn trim(&mut self) {
        while self.entries.len() > MAX_CACHE_ENTRIES || self.bytes > MAX_CACHE_BYTES {
            // 队首那一枚就是"最先到这儿"的那一位。刚写进去的那位在队尾，永远最后才被赶走。
            let Some((key, _)) = self.entries.first() else { break };
            let key = key.clone();
            self.drop(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::control::REPLY_MAX_BYTES;
    use super::*;

    fn note(note_id: i64, folder_id: i64, name: &str) -> ProviderNode {
        ProviderNode::Note {
            id: format!("note:{note_id}"),
            note_id,
            folder_id,
            name: name.to_string(),
            updated_at_ms: 1,
        }
    }

    fn directory(nodes: Vec<ProviderNode>) -> Cached {
        Cached::Directory(Page { nodes, next_cursor: None })
    }

    fn roots() -> CacheKey {
        CacheKey::Roots { cursor: None }
    }

    fn folder(folder_id: i64) -> CacheKey {
        CacheKey::Children { folder_id, cursor: None }
    }

    /// 同一夹的第二页往后：键只多一枚游标，格子却各占一位。
    fn page(folder_id: i64, cursor: &str) -> CacheKey {
        CacheKey::Children { folder_id, cursor: Some(cursor.to_string()) }
    }

    /// 一枚带 `next_cursor` 的答复：翻页那一格要的正是这一形。
    fn paged(nodes: Vec<ProviderNode>, cursor: &str) -> Cached {
        Cached::Directory(Page { nodes, next_cursor: Some(cursor.to_string()) })
    }

    /// 一枚带 4 KiB 标题的页：项数远没到上限时字节那把尺就该先响。
    fn wide_note(note_id: i64) -> ProviderNode {
        note(note_id, 1, &"标".repeat(1400))
    }

    #[test]
    fn the_byte_ceiling_evicts_before_the_count_one_does() {
        let mut cache = ProviderCache::default();
        for at in 0..400 {
            cache.insert(&folder(at), directory(vec![wide_note(at)]));
        }
        assert!(cache.bytes() <= MAX_CACHE_BYTES, "字节账越界：{}", cache.bytes());
        assert!(
            cache.len() < MAX_CACHE_ENTRIES,
            "只数了条数：{} 项却已经 {} 字节",
            cache.len(),
            cache.bytes()
        );
        // 队尾那位是最后写进去的，它必须还在；被赶走的应是队首那一批。
        assert!(cache.get(&folder(399)).is_some());
        assert!(cache.get(&folder(0)).is_none());
    }

    #[test]
    fn the_count_ceiling_evicts_the_oldest_first() {
        let mut cache = ProviderCache::default();
        for at in 0..(MAX_CACHE_ENTRIES + 88) {
            cache.insert(&folder(at as i64), directory(vec![]));
        }
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(
            cache.bytes() * 8 < MAX_CACHE_BYTES,
            "条数那把尺独自生效时字节账应远没到上限，实际 {}",
            cache.bytes()
        );
        for at in 0..88 {
            assert!(cache.get(&folder(at as i64)).is_none(), "{at} 没被赶走");
        }
        assert!(cache.get(&folder(88)).is_some());
        assert!(cache.get(&folder((MAX_CACHE_ENTRIES + 87) as i64)).is_some());
    }

    #[test]
    fn a_page_bigger_than_the_whole_cache_is_not_stored() {
        let mut cache = ProviderCache::default();
        assert!(cache.insert(&roots(), directory(vec![note(1, 1, "甲")])));
        let before = (cache.len(), cache.bytes());

        let huge = directory(vec![note(9, 1, &"x".repeat(MAX_CACHE_BYTES + (1 << 16)))]);
        assert!(!cache.insert(&folder(7), huge), "单格比整缓存还大却被留下了");
        assert_eq!((cache.len(), cache.bytes()), before, "留不下的那一格动了别人");
        assert!(cache.get(&folder(7)).is_none());
        assert!(cache.get(&roots()).is_some(), "已有目录被一次失败翻页清空");
    }

    #[test]
    fn the_revision_moves_only_when_the_cache_really_changes() {
        let mut cache = ProviderCache::default();
        assert_eq!(cache.revision(), 0);
        assert!(!cache.invalidate(&folder(9)));
        assert!(cache.invalidate_folder(9).is_empty());
        cache.clear();
        assert_eq!(cache.revision(), 0, "空账上的无效操作不该唤醒屏");

        assert!(cache.insert(&folder(1), directory(vec![])));
        assert_eq!(cache.revision(), 1);
        assert!(!cache.insert(
            &folder(2),
            directory(vec![note(9, 2, &"x".repeat(MAX_CACHE_BYTES + (1 << 16)))])
        ));
        assert_eq!(cache.revision(), 1, "存不下的那一格不该报成变化");
        assert!(cache.invalidate(&folder(1)));
        assert_eq!(cache.revision(), 2);
        assert!(!cache.invalidate(&folder(1)));
        assert_eq!(cache.revision(), 2, "重复作废不该再报一次变化");

        assert!(cache.insert(&folder(3), directory(vec![])));
        assert!(cache.insert(&folder(4), directory(vec![])));
        let before = cache.revision();
        assert_eq!(cache.invalidate_folder(3), vec![folder(3)]);
        assert_eq!(cache.revision(), before + 1);
        assert!(cache.invalidate_folder(3).is_empty());
        assert_eq!(cache.revision(), before + 1);
        cache.clear();
        assert_eq!(cache.revision(), before + 2);
        cache.clear();
        assert_eq!(cache.revision(), before + 2, "重复清空不该再报一次变化");
    }

    /// 同键重写：值换新的、条数不双计，且这一枚挪到队尾——刚翻页看到的目录下一轮就被赶走是不合理的。
    #[test]
    fn rewriting_a_key_refreshes_it_and_moves_it_to_the_tail() {
        let mut cache = ProviderCache::default();
        cache.insert(&folder(1), directory(vec![note(1, 1, &"y".repeat(64))]));
        let small = (cache.len(), cache.bytes());
        cache.insert(&folder(1), directory(vec![]));
        assert_eq!(cache.len(), small.0, "同键重写多出一格");
        assert!(cache.bytes() < small.1, "字节账跟着旧值走：{}", cache.bytes());
        assert_eq!(cache.get(&folder(1)).unwrap().as_directory().unwrap().nodes.len(), 0);

        let mut cache = ProviderCache::default();
        for at in 0..MAX_CACHE_ENTRIES as i64 {
            cache.insert(&folder(at), directory(vec![]));
        }
        cache.insert(&folder(0), directory(vec![]));
        cache.insert(&folder(MAX_CACHE_ENTRIES as i64), directory(vec![]));
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(cache.get(&folder(0)).is_some(), "刷新过的那位还被当最旧的赶走");
        assert!(cache.get(&folder(1)).is_none(), "该被赶走的是队首那位");
    }

    #[test]
    fn an_empty_directory_is_still_a_cached_reading() {
        let mut cache = ProviderCache::default();
        assert!(cache.insert(&roots(), directory(vec![])));
        let page = cache.get(&roots()).expect("空目录也是一次合法读数").as_directory().expect("键形与值形不符");
        assert!(page.nodes.is_empty() && page.next_cursor.is_none());
    }

    #[test]
    fn invalidating_one_folder_leaves_roots_and_siblings() {
        let mut cache = ProviderCache::default();
        cache.insert(&roots(), directory(vec![note(1, 1, "甲")]));
        cache.insert(&folder(1), directory(vec![note(2, 1, "乙")]));
        cache.insert(&folder(2), directory(vec![note(3, 2, "丙")]));
        let recent = Cached::Recent(RecentPage {
            as_of_ms: 5,
            items: vec![RecentEntry { node: note(2, 1, "乙"), opened_at_ms: 4 }],
        });
        assert!(cache.insert(&CacheKey::Recent, recent));

        assert!(cache.invalidate(&folder(1)));
        assert!(!cache.invalidate(&folder(9)), "没 cached 过的键报成失效了");
        assert_eq!(cache.len(), 3);
        assert!(cache.get(&folder(1)).is_none());
        assert!(cache.get(&roots()).is_some() && cache.get(&folder(2)).is_some());
        let kept = cache.get(&CacheKey::Recent).unwrap().as_recent().expect("recent 键该读出 recent 形");
        assert_eq!((kept.as_of_ms, kept.items.len()), (5, 1));
        assert!(cache.get(&CacheKey::Recent).unwrap().as_directory().is_none());

        let before = cache.bytes();
        cache.clear();
        assert_eq!((cache.len(), cache.bytes()), (0, 0));
        assert!(before > 0, "清空前字节账是空的，说明它本来就没在算");
    }

    /// 翻页各占一格：第二页写进来不能把刚看过的第一页赶掉，两格读回来也各自是各自那一页。
    #[test]
    fn each_page_of_a_folder_holds_its_own_slot() {
        let mut cache = ProviderCache::default();
        assert!(cache.insert(&folder(7), paged(vec![note(1, 7, "甲")], "abc")));
        assert!(cache.insert(&page(7, "abc"), directory(vec![note(2, 7, "乙")])));
        assert_eq!(cache.len(), 2, "第二页把第一页顶掉了");
        let first = cache.get(&folder(7)).unwrap().as_directory().unwrap();
        let second = cache.get(&page(7, "abc")).unwrap().as_directory().unwrap();
        assert_eq!((first.nodes.len(), second.nodes.len()), (1, 1));
        assert_eq!(first.nodes[0].name(), Some("甲"));
        assert_eq!(second.nodes[0].name(), Some("乙"));
        assert!(cache.get(&page(7, "zzz")).is_none(), "没问过的游标读到了别人那一页");
    }

    /// 格子认的是键本身：游标长成什么形状都各归各格，`None` 与 `Some("")` 也是两页。
    #[test]
    fn a_page_is_told_apart_by_its_key_not_by_a_spelled_out_slot() {
        let mut cache = ProviderCache::default();
        cache.insert(&page(7, "abc"), directory(vec![note(1, 7, "甲")]));
        cache.insert(&page(7, "3:abc"), directory(vec![note(2, 7, "乙")]));
        cache.insert(&page(7, ""), directory(vec![note(3, 7, "丙")]));
        assert_eq!(cache.len(), 3, "游标形状相近的两页被并成了一格");
        for (cursor, name) in [("abc", "甲"), ("3:abc", "乙"), ("", "丙")] {
            let page = cache
                .get(&page(7, cursor))
                .unwrap()
                .as_directory()
                .expect("每一页各占一格，不该读到别形的值");
            assert_eq!(page.nodes[0].name(), Some(name), "{cursor:?} 读到了别一页");
        }
        assert!(cache.get(&folder(7)).is_none(), "首次展开那一格凭空多了一次读数");
    }

    /// 游标在键里就在账上：同一页内容配长游标那一格必须更贵，作废后账要落回零。
    #[test]
    fn the_cursor_in_the_key_is_part_of_the_bill() {
        let mut cache = ProviderCache::default();
        cache.insert(&page(7, "c"), directory(vec![note(1, 7, "甲")]));
        let short = cache.bytes();
        cache.insert(&page(7, &"d".repeat(1000)), directory(vec![note(1, 7, "甲")]));
        assert!(
            cache.bytes() + 1 >= short + 1000,
            "键上那枚游标没进账：短游标 {short} 字节，长游标 {}",
            cache.bytes()
        );
        cache.invalidate_folder(7);
        assert_eq!(cache.bytes(), 0, "作废后字节账没落回零：{}", cache.bytes());
    }

    /// 按夹作废：宿主只点名文件夹，第几页是这一侧的事，所以那一夹的每一格都得走，且交回来的正是它们。
    #[test]
    fn invalidating_a_folder_drops_every_page_it_holds() {
        let mut cache = ProviderCache::default();
        cache.insert(&roots(), directory(vec![note(0, 1, "夹")]));
        cache.insert(&folder(7), paged(vec![note(1, 7, "甲")], "abc"));
        cache.insert(&page(7, "abc"), paged(vec![note(2, 7, "乙")], "def"));
        cache.insert(&page(7, "def"), directory(vec![note(3, 7, "丙")]));
        cache.insert(&folder(70), directory(vec![note(4, 70, "丁")]));
        cache.insert(&CacheKey::Recent, Cached::Recent(RecentPage { as_of_ms: 1, items: vec![] }));
        assert_eq!(cache.len(), 6);
        let before = cache.bytes();

        let dropped = cache.invalidate_folder(7);
        assert_eq!(
            dropped,
            vec![folder(7), page(7, "abc"), page(7, "def")],
            "交回来的键不是那一夹写入的那三枚"
        );
        assert_eq!(cache.len(), 3, "整夹作废没把每一格都赶走");
        assert!(cache.bytes() < before, "字节账没跟着页一起落");
        for key in [&folder(7), &page(7, "abc"), &page(7, "def")] {
            assert!(cache.get(key).is_none(), "{key:?} 还留在缓存里");
        }
        // 别的夹、根目录与首页一位不动——`children:7` 是 `children:70` 的字面前缀，按前缀数就会踩到。
        assert!(cache.get(&folder(70)).is_some() && cache.get(&roots()).is_some());
        assert!(cache.get(&CacheKey::Recent).is_some());

        assert!(cache.invalidate_folder(9).is_empty(), "没 cached 过的一夹报成作废了");
        assert_eq!(cache.len(), 3);
    }

    /// 两把尺是两个数，不是一个数的两份：答复那一把比缓存那一把宽，才需要"单格存不下"这一支。
    #[test]
    fn the_cache_ruler_is_shorter_than_the_reply_ruler() {
        assert!(MAX_CACHE_BYTES < REPLY_MAX_BYTES);
        assert_eq!(MAX_CACHE_ENTRIES, 512);
        // 512 枚各自到顶的页远大于 1 MiB：只数条数根本守不住内存。
        let alone = directory(vec![note(1, 1, &"z".repeat(200_000))]).estimated_bytes();
        assert!(alone * MAX_CACHE_ENTRIES > REPLY_MAX_BYTES);
    }

    /// 游标也在字节账上：一页光带着 next_cursor 就能吃掉整格预算的话，只按节点正文估会把上限钉穿。
    #[test]
    fn the_next_cursor_is_part_of_the_bill() {
        let bare = directory(vec![note(1, 1, "甲")]).estimated_bytes();
        let with_cursor = Cached::Directory(Page {
            nodes: vec![note(1, 1, "甲")],
            next_cursor: Some("c".repeat(1024)),
        })
        .estimated_bytes();
        assert!(
            with_cursor >= bare + 1024,
            "游标没进账：裸页 {bare} 字节，带 1024 字节游标 {with_cursor} 字节"
        );
    }

    #[test]
    fn the_cache_holds_no_place_for_a_body() {
        let production = include_str!("cache.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("测试模块之前是生产那一截");
        let scanned = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["PathBuf", "body_path", "std::fs", "read_to_string", "Instant"] {
            assert!(!scanned.contains(forbidden), "缓存那一截出现了 {forbidden}");
        }
        // 时间维不住在这儿：陈旧判断归调用方，缓存只管"留多少"。
        assert!(scanned.contains("MAX_CACHE_ENTRIES") && scanned.contains("MAX_CACHE_BYTES"));
    }
}
