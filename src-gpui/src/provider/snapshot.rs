//! UI 只读 provider 快照：共享一棵缓存，加载与失败状态有界，后台只发最新一版。
use std::sync::Arc;

use gpui::{App, AppContext, Context, Entity, EventEmitter, Global, Task};

use super::cache::{CacheKey, Cached, MAX_CACHE_ENTRIES};
use super::link::LinkEvent;
use super::worker::{Intent, Pump, Worker, WorkerSnapshot};

const MAX_ERROR_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PageStatus {
    Loading,
    Failed(String),
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderSnapshot {
    worker: WorkerSnapshot,
    pages: Arc<Vec<(CacheKey, PageStatus)>>,
}

impl ProviderSnapshot {
    fn empty() -> Self {
        Self {
            worker: Pump::new().snapshot(),
            pages: Arc::new(Vec::new()),
        }
    }

    pub(crate) fn online(&self) -> bool {
        self.worker.online
    }

    pub(crate) fn generation(&self) -> u64 {
        self.worker.generation
    }

    pub(crate) fn page(&self, key: &CacheKey) -> Option<&Cached> {
        self.worker.cache.get(key)
    }

    pub(crate) fn page_status(&self, key: &CacheKey) -> Option<&PageStatus> {
        self.pages
            .iter()
            .find(|(at, _)| at == key)
            .map(|(_, status)| status)
    }

    fn mark_requested(&mut self, key: &CacheKey) -> bool {
        if self.worker.cache.get(key).is_some() {
            if self.page_status(key).is_none() {
                return false;
            }
            self.remove_status(key);
            return true;
        }
        if matches!(self.page_status(key), Some(PageStatus::Loading)) {
            return false;
        }
        self.set_status(key, PageStatus::Loading);
        true
    }

    fn apply_event(&mut self, event: &LinkEvent) {
        match event {
            LinkEvent::PageReady(key) | LinkEvent::Invalidated(key) => self.remove_status(key),
            LinkEvent::PageFailed(key, reason) => {
                self.set_status(key, PageStatus::Failed(bounded_error(reason)))
            }
            LinkEvent::CommandDone(_) | LinkEvent::CommandFailed(_, _) | LinkEvent::Broken(_) => {}
        }
    }

    fn set_status(&mut self, key: &CacheKey, status: PageStatus) {
        let pages = Arc::make_mut(&mut self.pages);
        if let Some((_, old)) = pages.iter_mut().find(|(at, _)| at == key) {
            *old = status;
            return;
        }
        if pages.len() >= MAX_CACHE_ENTRIES {
            pages.remove(0);
        }
        pages.push((key.clone(), status));
    }

    fn remove_status(&mut self, key: &CacheKey) {
        Arc::make_mut(&mut self.pages).retain(|(at, _)| at != key);
    }
}

fn bounded_error(reason: &str) -> String {
    let mut end = reason.len().min(MAX_ERROR_BYTES);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_string()
}

#[derive(Clone, Debug)]
pub(crate) struct ProviderEvent(pub(crate) LinkEvent);

pub(crate) struct ProviderStore {
    worker: Option<Worker>,
    snapshot: Arc<ProviderSnapshot>,
    pump: Option<Task<()>>,
}

impl EventEmitter<ProviderEvent> for ProviderStore {}

impl ProviderStore {
    fn new() -> Self {
        Self {
            worker: None,
            snapshot: Arc::new(ProviderSnapshot::empty()),
            pump: None,
        }
    }

    pub(crate) fn snapshot(&self) -> &ProviderSnapshot {
        &self.snapshot
    }

    pub(crate) fn ask(&mut self, intent: Intent, cx: &mut Context<Self>) -> Result<(), Intent> {
        if self.worker.is_none() {
            let worker = match Worker::start(Pump::new()) {
                Ok(worker) => worker,
                Err(_) => return Err(intent),
            };
            self.install(worker, cx);
        }
        let page = match &intent {
            Intent::Page(key) => Some(key.clone()),
            _ => None,
        };
        self.worker.as_ref().expect("刚装上的 worker").ask(intent)?;
        if let Some(key) = page {
            if Arc::make_mut(&mut self.snapshot).mark_requested(&key) {
                cx.notify();
            }
        }
        Ok(())
    }

    fn install(&mut self, worker: Worker, cx: &mut Context<Self>) {
        let mut snapshots = worker.snapshot_watch();
        self.worker = Some(worker);
        self.drain(cx);
        self.pump = Some(cx.spawn(async move |this, cx| {
            loop {
                if snapshots.changed().await.is_err() {
                    return;
                }
                if this.update(cx, |store, cx| store.drain(cx)).is_err() {
                    return;
                }
            }
        }));
    }

    fn drain(&mut self, cx: &mut Context<Self>) {
        let Some(worker) = self.worker.as_mut() else {
            return;
        };
        let events = worker.take_events();
        let latest = worker.take_snapshot();
        let changed = !events.is_empty()
            || latest.generation != self.snapshot.worker.generation
            || latest.online != self.snapshot.worker.online
            || latest.cache.revision() != self.snapshot.worker.cache.revision();
        let mut next = (*self.snapshot).clone();
        for event in &events {
            next.apply_event(event);
        }
        next.worker = latest;
        self.snapshot = Arc::new(next);
        for event in events {
            cx.emit(ProviderEvent(event));
        }
        if changed {
            cx.notify();
        }
    }
}

#[derive(Clone)]
struct SharedProvider(Entity<ProviderStore>);

impl Global for SharedProvider {}

pub(crate) fn shared(cx: &mut App) -> Entity<ProviderStore> {
    if let Some(shared) = cx.try_global::<SharedProvider>() {
        return shared.0.clone();
    }
    let store = cx.new(|_| ProviderStore::new());
    cx.set_global(SharedProvider(store.clone()));
    store
}

#[cfg(test)]
mod tests {
    use super::super::cache::{Cached, ProviderCache};
    use super::super::node::Page;
    use super::*;

    fn roots() -> CacheKey {
        CacheKey::Roots { cursor: None }
    }

    fn folder(folder_id: i64) -> CacheKey {
        CacheKey::Children {
            folder_id,
            cursor: None,
        }
    }

    fn snapshot() -> ProviderSnapshot {
        ProviderSnapshot::empty()
    }

    fn cache_with_roots() -> Arc<ProviderCache> {
        let mut cache = ProviderCache::default();
        assert!(cache.insert(
            &roots(),
            Cached::Directory(Page {
                nodes: Vec::new(),
                next_cursor: None
            })
        ));
        Arc::new(cache)
    }

    #[test]
    fn a_cached_request_does_not_leave_a_loading_state() {
        let mut snapshot = snapshot();
        snapshot.worker.cache = cache_with_roots();
        snapshot.apply_event(&LinkEvent::PageFailed(roots(), "上一次没等到".to_string()));
        assert!(snapshot.mark_requested(&roots()));
        assert!(!snapshot.mark_requested(&roots()), "重复命中不该再动状态账");
        assert!(snapshot.page_status(&roots()).is_none());
        assert!(snapshot.page(&roots()).is_some());
    }

    #[test]
    fn a_failed_wait_keeps_the_last_visible_page() {
        let mut snapshot = snapshot();
        snapshot.worker.cache = cache_with_roots();
        snapshot.mark_requested(&folder(4));
        assert_eq!(snapshot.page_status(&folder(4)), Some(&PageStatus::Loading));
        snapshot.apply_event(&LinkEvent::PageFailed(folder(4), "离线".to_string()));
        assert_eq!(
            snapshot.page_status(&folder(4)),
            Some(&PageStatus::Failed("离线".to_string()))
        );
        assert!(snapshot.page(&roots()).is_some());
        assert!(snapshot.page(&folder(4)).is_none());
    }

    #[test]
    fn a_ready_page_clears_the_failure_without_copying_the_cache() {
        let mut snapshot = snapshot();
        snapshot.apply_event(&LinkEvent::PageFailed(roots(), "超时".to_string()));
        snapshot.worker.cache = cache_with_roots();
        let cache = snapshot.worker.cache.clone();
        snapshot.apply_event(&LinkEvent::PageReady(roots()));
        assert!(snapshot.page_status(&roots()).is_none());
        assert!(Arc::ptr_eq(&cache, &snapshot.worker.cache));
    }

    #[test]
    fn failure_text_and_the_state_book_are_bounded() {
        let mut snapshot = snapshot();
        let reason = "错".repeat(MAX_ERROR_BYTES);
        for folder_id in 0..=MAX_CACHE_ENTRIES {
            snapshot.apply_event(&LinkEvent::PageFailed(
                folder(folder_id as i64),
                reason.clone(),
            ));
        }
        assert_eq!(snapshot.pages.len(), MAX_CACHE_ENTRIES);
        assert!(snapshot.page_status(&folder(0)).is_none());
        let status = snapshot
            .page_status(&folder(MAX_CACHE_ENTRIES as i64))
            .unwrap();
        let PageStatus::Failed(text) = status else {
            panic!("这一格该是失败")
        };
        assert!(text.len() <= MAX_ERROR_BYTES);
        assert!(text.is_char_boundary(text.len()));
    }

    #[test]
    fn invalidation_clears_only_the_named_page_state() {
        let mut snapshot = snapshot();
        snapshot.mark_requested(&roots());
        snapshot.mark_requested(&folder(9));
        snapshot.apply_event(&LinkEvent::Invalidated(roots()));
        assert!(snapshot.page_status(&roots()).is_none());
        assert_eq!(snapshot.page_status(&folder(9)), Some(&PageStatus::Loading));
    }

    #[test]
    fn command_events_leave_the_visible_snapshot_untouched() {
        let mut snapshot = snapshot();
        snapshot.worker.cache = cache_with_roots();
        snapshot.mark_requested(&folder(3));
        let cache = snapshot.worker.cache.clone();
        let pages = snapshot.pages.clone();
        snapshot.apply_event(&LinkEvent::CommandDone(
            super::super::session::Command::ClearRecent,
        ));
        assert!(Arc::ptr_eq(&cache, &snapshot.worker.cache));
        assert!(Arc::ptr_eq(&pages, &snapshot.pages));
    }
}
