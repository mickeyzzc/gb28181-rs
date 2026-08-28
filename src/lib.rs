//! GB/T 28181-2016/2022 Device library for Rust.
//!
//! This crate provides an implementation of the Chinese national standard
//! for video surveillance systems, GB/T 28181-2016 and GB/T 28181-2022,
//! operating as a **device** that registers with a SIP platform.
//!
//! Extracted verbatim from the production implementation in
//! `mibee-eye-raspi-rs`, where it has been hardened against real GB28181
//! platforms (digest-auth URI matching, Via branch uniqueness, local-IP
//! detection, MANSCDP attribute/element dual forms — see the interop notes
//! in that repository and issues #1–#12 of the shared camera tracker).
//!
//! ## Architecture
//!
//! - **Device ID**: 20-digit national standard format ([`device_id`])
//! - **SIP signaling**: Hand-written parser/serializer for the SIP subset
//!   used by GB/T 28181 (REGISTER, INVITE, MESSAGE, BYE, etc.) ([`sip`])
//! - **Digest Auth**: RFC 7616 Digest authentication for SIP REGISTER ([`sip`])
//! - **PS (Program Stream)**: Mux H.264 NAL units into MPEG-2 Program Stream
//!   for RTP media transport, plus parsing ([`ps`])
//! - **SipDeviceClient**: Manages device registration with a SIP platform ([`client`])
//! - **RtpPusher**: Constructs and sends RTP packets to a destination ([`rtp_pusher`])
//! - **Gb28181Server**: Full device server — registration lifecycle, catalog,
//!   INVITE-driven live streaming and playback, keepalive ([`server`])
//!
//! ## Host integration seams
//!
//! The crate is transport- and storage-agnostic; the host injects:
//!
//! - [`FrameSource`] — live video access units (implement over your frame hub)
//! - [`RecordingSource`] — recorded-segment index for RecordInfo/playback
//! - [`config::Gb28181Config`] — connection settings (serde, TOML/JSON-friendly)
//!
//! ## Segment format
//!
//! The [`segment`] module implements the reference recording format used by
//! the playback path: a bare Annex-B H.264 bytestream with a per-frame
//! `<segment>.ts.jsonl` sidecar of millisecond timestamps.

use std::sync::atomic::AtomicBool;

pub mod client;
pub mod config;
pub mod device_id;
pub mod frame;
pub mod manscdp;
pub mod mock;
pub mod playback;
pub mod ps;
pub mod rtp_pusher;
pub mod segment;
pub mod server;
pub mod sip;

pub use client::{parse_401_challenge, parse_invite, InviteInfo, SipDeviceClient};
pub use config::Gb28181Config;
pub use device_id::device_types;
pub use device_id::{format_device_id, parse_device_id, DeviceIdParts};
pub use frame::{AccessUnit, FrameSource, FrameSubscription, Nalu};
pub use manscdp::{ChannelItem, DeviceItem, DeviceList, Notify, Query, Response};
pub use ps::{
    parse_pes_packet, parse_ps_pack_header, parse_ps_to_h264, parse_ps_to_nal_units, PesPacket,
    PsPackHeader,
};
pub use rtp_pusher::{RtpPusher, RtpStreamInfo};
pub use segment::{read_segment, RecordedAu};
pub use server::Gb28181Server;
pub use sip::{
    build_bye_request, build_digest_auth, build_invite_response, build_register_request,
    parse_digest_auth, DigestAuthParams, SdpMedia, SdpSession, SessionType, SipMessage, SipMethod,
    SipStatusCode, Transport,
};

/// Shared flag reflecting whether local recording is currently active.
///
/// Set by the host recording writer (see [`set_record_active`]); read by the
/// DeviceStatus response builder to emit `<Record>ON/OFF</Record>`.
pub static RECORD_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Update the shared local-recording flag.
pub fn set_record_active(active: bool) {
    RECORD_ACTIVE.store(active, std::sync::atomic::Ordering::SeqCst);
}

/// Metadata for a single recorded segment, as surfaced to RecordInfo queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentMeta {
    /// Path to the segment file, relative to the recording root.
    pub file: String,
    /// Wall-clock start time in milliseconds since the Unix epoch.
    pub start_ms: u64,
    /// Wall-clock end time in milliseconds since the Unix epoch.
    pub end_ms: u64,
}

/// Source of recorded-segment metadata for RecordInfo range queries and
/// playback INVITEs.
///
/// Implemented by the host over its recording index. Segment *reading*
/// itself uses the crate's [`segment`] reference format ([`read_segment`]).
pub trait RecordingSource: Send + Sync {
    /// Return all segments overlapping the inclusive `[start_ms, end_ms]` range.
    fn lookup(&self, start_ms: u64, end_ms: u64) -> Vec<SegmentMeta>;
    /// Resolve a relative segment `file` to an absolute path for reading.
    ///
    /// Defaults to the path as-is (relative to the process cwd); adapters
    /// rooted at a recording directory must override this.
    fn resolve_path(&self, file: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(file)
    }
}
