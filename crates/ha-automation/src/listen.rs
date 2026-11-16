//! Who hears a state change: listeners by entity id, by domain, or for
//! every change (HA's `async_track_state_change_event`,
//! `_async_track_state_added_domain` and the bus listener a template that
//! iterates every state needs), called in the order they attached.
//!
//! The state feed reaches them through one task. When a listener can't
//! decide at once (a template has to be rendered, on a render thread), it
//! takes a [`Guard`]; the feed's task waits for every guard to drop before
//! it hands out the next change. HA handles a change completely, in its
//! event loop, before the next one, and so does this.

use ha_core::{Event, EventKind};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::Notify;

/// Called with the change's sequence number and the change.
pub(crate) type Callback = Arc<dyn Fn(u64, &Arc<Event>) + Send + Sync>;

/// A listener: (id, the first change it hears, callback).
type Entry = (u64, u64, Callback);

enum Key {
    Entity(String),
    Domain(String),
    All,
}

#[derive(Default)]
pub(crate) struct Listeners {
    next: Mutex<u64>,
    map: Mutex<Map>,
    busy: Arc<Busy>,
}

#[derive(Default)]
struct Map {
    by_entity: HashMap<String, Vec<Entry>>,
    by_domain: HashMap<String, Vec<Entry>>,
    all: Vec<Entry>,
}

impl Map {
    fn list(&mut self, key: &Key) -> Option<&mut Vec<Entry>> {
        match key {
            Key::Entity(e) => self.by_entity.get_mut(e),
            Key::Domain(d) => self.by_domain.get_mut(d),
            Key::All => Some(&mut self.all),
        }
    }
}

/// A listener; dropping it unsubscribes.
pub(crate) struct Listening {
    listeners: Weak<Listeners>,
    key: Key,
    id: u64,
}

impl Drop for Listening {
    fn drop(&mut self) {
        if let Some(l) = self.listeners.upgrade() {
            let mut map = l.map.lock();
            let empty = map.list(&self.key).is_some_and(|v| {
                v.retain(|(id, _, _)| *id != self.id);
                v.is_empty()
            });
            match &self.key {
                Key::Entity(e) if empty => drop(map.by_entity.remove(e)),
                Key::Domain(d) if empty => drop(map.by_domain.remove(d)),
                _ => {}
            }
        }
    }
}

impl Listeners {
    fn add(self: &Arc<Self>, key: Key, since: u64, cb: Callback) -> Listening {
        let id = {
            let mut n = self.next.lock();
            *n += 1;
            *n
        };
        let entry = (id, since, cb);
        let mut map = self.map.lock();
        match &key {
            Key::Entity(e) => map.by_entity.entry(e.clone()).or_default().push(entry),
            Key::Domain(d) => map.by_domain.entry(d.clone()).or_default().push(entry),
            Key::All => map.all.push(entry),
        }
        Listening {
            listeners: Arc::downgrade(self),
            key,
            id,
        }
    }

    /// Listen to an entity's changes from `since` on (the state feed's
    /// sequence when attaching: a change made earlier but still queued
    /// isn't heard, as in HA, where it was dispatched before).
    pub(crate) fn listen(self: &Arc<Self>, entity: &str, since: u64, cb: Callback) -> Listening {
        self.add(Key::Entity(entity.to_owned()), since, cb)
    }

    /// Listen to the changes of every entity of a domain.
    pub(crate) fn listen_domain(
        self: &Arc<Self>,
        domain: &str,
        since: u64,
        cb: Callback,
    ) -> Listening {
        self.add(Key::Domain(domain.to_owned()), since, cb)
    }

    /// Listen to every change.
    pub(crate) fn listen_all(self: &Arc<Self>, since: u64, cb: Callback) -> Listening {
        self.add(Key::All, since, cb)
    }

    /// A guard the feed waits for before the next change.
    pub(crate) fn guard(&self) -> Guard {
        self.busy.count.fetch_add(1, Ordering::AcqRel);
        Guard(self.busy.clone())
    }

    /// Wait until no guard is held.
    pub(crate) async fn idle(&self) {
        self.busy.idle().await;
    }

    /// Every listener of the change, in the order they attached. Whether
    /// there were any.
    pub(crate) fn dispatch(&self, seq: u64, ev: &Arc<Event>) -> bool {
        let EventKind::StateChanged { entity_id, .. } = &ev.kind else {
            return false;
        };
        let mut cbs: Vec<(u64, Callback)> = Vec::new();
        {
            let map = self.map.lock();
            let lists = [
                map.by_entity.get(entity_id.as_str()),
                map.by_domain.get(entity_id.domain()),
                Some(&map.all),
            ];
            for v in lists.into_iter().flatten() {
                cbs.extend(
                    v.iter()
                        .filter(|(_, since, _)| seq >= *since)
                        .map(|(id, _, cb)| (*id, cb.clone())),
                );
            }
        }
        if cbs.is_empty() {
            return false;
        }
        cbs.sort_unstable_by_key(|(id, _)| *id);
        for (_, cb) in cbs {
            cb(seq, ev);
        }
        true
    }
}

/// Guards held.
#[derive(Default)]
struct Busy {
    count: AtomicUsize,
    notify: Notify,
}

impl Busy {
    async fn idle(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// Work a change started that the next change waits for; dropping it says
/// the work is done.
pub(crate) struct Guard(Arc<Busy>);

impl Drop for Guard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.notify.notify_waiters();
        }
    }
}
