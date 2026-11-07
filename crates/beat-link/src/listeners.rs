//! A small, thread-safe listener registry. Beat-link exposes dozens of listener
//! interfaces; in Rust each becomes a `ListenerSet<Event>` of boxed closures.

use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Handle used to remove a previously added listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListenerId(u64);

type Callback<E> = Arc<dyn Fn(&E) + Send + Sync>;

/// A set of callbacks that are all invoked when an event is delivered.
pub struct ListenerSet<E> {
    next: AtomicU64,
    listeners: RwLock<Vec<(ListenerId, Callback<E>)>>,
}

impl<E> Default for ListenerSet<E> {
    fn default() -> Self {
        ListenerSet { next: AtomicU64::new(1), listeners: RwLock::new(Vec::new()) }
    }
}

impl<E> ListenerSet<E> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a callback, returning a handle that can remove it again.
    pub fn add<F>(&self, f: F) -> ListenerId
    where
        F: Fn(&E) + Send + Sync + 'static,
    {
        let id = ListenerId(self.next.fetch_add(1, Ordering::Relaxed));
        self.listeners.write().push((id, Arc::new(f)));
        id
    }

    /// Remove a callback. Returns whether it was present.
    pub fn remove(&self, id: ListenerId) -> bool {
        let mut l = self.listeners.write();
        let before = l.len();
        l.retain(|(i, _)| *i != id);
        l.len() != before
    }

    /// How many listeners are registered.
    pub fn len(&self) -> usize {
        self.listeners.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Deliver an event to every listener. A panicking listener is logged and does
    /// not prevent delivery to the others (mirroring beat-link's catch-and-log).
    pub fn deliver(&self, event: &E) {
        let snapshot: Vec<Callback<E>> = self.listeners.read().iter().map(|(_, f)| f.clone()).collect();
        for f in snapshot {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(event))).is_err() {
                log::warn!("Listener panicked while handling event; continuing with remaining listeners.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn add_deliver_remove() {
        let set: ListenerSet<u32> = ListenerSet::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let id = set.add(move |v| {
            c.fetch_add(*v as usize, Ordering::SeqCst);
        });
        set.add(|_| panic!("boom"));
        set.deliver(&5);
        assert_eq!(count.load(Ordering::SeqCst), 5);
        assert!(set.remove(id));
        set.deliver(&5);
        assert_eq!(count.load(Ordering::SeqCst), 5);
    }
}
