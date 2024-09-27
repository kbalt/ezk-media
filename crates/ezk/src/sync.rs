//! Media stream synchronization tools

use parking_lot::Mutex;
use slotmap::{DefaultKey, SlotMap};
use std::sync::Arc;

/// Defines a common point of synchronization for media streams between nodes
#[derive(Debug, Default, Clone)]
pub struct CommonSyncPoint {
    shared: Arc<Mutex<Shared>>,
}

#[derive(Debug, Default)]
struct Shared {
    map: SlotMap<DefaultKey, Entry>,
}

struct Entry {
    /// First and last seen timestamp
    ts: Option<(u64, u64)>,
    /// Is the handle currently waiting for a notify
    waiting: bool,

    /// Should this handle wait for other blockers that lag behind
    wait_for_others: bool,

    notify: Box<dyn FnMut()>,
}

impl CommonSyncPoint {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_handle(&self, notify: Box<dyn FnMut()>, wait_for_others: bool) -> SyncHandle {
        let key = self.shared.lock().map.insert(Entry {
            ts: None,
            notify,
            waiting: false,
            wait_for_others,
        });

        SyncHandle {
            key,
            shared: self.shared.clone(),
        }
    }
}

impl Shared {
    /// Unblock lowest entries skipping over origin
    fn maybe_unblock_lowest(&mut self, origin: DefaultKey) {
        let lowest_ts = self.map.values().filter_map(|e| e.ts()).min();

        let Some(lowest_ts) = lowest_ts else { return };

        for (key, entry) in &mut self.map {
            if key == origin || !entry.waiting {
                continue;
            }

            let Some(ts) = entry.ts() else {
                continue;
            };

            if ts == lowest_ts {
                entry.waiting = false;
                (entry.notify)();
            }
        }
    }
}

impl Entry {
    fn update_last_ts(&mut self, ts: u64) -> u64 {
        match &mut self.ts {
            Some((first_seen, last_seen)) => {
                *last_seen = ts;
                ts.saturating_sub(*first_seen)
            }
            None => {
                self.ts = Some((ts, ts));
                0
            }
        }
    }

    fn ts(&self) -> Option<u64> {
        self.ts
            .map(|(first_seen, last_seen)| last_seen.saturating_sub(first_seen))
    }
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("ts", &self.ts)
            .field("waiting", &self.waiting)
            .field("wait_for_others", &self.wait_for_others)
            .finish()
    }
}

pub struct SyncHandle {
    key: DefaultKey,
    shared: Arc<Mutex<Shared>>,
}

#[derive(Debug, PartialEq)]
pub enum ReportAction {
    Continue,
    WaitForNotify,
}

impl SyncHandle {
    /// Report a new frame with the given timestamp
    #[must_use]
    pub fn report_new_frame(&self, new_ts: u64) -> ReportAction {
        let mut shared = self.shared.lock();

        let new_ts = shared.map[self.key].update_last_ts(new_ts);
        shared.maybe_unblock_lowest(self.key);

        if shared.map[self.key].wait_for_others {
            if let Some(lowest_ts) = shared.map.values().filter_map(|e| e.ts()).min() {
                if lowest_ts < new_ts {
                    shared.map[self.key].waiting = true;
                    return ReportAction::WaitForNotify;
                }
            }
        }

        ReportAction::Continue
    }
}

impl Drop for SyncHandle {
    fn drop(&mut self) {
        let mut shared = self.shared.lock();

        let removed_entry = shared
            .map
            .remove(self.key)
            .expect("self.key must always be in shared.map");

        if removed_entry.ts().is_none() {
            return;
        };

        shared.maybe_unblock_lowest(self.key);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn notify_counter() -> (Box<dyn FnMut()>, Arc<AtomicU32>) {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        (
            Box::new(move || {
                counter_clone.fetch_add(1, Ordering::Relaxed);
            }),
            counter,
        )
    }

    #[test]
    fn sync_x2_scenario1() {
        let p = CommonSyncPoint::new();

        let (notify1, counter1) = notify_counter();
        let (notify2, counter2) = notify_counter();

        let handle1 = p.create_handle(notify1, true);
        let handle2 = p.create_handle(notify2, true);

        assert_eq!(handle1.report_new_frame(0), ReportAction::Continue);
        assert_eq!(handle2.report_new_frame(0), ReportAction::Continue);

        assert_eq!(handle1.report_new_frame(1), ReportAction::WaitForNotify);
        assert_eq!(handle2.report_new_frame(2), ReportAction::WaitForNotify);
        assert_eq!(counter1.load(Ordering::Relaxed), 1);
        assert_eq!(counter2.load(Ordering::Relaxed), 0);

        assert_eq!(handle1.report_new_frame(3), ReportAction::WaitForNotify);
        assert_eq!(counter1.load(Ordering::Relaxed), 1);
        assert_eq!(counter2.load(Ordering::Relaxed), 1);

        assert_eq!(handle2.report_new_frame(2), ReportAction::Continue);
        assert_eq!(counter1.load(Ordering::Relaxed), 1);
        assert_eq!(counter2.load(Ordering::Relaxed), 1);

        assert_eq!(handle2.report_new_frame(3), ReportAction::Continue);
        assert_eq!(counter1.load(Ordering::Relaxed), 2);
        assert_eq!(counter2.load(Ordering::Relaxed), 1);

        assert_eq!(handle2.report_new_frame(4), ReportAction::WaitForNotify);

        assert_eq!(handle1.report_new_frame(4), ReportAction::Continue);
        assert_eq!(counter1.load(Ordering::Relaxed), 2);
        assert_eq!(counter2.load(Ordering::Relaxed), 2);
        assert_eq!(handle2.report_new_frame(4), ReportAction::Continue);

        assert_eq!(handle2.report_new_frame(5), ReportAction::WaitForNotify);
        drop(handle1);
        assert_eq!(counter2.load(Ordering::Relaxed), 3);
        assert_eq!(handle2.report_new_frame(5), ReportAction::Continue);
    }

    #[test]
    fn sync_x2_scenario2() {
        let p = CommonSyncPoint::new();

        let (notify, counter) = notify_counter();

        let handle1 = p.create_handle(Box::new(|| {}), false);
        let handle2 = p.create_handle(notify, true);

        assert_eq!(handle1.report_new_frame(0), ReportAction::Continue);
        assert_eq!(handle2.report_new_frame(0), ReportAction::Continue);
        assert_eq!(handle2.report_new_frame(0), ReportAction::Continue);
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        assert_eq!(handle2.report_new_frame(1), ReportAction::WaitForNotify);
        assert_eq!(handle1.report_new_frame(2), ReportAction::Continue);
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        assert_eq!(handle1.report_new_frame(3), ReportAction::Continue);
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        assert_eq!(handle2.report_new_frame(2), ReportAction::Continue);
        assert_eq!(handle2.report_new_frame(3), ReportAction::Continue);
        assert_eq!(handle2.report_new_frame(4), ReportAction::WaitForNotify);
        assert_eq!(handle1.report_new_frame(4), ReportAction::Continue);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        assert_eq!(handle2.report_new_frame(4), ReportAction::Continue);

        assert_eq!(handle2.report_new_frame(5), ReportAction::WaitForNotify);
        drop(handle1);
        assert_eq!(counter.load(Ordering::Relaxed), 3);
        assert_eq!(handle2.report_new_frame(5), ReportAction::Continue);
    }

    #[test]
    fn sync_x3_scenario1() {
        let p = CommonSyncPoint::new();

        let (notify1, counter1) = notify_counter();
        let (notify2, counter2) = notify_counter();
        let (notify3, counter3) = notify_counter();

        let handle1 = p.create_handle(notify1, false);
        let handle2 = p.create_handle(notify2, true);
        let handle3 = p.create_handle(notify3, true);

        assert_eq!(handle1.report_new_frame(0), ReportAction::Continue);
        assert_eq!(handle2.report_new_frame(0), ReportAction::Continue);
        assert_eq!(handle3.report_new_frame(0), ReportAction::Continue);
        assert_eq!(counter1.load(Ordering::Relaxed), 0);
        assert_eq!(counter2.load(Ordering::Relaxed), 0);
        assert_eq!(counter3.load(Ordering::Relaxed), 0);

        assert_eq!(handle1.report_new_frame(1), ReportAction::Continue);
        assert_eq!(handle2.report_new_frame(1), ReportAction::WaitForNotify);
        assert_eq!(handle3.report_new_frame(1), ReportAction::Continue);
        assert_eq!(counter1.load(Ordering::Relaxed), 0);
        assert_eq!(counter2.load(Ordering::Relaxed), 1);
        assert_eq!(counter3.load(Ordering::Relaxed), 0);

        assert_eq!(handle1.report_new_frame(3), ReportAction::Continue);
        assert_eq!(handle2.report_new_frame(2), ReportAction::WaitForNotify);
        assert_eq!(handle3.report_new_frame(3), ReportAction::WaitForNotify);
        assert_eq!(counter1.load(Ordering::Relaxed), 0);
        assert_eq!(counter2.load(Ordering::Relaxed), 2);
        assert_eq!(counter3.load(Ordering::Relaxed), 0);

        drop(handle2);
        assert_eq!(counter1.load(Ordering::Relaxed), 0);
        assert_eq!(counter2.load(Ordering::Relaxed), 2);
        assert_eq!(counter3.load(Ordering::Relaxed), 1);
    }
}
