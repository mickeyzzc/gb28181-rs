//! Test doubles for host-side seams.
//!
//! [`MockFrameHub`] is a reference [`FrameSource`] implementation with the
//! same bounded-channel, drop-on-full semantics as the original production
//! frame hub — useful for exercising the server without a capture pipeline,
//! both in this crate's tests and in host test suites.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;

use crate::frame::{AccessUnit, FrameSource, FrameSubscription};

/// Bounded-channel frame fan-out hub for tests.
///
/// `write` drops the unit for any subscriber whose channel buffer is full
/// (never blocks the producer), mirroring the reference hub behaviour the
/// server expects.
pub struct MockFrameHub {
    subscribers: Mutex<HashMap<u64, mpsc::SyncSender<AccessUnit>>>,
    next_id: AtomicU64,
}

impl MockFrameHub {
    /// Creates an empty hub.
    pub fn new() -> Self {
        MockFrameHub {
            subscribers: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Writes an access unit to all subscribers (non-blocking, drops on full).
    pub fn write(&self, au: AccessUnit) {
        let guard = self.subscribers.lock().unwrap();
        for sender in guard.values() {
            let _ = sender.try_send(au.clone());
        }
    }

    /// Number of currently registered subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }
}

impl Default for MockFrameHub {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSource for MockFrameHub {
    fn subscribe_with_capacity(&self, capacity: usize) -> FrameSubscription {
        let (tx, rx) = mpsc::sync_channel(capacity);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.subscribers.lock().unwrap().insert(id, tx);
        FrameSubscription { id, receiver: rx }
    }

    fn unsubscribe(&self, id: u64) {
        self.subscribers.lock().unwrap().remove(&id);
    }
}
