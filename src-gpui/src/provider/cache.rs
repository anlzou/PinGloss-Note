//! provider 元数据的有界缓存：只留投出来的页，两把尺先到者淘汰。
//!
//! 目录树与首页都要"这一屏别再去问宿主"，而一次答复最大可以到 4 MiB——只限条数会让几枚大页把常驻
//! 钉死，只限字节又让 512 枚小页无限长下去，所以两把尺共用同一个淘汰循环，谁先到听谁的。
//! 缓存里住的是 `provider::node` 的投影，正文、附件路径与私密明文从类型上就进不来。
use super::node::{Page, ProviderNode, RecentEntry, RecentPage};

/// 项数上限，与 §3.7.9 那格预算同值。
pub(crate) const MAX_CACHE_ENTRIES: usize = 512;

/// 字节上限（约 1 MiB）。⚠ 它比 `control::REPLY_MAX_BYTES` 小一档是有意的：单格比整缓存还大的答复
/// 根本留不住，那一格当场不存，而不是把别人挤光只留下它。
pub(crate) const MAX_CACHE_BYTES: usize = 1 << 20;

/// 缓存的键。三形各自一枚，没有路径那一形。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CacheKey {
    Roots,
    Children { folder_id: i64 },
    Recent,
}

impl CacheKey {
    fn slot(&self) -> String {
        match self {
            Self::Roots => "roots".to_string(),
            Self::Children { folder_id } => format!("children:{folder_id}"),
            Self::Recent => "recent".to_string(),
        }
    }
}

/// 存下来的那一格。两种形状共用一枚 enum，字节账因此只有一处。
#[derive(Debug)]
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

#[derive(Default, Debug)]
pub(crate) struct ProviderCache {
    /// 队首是最先写进来的那一位，也就是最先被赶走的那一位。
    entries: Vec<(String, Cached)>,
    bytes: usize,
}

impl ProviderCache {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn get(&self, key: CacheKey) -> Option<&Cached> {
        let slot = key.slot();
        self.entries.iter().find(|(at, _)| *at == slot).map(|(_, page)| page)
    }

    /// `false`＝这一格比整个缓存还大，当场不存。已经留着的那些一位都不动：
    /// 一次翻页失败不该把用户已经看得见的目录清空。
    pub(crate) fn insert(&mut self, key: CacheKey, page: Cached) -> bool {
        let slot = key.slot();
        let cost = slot.len() + size_of::<Cached>() + page.estimated_bytes();
        if cost > MAX_CACHE_BYTES {
            return false;
        }
        self.drop_slot(&slot);
        self.entries.push((slot, page));
        self.bytes += cost;
        self.trim();
        true
    }

    pub(crate) fn invalidate(&mut self, key: CacheKey) -> bool {
        let slot = key.slot();
        let had = self.entries.iter().any(|(at, _)| *at == slot);
        self.drop_slot(&slot);
        had
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn drop_slot(&mut self, slot: &str) {
        if let Some(at) = self.entries.iter().position(|(at, _)| *at == slot) {
            self.bytes -=
                slot.len() + size_of::<Cached>() + self.entries[at].1.estimated_bytes();
            self.entries.remove(at);
        }
    }

    fn trim(&mut self) {
        while self.entries.len() > MAX_CACHE_ENTRIES || self.bytes > MAX_CACHE_BYTES {
            // 队首那一枚就是"最先到这儿"的那一位。刚写进去的那位在队尾，永远最后才被赶走。
            let Some((slot, _)) = self.entries.first() else { break };
            let slot = slot.clone();
            self.drop_slot(&slot);
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

    fn folder(folder_id: i64) -> CacheKey {
        CacheKey::Children { folder_id }
    }

    /// 一枚带 4 KiB 标题的页：项数远没到上限时字节那把尺就该先响。
    fn wide_note(note_id: i64) -> ProviderNode {
        note(note_id, 1, &"标".repeat(1400))
    }

    #[test]
    fn the_byte_ceiling_evicts_before_the_count_one_does() {
        let mut cache = ProviderCache::default();
        for at in 0..400 {
            cache.insert(folder(at), directory(vec![wide_note(at)]));
        }
        assert!(cache.bytes() <= MAX_CACHE_BYTES, "字节账越界：{}", cache.bytes());
        assert!(
            cache.len() < MAX_CACHE_ENTRIES,
            "只数了条数：{} 项却已经 {} 字节",
            cache.len(),
            cache.bytes()
        );
        // 队尾那位是最后写进去的，它必须还在；被赶走的应是队首那一批。
        assert!(cache.get(folder(399)).is_some());
        assert!(cache.get(folder(0)).is_none());
    }

    #[test]
    fn the_count_ceiling_evicts_the_oldest_first() {
        let mut cache = ProviderCache::default();
        for at in 0..(MAX_CACHE_ENTRIES + 88) {
            cache.insert(folder(at as i64), directory(vec![]));
        }
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(
            cache.bytes() * 8 < MAX_CACHE_BYTES,
            "条数那把尺独自生效时字节账应远没到上限，实际 {}",
            cache.bytes()
        );
        for at in 0..88 {
            assert!(cache.get(folder(at as i64)).is_none(), "{at} 没被赶走");
        }
        assert!(cache.get(folder(88)).is_some());
        assert!(cache.get(folder((MAX_CACHE_ENTRIES + 87) as i64)).is_some());
    }

    #[test]
    fn a_page_bigger_than_the_whole_cache_is_not_stored() {
        let mut cache = ProviderCache::default();
        assert!(cache.insert(CacheKey::Roots, directory(vec![note(1, 1, "甲")])));
        let before = (cache.len(), cache.bytes());

        let huge = directory(vec![note(9, 1, &"x".repeat(MAX_CACHE_BYTES + (1 << 16)))]);
        assert!(!cache.insert(folder(7), huge), "单格比整缓存还大却被留下了");
        assert_eq!((cache.len(), cache.bytes()), before, "留不下的那一格动了别人");
        assert!(cache.get(folder(7)).is_none());
        assert!(cache.get(CacheKey::Roots).is_some(), "已有目录被一次失败翻页清空");
    }

    /// 同键重写：值换新的、条数不双计，且这一枚挪到队尾——刚翻页看到的目录下一轮就被赶走是不合理的。
    #[test]
    fn rewriting_a_key_refreshes_it_and_moves_it_to_the_tail() {
        let mut cache = ProviderCache::default();
        cache.insert(folder(1), directory(vec![note(1, 1, &"y".repeat(64))]));
        let small = (cache.len(), cache.bytes());
        cache.insert(folder(1), directory(vec![]));
        assert_eq!(cache.len(), small.0, "同键重写多出一格");
        assert!(cache.bytes() < small.1, "字节账跟着旧值走：{}", cache.bytes());
        assert_eq!(cache.get(folder(1)).unwrap().as_directory().unwrap().nodes.len(), 0);

        let mut cache = ProviderCache::default();
        for at in 0..MAX_CACHE_ENTRIES as i64 {
            cache.insert(folder(at), directory(vec![]));
        }
        cache.insert(folder(0), directory(vec![]));
        cache.insert(folder(MAX_CACHE_ENTRIES as i64), directory(vec![]));
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(cache.get(folder(0)).is_some(), "刷新过的那位还被当最旧的赶走");
        assert!(cache.get(folder(1)).is_none(), "该被赶走的是队首那位");
    }

    #[test]
    fn an_empty_directory_is_still_a_cached_reading() {
        let mut cache = ProviderCache::default();
        assert!(cache.insert(CacheKey::Roots, directory(vec![])));
        let page = cache.get(CacheKey::Roots).expect("空目录也是一次合法读数").as_directory().expect("键形与值形不符");
        assert!(page.nodes.is_empty() && page.next_cursor.is_none());
    }

    #[test]
    fn invalidating_one_folder_leaves_roots_and_siblings() {
        let mut cache = ProviderCache::default();
        cache.insert(CacheKey::Roots, directory(vec![note(1, 1, "甲")]));
        cache.insert(folder(1), directory(vec![note(2, 1, "乙")]));
        cache.insert(folder(2), directory(vec![note(3, 2, "丙")]));
        let recent = Cached::Recent(RecentPage {
            as_of_ms: 5,
            items: vec![RecentEntry { node: note(2, 1, "乙"), opened_at_ms: 4 }],
        });
        assert!(cache.insert(CacheKey::Recent, recent));

        assert!(cache.invalidate(folder(1)));
        assert!(!cache.invalidate(folder(9)), "没 cached 过的键报成失效了");
        assert_eq!(cache.len(), 3);
        assert!(cache.get(folder(1)).is_none());
        assert!(cache.get(CacheKey::Roots).is_some() && cache.get(folder(2)).is_some());
        let kept = cache.get(CacheKey::Recent).unwrap().as_recent().expect("recent 键该读出 recent 形");
        assert_eq!((kept.as_of_ms, kept.items.len()), (5, 1));
        assert!(cache.get(CacheKey::Recent).unwrap().as_directory().is_none());

        let before = cache.bytes();
        cache.clear();
        assert_eq!((cache.len(), cache.bytes()), (0, 0));
        assert!(before > 0, "清空前字节账是空的，说明它本来就没在算");
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
