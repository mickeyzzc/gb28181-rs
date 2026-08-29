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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn nalu(nalu_type: u8) -> crate::frame::Nalu {
        crate::frame::Nalu {
            nalu_type,
            data: vec![nalu_type, 0xAA, 0xBB],
            is_idr: nalu_type == 5,
            is_sps: nalu_type == 7,
            is_pps: nalu_type == 8,
            is_aud: nalu_type == 9,
        }
    }

    fn au(key_frame: bool) -> AccessUnit {
        AccessUnit {
            nalus: if key_frame {
                vec![nalu(7), nalu(5)]
            } else {
                vec![nalu(1)]
            },
            timestamp: Instant::now(),
            is_key_frame: key_frame,
        }
    }

    #[test]
    fn subscribe_and_unsubscribe_track_subscriber_count() {
        let hub = MockFrameHub::new();
        assert_eq!(hub.subscriber_count(), 0);
        let sub1 = hub.subscribe_with_capacity(2);
        assert_eq!(hub.subscriber_count(), 1);
        let sub2 = hub.subscribe_with_capacity(2);
        assert_eq!(hub.subscriber_count(), 2);
        hub.unsubscribe(sub1.id);
        assert_eq!(hub.subscriber_count(), 1);
        hub.unsubscribe(sub2.id);
        assert_eq!(hub.subscriber_count(), 0);
    }

    #[test]
    fn subscription_ids_are_unique() {
        let hub = MockFrameHub::new();
        let a = hub.subscribe_with_capacity(1);
        let b = hub.subscribe_with_capacity(1);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn write_fans_out_clones_to_all_subscribers() {
        let hub = MockFrameHub::new();
        let sub1 = hub.subscribe_with_capacity(4);
        let sub2 = hub.subscribe_with_capacity(4);

        hub.write(au(true));
        hub.write(au(false));

        for rx in [&sub1.receiver, &sub2.receiver] {
            let got = rx.recv_timeout(Duration::from_secs(1)).expect("keyframe");
            assert!(got.is_key_frame);
            assert_eq!(got.nalus.len(), 2);
            let got = rx.recv_timeout(Duration::from_secs(1)).expect("p-frame");
            assert!(!got.is_key_frame);
            assert_eq!(got.nalus.len(), 1);
        }
    }

    #[test]
    fn write_drops_on_full_channel_and_never_blocks() {
        let hub = MockFrameHub::new();
        let sub = hub.subscribe_with_capacity(1);

        hub.write(au(true)); // fills the buffer
        hub.write(au(false)); // must be dropped, not block the producer
        hub.write(au(false)); // ditto

        let got = sub
            .receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first");
        assert!(got.is_key_frame);
        assert!(
            sub.receiver
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "overflow frames must be dropped, not queued"
        );
    }

    #[test]
    fn unsubscribe_closes_the_channel() {
        let hub = MockFrameHub::new();
        let sub = hub.subscribe_with_capacity(1);
        hub.unsubscribe(sub.id);
        // Sender dropped → recv fails immediately instead of blocking.
        assert!(sub
            .receiver
            .recv_timeout(Duration::from_millis(50))
            .is_err());
    }
}
