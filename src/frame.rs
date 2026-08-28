//! Live frame integration seam.
//!
//! The device server consumes live video through [`FrameSource`] instead of
//! binding to any concrete frame hub, so hosts with their own capture
//! pipelines adapt with a thin wrapper (see the `mibee-eye-raspi-rs`
//! `AuHubFrameSource` adapter for the reference implementation over a
//! bounded-channel fan-out hub).
//!
//! Frames are plain data: [`Nalu`] payloads without Annex-B start codes,
//! grouped into [`AccessUnit`]s (one encoder frame each).

use std::sync::mpsc;
use std::time::Instant;

/// A single H.264 NAL unit (payload without start code).
#[derive(Debug, Clone, PartialEq)]
pub struct Nalu {
    /// NAL unit type (first byte & 0x1F).
    pub nalu_type: u8,
    /// Raw NALU data (without start code).
    pub data: Vec<u8>,
    /// True if type == 5 (IDR slice).
    pub is_idr: bool,
    /// True if type == 7 (SPS).
    pub is_sps: bool,
    /// True if type == 8 (PPS).
    pub is_pps: bool,
    /// True if type == 9 (AUD — Access Unit Delimiter).
    pub is_aud: bool,
}

/// A complete H.264 access unit (one or more NALUs forming a frame).
#[derive(Debug, Clone)]
pub struct AccessUnit {
    /// NAL units belonging to this access unit.
    pub nalus: Vec<Nalu>,
    /// Capture or presentation timestamp.
    pub timestamp: Instant,
    /// True if this access unit contains an IDR slice (key frame).
    pub is_key_frame: bool,
}

/// A live-frame subscription handed out by [`FrameSource::subscribe_with_capacity`].
///
/// Mirrors the shape of a bounded-channel subscription: an opaque `id` used
/// to unsubscribe, plus the receiving end of a bounded channel.
pub struct FrameSubscription {
    /// Subscriber identifier for [`FrameSource::unsubscribe`].
    pub id: u64,
    /// Receiving end of the bounded frame channel.
    pub receiver: mpsc::Receiver<AccessUnit>,
}

/// Live H.264 access-unit source the device server streams from.
///
/// Implement over your capture pipeline's fan-out hub. Semantics expected by
/// the server (matching the reference hub):
///
/// - `subscribe_with_capacity(2)` — small bounded buffer; the source drops
///   frames for slow subscribers rather than blocking the producer
/// - `unsubscribe(id)` — removes the subscriber and closes its channel
pub trait FrameSource: Send + Sync {
    /// Register a subscriber with a bounded channel of the given capacity.
    fn subscribe_with_capacity(&self, capacity: usize) -> FrameSubscription;
    /// Remove a subscriber by ID and close its channel.
    fn unsubscribe(&self, id: u64);
}
