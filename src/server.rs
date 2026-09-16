//! GB28181 SIP server - manages SIP signaling and media streaming.
//!
//! This module implements the device side of GB/T 28181, which:
//! 1. Registers with a SIP platform via SIP REGISTER with Digest authentication
//! 2. Sends periodic Keepalive MESSAGE to maintain registration
//! 3. Responds to SIP INVITE by streaming PS-over-RTP video
//! 4. Responds to SIP BYE by stopping media and unsubscribing from AuHub
//! 5. Handles inbound MESSAGE requests (Catalog, DeviceInfo queries)

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch, Mutex};

use crate::authenticator::RegisterAuthenticator;
use crate::config::{Gb28181Config, Transport};
use crate::frame::{AccessUnit, FrameSource};

use super::client::{
    build_catalog_response, build_device_info_response, build_keepalive_notify,
    parse_401_challenge, parse_invite, AudioCodec, InviteInfo, MediaKind, MediaTransport,
    SipDeviceClient,
};
use super::manscdp::{ChannelItem, DeviceItem};
use super::playback::{parse_playback_control, run_playback_task, PlaybackControl};
use super::ps::mux_h264_to_ps;
use super::rtp_pusher::RtpPusher;
use super::sip::{
    build_invite_response, build_media_status_info_request, SessionType, SipMessage, SipMethod,
    SipStatusCode,
};
use crate::RecordingSource;

// Maximum UDP packet size for SIP (should handle most messages)
const MAX_SIP_PACKET_SIZE: usize = 65535;
// RTP payload type for PS (GB28181 standard)
pub(super) const PS_PAYLOAD_TYPE: u8 = 96;

/// Handle to a running GB28181 server.
///
/// Created by [`Gb28181Server::start`] / [`Gb28181Server::spawn`]. Await it
/// (`handle.await`) to wait for the server task to finish, or call
/// [`ServerHandle::shutdown`] for a graceful stop (the SIP recv/accept loop,
/// the keepalive task, and any active media task all stop).
///
/// `#[must_use]`: dropping the handle detaches you from the server (it keeps
/// running), but hosts that spawned it inside a `tokio::spawn` and let the
/// handle drop have repeatedly ended up with dead servers — await it or keep
/// it for shutdown.
/// Shutdown mode carried on the shutdown watch channel (issue #62).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownMode {
    /// Initial value — no shutdown requested yet.
    Init,
    /// Stop the server without touching the registration.
    Fast,
    /// De-register first (best-effort REGISTER with `Expires: 0`), then
    /// stop.
    Deregister,
}

#[derive(Debug)]
#[must_use = "dropping the handle leaves the server unsupervised; await it or call shutdown()"]
pub struct ServerHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown: watch::Sender<ShutdownMode>,
    /// Platform X-GB-Ver as last seen on a REGISTER response (Annex I),
    /// shared with the server task.
    platform_proto_ver: Arc<std::sync::Mutex<Option<String>>>,
    /// Platform clock from the REGISTER response's SIP Date header
    /// (§9.10.2), shared with the server task.
    platform_date: Arc<std::sync::Mutex<Option<i64>>>,
}

impl ServerHandle {
    /// Request a graceful shutdown and wait for the server task to finish.
    ///
    /// Stops the SIP recv/accept loop and the keepalive task, aborts any
    /// active media/playback task, and unsubscribes from the frame source.
    /// The registration is left to expire on the platform — use
    /// [`ServerHandle::shutdown_with_deregister`] to de-register first.
    pub async fn shutdown(&mut self) -> Result<()> {
        // Ignore a send error: every receiver may already be dropped.
        let _ = self.shutdown.send(ShutdownMode::Fast);
        (&mut self.task)
            .await
            .context("gb28181: server task join failed")?;
        Ok(())
    }

    /// Graceful shutdown that first de-registers from the platform
    /// (issue #62): the server sends REGISTER with `Expires: 0` — the
    /// same 401 Digest dance as registration, 2s response timeouts —
    /// before tearing down. Every deregistration failure (an
    /// unresponsive platform included) is logged and ignored: shutdown
    /// itself must always succeed. No-op on the UDP path when
    /// registration never succeeded; the TCP transport keeps the
    /// fast-stop behavior (its connection handlers do not de-register).
    pub async fn shutdown_with_deregister(&mut self) -> Result<()> {
        // Ignore a send error: every receiver may already be dropped.
        let _ = self.shutdown.send(ShutdownMode::Deregister);
        (&mut self.task)
            .await
            .context("gb28181: server task join failed")?;
        Ok(())
    }

    /// Abort the server task immediately (tokio abort semantics — no cleanup
    /// of active media tasks is guaranteed).
    pub fn abort(&self) {
        self.task.abort();
    }

    /// The platform's `X-GB-Ver` as last seen on a REGISTER response
    /// (GB/T 28181-2022 Annex I). `None` when the platform never
    /// announced one.
    pub fn platform_protocol_version(&self) -> Option<String> {
        self.platform_proto_ver
            .lock()
            .expect("platform protocol version lock")
            .clone()
    }

    /// The platform clock as last carried by a REGISTER response's SIP
    /// `Date` header (§9.10.2), Unix seconds — the device-side
    /// time-sync source. `None` when no response carried a parseable
    /// Date. Hosts compare against their own clock and decide (log the
    /// drift, or discipline the clock on NTP-less deployments).
    pub fn platform_date_unix(&self) -> Option<i64> {
        *self.platform_date.lock().expect("platform date lock")
    }
}

impl std::future::Future for ServerHandle {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.task).poll(cx).map(|_| ())
    }
}

/// GB28181 SIP server.
///
/// Manages the device's registration with a SIP platform and handles
/// INVITE/BYE sessions for streaming video.
/// Receives G.711 audio payload from a talkback session
/// (audio-only INVITE, GB/T 28181-2022 §9.2 — the device-side receive half
/// of voice talkback).
///
/// `on_audio` is called once per received RTP packet on the media task;
/// keep it cheap (copy and forward, e.g. into a channel to the audio
/// output thread). Implement this trait, or just pass a closure
/// `Fn(&[u8], u32)` — a blanket impl covers closures.
pub trait AudioTalkbackSink: Send + Sync {
    /// One RTP packet's audio payload (G.711 A-law/μ-law bytes) and the
    /// packet's SSRC (falls back to the session SSRC when the header
    /// carries 0).
    fn on_audio(&self, payload: &[u8], ssrc: u32);

    /// Codec-aware delivery: the same packet plus the G.711 variant
    /// negotiated for the session (from the offer's payload type).
    /// Sinks that need to decode (A-law vs μ-law) override this; the
    /// default forwards to [`AudioTalkbackSink::on_audio`], so
    /// codec-agnostic sinks (closures) keep working unchanged.
    fn on_audio_codec(&self, payload: &[u8], ssrc: u32, _codec: AudioCodec) {
        self.on_audio(payload, ssrc);
    }
}

impl<F: Fn(&[u8], u32) + Send + Sync> AudioTalkbackSink for F {
    fn on_audio(&self, payload: &[u8], ssrc: u32) {
        self(payload, ssrc)
    }
}

/// Host-side handling of decoded DeviceControl sub-commands
/// (GB/T 28181-2016 §9.3.2 / 2022 §9.3, issue #58). Every method has a
/// no-op default so hosts implement only what their hardware can do.
///
/// Callbacks fire on the SIP receive task — keep them cheap (copy and
/// forward into a channel); a blocking handler stalls signaling.
pub trait DeviceControlHandler: Send + Sync {
    /// `<IFrameCmd>Send</IFrameCmd>` — force the next encoded frame to be
    /// an IDR (platforms send this when starting a pull or after loss).
    fn on_force_iframe(&self) {}
    /// `<RecordCmd>Record|StopRecord</RecordCmd>` — toggle
    /// platform-requested local recording.
    fn on_record(&self, _start: bool) {}
    /// `<GuardCmd>SetGuard|ResetGuard</GuardCmd>` — arm/disarm.
    fn on_guard(&self, _arm: bool) {}
    /// `<AlarmCmd>ResetAlarm</AlarmCmd>` — clear the active alarm.
    fn on_reset_alarm(&self) {}
    /// `<TeleBoot>Boot</TeleBoot>` — remote restart. Gate the actual
    /// reboot behind an explicit host opt-in; the default no-op makes the
    /// command a safe ack-only.
    fn on_teleboot(&self) {}
    /// `<PTZCmd>` — A.3/A.4 command, bit-level decoded
    /// ([`crate::manscdp::PtzCommand`], #57): movement direction/speed
    /// bits, presets, cruise, FI lens, auxiliary switches. Undecodable
    /// hex arrives as `PtzCommand::Invalid` with the raw string
    /// preserved.
    fn on_ptz(&self, _cmd: &crate::manscdp::PtzCommand) {}
    /// `<HomePosition>` — 看守位 control (A.2.3.1.10): auto-return to a
    /// preset after `reset_time` seconds of inactivity (enabled=0
    /// disables). Absent optional fields mean "keep current".
    fn on_home_position(
        &self,
        _enabled: u32,
        _reset_time: Option<u32>,
        _preset_index: Option<u32>,
    ) {
    }
    /// `<DragZoomIn>`/`<DragZoomOut>` — 拉框放大/缩小 control
    /// (A.2.3.1.8/9, [`crate::manscdp::DragZoom`]): zoom the drawn box to
    /// fill the playback window (In) or the window into the box (Out);
    /// coordinates are window pixels with the origin at the top-left.
    /// Hosts without a PTZ keep the default no-op (ack-only).
    fn on_drag_zoom(&self, _cmd: &crate::manscdp::DragZoom) {}
}

/// Host seam for DeviceConfig sub-commands (GB/T 28181-2022 §9.3.3 /
/// A.2.3.2, issue #57 minimum). Every method has a no-op default —
/// install only what your hardware acts on; a command whose handler is
/// the no-op default keeps the reject answer.
pub trait DeviceConfigHandler: Send + Sync {
    /// A.2.3.2.2 基本参数配置 — device name and registration tuning.
    /// The library never hot-applies these; hosts decide what sticks.
    fn on_basic_param(
        &self,
        _name: Option<&str>,
        _expiration: Option<u64>,
        _heartbeat_interval: Option<u64>,
        _heartbeat_count: Option<u32>,
    ) {
    }
    /// A.2.3.2.9 画面翻转配置 — 0 none, 1 horizontal, 2 vertical,
    /// 3 both (A.2.1.22 frameMirrorCfgType).
    fn on_frame_mirror(&self, _mode: u32) {}
    /// A.2.3.2.10 报警上报开关配置 — motion-detection /
    /// field-detection event report switches (0 off, 1 on).
    fn on_alarm_report(&self, _motion_detection: u32, _field_detection: u32) {}
}

/// Stamp the Annex I X-GB-Ver header on a REGISTER when configured.
fn stamp_xgbver(register: &mut SipMessage, version: &Option<String>) {
    if let Some(ver) = version {
        register.headers.push(("X-GB-Ver".to_string(), ver.clone()));
    }
}

/// Executes a decoded DeviceControl against the installed handler.
fn dispatch_device_control(
    handler: Arc<dyn DeviceControlHandler>,
    control: &crate::manscdp::DeviceControl,
) {
    use crate::manscdp::DeviceControlKind;
    match &control.kind {
        DeviceControlKind::ForceIFrame => handler.on_force_iframe(),
        DeviceControlKind::Record(start) => handler.on_record(*start),
        DeviceControlKind::Guard(arm) => handler.on_guard(*arm),
        DeviceControlKind::ResetAlarm => handler.on_reset_alarm(),
        DeviceControlKind::TeleBoot => handler.on_teleboot(),
        DeviceControlKind::Ptz(cmd) => handler.on_ptz(cmd),
        DeviceControlKind::HomePosition {
            enabled,
            reset_time,
            preset_index,
        } => handler.on_home_position(*enabled, *reset_time, *preset_index),
        DeviceControlKind::DragZoom(cmd) => handler.on_drag_zoom(cmd),
    }
}

/// Executes a decoded DeviceConfig against the installed handler.
fn dispatch_device_config(
    handler: Arc<dyn DeviceConfigHandler>,
    config: &crate::manscdp::DeviceConfig,
) {
    use crate::manscdp::DeviceConfigKind;
    match &config.kind {
        DeviceConfigKind::BasicParam {
            name,
            expiration,
            heartbeat_interval,
            heartbeat_count,
        } => handler.on_basic_param(
            name.as_deref(),
            *expiration,
            *heartbeat_interval,
            *heartbeat_count,
        ),
        DeviceConfigKind::FrameMirror(mode) => handler.on_frame_mirror(*mode),
        DeviceConfigKind::AlarmReport {
            motion_detection,
            field_detection,
        } => handler.on_alarm_report(*motion_detection, *field_detection),
    }
}

pub struct Gb28181Server {
    /// Configuration for the GB28181 server
    config: Gb28181Config,
    /// Access unit hub for subscribing to H.264 frames
    au_hub: Arc<dyn FrameSource>,
    /// SIP signaling socket (UDP; `None` until bound in `spawn`/`start` or
    /// by the TCP connection handler)
    sip_socket: Option<Arc<UdpSocket>>,
    /// TCP connection for SIP (when transport == Tcp)
    tcp_conn: Option<OwnedWriteHalf>,
    /// Media (RTP) socket (bound on INVITE)
    media_socket: Option<Arc<UdpSocket>>,
    /// TCP media connection (when transport == Tcp — active mode: device
    /// connects out to the platform's media port, GB/T 28181 Annex C.2)
    media_tcp_conn: Option<Arc<Mutex<TcpStream>>>,
    /// Media streaming task handle
    media_task: Option<tokio::task::JoinHandle<()>>,
    /// Current subscriber ID for AuHub
    subscriber_id: Option<u64>,
    /// Current INVITE dialog info
    invite_info: Option<InviteDialog>,
    /// Pending outbound voice-broadcast INVITE (§9.12.1 信令5): awaiting
    /// the platform's SIP response, which the recv loop routes in by
    /// Call-ID (see the fallback arm of `handle_message`). Dropped
    /// (media socket closed) on answer, rejection or 5s expiry.
    broadcast_pending: Option<PendingBroadcast>,
    /// Detected local IP advertised in Contact headers.
    local_ip: String,
    /// Optional source of recorded-segment metadata for RecordInfo queries.
    recording_index: Option<Arc<dyn RecordingSource>>,
    /// Control channel for an active playback session (SIP INFO PlaybackControl).
    playback_ctl: Option<mpsc::Sender<PlaybackControl>>,
    /// Audio talkback sink (audio-only INVITE receive). `None` = talkback
    /// INVITEs are refused with 488.
    audio_sink: Option<Arc<dyn AudioTalkbackSink>>,
    /// Upstream talkback frames (§9.2 send half, issue #61): host-fed
    /// pre-framed G.711 bytes the media task packetizes toward the
    /// platform. Shared behind a mutex so the session task can drain it
    /// while the server keeps it for future sessions.
    talkback_source: Option<Arc<std::sync::Mutex<std::sync::mpsc::Receiver<Vec<u8>>>>>,
    /// Optional replacement for Digest REGISTER authentication (GB 35114
    /// A-level via the `gb35114` feature). `None` keeps the Digest flow.
    authenticator: Option<Arc<dyn RegisterAuthenticator>>,
    /// Platform X-GB-Ver as last seen on a REGISTER response (Annex I).
    /// Shared with the [`ServerHandle`] accessor.
    platform_proto_ver: Arc<std::sync::Mutex<Option<String>>>,
    /// Platform clock as last carried by a REGISTER response's SIP Date
    /// header (§9.10.2), Unix seconds. Shared with the handle accessor.
    platform_date: Arc<std::sync::Mutex<Option<i64>>>,
    /// Device-side snapshot executor (A.2.1.24). `None` = control reject.
    snapshot_executor: Option<Arc<dyn crate::snapshot::SnapshotExecutor>>,
    /// DeviceControl sub-command handler (issue #58). `None` = recognized
    /// sub-commands keep the control-reject behavior.
    control_handler: Option<Arc<dyn DeviceControlHandler>>,
    /// Host seam for DeviceConfig sub-commands (issue #57); `None` keeps
    /// the reject behavior.
    config_handler: Option<Arc<dyn DeviceConfigHandler>>,
    /// SUBSCRIBE/NOTIFY bookkeeping + host-facing notifier (issue #57's
    /// subscription half). Shared with the host via [`Self::device_notifier`].
    notifier: Arc<crate::subscribe::DeviceNotifier>,
    /// Periodic MobilePosition source; installed via
    /// [`Self::with_position_source`] (None = position NOTIFYs only via
    /// the notifier's direct sends).
    position_source: Option<Arc<dyn crate::subscribe::MobilePositionSource>>,
    /// Cancels the running position report task (replaced on re-SUBSCRIBE).
    position_cancel: Option<tokio::sync::mpsc::Sender<()>>,
    /// Blocking clone of the SIP UDP socket handed to the notifier in
    /// run_udp (std send_to — safe from non-async host threads).
    notifier_std_sock: Option<std::sync::Arc<std::net::UdpSocket>>,
    /// Observability hooks (no-op by default); see `metrics` module docs.
    metrics: Arc<dyn crate::metrics::MetricsHooks>,
}

/// Information about an active INVITE dialog.
#[derive(Debug, Clone)]
struct InviteDialog {
    /// Call-ID of the dialog
    call_id: String,
    /// Remote tag from From header
    _remote_tag: String,
    /// Local tag we generated
    _local_tag: u32,
    /// CSeq of the INVITE that established (or last re-negotiated) the dialog
    cseq: u32,
    /// The 200 OK sent for that INVITE — re-sent verbatim on retransmission
    /// (RFC 3261 §13.3.1.4, issue #18).
    invite_response: Option<SipMessage>,
    /// Remote platform address for SIP signaling
    _remote_addr: SocketAddr,
    /// SSRC from INVITE SDP (y= field)
    _ssrc: u32,
    /// Platform's media (RTP) address
    _media_addr: String,
    /// Platform's media (RTP) port
    _media_port: u16,
}

/// A §9.12.1 voice-broadcast session in the INVITE-sent state (信令5
/// outstanding): the recv loop completes it when the platform's SIP
/// response arrives with the matching Call-ID.
struct PendingBroadcast {
    call_id: String,
    /// The outbound INVITE as sent — the ACK reuses its routing headers.
    invite: SipMessage,
    /// The ephemeral UDP socket announced in the INVITE SDP; the RTP
    /// receiver owns it after the 200 OK, dropping it closes the socket.
    media_socket: Arc<UdpSocket>,
    /// SSRC we announced (y= and Subject) — RTP fallback key.
    ssrc: u32,
    sink: Arc<dyn AudioTalkbackSink>,
    platform_addr: SocketAddr,
    sent_at: std::time::Instant,
}

/// Local-IP route probe retry budget: 30 attempts × 3 s ≈ 90 s, matching
/// `systemd-networkd-wait-online`'s default timeout — the probe outlives a
/// normal boot-time DHCP wait instead of killing the server.
const LOCAL_IP_PROBE_ATTEMPTS: u32 = 30;
const LOCAL_IP_PROBE_BACKOFF: Duration = Duration::from_secs(3);

/// Retry a local-IP probe until it succeeds, attempts run out, or shutdown
/// is requested.
///
/// `attempt` is an injectable async probe (production: bind a UDP socket and
/// `connect()` to the platform so the kernel picks the outgoing interface).
/// Returns `Ok(Some(local_ip))` on success, `Ok(None)` when shutdown was
/// requested while probing (caller should stop cleanly), or `Err` once all
/// attempts failed.
async fn probe_local_ip_with_retry<F, Fut>(
    mut attempt: F,
    max_attempts: u32,
    backoff: Duration,
    shutdown: &mut watch::Receiver<ShutdownMode>,
) -> Result<Option<String>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<String>>,
{
    for attempt_no in 1..=max_attempts {
        tokio::select! {
            outcome = attempt() => match outcome {
                Ok(ip) => return Ok(Some(ip)),
                Err(e) if attempt_no < max_attempts => {
                    log::warn!(
                        "gb28181: local IP probe attempt {attempt_no}/{max_attempts} failed: {e} — retrying in {backoff:?}"
                    );
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "gb28181: local IP probe to the platform failed after {max_attempts} attempts: {e}"
                    ));
                }
            },
            _ = shutdown.changed() => return Ok(None),
        }
        if attempt_no < max_attempts {
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shutdown.changed() => return Ok(None),
            }
        }
    }
    Err(anyhow::anyhow!(
        "gb28181: local IP probe attempts exhausted"
    ))
}

impl Gb28181Server {
    /// Create a new GB28181 server instance.
    ///
    /// The returned instance performs no I/O and never panics; it only
    /// stores the configuration. Call [`Gb28181Server::spawn`] to bind the
    /// SIP socket and run. Equivalent to
    /// `with_recording_index(config, au_hub, None)`.
    pub fn new(config: Gb28181Config, au_hub: Arc<dyn FrameSource>) -> Self {
        Self::with_recording_index(config, au_hub, None)
    }

    /// Create a new GB28181 server with an optional recording index source.
    ///
    /// Like [`Gb28181Server::new`], this performs no I/O and never panics;
    /// the SIP socket is bound later by [`Gb28181Server::spawn`].
    pub fn with_recording_index(
        config: Gb28181Config,
        au_hub: Arc<dyn FrameSource>,
        recording_index: Option<Arc<dyn RecordingSource>>,
    ) -> Self {
        Self {
            config,
            au_hub,
            sip_socket: None,
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: String::new(),
            recording_index,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
            metrics: Arc::new(crate::metrics::NoopMetrics),
        }
    }

    /// Installs an alternative REGISTER authentication strategy (GB 35114
    /// A-level when built with the `gb35114` feature). `None` (default)
    /// keeps the built-in SIP Digest flow.
    pub fn with_register_authenticator(
        mut self,
        authenticator: Option<Arc<dyn RegisterAuthenticator>>,
    ) -> Self {
        self.authenticator = authenticator;
        self
    }

    /// Installs the device-side snapshot executor (GB/T 28181-2022
    /// A.2.1.24): DeviceControl(SnapShot) commands run against it and
    /// complete asynchronously with the A.2.5.7 UploadSnapShotFinished
    /// notify. `None` (default) keeps the historical control-reject
    /// behavior. Requires the UDP transport — over TCP the command is
    /// rejected with a warning.
    pub fn with_snapshot_executor(
        mut self,
        executor: Option<Arc<dyn crate::snapshot::SnapshotExecutor>>,
    ) -> Self {
        self.snapshot_executor = executor;
        self
    }

    /// The host-facing SUBSCRIBE/NOTIFY sender (issue #57): hold this
    /// before/after [`Self::spawn`] and call `send_alarm` /
    /// `send_catalog_change` / `send_mobile_position` whenever the
    /// business side has something to report — no-ops until the
    /// platform subscribes (and safe before the socket is bound).
    #[must_use]
    pub fn notifier(&self) -> Arc<crate::subscribe::DeviceNotifier> {
        Arc::clone(&self.notifier)
    }

    /// Attach the audio talkback sink (receive half of GB/T 28181-2022
    /// §9.2 voice talkback). Without it, audio-only INVITEs are refused
    /// with 488.
    #[must_use]
    /// Sets the observability hooks (no-op by default). Hooks fire on the
    /// REGISTER/keepalive lifecycle and the media paths; they must be
    /// cheap and never block.
    pub fn with_metrics(mut self, hooks: Arc<dyn crate::metrics::MetricsHooks>) -> Self {
        self.metrics = hooks;
        self
    }

    pub fn with_audio_sink(mut self, sink: Arc<dyn AudioTalkbackSink>) -> Self {
        self.audio_sink = Some(sink);
        self
    }

    /// Install the talkback upstream source (§9.2 send half, issue #61):
    /// pre-framed G.711 bytes packetized as RTP toward the platform's
    /// media address at one frame per 20 ms tick. Push ~160-byte frames
    /// (20 ms of 8 kHz G.711) at a real-time cadence; the channel
    /// buffers bursts. An offer requiring upstream audio (`a=recvonly`)
    /// without a source is refused with 488, mirroring the no-sink
    /// refusal. Call before `spawn`.
    pub fn with_talkback_source(mut self, frames: std::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        self.talkback_source = Some(Arc::new(std::sync::Mutex::new(frames)));
        self
    }

    /// Installs the DeviceControl sub-command handler (issue #58):
    /// recognized controls (IFrameCmd force-IDR, RecordCmd, GuardCmd,
    /// AlarmCmd, TeleBoot, PTZCmd passthrough) execute against it and the
    /// 200 OK is the whole answer. `None` (default) keeps the
    /// control-reject behavior. UDP transport only for now; TCP dispatch
    /// is a follow-up.
    pub fn with_control_handler(mut self, handler: Option<Arc<dyn DeviceControlHandler>>) -> Self {
        self.control_handler = handler;
        self
    }

    /// Installs the DeviceConfig host seam (issue #57). `None` (default)
    /// keeps every DeviceConfig command rejected with `Result=ERROR`.
    pub fn with_config_handler(mut self, handler: Option<Arc<dyn DeviceConfigHandler>>) -> Self {
        self.config_handler = handler;
        self
    }

    /// The host-facing NOTIFY sender (issue #57): hold this `Arc` and call
    /// `send_alarm` / `send_catalog_change` / `send_mobile_position`
    /// whenever the business side has something to report — no-ops until
    /// the platform subscribes.
    #[must_use]
    pub fn device_notifier(&self) -> Arc<crate::subscribe::DeviceNotifier> {
        Arc::clone(&self.notifier)
    }

    /// Installs the periodic MobilePosition source: while a
    /// MobilePosition subscription is live, the server pulls the source
    /// on the SUBSCRIBE's `Interval` (default 5s, matching the Go
    /// platform's request cadence) and sends position NOTIFYs.
    #[must_use]
    pub fn with_position_source(
        mut self,
        source: Option<Arc<dyn crate::subscribe::MobilePositionSource>>,
    ) -> Self {
        self.position_source = source;
        self
    }

    /// Bind the SIP socket and run this server (instance flavor of
    /// [`Gb28181Server::start`]).
    ///
    /// Branches on `config.transport` exactly like `start`. Returns a
    /// [`ServerHandle`] for graceful shutdown.
    pub async fn spawn(self) -> Result<ServerHandle> {
        // Fail fast on spec-example defaults in strict mode — before any
        // socket is bound (issue #32). Non-strict keeps the warn-only
        // behavior at the run paths.
        if let Err(e) = self.config.check_example_defaults() {
            return Err(anyhow!(e));
        }
        match self.config.transport {
            Transport::Udp => self.spawn_udp().await,
            Transport::Tcp => self.spawn_tcp().await,
        }
    }

    async fn spawn_udp(mut self) -> Result<ServerHandle> {
        let sip_addr = format!("0.0.0.0:{}", self.config.local_sip_port);
        // Bind the std socket first so the notifier can hold a blocking
        // clone (NOTIFY sends fire from host threads that may not run in
        // a tokio context; tokio's try_send_to returns WouldBlock there).
        let std_sock = std::net::UdpSocket::bind(&sip_addr)
            .context(format!("gb28181: failed to bind SIP socket on {sip_addr}"))?;
        std_sock
            .set_nonblocking(true)
            .context("gb28181: set_nonblocking on SIP socket")?;
        let notifier_std_sock = std_sock
            .try_clone()
            .context("gb28181: clone SIP socket for notifier")?;
        let sip_socket = UdpSocket::from_std(std_sock)?;
        self.sip_socket = Some(Arc::new(sip_socket));
        self.notifier_std_sock = Some(std::sync::Arc::new(notifier_std_sock));
        self.run_bound().await
    }

    async fn spawn_tcp(self) -> Result<ServerHandle> {
        let sip_addr = format!("0.0.0.0:{}", self.config.local_sip_port);
        let listener = TcpListener::bind(&sip_addr).await.context(format!(
            "gb28181: failed to bind TCP listener on {sip_addr}"
        ))?;
        self.run_tcp_bound(listener).await
    }

    /// Start the GB28181 server (associated-function flavor).
    ///
    /// Branches based on config.transport:
    /// - UDP (default): binds UDP socket, runs REGISTER lifecycle, enters recv loop
    /// - TCP: binds TCP listener, spawns per-connection handlers
    ///
    /// Returns a [`ServerHandle`] — await it for server exit, or call
    /// `shutdown()` for a graceful stop.
    pub async fn start(
        config: Gb28181Config,
        au_hub: Arc<dyn FrameSource>,
        recording_index: Option<Arc<dyn RecordingSource>>,
    ) -> Result<ServerHandle> {
        Gb28181Server::with_recording_index(config, au_hub, recording_index)
            .spawn()
            .await
    }

    /// Common post-bind path for UDP: warn on example defaults, spawn the
    /// run task with a shutdown watch channel.
    async fn run_bound(mut self) -> Result<ServerHandle> {
        let local_sip_port = self.config.local_sip_port;
        log::info!("gb28181: listening on SIP port {local_sip_port} (UDP)");
        self.config.check_example_defaults().ok();

        let platform_proto_ver = Arc::clone(&self.platform_proto_ver);
        let platform_date = Arc::clone(&self.platform_date);
        let (shutdown_tx, mut shutdown_rx) = watch::channel(ShutdownMode::Init);
        let handle = tokio::spawn(async move {
            if let Err(e) = self.run_udp(&mut shutdown_rx).await {
                log::error!("gb28181: server error: {e}");
            }
        });

        Ok(ServerHandle {
            task: handle,
            shutdown: shutdown_tx,
            platform_proto_ver,
            platform_date,
        })
    }

    /// Common post-bind path for TCP.
    async fn run_tcp_bound(self, listener: TcpListener) -> Result<ServerHandle> {
        let local_sip_port = self.config.local_sip_port;
        log::info!("gb28181: listening on SIP port {local_sip_port} (TCP)");
        self.config.check_example_defaults().ok();

        let (shutdown_tx, mut shutdown_rx) = watch::channel(ShutdownMode::Init);
        let au_hub = self.au_hub;
        let config = self.config;
        let recording_index = self.recording_index;
        let audio_sink = self.audio_sink;
        let platform_proto_ver = Arc::clone(&self.platform_proto_ver);
        let platform_date = Arc::clone(&self.platform_date);

        let handle = tokio::spawn(async move {
            // Accept loop for TCP connections
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((conn, peer_addr)) => {
                                let au_hub_clone = Arc::clone(&au_hub);
                                let config_clone = config.clone();
                                let rec_clone = recording_index.clone();
                                let sink_clone = audio_sink.clone();
                                let mut shutdown_conn = shutdown_rx.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_tcp_connection(
                                        conn,
                                        peer_addr,
                                        au_hub_clone,
                                        config_clone,
                                        rec_clone,
                                        sink_clone,
                                        &mut shutdown_conn,
                                    )
                                    .await
                                    {
                                        log::error!(
                                            "gb28181: TCP connection error from {}: {}",
                                            peer_addr, e
                                        );
                                    }
                                });
                            }
                            Err(e) => {
                                log::error!("gb28181: TCP accept error: {e}");
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        log::info!("gb28181: shutdown requested — closing TCP accept loop");
                        break;
                    }
                }
            }
        });

        Ok(ServerHandle {
            task: handle,
            shutdown: shutdown_tx,
            platform_proto_ver,
            platform_date,
        })
    }

    /// Main UDP server loop.
    async fn run_udp(&mut self, shutdown: &mut watch::Receiver<ShutdownMode>) -> Result<()> {
        // Parse platform SIP address
        let platform_sip_addr: SocketAddr = format!(
            "{}:{}",
            self.config.platform_sip_address, self.config.platform_sip_port
        )
        .parse()
        .context("gb28181: invalid platform SIP address")?;

        // Detect the real local IP by probing the route to the platform
        // (the SIP socket binds 0.0.0.0, so its local_addr() is not usable).
        // Boot race: at service start the network may not be up yet, making
        // the probe fail with ENETUNREACH — retry until the route appears,
        // mirroring the REGISTER lifecycle's wait-for-platform behavior.
        let local_ip = {
            let probe_target = platform_sip_addr;
            match probe_local_ip_with_retry(
                || async move {
                    let probe = UdpSocket::bind("0.0.0.0:0").await?;
                    probe.connect(probe_target).await?;
                    Ok(probe.local_addr()?.ip().to_string())
                },
                LOCAL_IP_PROBE_ATTEMPTS,
                LOCAL_IP_PROBE_BACKOFF,
                shutdown,
            )
            .await?
            {
                Some(ip) => ip,
                None => {
                    log::info!("gb28181: shutdown requested during local IP probe — stopping");
                    return Ok(());
                }
            }
        };
        self.local_ip = local_ip.clone();
        let local_sip_port = self.config.local_sip_port;
        let sip_socket = self
            .sip_socket
            .clone()
            .ok_or_else(|| anyhow::anyhow!("gb28181: SIP socket not bound"))?;

        // SUBSCRIBE/NOTIFY (issue #57): give the host-facing notifier the
        // sending context before the loop starts answering SUBSCRIBEs.
        // The std clone keeps NOTIFY sends blocking-safe from any host
        // thread (tokio's try_send_to would return WouldBlock off-runtime).
        self.notifier.bind(
            self.notifier_std_sock.clone().unwrap_or_else(|| {
                Arc::new(std::net::UdpSocket::bind("0.0.0.0:0").expect("fallback notifier socket"))
            }),
            self.config.device_id.clone(),
            self.config.sip_domain.clone(),
            local_ip.clone(),
            local_sip_port,
        );

        // Create SIP device client (User-Agent from config; neutral default).
        let mut sip_client = SipDeviceClient::new(
            &self.config.device_id,
            platform_sip_addr,
            &local_ip,
            local_sip_port,
            &self.config.sip_domain,
            &self.config.password,
            self.config.register_interval_secs as u32,
        )
        .with_user_agent(&self.config.effective_user_agent());

        // REGISTER lifecycle: retry with backoff, do NOT exit on failure.
        // Both the attempts and the backoff sleeps race against shutdown so
        // a shutdown request is honored immediately during startup.
        let mut registered = false;
        const MAX_REG_ATTEMPTS: u32 = 3;
        const REG_BACKOFF_SECS: u64 = 10;
        for attempt in 1..=MAX_REG_ATTEMPTS {
            self.metrics.register_attempt();
            tokio::select! {
                result = self.perform_register(&mut sip_client, platform_sip_addr) => {
                    match result {
                        Ok(()) => {
                            registered = true;
                            self.metrics.register_ok();
                            log::info!(
                                "gb28181: registered with platform {} (attempt {}/{})",
                                platform_sip_addr,
                                attempt,
                                MAX_REG_ATTEMPTS
                            );
                            break;
                        }
                        Err(e) => {
                            self.metrics.register_fail();
                            log::warn!(
                                "gb28181: registration attempt {}/{} failed: {e}",
                                attempt,
                                MAX_REG_ATTEMPTS
                            );
                        }
                    }
                }
                _ = shutdown.changed() => {
                    log::info!("gb28181: shutdown requested during registration — stopping");
                    return Ok(());
                }
            }
            if attempt < MAX_REG_ATTEMPTS {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(REG_BACKOFF_SECS)) => {}
                    _ = shutdown.changed() => {
                        log::info!("gb28181: shutdown requested during registration backoff — stopping");
                        return Ok(());
                    }
                }
            }
        }

        if registered {
            // Spawn keepalive task (stops on shutdown)
            let sip_socket_for_keepalive = Arc::clone(&sip_socket);
            let keepalive_device_id = self.config.device_id.clone();
            let keepalive_interval_secs = self.config.heartbeat_interval_secs;
            let keepalive_domain = self.config.sip_domain.clone();
            let keepalive_local_ip = local_ip.clone();
            let keepalive_local_port = local_sip_port;
            let mut keepalive_shutdown = shutdown.clone();
            let keepalive_authenticator = self.authenticator.clone();
            let keepalive_metrics = Arc::clone(&self.metrics);
            tokio::spawn(async move {
                if let Err(e) = run_keepalive(
                    sip_socket_for_keepalive,
                    platform_sip_addr,
                    &keepalive_device_id,
                    &keepalive_domain,
                    &keepalive_local_ip,
                    keepalive_local_port,
                    keepalive_interval_secs,
                    keepalive_authenticator,
                    Arc::clone(&keepalive_metrics),
                    &mut keepalive_shutdown,
                )
                .await
                {
                    log::error!("gb28181: keepalive error: {e}");
                }
            });
        } else {
            log::warn!(
                "gb28181: all {} registration attempts failed — continuing in listen-only mode (SIP port stays bound)",
                MAX_REG_ATTEMPTS
            );
        }

        // Enter SIP recv loop
        let mut buf = vec![0u8; MAX_SIP_PACKET_SIZE];
        let mut keepalive_failures = 0u32;

        // Refresh the registration at half the negotiated expiry (RFC 3261
        // §10.2) so a platform restart recovers without waiting for keepalive
        // timeouts (issue #19).
        let mut re_register_interval = tokio::time::interval(Duration::from_secs(
            registration_refresh_interval_secs(self.config.register_interval_secs),
        ));
        re_register_interval.tick().await; // skip immediate first tick

        loop {
            tokio::select! {
                recv_result = sip_socket.recv_from(&mut buf) => {
                    match recv_result {
                        Ok((len, peer_addr)) => {
                            let data = &buf[..len];
                            // Fast path: strict UTF-8 (zero copy). Legacy
                            // platforms send GB2312/GBK/GB18030 — decode
                            // instead of dropping the datagram.
                            let parsed = match std::str::from_utf8(data) {
                                Ok(s) => SipMessage::parse(s),
                                Err(_) => {
                                    let decoded = crate::charset::decode_wire_body(data);
                                    log::debug!("gb28181: decoded non-UTF-8 SIP datagram as GB18030");
                                    SipMessage::parse(&decoded)
                                }
                            };
                            if let Ok(msg) = parsed {
                                if let Err(e) = self
                                    .handle_message(
                                        &msg,
                                        peer_addr,
                                        &mut sip_client,
                                        platform_sip_addr,
                                        &mut keepalive_failures,
                                    )
                                    .await
                                {
                                    log::error!("gb28181: message handling error: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("gb28181: socket recv error: {e}");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
                _ = re_register_interval.tick() => {
                    // Registration (re-)attempt races against shutdown so the
                    // tick arm cannot delay a pending shutdown by its 5s
                    // response timeouts.
                    let mut sd = shutdown.clone();
                    tokio::select! {
                        _ = sd.changed() => {
                            log::info!("gb28181: shutdown requested during re-registration — stopping");
                            if matches!(*sd.borrow_and_update(), ShutdownMode::Deregister) && registered {
                                self.perform_deregister(&mut sip_client, platform_sip_addr).await;
                            }
                            return self.shutdown_cleanup();
                        }
                        result = self.perform_register(&mut sip_client, platform_sip_addr) => {
                            if registered {
                                // Registration refresh (issue #19): re-REGISTER before
                                // the negotiated expiry. A restarted platform has an
                                // empty registration table while we still believe we
                                // are registered — refreshing recovers immediately
                                // instead of deadlocking until keepalive timeouts.
                                if let Err(e) = result {
                                    log::warn!(
                                        "gb28181: registration refresh failed: {e} — marking unregistered, will retry"
                                    );
                                    registered = false;
                                } else {
                                    log::info!(
                                        "gb28181: registration refreshed with platform {}",
                                        platform_sip_addr
                                    );
                                }
                            } else {
                                log::info!("gb28181: periodic re-registration attempt");
                                match result {
                            Ok(()) => {
                                registered = true;
                                log::info!("gb28181: registered with platform {} (periodic retry)", platform_sip_addr);
                                // Spawn keepalive now that we're registered
                                let sip_socket_for_keepalive = Arc::clone(&sip_socket);
                                let keepalive_device_id = self.config.device_id.clone();
                                let keepalive_interval_secs = self.config.heartbeat_interval_secs;
                                let keepalive_domain = self.config.sip_domain.clone();
                                let keepalive_local_ip = local_ip.clone();
                                let keepalive_local_port = local_sip_port;
                                let mut keepalive_shutdown = shutdown.clone();
                                let keepalive_authenticator = self.authenticator.clone();
                                let keepalive_metrics = Arc::clone(&self.metrics);
                                tokio::spawn(async move {
                                    if let Err(e) = run_keepalive(
                                        sip_socket_for_keepalive,
                                        platform_sip_addr,
                                        &keepalive_device_id,
                                        &keepalive_domain,
                                        &keepalive_local_ip,
                                        keepalive_local_port,
                                        keepalive_interval_secs,
                                        keepalive_authenticator,
                                        keepalive_metrics,
                                        &mut keepalive_shutdown,
                                    )
                                    .await
                                    {
                                        log::error!("gb28181: keepalive error: {e}");
                                    }
                                });
                            }
                            Err(e) => {
                                log::warn!("gb28181: periodic re-registration failed: {e}");
                            }
                        }
                            }
                        }
                    }
                }
                _ = shutdown.changed() => {
                    log::info!("gb28181: shutdown requested — stopping SIP recv loop");
                    if matches!(*shutdown.borrow(), ShutdownMode::Deregister) && registered {
                        self.perform_deregister(&mut sip_client, platform_sip_addr).await;
                    }
                    return self.shutdown_cleanup();
                }
            }
        }
    }

    /// Graceful-shutdown cleanup: unsubscribe from the frame source and
    /// abort any active media session so spawned tasks do not outlive the
    /// server.
    fn shutdown_cleanup(&mut self) -> Result<()> {
        if let Some(subscriber_id) = self.subscriber_id.take() {
            self.au_hub.unsubscribe(subscriber_id);
        }
        self.broadcast_pending = None;
        if let Some(task) = self.media_task.take() {
            task.abort();
        }
        self.media_socket = None;
        self.media_tcp_conn = None;
        self.invite_info = None;
        self.playback_ctl = None;
        Ok(())
    }

    /// Record the platform clock from a REGISTER response's SIP Date
    /// header (§9.10.2 — the device-side time-sync source). Logs the
    /// measured drift; applying the clock stays with the host (NTP-less
    /// deployments set it, NTP-fed ones just observe).
    fn note_platform_date(&self, resp: &SipMessage) {
        let Some(raw) = resp.get_header("Date") else {
            return;
        };
        let Some(unix) = super::sip::parse_sip_date(raw) else {
            log::warn!("gb28181: unparseable SIP Date header: {raw:?}");
            return;
        };
        let local = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let drift = local - unix;
        if drift.unsigned_abs() > 5 {
            log::warn!(
                "gb28181: platform clock differs by {drift}s (SIP Date {unix}, local {local})"
            );
        }
        *self.platform_date.lock().expect("platform date lock") = Some(unix);
    }

    /// Record the platform's X-GB-Ver off a REGISTER response (Annex I).
    /// An absent header (2016-era platforms) keeps the previous value.
    fn note_platform_protocol_version(&self, resp: &SipMessage) {
        let Some(ver) = resp.get_header("X-GB-Ver") else {
            return;
        };
        let mut guard = self
            .platform_proto_ver
            .lock()
            .expect("platform protocol version lock");
        if guard.as_deref() != Some(ver) {
            log::info!("gb28181: platform protocol version (X-GB-Ver): {ver}");
            *guard = Some(ver.to_string());
        }
    }

    /// Perform REGISTER lifecycle.
    async fn perform_register(
        &mut self,
        client: &mut SipDeviceClient,
        platform_addr: SocketAddr,
    ) -> Result<()> {
        // Step 1: Send initial REGISTER — a RegisterAuthenticator may
        // announce capabilities (GB 35114 Capability); None keeps the
        // plain Digest-era behavior.
        let mut register = client.build_register();
        if let Some(auth) = &self.authenticator {
            let authz = auth.initial_authorization();
            if !authz.is_empty() {
                register.headers.push(("Authorization".to_string(), authz));
            }
        }
        stamp_xgbver(&mut register, &self.config.protocol_version);
        let initial_cseq = client.cseq;
        self.send_sip_message(&register, platform_addr).await?;

        // Step 2: Wait for the 401 challenge OF THIS REQUEST. A late
        // response from a previous cycle (stale 200 OK / old-nonce 401)
        // must be skipped, not consumed — otherwise cycles go one-off
        // (issue #11).
        let msg = self
            .receive_register_response(initial_cseq, Duration::from_secs(5))
            .await?;
        self.note_platform_protocol_version(&msg);
        self.note_platform_date(&msg);
        if msg.status_code == Some(SipStatusCode::Ok) {
            // No-auth platforms answer the initial REGISTER directly
            // (RFC 3261 §10.2 allows unauthenticated registration; the Go
            // twin has always accepted this path). Also covers a stale
            // 200 from a previous instance's superseded REGISTER after a
            // restart — accepting it is registered.
            log::info!("gb28181: registered without challenge (no-auth platform)");
            client.inc_cseq();
            return Ok(());
        }
        if msg.status_code != Some(SipStatusCode::Unauthorized) {
            bail!("Expected 401 Unauthorized, got {:?}", msg.status_code);
        }

        // Step 3: Answer the challenge — the authenticator's header when
        // installed, classic Digest otherwise.
        client.inc_cseq();
        let authed_cseq = client.cseq;
        let mut authed_register = if let Some(auth) = &self.authenticator {
            let www_auth = msg
                .get_header("WWW-Authenticate")
                .ok_or_else(|| anyhow!("401 response missing WWW-Authenticate header"))?;
            let authz = auth.authorize_with_challenge(www_auth)?;
            let mut reg = client.build_register();
            reg.headers.push(("Authorization".to_string(), authz));
            reg
        } else {
            let auth = parse_401_challenge(&msg)?;
            client.build_register_with_auth(&auth)
        };
        stamp_xgbver(&mut authed_register, &self.config.protocol_version);
        self.send_sip_message(&authed_register, platform_addr)
            .await?;

        // Step 4: Wait for the 200 OK OF THIS REQUEST (skip stale).
        let msg = self
            .receive_register_response(authed_cseq, Duration::from_secs(5))
            .await?;
        self.note_platform_protocol_version(&msg);
        self.note_platform_date(&msg);
        if msg.status_code != Some(SipStatusCode::Ok) {
            bail!("Expected 200 OK, got {:?}", msg.status_code);
        }
        if let Some(auth) = &self.authenticator {
            auth.verify_ok(msg.get_header("SecurityInfo").unwrap_or(""))?;
        }

        client.inc_cseq();
        Ok(())
    }

    /// Best-effort SIP de-registration (REGISTER with `Expires: 0`,
    /// issue #62). Runs the same 401 Digest dance as registration with
    /// short timeouts; every failure path — a silent platform, an
    /// unexpected status, a malformed challenge — logs a warning and
    /// returns, so shutdown is never blocked by an unresponsive
    /// platform.
    async fn perform_deregister(
        &mut self,
        client: &mut SipDeviceClient,
        platform_addr: SocketAddr,
    ) {
        const DEREG_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);

        // Leg 1: unauthenticated de-register. A platform may accept it
        // outright — then we are done.
        let mut dereg = client.build_deregister();
        stamp_xgbver(&mut dereg, &self.config.protocol_version);
        let initial_cseq = client.cseq;
        if let Err(e) = self.send_sip_message(&dereg, platform_addr).await {
            log::warn!("gb28181: deregistration send failed: {e}");
            return;
        }
        let resp = match self
            .receive_register_response(initial_cseq, DEREG_RESPONSE_TIMEOUT)
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                log::warn!("gb28181: deregistration unanswered: {e}");
                return;
            }
        };
        match resp.status_code {
            Some(SipStatusCode::Ok) => {
                log::info!("gb28181: deregistered from platform (Expires: 0 accepted)");
                client.inc_cseq();
                return;
            }
            Some(SipStatusCode::Unauthorized) => {}
            other => {
                log::warn!("gb28181: deregistration rejected with {other:?}");
                client.inc_cseq();
                return;
            }
        }

        // Leg 2: answer the 401 like the registration path.
        client.inc_cseq();
        let authed_cseq = client.cseq;
        let mut authed = if let Some(auth) = &self.authenticator {
            match resp.get_header("WWW-Authenticate") {
                Some(www_auth) => match auth.authorize_with_challenge(www_auth) {
                    Ok(authz) => {
                        let mut reg = client.build_deregister();
                        reg.headers.push(("Authorization".to_string(), authz));
                        reg
                    }
                    Err(e) => {
                        log::warn!("gb28181: deregistration challenge failed: {e}");
                        return;
                    }
                },
                None => {
                    log::warn!("gb28181: deregistration 401 missing WWW-Authenticate header");
                    return;
                }
            }
        } else {
            match parse_401_challenge(&resp) {
                Ok(auth) => client.build_deregister_with_auth(&auth),
                Err(e) => {
                    log::warn!("gb28181: deregistration challenge unparseable: {e}");
                    return;
                }
            }
        };
        stamp_xgbver(&mut authed, &self.config.protocol_version);
        if let Err(e) = self.send_sip_message(&authed, platform_addr).await {
            log::warn!("gb28181: deregistration send failed: {e}");
            return;
        }
        match self
            .receive_register_response(authed_cseq, DEREG_RESPONSE_TIMEOUT)
            .await
        {
            Ok(resp) if resp.status_code == Some(SipStatusCode::Ok) => {
                log::info!(
                    "gb28181: deregistered from platform {platform_addr} (Expires: 0, authed)"
                );
                client.inc_cseq();
            }
            Ok(resp) => {
                log::warn!(
                    "gb28181: deregistration rejected with {:?}",
                    resp.status_code
                );
                client.inc_cseq();
            }
            Err(e) => log::warn!("gb28181: deregistration unanswered: {e}"),
        }
    }

    /// Handle incoming SIP message.
    async fn handle_message(
        &mut self,
        msg: &SipMessage,
        peer_addr: SocketAddr,
        client: &mut SipDeviceClient,
        platform_addr: SocketAddr,
        keepalive_failures: &mut u32,
    ) -> Result<()> {
        // GB35114 A-level: platform→device requests are Note-verified
        // before any method dispatch (issue #41).
        if msg.method.is_some() && !self.allow_incoming_note(msg) {
            let forbidden = build_error_response(msg, 403, "Forbidden");
            self.send_sip_message(&forbidden, peer_addr).await?;
            return Ok(());
        }
        match msg.method {
            Some(SipMethod::Invite) => {
                self.handle_invite(msg, peer_addr).await?;
            }
            Some(SipMethod::Bye) => {
                self.handle_bye(msg, peer_addr).await?;
            }
            Some(SipMethod::Message) => {
                // Dispatch inbound MESSAGE and respond
                if let Ok((ok_response, _queued)) = super::client::dispatch_inbound_message(msg) {
                    self.send_sip_message(&ok_response, peer_addr).await?;
                }

                // DeviceControl(SnapShot) (A.2.1.24): with an executor
                // installed the 200 above is the whole synchronous answer;
                // the exchange runs in a spawned task and completes
                // asynchronously via the A.2.5.7 notify. Without an
                // executor — or over the TCP transport — the control
                // reject below keeps the historical behavior.
                if let Some(control) = crate::manscdp::parse_control_snapshot(&msg.body) {
                    if let Some(executor) = self.snapshot_executor.clone() {
                        // UDP only: the notify leaves through the shared
                        // SIP UDP socket; per-connection TCP servers carry
                        // a placeholder UDP socket, so key off tcp_conn.
                        if self.tcp_conn.is_none() && self.sip_socket.is_some() {
                            self.spawn_snapshot_exchange(&control, executor, platform_addr);
                            return Ok(());
                        }
                        log::warn!(
                            "gb28181: snapshot command over TCP transport — executor requires UDP, rejecting"
                        );
                    }
                }

                // DeviceControl sub-commands (issue #58): a recognized
                // command executes the installed handler and the 200 OK
                // above is the whole synchronous answer. Without a
                // handler — or for not-yet-decoded kinds (DragZoom is
                // deferred; PTZ passes through as raw A505) — fall
                // through to the control reject, preserving the
                // historical behavior. UDP transport only for now.
                if let Some(control) = crate::manscdp::parse_device_control(&msg.body) {
                    if let Some(handler) = self.control_handler.clone() {
                        log::info!("gb28181: DeviceControl {:?} executed", control.kind);
                        dispatch_device_control(handler, &control);
                        return Ok(());
                    }
                    log::warn!(
                        "gb28181: DeviceControl {:?} without a control handler — rejecting",
                        control.kind
                    );
                }

                // DeviceConfig sub-commands (issue #57, A.2.3.2): unlike
                // controls, the answer is a Response body with Result
                // (A.2.6.8) — OK when a handler executed, ERROR (the
                // historical reject below) otherwise.
                if let Some(config) = crate::manscdp::parse_device_config(&msg.body) {
                    if let Some(handler) = self.config_handler.clone() {
                        log::info!("gb28181: DeviceConfig {:?} executed", config.kind);
                        dispatch_device_config(handler, &config);
                        let cseq = random_cseq();
                        let response = super::client::build_device_config_response(
                            true,
                            &config.sn,
                            &config.device_id,
                            &self.config.sip_domain,
                            &self.local_ip,
                            self.config.local_sip_port,
                            cseq,
                        )?;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        self.send_sip_message(&response, peer_addr).await?;
                        return Ok(());
                    }
                    log::warn!(
                        "gb28181: DeviceConfig {:?} without a config handler — rejecting",
                        config.kind
                    );
                }

                // Voice broadcast (§9.12.1, A.2.5.5): acknowledge the
                // notification, then — with an audio sink installed —
                // run the 信令3/信令5 device half. UDP transport only
                // (the response and INVITE leave through the shared SIP
                // UDP socket).
                if let Some(notify) = crate::manscdp::parse_broadcast_notify(&msg.body) {
                    if self.tcp_conn.is_none() && self.sip_socket.is_some() {
                        let ok = build_error_response(msg, 200, "OK");
                        self.send_sip_message(&ok, peer_addr).await?;
                        self.start_broadcast(notify, platform_addr).await?;
                        return Ok(());
                    }
                }

                // Build and send Catalog/DeviceInfo response if this was a query
                if let Some(response_msg) = self.build_query_response(msg)? {
                    // Small delay to let 200 OK be processed first
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    self.send_sip_message(&response_msg, peer_addr).await?;
                }
            }
            Some(SipMethod::Info) => {
                self.handle_info(msg, peer_addr).await?;
            }
            Some(SipMethod::Subscribe) => {
                let response = self.handle_subscribe(msg, peer_addr)?;
                self.send_sip_message(&response, peer_addr).await?;
            }
            Some(SipMethod::Notify) | Some(SipMethod::Options) => {
                log::warn!(
                    "gb28181: received {}, responding 200 OK",
                    msg.method.map(|m| m.to_string()).unwrap_or_default()
                );
                let ok_response = build_error_response(msg, 200, "OK");
                self.send_sip_message(&ok_response, peer_addr).await?;
            }
            _ => {
                // Route a SIP response to the pending broadcast INVITE by
                // Call-ID (§9.12.1 信令5→13). Anything else falls through
                // to the keepalive heuristic.
                if msg.status_code.is_some() {
                    let call_id = msg.get_header("Call-ID").unwrap_or("");
                    let matches = self
                        .broadcast_pending
                        .as_ref()
                        .map(|p| p.call_id == call_id)
                        .unwrap_or(false);
                    if matches {
                        let pending = self.broadcast_pending.take().expect("checked above");
                        self.complete_broadcast(pending, msg.clone()).await?;
                        return Ok(());
                    }
                    // Expire a stale pending (5s answer window) — dropping
                    // it closes the media socket.
                    if let Some(p) = self.broadcast_pending.as_ref() {
                        if p.sent_at.elapsed() > Duration::from_secs(5) {
                            log::warn!("gb28181: broadcast INVITE unanswered — session abandoned");
                            self.broadcast_pending = None;
                        }
                    }
                }
                // Check if this is a response to our keepalive
                if msg.status_code == Some(SipStatusCode::Ok) {
                    *keepalive_failures = 0; // Reset failure counter on OK
                } else if msg.status_code.is_some() && msg.status_code != Some(SipStatusCode::Ok) {
                    *keepalive_failures += 1;
                    self.metrics.keepalive_fail();
                    if *keepalive_failures >= self.config.heartbeat_timeout_count {
                        log::warn!(
                            "gb28181: keepalive timeout after {} failures, re-registering",
                            *keepalive_failures
                        );
                        if let Err(e) = self.perform_register(client, platform_addr).await {
                            log::warn!("gb28181: re-registration failed: {e}");
                        }
                        *keepalive_failures = 0;
                    }
                }
            }
        }
        Ok(())
    }

    /// §9.12.1 voice-broadcast device half, phase 1: send the A.2.6.11
    /// acknowledgement (信令3 — OK iff an audio sink is installed), then
    /// with a sink bind the receive socket and send the audio INVITE
    /// (信令5: s=Play / m=audio / Subject header). The platform's SIP
    /// response completes the session in the recv loop
    /// ([`Self::complete_broadcast`]).
    async fn start_broadcast(
        &mut self,
        notify: crate::manscdp::BroadcastNotify,
        platform_addr: SocketAddr,
    ) -> Result<()> {
        let sink = self.audio_sink.clone();
        let resp = build_broadcast_response_message(
            notify.sn,
            &self.config.device_id,
            &self.config.sip_domain,
            &self.local_ip,
            self.config.local_sip_port,
            sink.is_some(),
        );
        self.send_sip_message(&resp, platform_addr).await?;

        let Some(sink) = sink else {
            log::info!("gb28181: broadcast declined — no audio sink installed");
            return Ok(());
        };

        // Occupies the shared media slot (single session at a time).
        if let Some(task) = self.media_task.take() {
            task.abort();
        }
        let media_socket = Arc::new(
            UdpSocket::bind("0.0.0.0:0")
                .await
                .context("gb28181: failed to bind broadcast media socket")?,
        );
        let media_port = media_socket.local_addr()?.port();
        let ssrc = rand::random::<u32>();
        let invite = build_broadcast_invite(
            &self.config.device_id,
            &self.config.sip_domain,
            &self.local_ip,
            self.config.local_sip_port,
            &notify.source_id,
            media_port,
            ssrc,
            platform_addr,
        );
        log::info!(
            "gb28181: broadcast INVITE sent, source {}, ssrc {}",
            notify.source_id,
            ssrc
        );
        self.send_sip_message(&invite, platform_addr).await?;
        self.broadcast_pending = Some(PendingBroadcast {
            call_id: invite.get_header("Call-ID").unwrap_or("").to_string(),
            invite,
            media_socket,
            ssrc,
            sink,
            platform_addr,
            sent_at: std::time::Instant::now(),
        });
        Ok(())
    }

    /// §9.12.1 voice-broadcast device half, phase 2: the platform
    /// answered the outbound INVITE. On 200, send the in-dialog ACK
    /// (信令15), register the dialog for BYE cleanup (信令17→18) and
    /// receive G.711 RTP into the sink on the shared media slot.
    async fn complete_broadcast(
        &mut self,
        pending: PendingBroadcast,
        resp: SipMessage,
    ) -> Result<()> {
        if resp.status_code != Some(SipStatusCode::Ok) {
            log::warn!(
                "gb28181: broadcast INVITE rejected (status {:?}) — session dropped",
                resp.status_code
            );
            return Ok(());
        }
        let ack = build_broadcast_ack(&pending.invite, &resp);
        self.send_sip_message(&ack, pending.platform_addr).await?;

        let (call_id, media_socket, ssrc, sink) = (
            pending.call_id,
            pending.media_socket,
            pending.ssrc,
            pending.sink,
        );
        // Dialog bookkeeping so the platform's BYE (信令17) tears the
        // receiver down through the existing handle_bye path.
        self.invite_info = Some(InviteDialog {
            call_id: call_id.clone(),
            _remote_tag: String::new(),
            _local_tag: 0,
            cseq: 1,
            invite_response: None,
            _remote_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            _ssrc: ssrc,
            _media_addr: String::new(),
            _media_port: 0,
        });
        let receiver = tokio::spawn(run_broadcast_receiver(media_socket, sink, ssrc));
        self.media_task = Some(receiver);
        log::info!("gb28181: broadcast session receiving, call-id {}", call_id);
        Ok(())
    }

    /// Runs device-side Note verification on a platform→device request
    /// (issue #41). Requests without a Note pass (mixed-mode Digest
    /// platforms); a Note that fails verification follows
    /// `incoming_note_policy` (log-only under Warn, 403 under the
    /// default Reject).
    fn allow_incoming_note(&self, msg: &SipMessage) -> bool {
        use crate::authenticator::IncomingNotePolicy;

        if self.config.incoming_note_policy == IncomingNotePolicy::Off {
            return true;
        }
        let Some(auth) = &self.authenticator else {
            return true;
        };
        let note = msg.get_header("Note").unwrap_or("");
        if note.is_empty() {
            return true;
        }
        let method = msg.method.map(|m| m.to_string()).unwrap_or_default();
        match auth.verify_incoming_note(
            &method,
            msg.get_header("From").unwrap_or(""),
            msg.get_header("To").unwrap_or(""),
            msg.get_header("Call-ID").unwrap_or(""),
            msg.get_header("Date").unwrap_or(""),
            note,
            &msg.body,
        ) {
            Ok(()) => true,
            Err(e) => {
                if self.config.incoming_note_policy == IncomingNotePolicy::Warn {
                    log::warn!("gb28181: incoming Note verification failed (warn policy, serving anyway): {e}");
                    true
                } else {
                    log::warn!("gb28181: incoming Note verification failed, rejecting: {e}");
                    false
                }
            }
        }
    }

    /// Build a Catalog or DeviceInfo response MESSAGE for an inbound MANSCDP query.
    fn build_query_response(&self, msg: &SipMessage) -> Result<Option<SipMessage>> {
        if msg.get_header("Content-Type").unwrap_or("") != "Application/MANSCDP+xml" {
            return Ok(None);
        }
        let query = match super::manscdp::parse_query_dual(&msg.body) {
            Some(q) => q,
            None => return Ok(None),
        };
        let cseq = random_cseq();
        match query.cmd_type.as_str() {
            "Catalog" => {
                let channel = ChannelItem {
                    device_id: self.config.device_id.clone(),
                    name: self.config.effective_device_name(),
                    manufacturer: self.config.effective_manufacturer(),
                    model: self.config.effective_model(),
                    owner: String::new(),
                    civil_code: String::new(),
                    address: String::new(),
                    parental: 0,
                    parent_id: self.config.device_id.clone(),
                    safety_way: 0,
                    register_way: 1,
                    secrecy: 0,
                    status: "ON".to_string(),
                    ip_address: self.local_ip.clone(),
                    port: self.config.local_sip_port,
                    longitude: 0.0,
                    latitude: 0.0,
                };
                let response = build_catalog_response(
                    &query.sn,
                    &self.config.device_id,
                    &self.config.sip_domain,
                    &self.local_ip,
                    self.config.local_sip_port,
                    cseq,
                    &[channel],
                )?;
                Ok(Some(response))
            }
            "DeviceInfo" => {
                let info = DeviceItem {
                    device_id: self.config.device_id.clone(),
                    name: self.config.effective_device_name(),
                    manufacturer: self.config.effective_manufacturer(),
                    model: self.config.effective_model(),
                    firmware: self.config.effective_firmware(),
                };
                let response = build_device_info_response(
                    &query.sn,
                    &self.config.device_id,
                    &self.config.sip_domain,
                    &self.local_ip,
                    self.config.local_sip_port,
                    cseq,
                    &info,
                )?;
                Ok(Some(response))
            }
            "RecordInfo" => {
                let riq = match super::manscdp::parse_recordinfo_query_dual(&msg.body) {
                    Some(q) => q,
                    None => return Ok(None),
                };
                // Both window bounds must parse; otherwise return an empty list.
                let items = match (riq.start_ms, riq.end_ms) {
                    (Some(start), Some(end)) => {
                        let segments = self
                            .recording_index
                            .as_ref()
                            .map(|src| src.lookup(start, end))
                            .unwrap_or_default();
                        segments
                            .into_iter()
                            .map(|seg| super::client::RecordItem {
                                device_id: riq.device_id.clone(),
                                name: seg.file.rsplit('/').next().unwrap_or(&seg.file).to_string(),
                                file_path: seg.file,
                                address: riq.device_id.clone(),
                                start_time: super::client::format_gb_time_ms(seg.start_ms),
                                end_time: super::client::format_gb_time_ms(seg.end_ms),
                                secrecy: "0".to_string(),
                                r#type: "time".to_string(),
                            })
                            .collect::<Vec<_>>()
                    }
                    _ => Vec::new(),
                };
                let response = super::client::build_recordinfo_response(
                    &riq.sn,
                    &riq.device_id,
                    &self.config.sip_domain,
                    &self.local_ip,
                    self.config.local_sip_port,
                    cseq,
                    &items,
                )?;
                Ok(Some(response))
            }
            "DeviceStatus" => {
                let response = super::client::build_device_status_response(
                    &query.sn,
                    &query.device_id,
                    &self.config.sip_domain,
                    &self.local_ip,
                    self.config.local_sip_port,
                    cseq,
                )?;
                Ok(Some(response))
            }
            "HomePositionQuery"
            | "CruiseTrackListQuery"
            | "CruiseTrackQuery"
            | "PTZPosition"
            | "SDCardStatus" => {
                // GB/T 28181-2022 information queries (A.2.4.10-14):
                // answer with the minimal valid Response (A.2.6.12-16).
                // This device has no PTZ hardware, cruise tracks or
                // storage card, so every optional block is omitted and
                // required SumNum fields are zero — previously these fell
                // through to the unknown-CmdType warn + silence.
                // Semantics and goldens mirror gb28181-go #78 (issue #59).
                let response = super::client::build_gb2022_query_response(
                    &query.cmd_type,
                    &query.sn,
                    &query.device_id,
                    query.number.as_deref(),
                    &self.config.sip_domain,
                    &self.local_ip,
                    self.config.local_sip_port,
                    cseq,
                )?;
                Ok(Some(response))
            }
            "ConfigDownload" => {
                // A.2.4.7 / A.2.6.9: answer the minimal valid Response —
                // OK plus the BasicParam block (name + registration
                // tuning from the live config) when the request asked
                // for it; every other config block is optional and
                // omitted. ConfigType may list several types
                // "/"-separated.
                let requested_basic = query
                    .config_type
                    .as_deref()
                    .unwrap_or("")
                    .split('/')
                    .any(|t| t.trim() == "BasicParam");
                let basic = requested_basic.then(|| super::client::BasicParamBlock {
                    name: Some(self.config.effective_device_name()),
                    expiration: Some(self.config.register_interval_secs),
                    heartbeat_interval: Some(self.config.heartbeat_interval_secs),
                    heartbeat_count: Some(self.config.heartbeat_timeout_count),
                });
                let response = super::client::build_config_download_response(
                    &query.sn,
                    &query.device_id,
                    basic.as_ref(),
                    &self.config.sip_domain,
                    &self.local_ip,
                    self.config.local_sip_port,
                    cseq,
                )?;
                Ok(Some(response))
            }
            "DeviceControl" | "Broadcast" | "DeviceConfig" | "HomePosition" => {
                log::warn!("gb28181: control command not supported: {}", query.cmd_type);
                let response = super::client::build_control_reject_response(
                    &query.cmd_type,
                    &query.sn,
                    &query.device_id,
                    &self.config.sip_domain,
                    &self.local_ip,
                    self.config.local_sip_port,
                    cseq,
                )?;
                Ok(Some(response))
            }
            _ => Ok(None),
        }
    }

    /// Handle SIP INVITE request.
    async fn handle_invite(&mut self, msg: &SipMessage, peer_addr: SocketAddr) -> Result<()> {
        // Parse INVITE to extract stream target info
        let invite_info = parse_invite(msg)?;

        // Check if we already have an active session
        if self.media_task.is_some() {
            // CSeq of the incoming INVITE (needed to tell a retransmission of
            // the establishing INVITE from a same-dialog re-INVITE).
            let incoming_cseq = msg
                .get_header("CSeq")
                .and_then(|c| c.split_whitespace().next())
                .and_then(|c| c.parse::<u32>().ok())
                .unwrap_or(1);
            let existing = self.invite_info.as_ref();
            let same_dialog = existing
                .map(|d| d.call_id == invite_info.call_id)
                .unwrap_or(false);
            if let Some(dialog) = existing {
                if same_dialog && dialog.cseq == incoming_cseq {
                    // Retransmission of the INVITE that established this
                    // dialog: the 200 OK was lost. RFC 3261 §13.3.1.4 —
                    // re-send the SAME 200 OK, never 486 (issue #18: the
                    // platform aborts the session on 486 and the stream
                    // deadlocks until a dialog reset).
                    log::warn!(
                        "gb28181: INVITE retransmission for dialog {} — re-sending cached 200 OK",
                        dialog.call_id
                    );
                    if let Some(resp) = dialog.invite_response.clone() {
                        self.send_sip_message(&resp, peer_addr).await?;
                    }
                    return Ok(());
                }
                if same_dialog {
                    log::warn!(
                        "gb28181: re-INVITE on dialog {} (CSeq {} → {}) — recycling media session",
                        dialog.call_id,
                        dialog.cseq,
                        incoming_cseq
                    );
                } else {
                    // Different Call-ID = a NEW dialog (platform restarted and
                    // lost the old one, or the previous BYE never reached us).
                    // Recycling the stale session instead of 486-ing forever
                    // (issue #6).
                    log::warn!(
                        "gb28181: INVITE for new dialog {} — recycling stale session {}",
                        invite_info.call_id,
                        dialog.call_id
                    );
                }
            } else {
                log::warn!(
                    "gb28181: INVITE with no dialog tracked — recycling orphaned media session"
                );
            }
            if let Some(subscriber_id) = self.subscriber_id.take() {
                self.au_hub.unsubscribe(subscriber_id);
            }
            if let Some(task) = self.media_task.take() {
                task.abort();
            }
            self.media_socket = None;
            self.media_tcp_conn = None;
            self.invite_info = None;
            self.playback_ctl = None;
        }

        log::info!(
            "gb28181: INVITE from {} to {}:{}",
            invite_info.media_address,
            invite_info.media_port,
            invite_info.ssrc
        );

        // TCP media where the platform dials the device (a=setup:active in
        // the offer) is not supported — this device has no media listener.
        // Refuse with 488 instead of answering a mismatched transport and
        // streaming into a black hole (issue #14).
        if invite_info.media_transport == MediaTransport::TcpListen {
            log::warn!("gb28181: TCP media with setup:active unsupported — 488");
            let resp = build_error_response(msg, 488, "Not Acceptable Here");
            self.send_sip_message(&resp, peer_addr).await?;
            return Ok(());
        }

        // Audio-only offer = talkback receive (GB/T 28181-2022 §9.2): the
        // platform streams G.711 to us instead of us pushing PS video.
        if invite_info.media_kind == MediaKind::Audio {
            return self.handle_audio_invite(msg, invite_info, peer_addr).await;
        }

        // Bind local UDP for media (ephemeral port)
        let media_socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("gb28181: failed to bind media socket")?;
        let media_socket = Arc::new(media_socket);
        let media_port = media_socket.local_addr()?.port();

        // Build device SDP answer
        let local_tag = rand::random::<u32>();
        let cseq = msg
            .get_header("CSeq")
            .and_then(|c| c.split_whitespace().next())
            .and_then(|c| c.parse::<u32>().ok())
            .unwrap_or(1);

        let device_ip = self.local_ip.clone();
        let local_sip_port = self.config.local_sip_port;

        // Destination for RTP media comes from the platform's INVITE SDP.
        let media_dest = format!("{}:{}", invite_info.media_address, invite_info.media_port)
            .parse::<SocketAddr>()
            .context("gb28181: invalid media address from INVITE SDP")?;
        // Playback/Download: resolve the requested recording range. An empty
        // or unresolvable range is answered with 488 Not Acceptable Here
        // (plan binding #10).
        let playback = match invite_info.session_type {
            SessionType::Play => None,
            SessionType::Playback | SessionType::Download => {
                let start_ms = invite_info.start_secs.map(|s| s * 1000).unwrap_or(0);
                let end_ms = invite_info.end_secs.map(|s| s * 1000).unwrap_or(u64::MAX);
                let Some(source) = self.recording_index.clone() else {
                    log::warn!("gb28181: playback INVITE but no recording index — 488");
                    let resp = build_error_response(msg, 488, "Not Acceptable Here");
                    self.send_sip_message(&resp, peer_addr).await?;
                    return Ok(());
                };
                let segments = source.lookup(start_ms, end_ms);
                if segments.is_empty() {
                    log::warn!(
                        "gb28181: playback INVITE with no recordings in [{start_ms}, {end_ms}] — 488"
                    );
                    let resp = build_error_response(msg, 488, "Not Acceptable Here");
                    self.send_sip_message(&resp, peer_addr).await?;
                    return Ok(());
                }
                Some((source, segments, start_ms, end_ms))
            }
        };

        // The answer's m= transport mirrors the offer (RFC 3264): TCP media
        // is offered via TCP/RTP/AVP in the SDP regardless of the SIP
        // signaling transport (issue #14).
        let media_is_tcp = invite_info.media_transport == MediaTransport::TcpConnect;
        let sdp = build_device_sdp_answer(
            media_port,
            invite_info.ssrc,
            &device_ip,
            if media_is_tcp {
                Transport::Tcp
            } else {
                Transport::Udp
            },
            invite_info.session_type,
        );
        let response = build_invite_response(
            msg,
            &self.config.device_id,
            &sdp,
            local_tag,
            cseq,
            &device_ip,
            local_sip_port,
        );

        self.send_sip_message(&response, peer_addr).await?;

        // For TCP media (offer said TCP/RTP/AVP with setup:passive/actpass),
        // actively connect to the platform's media port — the device is the
        // active side per GB/T 28181 Annex C / RFC 4145 (issue #14).
        let media_tcp_conn = if media_is_tcp {
            let conn = TcpStream::connect(media_dest)
                .await
                .context("gb28181: failed to connect to TCP media port")?;
            log::info!("gb28181: connected to TCP media port {media_dest}");
            Some(Arc::new(Mutex::new(conn)))
        } else {
            None
        };

        // Store dialog info
        let remote_tag = msg
            .get_header("From")
            .and_then(|f| f.strip_prefix("<").and_then(|f| f.strip_suffix(">")))
            .and_then(|f| f.split(';').nth(1))
            .and_then(|t| t.strip_prefix("tag="))
            .unwrap_or("unknown")
            .to_string();

        let call_id = msg.get_header("Call-ID").unwrap_or("unknown").to_string();

        self.invite_info = Some(InviteDialog {
            call_id: call_id.clone(),
            _remote_tag: remote_tag,
            _local_tag: local_tag,
            cseq,
            invite_response: Some(response),
            _remote_addr: peer_addr,
            _ssrc: invite_info.ssrc,
            _media_addr: invite_info.media_address,
            _media_port: invite_info.media_port,
        });

        // Spawn the media task: live (AuHub) for Play, recorded segments for
        // Playback/Download.
        let media_socket_clone = Arc::clone(&media_socket);
        let ssrc = invite_info.ssrc;
        let device_id = self.config.device_id.clone();

        let media_task = match playback {
            Some((source, segments, start_ms, end_ms)) => {
                let paced = invite_info.session_type == SessionType::Playback;
                let media_task_conn = media_tcp_conn.clone();
                // §9.4.2 media-end notify (issue #60): on natural
                // completion the task sends the in-dialog MediaStatus
                // INFO itself. UDP SIP only — over TCP-SIP the dialog's
                // write half stays with the connection handler (deferred,
                // see #60). Everything is captured here because the task
                // owns only the media socket.
                let end_info = match (&self.sip_socket, self.tcp_conn.is_none()) {
                    (Some(sock), true) => {
                        let remote_id = msg
                            .get_header("From")
                            .and_then(|f| f.split('@').next())
                            .and_then(|f| f.strip_prefix("<sip:"))
                            .map(str::to_string)
                            .unwrap_or_else(|| self.config.sip_domain.clone());
                        Some(super::playback::MediaEndInfo {
                            sip_socket: Arc::clone(sock),
                            platform_sip_addr: peer_addr,
                            info: build_media_status_info_request(
                                &self.config.device_id,
                                &self.local_ip,
                                self.config.local_sip_port,
                                &remote_id,
                                &self.config.platform_sip_address,
                                &call_id,
                                cseq.wrapping_add(1),
                                local_tag,
                                invite_info.session_type == SessionType::Download,
                            ),
                        })
                    }
                    _ => None,
                };
                // Control channel for SIP INFO PlaybackControl on this session.
                let (playback_tx, playback_rx) = mpsc::channel::<PlaybackControl>(8);
                self.playback_ctl = Some(playback_tx);
                tokio::spawn(async move {
                    if let Err(e) = run_playback_task(
                        source,
                        segments,
                        start_ms,
                        end_ms,
                        media_socket_clone,
                        media_task_conn,
                        ssrc,
                        &device_id,
                        media_dest,
                        paced,
                        playback_rx,
                        end_info,
                    )
                    .await
                    {
                        log::warn!("gb28181: playback task error: {e}");
                    }
                })
            }
            None => {
                // Live session: no playback control channel.
                self.playback_ctl = None;
                // Subscribe to AuHub
                let subscriber = self.au_hub.subscribe_with_capacity(2);
                let subscriber_id = subscriber.id;
                let sync_rx = subscriber.receiver;

                // Bridge sync receiver to async channel
                let (async_tx, async_rx) = mpsc::channel::<AccessUnit>(2);
                tokio::task::spawn_blocking(move || {
                    while let Ok(au) = sync_rx.recv() {
                        if async_tx.blocking_send(au).is_err() {
                            break;
                        }
                    }
                });

                log::info!("gb28181: subscribed to AuHub (subscriber_id={subscriber_id})");
                self.subscriber_id = Some(subscriber_id);

                let media_task_conn = media_tcp_conn.clone();
                let media_metrics = Arc::clone(&self.metrics);
                tokio::spawn(async move {
                    if let Err(e) = run_media_task(
                        async_rx,
                        media_socket_clone,
                        media_task_conn,
                        ssrc,
                        &device_id,
                        media_dest,
                        media_metrics,
                    )
                    .await
                    {
                        log::warn!("gb28181: media task error: {e}");
                    }
                })
            }
        };

        self.media_socket = Some(media_socket);
        self.media_tcp_conn = media_tcp_conn;
        self.media_task = Some(media_task);

        log::info!("gb28181: media stream started on port {media_port}");
        Ok(())
    }

    /// Handle SIP BYE request.
    /// Handle an audio-only INVITE (talkback receive, GB/T 28181-2022 §9.2).
    ///
    /// The answer advertises an ephemeral UDP port; the platform streams
    /// G.711 RTP to it and each packet's audio payload goes to the
    /// configured [`AudioTalkbackSink`]. Refused with 488 when no sink is
    /// registered, the codec is not G.711 A/μ-law, or the offer asks for
    /// TCP media (UDP only in this revision).
    ///
    /// The talkback dialog occupies the same single-dialog slot as video
    /// sessions: an audio INVITE recycles any active video session and vice
    /// versa, and BYE tears the talkback receiver down through the shared
    /// media-task cleanup.
    async fn handle_audio_invite(
        &mut self,
        msg: &SipMessage,
        invite_info: InviteInfo,
        peer_addr: SocketAddr,
    ) -> Result<()> {
        let Some(sink) = self.audio_sink.clone() else {
            log::warn!("gb28181: talkback INVITE but no audio sink configured — 488");
            let resp = build_error_response(msg, 488, "Not Acceptable Here");
            self.send_sip_message(&resp, peer_addr).await?;
            return Ok(());
        };
        let Some(codec) = invite_info.audio_codec else {
            log::warn!("gb28181: talkback INVITE with non-G.711 codec — 488");
            let resp = build_error_response(msg, 488, "Not Acceptable Here");
            self.send_sip_message(&resp, peer_addr).await?;
            return Ok(());
        };
        if invite_info.media_transport != MediaTransport::Udp {
            log::warn!("gb28181: talkback over TCP media unsupported — 488");
            let resp = build_error_response(msg, 488, "Not Acceptable Here");
            self.send_sip_message(&resp, peer_addr).await?;
            return Ok(());
        }
        // Upstream-required offers (a=recvonly — the platform only
        // listens) must not be answered by a device that cannot send:
        // mirror the no-sink refusal (issue #61).
        if invite_info.recv_only && self.talkback_source.is_none() {
            log::warn!(
                "gb28181: talkback INVITE requires upstream audio but no talkback source installed — 488"
            );
            let resp = build_error_response(msg, 488, "Not Acceptable Here");
            self.send_sip_message(&resp, peer_addr).await?;
            return Ok(());
        }

        log::info!(
            "gb28181: talkback INVITE from {} ({}), ssrc {}",
            peer_addr,
            codec.name(),
            invite_info.ssrc
        );

        // Bind the receive port advertised in the answer (ephemeral UDP).
        let media_socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("gb28181: failed to bind talkback media socket")?;
        let media_socket = Arc::new(media_socket);
        let media_port = media_socket.local_addr()?.port();

        let local_tag = rand::random::<u32>();
        let cseq = msg
            .get_header("CSeq")
            .and_then(|c| c.split_whitespace().next())
            .and_then(|c| c.parse::<u32>().ok())
            .unwrap_or(1);
        let device_ip = self.local_ip.clone();
        let local_sip_port = self.config.local_sip_port;

        // Only the upstream-required form announces a direction — every
        // other answer stays byte-identical to the pre-upstream wire
        // form.
        let direction = if invite_info.recv_only {
            "a=sendonly\r\n"
        } else {
            ""
        };
        let sdp =
            build_audio_sdp_answer(media_port, invite_info.ssrc, &device_ip, codec, direction);
        let response = build_invite_response(
            msg,
            &self.config.device_id,
            &sdp,
            local_tag,
            cseq,
            &device_ip,
            local_sip_port,
        );
        self.send_sip_message(&response, peer_addr).await?;

        // Dialog bookkeeping on the shared slot so retransmission resend,
        // re-INVITE recycle and BYE cleanup all reuse the existing paths.
        let remote_tag = msg
            .get_header("From")
            .and_then(|f| f.strip_prefix("<").and_then(|f| f.strip_suffix(">")))
            .and_then(|f| f.split(';').nth(1))
            .and_then(|t| t.strip_prefix("tag="))
            .unwrap_or("unknown")
            .to_string();
        let call_id = msg.get_header("Call-ID").unwrap_or("unknown").to_string();
        self.invite_info = Some(InviteDialog {
            call_id: call_id.clone(),
            _remote_tag: remote_tag,
            _local_tag: local_tag,
            cseq,
            invite_response: Some(response),
            _remote_addr: peer_addr,
            _ssrc: invite_info.ssrc,
            _media_addr: invite_info.media_address.clone(),
            _media_port: invite_info.media_port,
        });

        // Upstream half (issue #61): with a source channel installed and
        // the offer not explicitly receive-only-for-the-device
        // (a=sendonly), the media task also packetizes pushed G.711
        // frames toward the offer's c=/m= address.
        let upstream = {
            let source = self
                .talkback_source
                .clone()
                .filter(|_| !invite_info.send_only);
            let dst = source.as_ref().and_then(|_| {
                format!("{}:{}", invite_info.media_address, invite_info.media_port)
                    .parse::<SocketAddr>()
                    .ok()
            });
            if self.talkback_source.is_some() && !invite_info.send_only && dst.is_none() {
                log::warn!(
                    "gb28181: talkback offer lacks a usable c=/m= media address — upstream disabled"
                );
            }
            dst.zip(source).map(|(dst, rx)| TalkbackUpstream {
                rx,
                dst,
                pt: codec.payload_type(),
                seq: rand::random::<u16>(),
                ts: rand::random::<u32>(),
                ssrc: rand::random::<u32>(),
            })
        };

        // Media loop: the receive half strips the fixed 12-byte header
        // (+ CSRC list) and hands the G.711 payload to the sink; the
        // upstream half drains one source frame per 20 ms tick. Both
        // live on the shared media_task slot, so BYE / dialog recycle
        // aborts them together.
        let session_ssrc = invite_info.ssrc;
        let recv_socket = Arc::clone(&media_socket);
        let media_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let mut up = upstream;
            let mut ticker = tokio::time::interval(Duration::from_millis(20));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await; // the first tick fires immediately — skip it
            loop {
                tokio::select! {
                    r = recv_socket.recv_from(&mut buf) => {
                        match r {
                            Ok((len, _)) => {
                                if len < 12 {
                                    continue;
                                }
                                let csrc_count = (buf[0] & 0x0F) as usize;
                                let header_len = 12 + csrc_count * 4;
                                if len <= header_len {
                                    continue;
                                }
                                let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                                sink.on_audio_codec(
                                    &buf[header_len..len],
                                    if ssrc != 0 { ssrc } else { session_ssrc },
                                    codec,
                                );
                            }
                            Err(e) => {
                                log::warn!("gb28181: talkback recv error: {e}");
                                break;
                            }
                        }
                    }
                    _ = ticker.tick(), if up.is_some() => {
                        if let Some(u) = up.as_mut() {
                            let frame =
                                match u.rx.lock().expect("talkback source lock").try_recv() {
                                    Ok(f) => f,
                                    Err(_) => continue,
                                };
                            if frame.is_empty() || frame.len() > 2048 {
                                continue;
                            }
                            let mut pkt = Vec::with_capacity(12 + frame.len());
                            pkt.push(0x80); // V=2, no padding/extension/CSRC
                            pkt.push(u.pt); // M=0 — continuous G.711 stream
                            pkt.extend_from_slice(&u.seq.to_be_bytes());
                            pkt.extend_from_slice(&u.ts.to_be_bytes());
                            pkt.extend_from_slice(&u.ssrc.to_be_bytes());
                            pkt.extend_from_slice(&frame);
                            if let Err(e) = recv_socket.send_to(&pkt, u.dst).await {
                                log::debug!("gb28181: talkback send loop ended: {e}");
                                break;
                            }
                            u.seq = u.seq.wrapping_add(1);
                            u.ts = u.ts.wrapping_add(frame.len() as u32);
                        }
                    }
                }
            }
        });
        self.media_task = Some(media_task);
        self.media_socket = Some(media_socket);
        log::info!(
            "gb28181: talkback session {call_id} receiving {} on UDP :{media_port}",
            codec.name()
        );
        Ok(())
    }

    async fn handle_bye(&mut self, msg: &SipMessage, peer_addr: SocketAddr) -> Result<()> {
        // BYE for a dialog we don't have (none active, or Call-ID mismatch):
        // 481, and NO side effects on registration/keepalive state (a
        // dialog-reset BYE from a restarted platform must never disturb
        // the engine — issue #6).
        let call_id = msg.get_header("Call-ID").unwrap_or("");
        let matches_dialog = self
            .invite_info
            .as_ref()
            .map(|d| d.call_id == call_id)
            .unwrap_or(false);
        if self.media_task.is_none() || !matches_dialog {
            log::warn!(
                "gb28181: received BYE for unknown dialog (Call-ID={call_id}) — replying 481"
            );
            let resp = build_error_response(msg, 481, "Call/Transaction Does Not Exist");
            self.send_sip_message(&resp, peer_addr).await?;
            return Ok(());
        }

        log::info!("gb28181: received BYE, stopping media stream");

        // Unsubscribe from AuHub
        if let Some(subscriber_id) = self.subscriber_id.take() {
            self.au_hub.unsubscribe(subscriber_id);
        }

        // Abort media task
        if let Some(task) = self.media_task.take() {
            task.abort();
        }

        // Close media socket
        self.media_socket = None;
        self.media_tcp_conn = None;
        self.invite_info = None;
        self.playback_ctl = None;

        // Send 200 OK to BYE — to the BYE's source address. The old code
        // derived the destination from Via (after already clearing
        // invite_info), yielding 0.0.0.0:5060 which Linux loops back to our
        // own SIP socket.
        let from = msg.get_header("From").unwrap_or("");
        let to = msg.get_header("To").unwrap_or("");
        let call_id = msg.get_header("Call-ID").unwrap_or("");
        let cseq = msg.get_header("CSeq").unwrap_or("0 BYE");

        let headers = vec![
            (
                "Via".to_string(),
                msg.get_header("Via").unwrap_or_default().to_string(),
            ),
            ("From".to_string(), from.to_string()),
            ("To".to_string(), to.to_string()),
            ("Call-ID".to_string(), call_id.to_string()),
            ("CSeq".to_string(), cseq.to_string()),
            ("Content-Length".to_string(), "0".to_string()),
        ];

        let response = SipMessage {
            start_line: "SIP/2.0 200 OK".to_string(),
            method: None,
            status_code: Some(SipStatusCode::Ok),
            uri: msg.uri.clone(),
            version: "SIP/2.0".to_string(),
            headers,
            body: String::new(),
        };

        if let Err(e) = self.send_sip_message(&response, peer_addr).await {
            log::warn!("gb28181: failed to send 200 OK to BYE: {e}");
        }

        Ok(())
    }

    /// Handle a SIP INFO request (PlaybackControl).
    ///
    /// Always answers 200 OK. If a playback session is active, the parsed
    /// control is forwarded to its task; otherwise (live session or none) it
    /// is a logged no-op.
    /// Answers an inbound SUBSCRIBE (issue #57): supported subjects
    /// (Catalog / Alarm / MobilePosition) are booked with their dialog
    /// snapshot and refreshed on re-SUBSCRIBE; every subject answers
    /// 200 OK with the request's `Expires` echoed (unknown subjects stay
    /// answered-but-unbooked — legacy-platform safe). A MobilePosition
    /// subscription with an installed position source also starts (or
    /// re-cadences) the periodic report task on the SUBSCRIBE's
    /// `Interval` (default 5s).
    fn handle_subscribe(&mut self, msg: &SipMessage, peer_addr: SocketAddr) -> Result<SipMessage> {
        let event_str = msg
            .get_header("Event")
            .map(str::to_string)
            .or_else(|| {
                // Some platforms only carry the subject in the body's
                // CmdType (the SUBSCRIBE body mirrors the MANSCDP form).
                crate::manscdp::parse_query_dual(&msg.body).map(|q| q.cmd_type)
            })
            .unwrap_or_default();
        let expires = msg
            .get_header("Expires")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(3600);

        if let Some(event) = crate::subscribe::SubscribeEvent::parse(&event_str) {
            let from = msg.get_header("From").unwrap_or_default().to_string();
            let to = msg.get_header("To").unwrap_or_default().to_string();
            let call_id = msg.get_header("Call-ID").unwrap_or_default().to_string();
            let cseq = msg
                .get_header("CSeq")
                .and_then(|c| c.split_whitespace().next())
                .and_then(|c| c.parse::<u32>().ok())
                .unwrap_or(1);
            self.notifier
                .registry()
                .upsert(event, expires, peer_addr, from, to, call_id, cseq);
            log::info!(
                "gb28181: SUBSCRIBE {event_str} booked (expires {expires}s) from {peer_addr}"
            );

            if event == crate::subscribe::SubscribeEvent::MobilePosition
                && self.position_source.is_some()
            {
                let interval = crate::subscribe::parse_subscribe_interval(&msg.body).unwrap_or(5);
                self.spawn_position_task(interval);
            }
        } else {
            log::warn!(
                "gb28181: SUBSCRIBE with unsupported Event {event_str:?} — answered, not booked"
            );
        }

        let mut response = build_error_response(msg, 200, "OK");
        if let Some(exp) = msg.get_header("Expires") {
            response
                .headers
                .push(("Expires".to_string(), exp.trim().to_string()));
        }
        Ok(response)
    }

    /// Periodic MobilePosition reports while the subscription is live.
    fn spawn_position_task(&mut self, interval_secs: u64) {
        // One task at a time: a fresh SUBSCRIBE re-cadences by replacing
        // the sender (the old task exits on the closed channel).
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
        if let Some(old) = self.position_cancel.take() {
            let _ = old.try_send(());
        }
        self.position_cancel = Some(tx);
        let notifier = Arc::clone(&self.notifier);
        let source = self.position_source.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(interval_secs.max(1))) => {}
                    _ = rx.recv() => break,
                }
                if !notifier.subscribed(crate::subscribe::SubscribeEvent::MobilePosition) {
                    continue;
                }
                if let Some(report) = source.as_ref().and_then(|s| s.current_position()) {
                    notifier.send_mobile_position(&report);
                }
            }
        });
    }

    async fn handle_info(&mut self, msg: &SipMessage, peer_addr: SocketAddr) -> Result<()> {
        let ok_response = build_error_response(msg, 200, "OK");
        self.send_sip_message(&ok_response, peer_addr).await?;

        match self.playback_ctl.as_ref() {
            Some(ctl) => match parse_playback_control(&msg.body) {
                Some(control) => {
                    if ctl.send(control).await.is_err() {
                        log::warn!("gb28181: playback control channel closed");
                    }
                }
                None => {
                    log::warn!("gb28181: INFO PlaybackControl with unknown/invalid body — ignored");
                }
            },
            None => {
                log::warn!(
                    "gb28181: received INFO PlaybackControl but no active playback session — no-op"
                );
            }
        }
        Ok(())
    }

    /// Send a SIP message over the configured transport.
    ///
    /// UDP (default): writes to the UDP socket via send_to.
    /// TCP: writes the serialized message to the active TCP connection.
    ///
    /// The body is wire-encoded via [`crate::charset`] (GB2312-declared
    /// non-ASCII bodies go out as GB18030; ASCII is byte-identical to the
    /// historical format) and Content-Length always matches the wire bytes.
    async fn send_sip_message(&mut self, msg: &SipMessage, dest: SocketAddr) -> Result<()> {
        let data = serialize_wire(msg);
        if let Some(conn) = self.tcp_conn.as_mut() {
            conn.write_all(&data)
                .await
                .context("gb28181: TCP write failed")?;
        } else if let Some(socket) = self.sip_socket.as_ref() {
            socket
                .send_to(&data, dest)
                .await
                .context("gb28181: send_to failed")?;
        } else {
            bail!("gb28181: no SIP transport bound (server not started)");
        }
        Ok(())
    }

    /// Runs one snapshot exchange to completion in a background task: the
    /// executor captures/uploads, then the UploadSnapShotFinished notify
    /// (A.2.5.7) goes out over the SIP UDP socket — same source port as
    /// every other device MESSAGE. Executor errors report a failed
    /// exchange (empty SnapShotList); `file_ids` pass through verbatim.
    fn spawn_snapshot_exchange(
        &self,
        control: &crate::manscdp::ControlSnapShot,
        executor: Arc<dyn crate::snapshot::SnapshotExecutor>,
        platform_addr: SocketAddr,
    ) {
        use crate::snapshot::SnapshotCommand;

        let cmd = SnapshotCommand {
            snap_num: control.snap_shot.snap_num,
            interval: control.snap_shot.interval,
            upload_url: control.snap_shot.upload_url.clone(),
            session_id: control.snap_shot.session_id.clone(),
        };
        let sn = control.sn.parse::<u32>().unwrap_or(0);
        let session_id = control.snap_shot.session_id.clone();
        let device_id = self.config.device_id.clone();
        let domain = self.config.sip_domain.clone();
        let local_ip = self.local_ip.clone();
        let local_port = self.config.local_sip_port;
        let socket = self
            .sip_socket
            .clone()
            .expect("caller checked UDP transport");

        tokio::spawn(async move {
            let file_ids = match executor.execute(cmd).await {
                Ok(ids) => ids,
                Err(e) => {
                    log::warn!("gb28181: snapshot exchange failed: {e}");
                    Vec::new()
                }
            };
            let notify = match super::client::build_upload_snapshot_finished_message(
                sn,
                &device_id,
                &session_id,
                &file_ids,
                &domain,
                &local_ip,
                local_port,
            ) {
                Ok(m) => m,
                Err(e) => {
                    log::error!("gb28181: snapshot-finished notify build failed: {e}");
                    return;
                }
            };
            let data = serialize_wire(&notify);
            if let Err(e) = socket.send_to(&data, platform_addr).await {
                log::error!("gb28181: snapshot-finished notify send failed: {e}");
            }
        });
    }

    /// Receive the REGISTER response whose CSeq matches `expected_cseq`,
    /// skipping stale responses from previous cycles (issue #11).
    ///
    /// The timeout bounds the TOTAL wait, not per-skipped-message.
    async fn receive_register_response(
        &self,
        expected_cseq: u32,
        timeout: Duration,
    ) -> Result<SipMessage> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remain.is_zero() {
                bail!("gb28181: register response timeout (cseq {expected_cseq})");
            }
            let msg = self.receive_with_timeout(remain).await?;
            // A REGISTER response echoes "CSeq: <n> REGISTER". Only an exact
            // CSeq match belongs to this attempt; anything else (late 200 of
            // the previous cycle, an old-nonce 401, mid-dialog traffic) is
            // stale or unrelated — skip it.
            let cseq_matches = msg
                .get_header("CSeq")
                .and_then(|v| v.split_whitespace().next())
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|n| n == expected_cseq);
            let is_register_response = msg
                .get_header("CSeq")
                .is_some_and(|v| v.contains("REGISTER"));
            if cseq_matches && is_register_response && msg.status_code.is_some() {
                return Ok(msg);
            }
        }
    }

    /// Receive a SIP message with timeout.
    async fn receive_with_timeout(&self, timeout: Duration) -> Result<SipMessage> {
        let socket = self
            .sip_socket
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("gb28181: SIP socket not bound"))?;
        let mut buf = vec![0u8; MAX_SIP_PACKET_SIZE];
        let (len, _) = tokio::time::timeout(timeout, socket.recv_from(&mut buf))
            .await
            .context("gb28181: receive timeout")?
            .context("gb28181: recv_from failed")?;

        let data = &buf[..len];
        let parsed = match std::str::from_utf8(data) {
            Ok(s) => SipMessage::parse(s),
            Err(_) => {
                let decoded = crate::charset::decode_wire_body(data);
                log::debug!("gb28181: decoded non-UTF-8 SIP datagram as GB18030");
                SipMessage::parse(&decoded)
            }
        };
        parsed.context("gb28181: parse failed")
    }
}

/// Registration refresh deadline: half the negotiated expiry, never below 1s
/// (RFC 3261 §10.2 — clients commonly refresh at 50% of the expiry window).
fn registration_refresh_interval_secs(expires_secs: u64) -> u64 {
    (expires_secs / 2).max(1)
}

/// Serialize a SIP message for the wire with charset-correct body encoding.
///
/// Headers are always ASCII. The body is encoded by [`crate::charset`]:
/// ASCII bodies (the overwhelmingly common case) produce bytes identical to
/// `serialize()`; a non-ASCII body whose XML declaration says GB2312 is
/// encoded as GB18030. `Content-Length` is recomputed from the encoded body
/// so it always matches the bytes actually sent.
pub(crate) fn serialize_wire(msg: &SipMessage) -> Vec<u8> {
    let body_bytes = crate::charset::encode_wire_body(&msg.body);
    let mut head = String::with_capacity(256);
    head.push_str(&msg.start_line);
    head.push_str("\r\n");
    for (name, value) in &msg.headers {
        if name.eq_ignore_ascii_case("Content-Length") {
            head.push_str(&format!("{name}: {}\r\n", body_bytes.len()));
        } else {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(&body_bytes);
    out
}

/// Build a device SDP answer for INVITE response.
fn build_device_sdp_answer(
    media_port: u16,
    ssrc: u32,
    device_ip: &str,
    transport: Transport,
    session_type: SessionType,
) -> String {
    let session_name = match session_type {
        SessionType::Play => "Play",
        SessionType::Playback => "Playback",
        SessionType::Download => "Download",
    };
    if transport == Transport::Tcp {
        // TCP/RTP/AVP with $-framing (GB/T 28181 Annex C.2), device is
        // the active side that connects to the platform's media port.
        format!(
            "v=0\r\n\
             o=- 0 0 IN IP4 {}\r\n\
             s={}\r\n\
             c=IN IP4 {}\r\n\
             t=0 0\r\n\
             m=video {} TCP/RTP/AVP 96\r\n\
             a=setup:active\r\n\
             a=connection:new\r\n\
             a=rtpmap:96 PS/90000\r\n\
             y={}\r\n",
            device_ip, session_name, device_ip, media_port, ssrc
        )
    } else {
        format!(
            "v=0\r\n\
             o=- 0 0 IN IP4 {}\r\n\
             s={}\r\n\
             c=IN IP4 {}\r\n\
             t=0 0\r\n\
             m=video {} RTP/AVP 96\r\n\
             a=rtpmap:96 PS/90000\r\n\
             y={}\r\n",
            device_ip, session_name, device_ip, media_port, ssrc
        )
    }
}

/// Build the SDP answer for an audio-only talkback INVITE
/// (GB/T 28181-2022 §9.2): the device advertises the UDP port its RTP
/// receive loop is bound to and mirrors the offered G.711 payload type.
/// Upstream talkback state (issue #61): the host-fed G.711 frame
/// channel plus the RTP bookkeeping for the send half.
struct TalkbackUpstream {
    rx: Arc<std::sync::Mutex<std::sync::mpsc::Receiver<Vec<u8>>>>,
    dst: SocketAddr,
    pt: u8,
    seq: u16,
    ts: u32,
    ssrc: u32,
}

fn build_audio_sdp_answer(
    media_port: u16,
    ssrc: u32,
    device_ip: &str,
    codec: AudioCodec,
    direction: &str,
) -> String {
    let pt = codec.payload_type();
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 {device_ip}\r\n\
         s=Play\r\n\
         c=IN IP4 {device_ip}\r\n\
         t=0 0\r\n\
         m=audio {media_port} RTP/AVP {pt}\r\n\
         a=rtpmap:{pt} {}/8000\r\n\
         {direction}y={ssrc}\r\n",
        codec.name()
    )
}

/// Build a SIP error response.
/// §9.12.1 信令3: the A.2.6.11 broadcast acknowledgement MESSAGE toward
/// the platform (To = SIP server, same shape as the keepalive notify).
#[must_use]
fn build_broadcast_response_message(
    sn: u32,
    device_id: &str,
    domain: &str,
    local_ip: &str,
    local_port: u16,
    ok: bool,
) -> SipMessage {
    let body = crate::manscdp::build_broadcast_response(sn, device_id, ok);
    let uri = format!("sip:{domain}@{domain}");
    let headers = vec![
        (
            "Via".to_string(),
            format!(
                "SIP/2.0/UDP {}:{};rport;branch={}",
                local_ip,
                local_port,
                super::sip::random_branch()
            ),
        ),
        ("From".to_string(), format!("<sip:{device_id}@{domain}>")),
        ("To".to_string(), format!("<sip:{domain}@{domain}>")),
        (
            "Call-ID".to_string(),
            format!("bresp{}@{device_id}", rand::random::<u64>()),
        ),
        ("CSeq".to_string(), "1 MESSAGE".to_string()),
        (
            "Contact".to_string(),
            format!("<sip:{device_id}@{local_ip}:{local_port}>"),
        ),
        ("Max-Forwards".to_string(), "70".to_string()),
        (
            "Content-Type".to_string(),
            "Application/MANSCDP+xml".to_string(),
        ),
        ("Content-Length".to_string(), body.len().to_string()),
    ];
    SipMessage {
        start_line: format!("MESSAGE {uri} SIP/2.0"),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some(uri),
        version: "SIP/2.0".to_string(),
        headers,
        body,
    }
}

/// §9.12.1 信令5: the audio-only INVITE toward the announced source —
/// s=Play (live), m=audio with the device's receive port, y= SSRC, and
/// the GB Subject convention `<sourceID>:<ssrc>,<deviceID>:0`. The
/// Request-URI targets the platform's actual SIP address (the domain is
/// not DNS-routable) with the announced source as the user part.
#[must_use]
#[allow(clippy::too_many_arguments)]
fn build_broadcast_invite(
    device_id: &str,
    domain: &str,
    local_ip: &str,
    local_sip_port: u16,
    source_id: &str,
    media_port: u16,
    ssrc: u32,
    platform_addr: SocketAddr,
) -> SipMessage {
    let uri = format!("sip:{source_id}@{platform_addr}");
    let sdp = format!(
        "v=0\r\no=- 0 0 IN IP4 {local_ip}\r\ns=Play\r\nc=IN IP4 {local_ip}\r\nt=0 0\r\nm=audio {media_port} RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\ny={ssrc}\r\n"
    );
    let headers = vec![
        (
            "Via".to_string(),
            format!(
                "SIP/2.0/UDP {}:{};rport;branch={}",
                local_ip,
                local_sip_port,
                super::sip::random_branch()
            ),
        ),
        (
            "From".to_string(),
            format!(
                "<sip:{device_id}@{domain}>;tag={:08x}",
                rand::random::<u32>()
            ),
        ),
        ("To".to_string(), format!("<sip:{source_id}@{domain}>")),
        (
            "Call-ID".to_string(),
            format!("bcast{}@{local_ip}", rand::random::<u64>()),
        ),
        ("CSeq".to_string(), "1 INVITE".to_string()),
        (
            "Contact".to_string(),
            format!("<sip:{device_id}@{local_ip}:{local_sip_port}>"),
        ),
        ("Max-Forwards".to_string(), "70".to_string()),
        ("Content-Type".to_string(), "application/sdp".to_string()),
        (
            "Subject".to_string(),
            format!("{source_id}:{ssrc},{device_id}:0"),
        ),
        ("Content-Length".to_string(), sdp.len().to_string()),
    ];
    SipMessage {
        start_line: format!("INVITE {uri} SIP/2.0"),
        method: Some(SipMethod::Invite),
        status_code: None,
        uri: Some(uri),
        version: "SIP/2.0".to_string(),
        headers,
        body: sdp,
    }
}

/// §9.12.1 信令15: the in-dialog ACK for the platform's 200 OK (the To
/// header — including its tag — comes verbatim from the response).
#[must_use]
fn build_broadcast_ack(invite: &SipMessage, resp: &SipMessage) -> SipMessage {
    let via_host = invite
        .get_header("Via")
        .map(|v| {
            let rest = v.strip_prefix("SIP/2.0/UDP").unwrap_or(v);
            rest.split(';').next().unwrap_or(rest).trim().to_string()
        })
        .unwrap_or_default();
    let mut headers = vec![
        (
            "Via".to_string(),
            format!(
                "SIP/2.0/UDP {via_host};branch={}",
                super::sip::random_branch()
            ),
        ),
        (
            "From".to_string(),
            invite.get_header("From").unwrap_or("").to_string(),
        ),
        (
            "To".to_string(),
            resp.get_header("To").unwrap_or("").to_string(),
        ),
        (
            "Call-ID".to_string(),
            invite.get_header("Call-ID").unwrap_or("").to_string(),
        ),
        ("CSeq".to_string(), "1 ACK".to_string()),
        ("Max-Forwards".to_string(), "70".to_string()),
        ("Content-Length".to_string(), "0".to_string()),
    ];
    headers.dedup_by(|a, b| a.0 == b.0);
    SipMessage {
        start_line: format!("ACK {} SIP/2.0", invite.uri.clone().unwrap_or_default()),
        method: Some(SipMethod::Ack),
        status_code: None,
        uri: invite.uri.clone(),
        version: "SIP/2.0".to_string(),
        headers,
        body: String::new(),
    }
}

/// Broadcast RTP receive loop (§9.12.1 信令16 onward): strip the fixed
/// 12-byte header (+ CSRC list) and hand the G.711 payload to the sink.
/// The announced SSRC is the fallback when the packet carries 0.
async fn run_broadcast_receiver(
    socket: Arc<UdpSocket>,
    sink: Arc<dyn AudioTalkbackSink>,
    session_ssrc: u32,
) {
    let mut buf = vec![0u8; 2048];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, _)) => {
                if len < 12 {
                    continue;
                }
                let csrc_count = (buf[0] & 0x0F) as usize;
                let header_len = 12 + csrc_count * 4;
                if len <= header_len {
                    continue;
                }
                let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                sink.on_audio_codec(
                    &buf[header_len..len],
                    if ssrc != 0 { ssrc } else { session_ssrc },
                    AudioCodec::Pcma,
                );
            }
            Err(e) => {
                log::debug!("gb28181: broadcast recv loop ended: {e}");
                break;
            }
        }
    }
}

fn build_error_response(request: &SipMessage, code: u16, reason: &str) -> SipMessage {
    let mut headers = Vec::new();

    // Copy headers from request
    if let Some(via) = request.get_header("Via") {
        headers.push(("Via".to_string(), via.to_string()));
    }
    if let Some(from) = request.get_header("From") {
        headers.push(("From".to_string(), from.to_string()));
    }
    if let Some(to) = request.get_header("To") {
        headers.push(("To".to_string(), to.to_string()));
    }
    if let Some(call_id) = request.get_header("Call-ID") {
        headers.push(("Call-ID".to_string(), call_id.to_string()));
    }
    if let Some(cseq) = request.get_header("CSeq") {
        headers.push(("CSeq".to_string(), cseq.to_string()));
    }

    headers.push(("Content-Length".to_string(), "0".to_string()));

    SipMessage {
        start_line: format!("SIP/2.0 {} {}", code, reason),
        method: None,
        status_code: None, // We're building a response, not a request
        uri: request.uri.clone(),
        version: "SIP/2.0".to_string(),
        headers,
        body: String::new(),
    }
}

/// Run the keepalive task (stops when `shutdown` fires).
#[allow(clippy::too_many_arguments)]
async fn run_keepalive(
    sip_socket: Arc<UdpSocket>,
    platform_addr: SocketAddr,
    device_id: &str,
    domain: &str,
    local_ip: &str,
    local_port: u16,
    interval_secs: u64,
    authenticator: Option<Arc<dyn RegisterAuthenticator>>,
    metrics: Arc<dyn crate::metrics::MetricsHooks>,
    shutdown: &mut watch::Receiver<ShutdownMode>,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
    let mut sn = 1u32;
    let mut cseq = 1000u32;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let sn_str = sn.to_string();
                let mut notify = build_keepalive_notify(
                    &sn_str, device_id, domain, local_ip, local_port, "OK", cseq,
                )?;

                // GB35114: stamp Date + Note (keyed-SM3 digest) on every
                // keepalive when an authenticator with signing support is
                // installed and the VKEK has been negotiated.
                if let Some(auth) = &authenticator {
                    let method = notify.method.map(|m| m.to_string()).unwrap_or_default();
                    let (date, note) = auth.decorate_outgoing(
                        &method,
                        notify.get_header("From").unwrap_or(""),
                        notify.get_header("To").unwrap_or(""),
                        notify.get_header("Call-ID").unwrap_or(""),
                        &notify.body,
                    );
                    if !date.is_empty() {
                        notify.headers.push(("Date".to_string(), date));
                        notify.headers.push(("Note".to_string(), note));
                    }
                }

                let data = serialize_wire(&notify);
                if let Err(e) = sip_socket.send_to(&data, platform_addr).await {
                    metrics.keepalive_fail();
                    log::error!("gb28181: keepalive send failed: {e}");
                }

                sn += 1;
                cseq += 1;
            }
            _ = shutdown.changed() => {
                log::info!("gb28181: keepalive task stopping (shutdown)");
                return Ok(());
            }
        }
    }
}

/// Run the media streaming task.
///
/// Receives H.264 AccessUnits from AuHub, multiplexes to PS,
/// and sends as RTP packets to the platform.
async fn run_media_task(
    mut rx: mpsc::Receiver<AccessUnit>,
    media_socket: Arc<UdpSocket>,
    media_tcp_conn: Option<Arc<Mutex<TcpStream>>>,
    ssrc: u32,
    device_id: &str,
    remote_addr: SocketAddr,
    metrics: Arc<dyn crate::metrics::MetricsHooks>,
) -> Result<()> {
    metrics.invite_session_started();
    struct StopOnDrop(Arc<dyn crate::metrics::MetricsHooks>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.invite_session_stopped();
        }
    }
    let _media_stop = StopOnDrop(Arc::clone(&metrics));
    let mut rtp_pusher = RtpPusher::new(remote_addr, ssrc, PS_PAYLOAD_TYPE);
    let mut pts = 0u64;
    // Capture timestamp of the previous access unit — PTS deltas derive from
    // real capture time (90 kHz), so 25 fps streams no longer play at 30 fps.
    let mut last_capture: Option<std::time::Instant> = None;

    log::info!("gb28181: media task started for device {device_id}");

    while let Some(au) = rx.recv().await {
        // Convert NAL units to slices for mux_h264_to_ps
        let nalu_slices: Vec<&[u8]> = au.nalus.iter().map(|n| n.data.as_slice()).collect();

        // PTS/DTS at 90 kHz from capture-time deltas. The first frame uses
        // the nominal 30 fps increment (3000 ticks); later frames use the
        // real inter-frame duration (clamped to 1..=100 s to survive clock
        // quirks and huge gaps after stream stalls).
        let delta_ticks: u32 = match last_capture.replace(au.timestamp) {
            None => 3000,
            Some(prev) => {
                let ticks = au
                    .timestamp
                    .saturating_duration_since(prev)
                    .as_millis()
                    .saturating_mul(90);
                u32::try_from(ticks).unwrap_or(u32::MAX).clamp(1, 9_000_000)
            }
        };
        pts += u64::from(delta_ticks);

        // Mux H.264 to PS
        let ps_data = mux_h264_to_ps(&nalu_slices, au.is_key_frame, pts, pts);

        // Send PS data as RTP packets
        // For PS, we just use the raw PS data as the RTP payload
        const MAX_RTP_PAYLOAD: usize = 1400;
        metrics.ps_bytes_out(ps_data.len() as u64);
        metrics.rtp_packets_out(1); // one RTP packet per chunk below
        let chunk_count = ps_data.len().div_ceil(MAX_RTP_PAYLOAD);

        for (i, chunk) in ps_data.chunks(MAX_RTP_PAYLOAD).enumerate() {
            let is_last = i == chunk_count - 1;
            let rtp_packet = build_rtp_packet_raw(
                ssrc,
                PS_PAYLOAD_TYPE,
                rtp_pusher.sequence_number,
                rtp_pusher.timestamp,
                chunk,
                is_last,
            );

            rtp_pusher.sequence_number = rtp_pusher.sequence_number.wrapping_add(1);

            if let Some(conn) = &media_tcp_conn {
                let mut conn = conn.lock().await;
                let frame = frame_rtp_over_tcp(&rtp_packet);
                if let Err(e) = conn.write_all(&frame).await {
                    log::error!("gb28181: failed to send RTP packet over TCP: {e}");
                    break;
                }
            } else if let Err(e) = media_socket.send_to(&rtp_packet, remote_addr).await {
                log::error!("gb28181: failed to send RTP packet: {e}");
                break;
            }
        }

        rtp_pusher.increment_timestamp(delta_ticks);
    }

    log::info!("gb28181: media task ended for device {device_id}");
    Ok(())
}

/// Build a raw RTP packet (for PS payload).
pub(super) fn build_rtp_packet_raw(
    ssrc: u32,
    payload_type: u8,
    seq_num: u16,
    timestamp: u32,
    payload: &[u8],
    marker: bool,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12 + payload.len());
    // First byte: version 2, padding 0, extension 0, csrc_count 0
    buf.push(0x80);
    // Second byte: marker + payload type
    let marker_byte = if marker { 0x80 } else { 0x00 };
    buf.push(marker_byte | (payload_type & 0x7F));
    buf.extend_from_slice(&seq_num.to_be_bytes());
    buf.extend_from_slice(&timestamp.to_be_bytes());
    buf.extend_from_slice(&ssrc.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Frame an RTP packet with GB28181 Annex C.2 $-framing.
///
/// Wire format (RTSP-interleaved style, as consumed by GB28181 platforms
/// and ZLMediaKit/wvp-class receivers): `[0x24 '$'] [channel: 0x00]
/// [2-byte big-endian length] [RTP packet bytes]` — a 4-byte header.
pub(super) fn frame_rtp_over_tcp(rtp_packet: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + rtp_packet.len());
    frame.push(0x24); // '$'
    frame.push(0x00); // channel
    frame.push((rtp_packet.len() >> 8) as u8);
    frame.push(rtp_packet.len() as u8);
    frame.extend_from_slice(rtp_packet);
    frame
}

/// Handle a TCP connection for SIP signaling.
///
/// Reads Content-Length framed SIP messages from the connection and
/// dispatches them through the same `handle_message` logic as UDP. Stops
/// cleanly when `shutdown` fires.
async fn handle_tcp_connection(
    conn: TcpStream,
    peer_addr: SocketAddr,
    au_hub: Arc<dyn FrameSource>,
    config: Gb28181Config,
    recording_index: Option<Arc<dyn RecordingSource>>,
    audio_sink: Option<Arc<dyn AudioTalkbackSink>>,
    shutdown: &mut watch::Receiver<ShutdownMode>,
) -> Result<()> {
    use tokio::io::BufReader;

    // Split the stream into read/write halves. The write half is
    // stored in the server's tcp_conn and used by send_sip_message.
    let (read_half, write_half) = conn.into_split();
    let mut reader = BufReader::new(read_half);

    // Detect local IP by probing the route to the platform
    let platform_sip_addr: SocketAddr = format!(
        "{}:{}",
        config.platform_sip_address, config.platform_sip_port
    )
    .parse()
    .context("gb28181: invalid platform SIP address")?;

    let local_ip = {
        let probe = UdpSocket::bind("0.0.0.0:0").await?;
        probe.connect(platform_sip_addr).await?;
        probe.local_addr()?.ip().to_string()
    };
    let local_sip_port = config.local_sip_port;

    // Placeholder UDP socket: never used for sending (tcp_conn takes
    // precedence in send_sip_message), bound so receive_with_timeout
    // does not error if re-registration is ever attempted on TCP.
    let placeholder_udp = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

    // Server instance bound to this TCP connection: all responses
    // produced by handle_message are written to the TCP conn.
    let mut server = Gb28181Server {
        config,
        au_hub,
        metrics: Arc::new(crate::metrics::NoopMetrics),
        sip_socket: Some(placeholder_udp),
        tcp_conn: Some(write_half),
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        broadcast_pending: None,
        local_ip: local_ip.clone(),
        recording_index,
        playback_ctl: None,
        audio_sink,
        talkback_source: None,
        authenticator: None,
        platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
        platform_date: Arc::new(std::sync::Mutex::new(None)),
        snapshot_executor: None,
        // DeviceControl dispatch over TCP is a follow-up (issue #58);
        // controls keep the reject path here.
        control_handler: None,
        config_handler: None,
        notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
        position_source: None,
        position_cancel: None,
        notifier_std_sock: None,
    };

    // Create SIP device client (User-Agent from config; neutral default)
    let mut sip_client = SipDeviceClient::new(
        &server.config.device_id,
        platform_sip_addr,
        &local_ip,
        local_sip_port,
        &server.config.sip_domain,
        &server.config.password,
        server.config.register_interval_secs as u32,
    )
    .with_user_agent(&server.config.effective_user_agent());

    // Message loop: read Content-Length framed SIP messages
    let mut keepalive_failures = 0u32;
    let mut buf = Vec::new();

    loop {
        buf.clear();
        tokio::select! {
            framed = read_sip_message_framed(&mut reader, &mut buf) => {
                match framed {
                    Ok(()) => {
                        let parsed = match std::str::from_utf8(&buf) {
                            Ok(s) => SipMessage::parse(s),
                            Err(_) => {
                                let decoded = crate::charset::decode_wire_body(&buf);
                                log::debug!("gb28181: decoded non-UTF-8 SIP frame as GB18030");
                                SipMessage::parse(&decoded)
                            }
                        };
                        if let Ok(msg) = parsed {
                            if let Err(e) = server
                                .handle_message(
                                    &msg,
                                    peer_addr,
                                    &mut sip_client,
                                    platform_sip_addr,
                                    &mut keepalive_failures,
                                )
                                .await
                            {
                                log::error!("gb28181: TCP message handling error: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("gb28181: TCP read error: {e}");
                        break;
                    }
                }
            }
            _ = shutdown.changed() => {
                log::info!("gb28181: shutdown requested — closing TCP connection handler");
                break;
            }
        }
    }

    Ok(())
}

/// Read a SIP message with Content-Length framing from a TCP stream.
async fn read_sip_message_framed<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<()> {
    // Read header section until an empty line (\r\n\r\n)
    let mut header_buf = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        header_buf.extend_from_slice(line.as_bytes());
        if line.trim().is_empty() {
            break;
        }
    }

    // Extract Content-Length from headers
    let content_length = {
        let header_str = std::str::from_utf8(&header_buf)?;
        header_str
            .lines()
            .find(|l| l.to_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split(':').nth(1))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
    };

    // Read body if Content-Length > 0
    let mut body_buf = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body_buf).await?;
    }

    // Combine headers and body
    buf.clear();
    buf.extend_from_slice(&header_buf);
    buf.extend_from_slice(&body_buf);

    Ok(())
}

/// Random CSeq sequence number for outbound SIP transactions.
///
/// Constrained to < 2^31 (issue #5): gosip (MiBee NVR) parses the CSeq
/// sequence into a signed int32 and silently drops the header (→ 400 Bad
/// Request) when the value overflows — a full-range u32 random fails ~50%
/// of the time.
fn random_cseq() -> u32 {
    rand::random::<u32>() % 2_000_000_000
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    // ── DeviceControl dispatch (issue #58) ─────────────────────────────

    /// Recording mock: captures which trait methods fired and with what
    /// arguments, so tests assert the exact kind→handler mapping.
    struct RecordingControl(Arc<std::sync::Mutex<Vec<String>>>);

    impl DeviceControlHandler for RecordingControl {
        fn on_force_iframe(&self) {
            self.0.lock().unwrap().push("iframe".into());
        }
        fn on_record(&self, start: bool) {
            self.0.lock().unwrap().push(format!("record:{start}"));
        }
        fn on_guard(&self, arm: bool) {
            self.0.lock().unwrap().push(format!("guard:{arm}"));
        }
        fn on_reset_alarm(&self) {
            self.0.lock().unwrap().push("alarm".into());
        }
        fn on_teleboot(&self) {
            self.0.lock().unwrap().push("boot".into());
        }
        fn on_ptz(&self, cmd: &crate::manscdp::PtzCommand) {
            self.0.lock().unwrap().push(format!("ptz:{cmd:?}"));
        }
        fn on_drag_zoom(&self, cmd: &crate::manscdp::DragZoom) {
            self.0.lock().unwrap().push(format!("dragzoom:{cmd:?}"));
        }
    }

    fn dispatch_of(body: &str) -> Vec<String> {
        let control = crate::manscdp::parse_device_control(body).expect("control must parse");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        dispatch_device_control(Arc::new(RecordingControl(Arc::clone(&calls))), &control);
        let mut out = calls.lock().unwrap().clone();
        out.sort();
        out
    }

    #[test]
    fn device_control_dispatch_fires_matching_handler() {
        // Wire path: parse (the same function the UDP MESSAGE arm calls)
        // → dispatch. One golden per kind proves the mapping.
        assert_eq!(
            dispatch_of(
                "<Control><CmdType>DeviceControl</CmdType><SN>1</SN>\
                 <DeviceID>d</DeviceID><IFrameCmd>Send</IFrameCmd></Control>"
            ),
            vec!["iframe".to_string()]
        );
        assert_eq!(
            dispatch_of(
                "<Control><CmdType>DeviceControl</CmdType><SN>1</SN>\
                 <DeviceID>d</DeviceID><RecordCmd>StopRecord</RecordCmd></Control>"
            ),
            vec!["record:false".to_string()]
        );
        assert_eq!(
            dispatch_of(
                "<Control><CmdType>DeviceControl</CmdType><SN>1</SN>\
                 <DeviceID>d</DeviceID><GuardCmd>SetGuard</GuardCmd></Control>"
            ),
            vec!["guard:true".to_string()]
        );
        assert_eq!(
            dispatch_of(
                "<Control><CmdType>DeviceControl</CmdType><SN>1</SN>\
                 <DeviceID>d</DeviceID><AlarmCmd>ResetAlarm</AlarmCmd></Control>"
            ),
            vec!["alarm".to_string()]
        );
        assert_eq!(
            dispatch_of(
                "<Control><CmdType>DeviceControl</CmdType><SN>1</SN>\
                 <DeviceID>d</DeviceID><TeleBoot>Boot</TeleBoot></Control>"
            ),
            vec!["boot".to_string()]
        );
        assert_eq!(
            dispatch_of(
                "<Control><CmdType>DeviceControl</CmdType><SN>1</SN>\
                 <DeviceID>d</DeviceID><PTZCmd>A50F0102200000D7</PTZCmd></Control>"
            ),
            vec![format!(
                "ptz:{:?}",
                crate::manscdp::PtzCommand::Move {
                    bits: crate::manscdp::PTZ_LEFT,
                    pan_speed: 0x20,
                    tilt_speed: 0,
                    zoom_speed: 0
                }
            )]
        );
        assert_eq!(
            dispatch_of(
                "<Control><CmdType>DeviceControl</CmdType><SN>1</SN>\
                 <DeviceID>d</DeviceID><DragZoomIn><Length>1920</Length><Width>1080</Width>\
                 <MidPointX>960</MidPointX><MidPointY>540</MidPointY><LengthX>480</LengthX>\
                 <LengthY>270</LengthY></DragZoomIn></Control>"
            ),
            vec![format!(
                "dragzoom:{:?}",
                crate::manscdp::DragZoom {
                    zoom_in: true,
                    length: 1920,
                    width: 1080,
                    mid_point_x: 960,
                    mid_point_y: 540,
                    length_x: 480,
                    length_y: 270,
                }
            )]
        );
    }

    // -- local IP probe retry (boot network race) ---------------------------

    /// A probe that fails twice with ENETUNREACH (the boot race: no route to
    /// the platform yet) and succeeds on the third call must succeed overall
    /// after exactly three attempts.
    #[tokio::test]
    async fn test_local_ip_probe_retries_transient_failures() {
        let (_tx, mut rx) = watch::channel(ShutdownMode::Init);
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let scripted = Arc::clone(&calls);
        let ip = probe_local_ip_with_retry(
            move || {
                let n = scripted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotConnected,
                            "network is unreachable (simulated boot race)",
                        ))
                    } else {
                        Ok("192.0.2.10".to_string())
                    }
                }
            },
            5,
            Duration::from_millis(1),
            &mut rx,
        )
        .await
        .expect("probe must succeed after transient failures");
        assert_eq!(ip.as_deref(), Some("192.0.2.10"));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "exactly three attempts expected"
        );
    }

    /// A permanently failing probe must return an error after exactly
    /// `max_attempts` attempts (not loop forever).
    #[tokio::test]
    async fn test_local_ip_probe_errors_after_max_attempts() {
        let (_tx, mut rx) = watch::channel(ShutdownMode::Init);
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let scripted = Arc::clone(&calls);
        let result = probe_local_ip_with_retry(
            move || {
                scripted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotConnected,
                        "network is unreachable",
                    ))
                }
            },
            3,
            Duration::from_millis(1),
            &mut rx,
        )
        .await;
        let err = result.expect_err("exhausted probe must error");
        assert!(
            err.to_string().contains("after 3 attempts"),
            "error should mention the attempt count: {err}"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    /// Shutdown requested during the backoff sleep must abort the probe
    /// immediately with `Ok(None)` instead of waiting out the backoff.
    #[tokio::test]
    async fn test_local_ip_probe_shutdown_during_backoff_aborts() {
        let (tx, mut rx) = watch::channel(ShutdownMode::Init);
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let scripted = Arc::clone(&calls);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(ShutdownMode::Fast);
        });
        let started = std::time::Instant::now();
        let ip = probe_local_ip_with_retry(
            move || {
                scripted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotConnected,
                        "network is unreachable",
                    ))
                }
            },
            100,
            Duration::from_secs(3600),
            &mut rx,
        )
        .await
        .expect("shutdown abort must not be an error");
        assert_eq!(ip, None, "shutdown must abort with Ok(None)");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown must preempt the (1-hour) backoff"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A probe that succeeds on the first attempt must not sleep at all.
    #[tokio::test]
    async fn test_local_ip_probe_first_try_success_no_retry() {
        let (_tx, mut rx) = watch::channel(ShutdownMode::Init);
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let scripted = Arc::clone(&calls);
        let ip = probe_local_ip_with_retry(
            move || {
                scripted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { Ok("203.0.113.7".to_string()) }
            },
            5,
            Duration::from_secs(3600),
            &mut rx,
        )
        .await
        .expect("immediate success");
        assert_eq!(ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn test_build_device_sdp_answer() {
        let sdp = build_device_sdp_answer(
            5004,
            12345,
            "192.168.1.100",
            Transport::Udp,
            SessionType::Play,
        );
        assert!(sdp.contains("m=video 5004 RTP/AVP 96"));
        assert!(sdp.contains("a=rtpmap:96 PS/90000"));
        assert!(sdp.contains("y=12345"));
        assert!(sdp.contains("c=IN IP4 192.168.1.100"));
    }

    /// Talkback SDP answer golden (GB/T 28181-2022 §9.2): m=audio mirrors
    /// the offered payload type, y= carries the session SSRC, and the
    /// device's receive port is the one the RTP recv loop binds.
    #[test]
    fn test_build_audio_sdp_answer_pcma() {
        let sdp = build_audio_sdp_answer(40000, 777, "192.168.62.104", AudioCodec::Pcma, "");
        assert_eq!(
            sdp,
            "v=0\r\no=- 0 0 IN IP4 192.168.62.104\r\ns=Play\r\nc=IN IP4 192.168.62.104\r\nt=0 0\r\nm=audio 40000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\ny=777\r\n"
        );
    }

    #[test]
    fn test_build_audio_sdp_answer_pcmu() {
        let sdp = build_audio_sdp_answer(40001, 778, "192.168.62.104", AudioCodec::Pcmu, "");
        assert_eq!(
            sdp,
            "v=0\r\no=- 0 0 IN IP4 192.168.62.104\r\ns=Play\r\nc=IN IP4 192.168.62.104\r\nt=0 0\r\nm=audio 40001 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\ny=778\r\n"
        );
    }

    /// Issue #5 regression: gosip (MiBee NVR) parses the CSeq sequence into
    /// a signed int32 and drops the header (→ 400 Bad Request) when the value
    /// exceeds i32::MAX. `random_cseq()` must NEVER return a value that
    /// overflows a signed 32-bit integer.
    #[test]
    fn test_random_cseq_stays_within_signed_int32() {
        // Many iterations: a full-range u32 rng would fail ~50% per draw, so
        // 200 draws make a regression essentially certain to be caught.
        for _ in 0..200 {
            let cseq = random_cseq();
            assert!(cseq < i32::MAX as u32, "CSeq {cseq} overflows signed int32");
            assert!(cseq > 0, "CSeq must be positive");
        }
    }

    /// TCP SDP answer: TCP/RTP/AVP + active-mode attributes (GB/T 28181).
    #[test]
    fn test_build_device_sdp_answer_tcp() {
        let sdp = build_device_sdp_answer(
            5004,
            12345,
            "192.168.1.100",
            Transport::Tcp,
            SessionType::Play,
        );
        assert!(sdp.contains("m=video 5004 TCP/RTP/AVP 96"));
        assert!(sdp.contains("a=setup:active"));
        assert!(sdp.contains("a=connection:new"));
        assert!(sdp.contains("a=rtpmap:96 PS/90000"));
        assert!(sdp.contains("y=12345"));
        assert!(sdp.contains("c=IN IP4 192.168.1.100"));
        assert!(
            !sdp.contains(" RTP/AVP 96"),
            "TCP SDP must not use the UDP m= line"
        );
    }

    /// Playback SDP answer echoes `s=Playback` (plan binding #7).
    #[test]
    fn test_build_device_sdp_answer_playback() {
        let sdp = build_device_sdp_answer(
            5004,
            12345,
            "192.168.1.100",
            Transport::Udp,
            SessionType::Playback,
        );
        assert!(sdp.contains("s=Playback\r\n"));
        assert!(sdp.contains("m=video 5004 RTP/AVP 96"));
        assert!(sdp.contains("y=12345"));
    }

    /// Download SDP answer echoes `s=Download` (plan binding #7).
    #[test]
    fn test_build_device_sdp_answer_download() {
        let sdp = build_device_sdp_answer(
            5004,
            12345,
            "192.168.1.100",
            Transport::Udp,
            SessionType::Download,
        );
        assert!(sdp.contains("s=Download\r\n"));
        assert!(sdp.contains("m=video 5004 RTP/AVP 96"));
        assert!(sdp.contains("y=12345"));
    }

    /// Playback over TCP: same echo, TCP/RTP/AVP + active-mode attributes.
    #[test]
    fn test_build_device_sdp_answer_playback_tcp() {
        let sdp = build_device_sdp_answer(
            5004,
            12345,
            "192.168.1.100",
            Transport::Tcp,
            SessionType::Playback,
        );
        assert!(sdp.contains("s=Playback\r\n"));
        assert!(sdp.contains("m=video 5004 TCP/RTP/AVP 96"));
        assert!(sdp.contains("a=setup:active"));
        assert!(sdp.contains("y=12345"));
    }

    /// GB/T 28181 Annex C.2 $-framing (RTSP-interleaved style):
    /// `[0x24] [channel 0x00] [len BE16] [payload]` — 4-byte header, the
    /// format GB28181 platforms actually demux (issue #14 regression).
    #[test]
    fn test_frame_rtp_over_tcp() {
        let rtp_packet = vec![0x80, 0x60, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01];
        let frame = frame_rtp_over_tcp(&rtp_packet);
        assert_eq!(frame.len(), 4 + rtp_packet.len());
        assert_eq!(frame[0], 0x24, "framing byte must be '$'");
        assert_eq!(frame[1], 0x00, "channel byte");
        let len = usize::from(frame[2]) << 8 | usize::from(frame[3]);
        assert_eq!(len, rtp_packet.len(), "big-endian length prefix");
        assert_eq!(
            &frame[4..],
            rtp_packet.as_slice(),
            "RTP payload after prefix"
        );
    }

    /// TCP transport: a Content-Length framed SIP MESSAGE (Keepalive body) sent
    /// to the server's TCP SIP listener gets a framed 200 OK back (GB/T 28181
    /// Annex C.1 framing; the 200 OK to a Keepalive has Content-Length: 0).
    #[tokio::test]
    async fn test_tcp_transport_handles_framed_sip() -> Result<()> {
        // Fake platform: a TCP listener standing in for the SIP platform (the
        // server only probes it for local-IP detection, never sends to it).
        let platform = TcpListener::bind("127.0.0.1:0").await?;
        let platform_port = platform.local_addr()?.port();

        // Reserve a free port for the server's TCP SIP listener.
        let probe = TcpListener::bind("127.0.0.1:0").await?;
        let server_port = probe.local_addr()?.port();
        drop(probe);

        let config = Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: platform_port,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: server_port,
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            transport: Transport::Tcp,
            ..Gb28181Config::default()
        };
        let handle =
            Gb28181Server::start(config, Arc::new(crate::mock::MockFrameHub::new()), None).await?;

        // Wait for the accept loop to bind the listener (spawned async).
        let mut conn = None;
        for _ in 0..100 {
            if let Ok(c) = TcpStream::connect(("127.0.0.1", server_port)).await {
                conn = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut conn = conn.ok_or_else(|| {
            anyhow!("gb28181: server TCP listener did not start on port {server_port}")
        })?;

        // Content-Length framed MESSAGE (Keepalive body).
        let body = "<Notify CmdType=\"Keepalive\" SN=\"1\"><DeviceID>34020000001320000001</DeviceID><Status>OK</Status></Notify>";
        let crlf = "\r\n";
        let wire = format!(
            "MESSAGE sip:3402000000@3402000000 SIP/2.0{crlf}\
             Via: SIP/2.0/TCP 127.0.0.1:{platform_port};branch=z9hG4bK-tcp-harness{crlf}\
             From: <sip:34020000002000000001@3402000000>;tag=platty{crlf}\
             To: <sip:34020000001320000001@3402000000>{crlf}\
             Call-ID: tcp-harness-1@example.com{crlf}\
             CSeq: 1 MESSAGE{crlf}\
             Max-Forwards: 70{crlf}\
             Content-Type: Application/MANSCDP+xml{crlf}\
             Content-Length: {}{crlf}\
             {crlf}\
             {}",
            body.len(),
            body
        );
        conn.write_all(wire.as_bytes()).await?;

        // Read the framed 200 OK response (Content-Length may be 0).
        let mut reader = tokio::io::BufReader::new(conn);
        let mut status_line = String::new();
        reader.read_line(&mut status_line).await?;
        assert!(
            status_line.contains("SIP/2.0 200"),
            "expected 200 OK, got {status_line:?}"
        );

        let mut content_length = None;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                bail!("connection closed while reading response headers");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if trimmed.to_lowercase().starts_with("content-length:") {
                content_length = trimmed
                    .split(':')
                    .nth(1)
                    .and_then(|v| v.trim().parse::<usize>().ok());
            }
        }
        let content_length =
            content_length.ok_or_else(|| anyhow!("gb28181: response missing Content-Length"))?;
        if content_length > 0 {
            let mut response_body = vec![0u8; content_length];
            reader.read_exact(&mut response_body).await?;
        }

        handle.abort();
        Ok(())
    }
}

/// Fake recording index returning a fixed segment list.
#[cfg(test)]
struct FakeRecordingSource {
    segments: Vec<super::SegmentMeta>,
}

#[cfg(test)]
impl super::RecordingSource for FakeRecordingSource {
    fn lookup(&self, _start_ms: u64, _end_ms: u64) -> Vec<super::SegmentMeta> {
        self.segments.clone()
    }
}

/// A RecordInfo query dispatched through `build_query_response` with a
/// recording source yields a response carrying one Item per segment.
#[tokio::test]
async fn test_recordinfo_dispatch_with_source() {
    let config = Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: 5060,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001320000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "12345678".to_string(),
        local_sip_port: 5060,
        register_interval_secs: 60,
        heartbeat_interval_secs: 60,
        heartbeat_timeout_count: 3,
        transport: Transport::Udp,
        ..Gb28181Config::default()
    };
    let source = FakeRecordingSource {
        segments: vec![super::SegmentMeta {
            file: "2026/08/15/14-30-00.h264".to_string(),
            start_ms: 1_786_804_200_000,
            end_ms: 1_786_804_500_000,
        }],
    };
    let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let server = Gb28181Server {
        config,
        au_hub: Arc::new(crate::mock::MockFrameHub::new()),
        metrics: Arc::new(crate::metrics::NoopMetrics),
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        broadcast_pending: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: Some(Arc::new(source)),
        playback_ctl: None,
        audio_sink: None,
        talkback_source: None,
        authenticator: None,
        platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
        platform_date: Arc::new(std::sync::Mutex::new(None)),
        snapshot_executor: None,
        control_handler: None,
        config_handler: None,
        notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
        position_source: None,
        position_cancel: None,
        notifier_std_sock: None,
    };

    // Query times are derived from the segment's own ms via the same
    // formatter, so the test is deterministic on any machine TZ.
    let start_s = super::client::format_gb_time_ms(1_786_804_200_000);
    let end_s = super::client::format_gb_time_ms(1_786_804_500_000);
    let body = format!(
        "<Query><CmdType>RecordInfo</CmdType><SN>9</SN><DeviceID>34020000001320000001</DeviceID><StartTime>{start_s}</StartTime><EndTime>{end_s}</EndTime></Query>"
    );
    let msg = SipMessage {
        start_line: "MESSAGE sip:3402000000@3402000000 SIP/2.0".to_string(),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some("sip:3402000000@3402000000".to_string()),
        version: "SIP/2.0".to_string(),
        headers: vec![(
            "Content-Type".to_string(),
            "Application/MANSCDP+xml".to_string(),
        )],
        body: body.to_string(),
    };

    let response = server
        .build_query_response(&msg)
        .expect("dispatch should succeed")
        .expect("RecordInfo should produce a response");
    assert!(response.body.contains("<SumNum>1</SumNum>"));
    assert!(response.body.contains("<RecordList Num=\"1\">"));
    assert!(response
        .body
        .contains("<FilePath>2026/08/15/14-30-00.h264</FilePath>"));
    assert!(response
        .body
        .contains(&format!("<StartTime>{start_s}</StartTime>")));
    assert!(response
        .body
        .contains(&format!("<EndTime>{end_s}</EndTime>")));
    assert!(response.body.contains("<Secrecy>0</Secrecy>"));
    // Cross-repo parity with Go: Name = segment file base name,
    // Address = the queried DeviceID.
    assert!(response.body.contains("<Name>14-30-00.h264</Name>"));
    assert!(response
        .body
        .contains("<Address>34020000001320000001</Address>"));
    assert!(response.body.contains("<Type>time</Type>"));
}

/// SUBSCRIBE handling (issue #57): supported subjects are booked and the
/// 200 OK echoes the request's Expires; unsupported subjects are answered
/// but not booked; renewal refreshes.
#[tokio::test]
async fn test_subscribe_books_and_echoes_expires() {
    let config = Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: 5060,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001320000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "12345678".to_string(),
        local_sip_port: 5060,
        register_interval_secs: 60,
        heartbeat_interval_secs: 60,
        heartbeat_timeout_count: 3,
        transport: Transport::Udp,
        ..Gb28181Config::default()
    };
    let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let mut server = Gb28181Server {
        config,
        au_hub: Arc::new(crate::mock::MockFrameHub::new()),
        metrics: Arc::new(crate::metrics::NoopMetrics),
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        broadcast_pending: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
        talkback_source: None,
        authenticator: None,
        platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
        platform_date: Arc::new(std::sync::Mutex::new(None)),
        snapshot_executor: None,
        control_handler: None,
        config_handler: None,
        notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
        position_source: None,
        position_cancel: None,
        notifier_std_sock: None,
    };

    let sub = SipMessage {
        start_line: "SUBSCRIBE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
        method: Some(SipMethod::Subscribe),
        status_code: None,
        uri: Some("sip:34020000001320000001@3402000000".to_string()),
        version: "SIP/2.0".to_string(),
        headers: vec![
            ("Event".to_string(), "Alarm".to_string()),
            ("Expires".to_string(), "1800".to_string()),
            ("From".to_string(), "<sip:p@3402000000>;tag=x".to_string()),
            (
                "To".to_string(),
                "<sip:34020000001320000001@3402000000>".to_string(),
            ),
            ("Call-ID".to_string(), "s-1".to_string()),
            ("CSeq".to_string(), "1 SUBSCRIBE".to_string()),
        ],
        body: String::new(),
    };
    let resp = server
        .handle_subscribe(&sub, "127.0.0.1:9".parse().unwrap())
        .expect("handle_subscribe");
    assert_eq!(resp.start_line, "SIP/2.0 200 OK");
    assert!(resp.get_header("Expires").is_some_and(|v| v == "1800"));
    assert!(server
        .notifier
        .subscribed(crate::subscribe::SubscribeEvent::Alarm));
    assert!(!server
        .notifier
        .subscribed(crate::subscribe::SubscribeEvent::Catalog));

    // Unsupported subject: answered, not booked.
    let mut other = sub.clone();
    other.headers[0].1 = "Presence".to_string();
    let resp = server
        .handle_subscribe(&other, "127.0.0.1:9".parse().unwrap())
        .expect("handle_subscribe");
    assert_eq!(resp.start_line, "SIP/2.0 200 OK");
    assert!(!server
        .notifier
        .subscribed(crate::subscribe::SubscribeEvent::MobilePosition));

    // Renewal keeps the event booked.
    let resp = server
        .handle_subscribe(&sub, "127.0.0.1:9".parse().unwrap())
        .expect("handle_subscribe");
    assert_eq!(resp.start_line, "SIP/2.0 200 OK");
    assert!(server
        .notifier
        .subscribed(crate::subscribe::SubscribeEvent::Alarm));
}

/// GB/T 28181-2022 information queries (A.2.4.10-14) must answer with
/// the minimal valid Response (issue #59) — previously they fell through
/// to the unknown-CmdType warn + silence.
#[tokio::test]
async fn test_gb2022_information_queries_dispatch() {
    let config = Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: 5060,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001320000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "12345678".to_string(),
        local_sip_port: 5060,
        register_interval_secs: 60,
        heartbeat_interval_secs: 60,
        heartbeat_timeout_count: 3,
        transport: Transport::Udp,
        ..Gb28181Config::default()
    };
    let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let server = Gb28181Server {
        config,
        au_hub: Arc::new(crate::mock::MockFrameHub::new()),
        metrics: Arc::new(crate::metrics::NoopMetrics),
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        broadcast_pending: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
        talkback_source: None,
        authenticator: None,
        platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
        platform_date: Arc::new(std::sync::Mutex::new(None)),
        snapshot_executor: None,
        control_handler: None,
        config_handler: None,
        notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
        position_source: None,
        position_cancel: None,
        notifier_std_sock: None,
    };

    for (cmd_type, body, want) in [
        ("HomePositionQuery", "<Query><CmdType>HomePositionQuery</CmdType><SN>61</SN><DeviceID>34020000001320000001</DeviceID></Query>", "<Response CmdType=\"HomePositionQuery\" SN=\"61\">"),
        ("CruiseTrackListQuery", "<Query><CmdType>CruiseTrackListQuery</CmdType><SN>62</SN><DeviceID>34020000001320000001</DeviceID></Query>", "<SumNum>0</SumNum>"),
        ("CruiseTrackQuery", "<Query><CmdType>CruiseTrackQuery</CmdType><SN>63</SN><DeviceID>34020000001320000001</DeviceID><Number>1</Number></Query>", "<SumNum>0</SumNum><Number>1</Number>"),
        ("PTZPosition", "<Query><CmdType>PTZPosition</CmdType><SN>64</SN><DeviceID>34020000001320000001</DeviceID></Query>", "<Response CmdType=\"PTZPosition\" SN=\"64\">"),
        ("SDCardStatus", "<Query><CmdType>SDCardStatus</CmdType><SN>65</SN><DeviceID>34020000001320000001</DeviceID></Query>", "<SumNum>0</SumNum>"),
    ] {
        let msg = SipMessage {
            start_line: "MESSAGE sip:3402000000@3402000000 SIP/2.0".to_string(),
            method: Some(SipMethod::Message),
            status_code: None,
            uri: Some("sip:3402000000@3402000000".to_string()),
            version: "SIP/2.0".to_string(),
            headers: vec![(
                "Content-Type".to_string(),
                "Application/MANSCDP+xml".to_string(),
            )],
            body: body.to_string(),
        };
        let response = server
            .build_query_response(&msg)
            .expect("dispatch should succeed")
            .unwrap_or_else(|| panic!("{cmd_type} must produce a response"));
        assert!(response.body.contains(want), "{cmd_type} body: {}", response.body);
    }
}

/// DeviceConfig dispatch (issue #57, A.2.3.2): a recognized sub-command
/// with an installed handler answers 200 OK + `Result=OK`; without a
/// handler the historical reject `Result=ERROR` stands. ConfigDownload
/// (A.2.4.7) answers OK with the BasicParam block only when requested.
#[tokio::test]
async fn test_deviceconfig_and_configdownload_dispatch() {
    use std::sync::Mutex;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    struct RecordingConfig(Arc<Mutex<Vec<String>>>);
    impl DeviceConfigHandler for RecordingConfig {
        fn on_basic_param(
            &self,
            name: Option<&str>,
            expiration: Option<u64>,
            heartbeat_interval: Option<u64>,
            heartbeat_count: Option<u32>,
        ) {
            self.0.lock().unwrap().push(format!(
                "basic:{name:?}/{expiration:?}/{heartbeat_interval:?}/{heartbeat_count:?}"
            ));
        }
        fn on_frame_mirror(&self, mode: u32) {
            self.0.lock().unwrap().push(format!("mirror:{mode}"));
        }
        fn on_alarm_report(&self, motion: u32, field: u32) {
            self.0
                .lock()
                .unwrap()
                .push(format!("alarm:{motion}/{field}"));
        }
    }

    let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let _server_addr = sip_socket.local_addr().expect("addr");
    let mut server = Gb28181Server {
        config: Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: 5060,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: 5060,
            register_interval_secs: 3600,
            heartbeat_interval_secs: 61,
            heartbeat_timeout_count: 4,
            transport: Transport::Udp,
            ..Gb28181Config::default()
        },
        au_hub: Arc::new(crate::mock::MockFrameHub::new()),
        metrics: Arc::new(crate::metrics::NoopMetrics),
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        broadcast_pending: None,
        local_ip: "127.0.0.1".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
        talkback_source: None,
        authenticator: None,
        platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
        platform_date: Arc::new(std::sync::Mutex::new(None)),
        snapshot_executor: None,
        control_handler: None,
        config_handler: Some(Arc::new(RecordingConfig(seen))),
        notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
        position_source: None,
        position_cancel: None,
        notifier_std_sock: None,
    };

    let peer = UdpSocket::bind("127.0.0.1:0").await.expect("peer bind");
    let peer_addr = peer.local_addr().expect("peer addr");
    let mut client = SipDeviceClient::new(
        "34020000001320000001",
        peer_addr,
        "127.0.0.1",
        5060,
        "3402000000",
        "12345678",
        3600,
    );
    let mut keepalive_failures = 0u32;

    let mut buf = vec![0u8; 65535];

    async fn read_peer(peer: &UdpSocket, buf: &mut [u8]) -> String {
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(buf))
            .await
            .expect("timed out")
            .expect("recv");
        String::from_utf8_lossy(&buf[..len]).to_string()
    }

    let message = |body: &str| SipMessage {
        start_line: "MESSAGE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some("sip:34020000001320000001@3402000000".to_string()),
        version: "SIP/2.0".to_string(),
        headers: vec![
            ("Call-ID".to_string(), "cfg-1".to_string()),
            ("CSeq".to_string(), "1 MESSAGE".to_string()),
            (
                "Content-Type".to_string(),
                "Application/MANSCDP+xml".to_string(),
            ),
        ],
        body: body.to_string(),
    };

    // 1) BasicParam with a handler: 200 OK, then Result=OK, handler fired.
    server
        .handle_message(
            &message(
                "<Control><CmdType>DeviceConfig</CmdType><SN>71</SN>\
                 <DeviceID>34020000001320000001</DeviceID>\
                 <BasicParam><Name>Dome</Name><Expiration>120</Expiration>\
                 <HeartBeatInterval>15</HeartBeatInterval><HeartBeatCount>5</HeartBeatCount>\
                 </BasicParam></Control>",
            ),
            peer_addr,
            &mut client,
            peer_addr,
            &mut keepalive_failures,
        )
        .await
        .expect("handle");
    let ok = read_peer(&peer, &mut buf).await;
    assert!(ok.starts_with("SIP/2.0 200 OK"), "first reply: {ok}");
    let resp = read_peer(&peer, &mut buf).await;
    assert!(
        resp.contains("<Response CmdType=\"DeviceConfig\" SN=\"71\">")
            && resp.contains("<Result>OK</Result>"),
        "config OK response: {resp}"
    );
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["basic:Some(\"Dome\")/Some(120)/Some(15)/Some(5)"]
    );

    // 2) FrameMirror: handler fired, Result=OK.
    calls.lock().unwrap().clear();
    server
        .handle_message(
            &message(
                "<Control><CmdType>DeviceConfig</CmdType><SN>72</SN>\
                 <DeviceID>34020000001320000001</DeviceID>\
                 <FrameMirror>1</FrameMirror></Control>",
            ),
            peer_addr,
            &mut client,
            peer_addr,
            &mut keepalive_failures,
        )
        .await
        .expect("handle");
    let _ = read_peer(&peer, &mut buf).await; // 200 OK
    let resp = read_peer(&peer, &mut buf).await;
    assert!(
        resp.contains("SN=\"72\"") && resp.contains("<Result>OK</Result>"),
        "{resp}"
    );
    assert_eq!(calls.lock().unwrap().as_slice(), ["mirror:1"]);

    // 3) Without a handler the historical reject stands (Result=ERROR).
    server.config_handler = None;
    server
        .handle_message(
            &message(
                "<Control><CmdType>DeviceConfig</CmdType><SN>73</SN>\
                 <DeviceID>34020000001320000001</DeviceID>\
                 <AlarmReport><MotionDetection>1</MotionDetection>\
                 <FieldDetection>0</FieldDetection></AlarmReport></Control>",
            ),
            peer_addr,
            &mut client,
            peer_addr,
            &mut keepalive_failures,
        )
        .await
        .expect("handle");
    let _ = read_peer(&peer, &mut buf).await; // 200 OK
    let resp = read_peer(&peer, &mut buf).await;
    assert!(
        resp.contains("<Response CmdType=\"DeviceConfig\" SN=\"73\">")
            && resp.contains("<Result>ERROR</Result>"),
        "config reject: {resp}"
    );

    // 4) ConfigDownload query: BasicParam block when requested (values
    // from the live config), bare OK otherwise.
    server.config_handler = Some(Arc::new(RecordingConfig(Arc::clone(&calls))));
    let query = |config_type: &str| SipMessage {
        start_line: "MESSAGE sip:3402000000@3402000000 SIP/2.0".to_string(),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some("sip:3402000000@3402000000".to_string()),
        version: "SIP/2.0".to_string(),
        headers: vec![(
            "Content-Type".to_string(),
            "Application/MANSCDP+xml".to_string(),
        )],
        body: format!(
            "<Query><CmdType>ConfigDownload</CmdType><SN>74</SN>\
             <DeviceID>34020000001320000001</DeviceID>\
             <ConfigType>{config_type}</ConfigType></Query>"
        ),
    };
    let with_basic = server
        .build_query_response(&query("BasicParam/FrameMirror"))
        .expect("dispatch")
        .expect("response");
    assert!(
        with_basic
            .body
            .contains("<Response CmdType=\"ConfigDownload\" SN=\"74\">")
            && with_basic.body.contains("<Result>OK</Result>")
            && with_basic.body.contains("<BasicParam><Name>")
            && with_basic.body.contains("<Expiration>3600</Expiration>")
            && with_basic
                .body
                .contains("<HeartBeatInterval>61</HeartBeatInterval>")
            && with_basic
                .body
                .contains("<HeartBeatCount>4</HeartBeatCount>"),
        "configdownload body: {}",
        with_basic.body
    );
    let without = server
        .build_query_response(&query("OSDConfig"))
        .expect("dispatch")
        .expect("response");
    assert!(
        without.body.contains("<Result>OK</Result>") && !without.body.contains("<BasicParam>"),
        "bare OK body: {}",
        without.body
    );
}

/// Without a recording source, a RecordInfo query yields the empty golden
/// response (byte-identical to the pre-R-RI output).
#[tokio::test]
async fn test_recordinfo_dispatch_without_source() {
    let config = Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: 5060,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001320000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "12345678".to_string(),
        local_sip_port: 5060,
        register_interval_secs: 60,
        heartbeat_interval_secs: 60,
        heartbeat_timeout_count: 3,
        transport: Transport::Udp,
        ..Gb28181Config::default()
    };
    let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let server = Gb28181Server {
        config,
        au_hub: Arc::new(crate::mock::MockFrameHub::new()),
        metrics: Arc::new(crate::metrics::NoopMetrics),
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        broadcast_pending: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
        talkback_source: None,
        authenticator: None,
        platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
        platform_date: Arc::new(std::sync::Mutex::new(None)),
        snapshot_executor: None,
        control_handler: None,
        config_handler: None,
        notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
        position_source: None,
        position_cancel: None,
        notifier_std_sock: None,
    };

    let body = "<Query><CmdType>RecordInfo</CmdType><SN>9</SN><DeviceID>34020000001320000001</DeviceID><StartTime>2026-08-15T14:00:00</StartTime><EndTime>2026-08-15T15:00:00</EndTime></Query>";
    let msg = SipMessage {
        start_line: "MESSAGE sip:3402000000@3402000000 SIP/2.0".to_string(),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some("sip:3402000000@3402000000".to_string()),
        version: "SIP/2.0".to_string(),
        headers: vec![(
            "Content-Type".to_string(),
            "Application/MANSCDP+xml".to_string(),
        )],
        body: body.to_string(),
    };

    let response = server
        .build_query_response(&msg)
        .expect("dispatch should succeed")
        .expect("RecordInfo should produce a response");
    assert_eq!(
            response.body,
            "<?xml version=\"1.0\" encoding=\"GB2312\"?><Response CmdType=\"RecordInfo\" SN=\"9\"><DeviceID>34020000001320000001</DeviceID><Name>34020000001320000001</Name><SumNum>0</SumNum><RecordList Num=\"0\"></RecordList></Response>"
        );
}

/// A Playback INVITE with no matching recordings is answered with
/// 488 Not Acceptable Here (plan binding #10).
#[tokio::test]
async fn test_playback_invite_empty_range_returns_488() {
    let config = Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: 5060,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001320000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "12345678".to_string(),
        local_sip_port: 5060,
        register_interval_secs: 60,
        heartbeat_interval_secs: 60,
        heartbeat_timeout_count: 3,
        transport: Transport::Udp,
        ..Gb28181Config::default()
    };
    let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let mut server = Gb28181Server {
        config,
        au_hub: Arc::new(crate::mock::MockFrameHub::new()),
        metrics: Arc::new(crate::metrics::NoopMetrics),
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        broadcast_pending: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
        talkback_source: None,
        authenticator: None,
        platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
        platform_date: Arc::new(std::sync::Mutex::new(None)),
        snapshot_executor: None,
        control_handler: None,
        config_handler: None,
        notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
        position_source: None,
        position_cancel: None,
        notifier_std_sock: None,
    };

    let body = "v=0\r\no=- 0 0 IN IP4 192.168.63.197\r\ns=Playback\r\nc=IN IP4 192.168.63.197\r\nt=1786804200 1786807800\r\nm=video 10000 RTP/AVP 96\r\ny=12345\r\n";
    let msg = SipMessage {
        start_line: "INVITE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
        method: Some(SipMethod::Invite),
        status_code: None,
        uri: Some("sip:34020000001320000001@3402000000".to_string()),
        version: "SIP/2.0".to_string(),
        headers: vec![
            ("Call-ID".to_string(), "playback-empty-1".to_string()),
            (
                "From".to_string(),
                "<sip:34020000002000000001@3402000000>;tag=plat".to_string(),
            ),
            (
                "To".to_string(),
                "<sip:34020000001320000001@3402000000>".to_string(),
            ),
            ("CSeq".to_string(), "1 INVITE".to_string()),
            (
                "Via".to_string(),
                "SIP/2.0/UDP 192.168.63.197:5060;branch=z9hG4bKtest".to_string(),
            ),
        ],
        body: body.to_string(),
    };

    let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
    let peer_addr = peer.local_addr().expect("peer addr");
    server
        .handle_invite(&msg, peer_addr)
        .await
        .expect("handle_invite should not error");

    let mut buf = vec![0u8; 65535];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
        .await
        .expect("timed out waiting for 488 response")
        .expect("recv failed");
    let resp =
        SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8")).expect("parse response");
    assert!(
        resp.start_line.contains("488"),
        "expected 488 Not Acceptable Here, got {}",
        resp.start_line
    );
}

/// A Playback INVITE with matching recordings gets 200 OK whose SDP echoes
/// `s=Playback` (plan binding #7).
#[tokio::test]
async fn test_playback_invite_returns_200_with_playback_sdp() {
    let config = Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: 5060,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001320000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "12345678".to_string(),
        local_sip_port: 5060,
        register_interval_secs: 60,
        heartbeat_interval_secs: 60,
        heartbeat_timeout_count: 3,
        transport: Transport::Udp,
        ..Gb28181Config::default()
    };
    let source = FakeRecordingSource {
        segments: vec![super::SegmentMeta {
            file: "2026/08/15/14-30-00.h264".to_string(),
            start_ms: 1_786_804_200_000,
            end_ms: 1_786_804_500_000,
        }],
    };
    let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let mut server = Gb28181Server {
        config,
        au_hub: Arc::new(crate::mock::MockFrameHub::new()),
        metrics: Arc::new(crate::metrics::NoopMetrics),
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        broadcast_pending: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: Some(Arc::new(source)),
        playback_ctl: None,
        audio_sink: None,
        talkback_source: None,
        authenticator: None,
        platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
        platform_date: Arc::new(std::sync::Mutex::new(None)),
        snapshot_executor: None,
        control_handler: None,
        config_handler: None,
        notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
        position_source: None,
        position_cancel: None,
        notifier_std_sock: None,
    };

    let body = "v=0\r\no=- 0 0 IN IP4 192.168.63.197\r\ns=Playback\r\nc=IN IP4 192.168.63.197\r\nt=1786804200 1786804500\r\nm=video 10000 RTP/AVP 96\r\ny=12345\r\n";
    let msg = SipMessage {
        start_line: "INVITE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
        method: Some(SipMethod::Invite),
        status_code: None,
        uri: Some("sip:34020000001320000001@3402000000".to_string()),
        version: "SIP/2.0".to_string(),
        headers: vec![
            ("Call-ID".to_string(), "playback-ok-1".to_string()),
            (
                "From".to_string(),
                "<sip:34020000002000000001@3402000000>;tag=plat".to_string(),
            ),
            (
                "To".to_string(),
                "<sip:34020000001320000001@3402000000>".to_string(),
            ),
            ("CSeq".to_string(), "1 INVITE".to_string()),
            (
                "Via".to_string(),
                "SIP/2.0/UDP 192.168.63.197:5060;branch=z9hG4bKtest".to_string(),
            ),
        ],
        body: body.to_string(),
    };

    let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
    let peer_addr = peer.local_addr().expect("peer addr");
    server
        .handle_invite(&msg, peer_addr)
        .await
        .expect("handle_invite should not error");

    let mut buf = vec![0u8; 65535];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
        .await
        .expect("timed out waiting for 200 OK")
        .expect("recv failed");
    let resp =
        SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8")).expect("parse response");
    assert!(
        resp.start_line.contains("200"),
        "expected 200 OK, got {}",
        resp.start_line
    );
    assert!(
        resp.body.contains("s=Playback"),
        "SDP answer must echo s=Playback, got: {}",
        resp.body
    );
}

/// Helper: a live-session server wired to a bound SIP socket + peer socket.
#[cfg(test)]
async fn live_invite_server() -> (Gb28181Server, UdpSocket, SocketAddr) {
    let config = Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: 5060,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001320000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "12345678".to_string(),
        local_sip_port: 5060,
        register_interval_secs: 60,
        heartbeat_interval_secs: 60,
        heartbeat_timeout_count: 3,
        transport: Transport::Udp,
        ..Gb28181Config::default()
    };
    let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let server = Gb28181Server {
        config,
        au_hub: Arc::new(crate::mock::MockFrameHub::new()),
        metrics: Arc::new(crate::metrics::NoopMetrics),
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        broadcast_pending: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
        talkback_source: None,
        authenticator: None,
        platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
        platform_date: Arc::new(std::sync::Mutex::new(None)),
        snapshot_executor: None,
        control_handler: None,
        config_handler: None,
        notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
        position_source: None,
        position_cancel: None,
        notifier_std_sock: None,
    };
    let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
    let peer_addr = peer.local_addr().expect("peer addr");
    (server, peer, peer_addr)
}

#[cfg(test)]
fn live_invite_msg(call_id: &str, cseq: u32, media_port: u16) -> SipMessage {
    let body = format!(
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=Play\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video {media_port} RTP/AVP 96\r\ny=12345\r\n"
    );
    SipMessage {
        start_line: "INVITE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
        method: Some(SipMethod::Invite),
        status_code: None,
        uri: Some("sip:34020000001320000001@3402000000".to_string()),
        version: "SIP/2.0".to_string(),
        headers: vec![
            ("Call-ID".to_string(), call_id.to_string()),
            (
                "From".to_string(),
                "<sip:34020000002000000001@3402000000>;tag=plat".to_string(),
            ),
            (
                "To".to_string(),
                "<sip:34020000001320000001@3402000000>".to_string(),
            ),
            ("CSeq".to_string(), format!("{cseq} INVITE")),
            (
                "Via".to_string(),
                "SIP/2.0/UDP 192.168.63.197:5060;branch=z9hG4bKtest".to_string(),
            ),
        ],
        body,
    }
}

#[cfg(test)]
async fn recv_sip(peer: &UdpSocket) -> SipMessage {
    let mut buf = vec![0u8; 65535];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
        .await
        .expect("timed out waiting for SIP response")
        .expect("recv failed");
    SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8")).expect("parse response")
}

/// Issue #18 regression: a retransmitted INVITE (same Call-ID, same CSeq —
/// the platform never saw the 200 OK) must be answered with the SAME 200 OK
/// again, not 486. RFC 3261 §13.3.1.4.
#[tokio::test]
async fn test_invite_retransmission_resends_cached_200() {
    let (mut server, peer, peer_addr) = live_invite_server().await;
    let invite = live_invite_msg("retrans-1", 1, 30000);
    server
        .handle_invite(&invite, peer_addr)
        .await
        .expect("first INVITE");
    let first = recv_sip(&peer).await;
    assert!(first.start_line.contains("200"), "got {}", first.start_line);

    // Retransmission of the same transaction.
    server
        .handle_invite(&invite, peer_addr)
        .await
        .expect("retransmitted INVITE");
    let second = recv_sip(&peer).await;
    assert!(
        second.start_line.contains("200"),
        "retransmission must re-receive 200 OK, got {}",
        second.start_line
    );
    // Same dialog → same local To tag.
    let tag_of = |m: &SipMessage| {
        m.get_header("To")
            .and_then(|t| t.split("tag=").nth(1).map(|s| s.to_string()))
    };
    assert_eq!(tag_of(&first), tag_of(&second), "To tag must be stable");
}

/// A same-dialog re-INVITE (same Call-ID, NEW CSeq — e.g. the platform
/// re-negotiating the media port) must be answered 200 with fresh SDP, not
/// rejected 486 (issue #18 family).
#[tokio::test]
async fn test_reinvite_new_cseq_recycles_session_and_answers_200() {
    let (mut server, peer, peer_addr) = live_invite_server().await;
    server
        .handle_invite(&live_invite_msg("reinvite-1", 1, 30000), peer_addr)
        .await
        .expect("initial INVITE");
    assert!(recv_sip(&peer).await.start_line.contains("200"));

    server
        .handle_invite(&live_invite_msg("reinvite-1", 2, 30001), peer_addr)
        .await
        .expect("re-INVITE");
    let resp = recv_sip(&peer).await;
    assert!(
        resp.start_line.contains("200"),
        "re-INVITE must get 200 OK, got {}",
        resp.start_line
    );
}

/// Issue #19: the registration refresh deadline is half the negotiated
/// expires so a platform restart recovers without keepalive-timeout delay.
#[test]
fn test_registration_refresh_interval_is_half_of_expires() {
    assert_eq!(registration_refresh_interval_secs(60), 30);
    assert_eq!(registration_refresh_interval_secs(3600), 1800);
    assert_eq!(registration_refresh_interval_secs(1), 1, "never below 1s");
}

/// A live (or no) session receiving SIP INFO PlaybackControl must get a
/// 200 OK and no crash — the control is a logged no-op.
#[tokio::test]
async fn test_info_playback_control_live_session_noop() {
    let config = Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: 5060,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001320000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "12345678".to_string(),
        local_sip_port: 5060,
        register_interval_secs: 60,
        heartbeat_interval_secs: 60,
        heartbeat_timeout_count: 3,
        transport: Transport::Udp,
        ..Gb28181Config::default()
    };
    let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
    let mut server = Gb28181Server {
        config,
        au_hub: Arc::new(crate::mock::MockFrameHub::new()),
        metrics: Arc::new(crate::metrics::NoopMetrics),
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        broadcast_pending: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
        talkback_source: None,
        authenticator: None,
        platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
        platform_date: Arc::new(std::sync::Mutex::new(None)),
        snapshot_executor: None,
        control_handler: None,
        config_handler: None,
        notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
        position_source: None,
        position_cancel: None,
        notifier_std_sock: None,
    };

    let body = "<Control><CmdType>PlaybackControl</CmdType><SN>1</SN><DeviceID>34020000001320000001</DeviceID><Info><ControlValue>PAUSE</ControlValue></Info></Control>";
    let msg = SipMessage {
        start_line: "INFO sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
        method: Some(SipMethod::Info),
        status_code: None,
        uri: Some("sip:34020000001320000001@3402000000".to_string()),
        version: "SIP/2.0".to_string(),
        headers: vec![
            (
                "From".to_string(),
                "<sip:34020000002000000001@3402000000>;tag=plat".to_string(),
            ),
            (
                "To".to_string(),
                "<sip:34020000001320000001@3402000000>".to_string(),
            ),
            ("CSeq".to_string(), "1 INFO".to_string()),
            (
                "Via".to_string(),
                "SIP/2.0/UDP 192.168.63.197:5060;branch=z9hG4bKinfo".to_string(),
            ),
            ("Call-ID".to_string(), "info-test".to_string()),
        ],
        body: body.to_string(),
    };

    let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
    let peer_addr = peer.local_addr().expect("peer addr");
    server
        .handle_info(&msg, peer_addr)
        .await
        .expect("handle_info should not error");

    let mut buf = vec![0u8; 65535];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
        .await
        .expect("timed out waiting for 200 OK")
        .expect("recv failed");
    let resp =
        SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8")).expect("parse response");
    assert!(
        resp.start_line.contains("200"),
        "expected 200 OK, got {}",
        resp.start_line
    );
}

#[cfg(test)]
mod tcp_media_tests {
    use super::*;
    /// A tcp-passive offer (TCP/RTP/AVP + a=setup:passive, the MiBeeNvr
    /// v0.11 default) must be answered with a TCP SDP declaring
    /// a=setup:active, and the device must CONNECT to the offered media
    /// port and send RFC 4571-framed RTP (2-byte length prefix). Issue #14.
    #[tokio::test]
    async fn test_invite_tcp_passive_connects_and_frames() {
        use tokio::io::AsyncReadExt;
        let config = Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: 5060,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: 5060,
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            transport: Transport::Udp, // SIP over UDP + TCP MEDIA — the #14 scenario
            ..Gb28181Config::default()
        };
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let hub = Arc::new(crate::mock::MockFrameHub::new());
        let mut server = Gb28181Server {
            config,
            au_hub: hub.clone(),
            metrics: Arc::new(crate::metrics::NoopMetrics),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
        };

        // Platform stand-in: TCP listener on an ephemeral port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listen");
        let media_port = listener.local_addr().expect("addr").port();

        let body = format!(
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=Play\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video {port} TCP/RTP/AVP 96\r\na=setup:passive\r\na=connection:new\r\ny=2000000001\r\n",
        port = media_port
    );
        let msg = SipMessage {
            start_line: "INVITE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
            method: Some(SipMethod::Invite),
            status_code: None,
            uri: Some("sip:34020000001320000001@3402000000".to_string()),
            version: "SIP/2.0".to_string(),
            headers: vec![
                ("Call-ID".to_string(), "tcp-passive-1".to_string()),
                (
                    "From".to_string(),
                    "<sip:34020000002000000001@3402000000>;tag=plat".to_string(),
                ),
                (
                    "To".to_string(),
                    "<sip:34020000001320000001@3402000000>".to_string(),
                ),
                ("CSeq".to_string(), "1 INVITE".to_string()),
                (
                    "Via".to_string(),
                    "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKmt".to_string(),
                ),
            ],
            body,
        };

        let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
        let peer_addr = peer.local_addr().expect("peer addr");
        server
            .handle_invite(&msg, peer_addr)
            .await
            .expect("handle_invite");

        // 200 OK SDP must echo TCP transport and declare setup:active.
        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("timed out waiting for 200 OK")
            .expect("recv failed");
        let resp = SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8"))
            .expect("parse response");
        assert!(resp.start_line.contains("200"), "got {}", resp.start_line);
        assert!(resp.body.contains("TCP/RTP/AVP 96"), "body: {}", resp.body);
        assert!(resp.body.contains("a=setup:active"), "body: {}", resp.body);

        // The device must dial the offered media port.
        let (mut media_conn, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("device never connected TCP media")
            .expect("accept failed");

        // Feed one keyframe; expect RFC 4571 framing: 2-byte BE length then RTP.
        hub.write(crate::frame::AccessUnit {
            nalus: vec![crate::frame::Nalu {
                nalu_type: 5,
                data: vec![0x65, 0x88, 0x84, 0x21, 0xa0],
                is_idr: true,
                is_sps: false,
                is_pps: false,
                is_aud: false,
            }],
            timestamp: std::time::Instant::now(),
            is_key_frame: true,
        });
        // GB28181 Annex C.2 $-framing (RTSP-interleaved style, 4-byte
        // header): '$' + channel byte + 2-byte BE length + RTP.
        let mut head = [0u8; 5];
        tokio::time::timeout(Duration::from_secs(3), media_conn.read_exact(&mut head))
            .await
            .expect("no framed RTP received")
            .expect("read failed");
        assert_eq!(head[0], 0x24, "framing byte, got {:#x}", head[0]);
        assert_eq!(head[1], 0x00, "channel byte, got {:#x}", head[1]);
        let frame_len = u16::from_be_bytes([head[2], head[3]]) as usize;
        assert!(
            frame_len >= 12,
            "frame len {frame_len} — smaller than an RTP header"
        );
        assert_eq!(head[4] & 0xc0, 0x80, "RTP version bits, got {:b}", head[4]);
    }

    /// setup:active offers (platform dials the device) are refused with 488
    /// instead of silently answering a mismatched transport. Issue #14.
    #[tokio::test]
    async fn test_invite_tcp_setup_active_returns_488() {
        let config = Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: 5060,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: 5060,
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            transport: Transport::Udp,
            ..Gb28181Config::default()
        };
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config,
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            metrics: Arc::new(crate::metrics::NoopMetrics),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
        };

        let body = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=Play\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=video 9 TCP/RTP/AVP 96\r\na=setup:active\r\ny=2000000001\r\n";
        let msg = SipMessage {
            start_line: "INVITE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
            method: Some(SipMethod::Invite),
            status_code: None,
            uri: Some("sip:34020000001320000001@3402000000".to_string()),
            version: "SIP/2.0".to_string(),
            headers: vec![
                ("Call-ID".to_string(), "tcp-active-1".to_string()),
                (
                    "From".to_string(),
                    "<sip:34020000002000000001@3402000000>;tag=plat".to_string(),
                ),
                (
                    "To".to_string(),
                    "<sip:34020000001320000001@3402000000>".to_string(),
                ),
                ("CSeq".to_string(), "1 INVITE".to_string()),
                (
                    "Via".to_string(),
                    "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKmt2".to_string(),
                ),
            ],
            body: body.to_string(),
        };

        let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
        server
            .handle_invite(&msg, peer.local_addr().expect("addr"))
            .await
            .expect("handle_invite");
        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("timed out waiting for 488")
            .expect("recv failed");
        let resp = SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8"))
            .expect("parse response");
        assert!(resp.start_line.contains("488"), "got {}", resp.start_line);
    }

    /// Stale responses from a previous register cycle (late 200 OK / old-nonce
    /// 401) must be skipped: perform_register matches responses by CSeq so a
    /// one-cycle-off response never poisons the current attempt (issue #11).
    #[tokio::test]
    async fn test_perform_register_skips_stale_responses() {
        use crate::client::SipDeviceClient;

        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config: Gb28181Config {
                enabled: true,
                platform_sip_address: "127.0.0.1".to_string(),
                platform_sip_port: 5060,
                device_id: "34020000001320000001".to_string(),
                channel_id: "34020000001320000001".to_string(),
                sip_domain: "3402000000".to_string(),
                password: "12345678".to_string(),
                local_sip_port: 5060,
                register_interval_secs: 60,
                heartbeat_interval_secs: 60,
                heartbeat_timeout_count: 3,
                transport: Transport::Udp,
                ..Gb28181Config::default()
            },
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
            metrics: Arc::new(crate::metrics::NoopMetrics),
        };

        // Fake platform: sends a STALE 200 OK (wrong CSeq) before the real 401,
        // then a STALE 401 (wrong CSeq) before the real 200 OK.
        let platform = UdpSocket::bind("127.0.0.1:0").await.expect("platform bind");
        let platform_addr = platform.local_addr().expect("addr");
        let server_addr = server
            .sip_socket
            .as_ref()
            .expect("socket bound")
            .local_addr()
            .expect("server addr");

        let stale_200 =
            "SIP/2.0 200 OK\r\nCSeq: 999 REGISTER\r\nCall-ID: stale\r\nContent-Length: 0\r\n\r\n";
        let stale_401 = "SIP/2.0 401 Unauthorized\r\nCSeq: 999 REGISTER\r\nCall-ID: stale\r\nContent-Length: 0\r\n\r\n";
        let fresh_401 = "SIP/2.0 401 Unauthorized\r\nCSeq: 1 REGISTER\r\nCall-ID: fresh\r\nWWW-Authenticate: Digest realm=\"3402000000\", nonce=\"abc\", algorithm=MD5\r\nContent-Length: 0\r\n\r\n";
        let fresh_200 =
            "SIP/2.0 200 OK\r\nCSeq: 2 REGISTER\r\nCall-ID: fresh\r\nContent-Length: 0\r\n\r\n";

        let sender = tokio::spawn(async move {
            let platform = platform;
            // Wait for the initial REGISTER, then stale-200, stale-401... no:
            // reply sequence interleaves stale before fresh.
            let mut buf = vec![0u8; 2048];
            let (_n, _) = platform.recv_from(&mut buf).await.expect("recv REGISTER 1");
            platform
                .send_to(stale_200.as_bytes(), server_addr)
                .await
                .unwrap();
            platform
                .send_to(fresh_401.as_bytes(), server_addr)
                .await
                .unwrap();
            let (_n, _) = platform.recv_from(&mut buf).await.expect("recv REGISTER 2");
            platform
                .send_to(stale_401.as_bytes(), server_addr)
                .await
                .unwrap();
            platform
                .send_to(fresh_200.as_bytes(), server_addr)
                .await
                .unwrap();
        });

        let mut client = SipDeviceClient::new(
            "34020000001320000001",
            platform_addr,
            "127.0.0.1",
            5060,
            "3402000000",
            "12345678",
            3600,
        );
        server
            .perform_register(&mut client, platform_addr)
            .await
            .expect("register must succeed despite stale interleaved responses");
        sender.await.unwrap();
    }

    /// GB/T 28181-2022 Annex I X-GB-Ver: configured REGISTERs carry the
    /// version header (initial AND authenticated), and the platform's
    /// version off the 200 OK is recorded (go twin: device/gbver_test.go).
    #[tokio::test]
    async fn register_lifecycle_xgbver() {
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config: Gb28181Config {
                enabled: true,
                platform_sip_address: "127.0.0.1".to_string(),
                platform_sip_port: 5060,
                device_id: "34020000001320000001".to_string(),
                channel_id: "34020000001320000001".to_string(),
                sip_domain: "3402000000".to_string(),
                password: "12345678".to_string(),
                local_sip_port: 5060,
                register_interval_secs: 3600,
                heartbeat_interval_secs: 3600,
                heartbeat_timeout_count: 3,
                transport: Transport::Udp,
                protocol_version: Some("3.0".to_string()),
                ..Gb28181Config::default()
            },
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
            metrics: Arc::new(crate::metrics::NoopMetrics),
        };

        let platform = UdpSocket::bind("127.0.0.1:0").await.expect("platform bind");
        let platform_addr = platform.local_addr().expect("addr");
        let server_addr = server
            .sip_socket
            .as_ref()
            .expect("socket bound")
            .local_addr()
            .expect("server addr");

        // 401 challenge without a version (2016-era), then 200 OK
        // announcing the platform's version.
        let fresh_401 = "SIP/2.0 401 Unauthorized\r\nCSeq: 1 REGISTER\r\nWWW-Authenticate: Digest realm=\"3402000000\", nonce=\"abc\", algorithm=MD5\r\nContent-Length: 0\r\n\r\n";
        let fresh_200 =
            "SIP/2.0 200 OK\r\nCSeq: 2 REGISTER\r\nX-GB-Ver: 2.0\r\nDate: Tue, 15 Sep 2026 07:29:00 GMT\r\nContent-Length: 0\r\n\r\n";

        let sender = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (n, _) = platform.recv_from(&mut buf).await.expect("recv REGISTER 1");
            let reg1 = String::from_utf8_lossy(&buf[..n]).to_string();
            platform
                .send_to(fresh_401.as_bytes(), server_addr)
                .await
                .unwrap();
            let (n, _) = platform.recv_from(&mut buf).await.expect("recv REGISTER 2");
            let reg2 = String::from_utf8_lossy(&buf[..n]).to_string();
            platform
                .send_to(fresh_200.as_bytes(), server_addr)
                .await
                .unwrap();
            (reg1, reg2)
        });

        let mut client = SipDeviceClient::new(
            "34020000001320000001",
            platform_addr,
            "127.0.0.1",
            5060,
            "3402000000",
            "12345678",
            3600,
        );
        server
            .perform_register(&mut client, platform_addr)
            .await
            .expect("register must succeed");
        let (reg1, reg2) = sender.await.unwrap();
        assert!(reg1.contains("X-GB-Ver: 3.0"), "initial REGISTER: {reg1}");
        assert!(reg2.contains("X-GB-Ver: 3.0"), "authed REGISTER: {reg2}");
        assert_eq!(
            *server.platform_proto_ver.lock().unwrap(),
            Some("2.0".to_string())
        );
        // §9.10.2: the same response's SIP Date is the platform clock.
        assert_eq!(*server.platform_date.lock().unwrap(), Some(1_789_457_340));
    }

    /// Without configuration the header is omitted (byte-identical to the
    /// pre-2022 wire form) and an absent platform version stays None.
    #[tokio::test]
    async fn register_lifecycle_omits_xgbver_when_unset() {
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config: Gb28181Config {
                enabled: true,
                platform_sip_address: "127.0.0.1".to_string(),
                platform_sip_port: 5060,
                device_id: "34020000001320000001".to_string(),
                channel_id: "34020000001320000001".to_string(),
                sip_domain: "3402000000".to_string(),
                password: "12345678".to_string(),
                local_sip_port: 5060,
                register_interval_secs: 3600,
                heartbeat_interval_secs: 3600,
                heartbeat_timeout_count: 3,
                transport: Transport::Udp,
                ..Gb28181Config::default()
            },
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
            metrics: Arc::new(crate::metrics::NoopMetrics),
        };

        let platform = UdpSocket::bind("127.0.0.1:0").await.expect("platform bind");
        let platform_addr = platform.local_addr().expect("addr");
        let server_addr = server
            .sip_socket
            .as_ref()
            .expect("socket bound")
            .local_addr()
            .expect("server addr");

        let fresh_401 = "SIP/2.0 401 Unauthorized\r\nCSeq: 1 REGISTER\r\nWWW-Authenticate: Digest realm=\"3402000000\", nonce=\"abc\", algorithm=MD5\r\nContent-Length: 0\r\n\r\n";
        let fresh_200 = "SIP/2.0 200 OK\r\nCSeq: 2 REGISTER\r\nContent-Length: 0\r\n\r\n";

        let sender = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (n, _) = platform.recv_from(&mut buf).await.expect("recv REGISTER 1");
            let reg1 = String::from_utf8_lossy(&buf[..n]).to_string();
            platform
                .send_to(fresh_401.as_bytes(), server_addr)
                .await
                .unwrap();
            let (n, _) = platform.recv_from(&mut buf).await.expect("recv REGISTER 2");
            let reg2 = String::from_utf8_lossy(&buf[..n]).to_string();
            platform
                .send_to(fresh_200.as_bytes(), server_addr)
                .await
                .unwrap();
            (reg1, reg2)
        });

        let mut client = SipDeviceClient::new(
            "34020000001320000001",
            platform_addr,
            "127.0.0.1",
            5060,
            "3402000000",
            "12345678",
            3600,
        );
        server
            .perform_register(&mut client, platform_addr)
            .await
            .expect("register must succeed");
        let (reg1, reg2) = sender.await.unwrap();
        assert!(!reg1.contains("X-GB-Ver"), "initial REGISTER: {reg1}");
        assert!(!reg2.contains("X-GB-Ver"), "authed REGISTER: {reg2}");
        assert_eq!(*server.platform_proto_ver.lock().unwrap(), None);
    }

    /// Deregistration helper (issue #62): REGISTER with Expires: 0 runs
    /// the same 401 Digest dance as registration. Both legs reuse the
    /// registration's Call-ID (RFC 3261 §10.2.2 — removing the binding
    /// established under the same dialog).
    #[tokio::test]
    async fn deregister_lifecycle_answers_401_then_200() {
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config: Gb28181Config {
                enabled: true,
                platform_sip_address: "127.0.0.1".to_string(),
                platform_sip_port: 5060,
                device_id: "34020000001320000001".to_string(),
                channel_id: "34020000001320000001".to_string(),
                sip_domain: "3402000000".to_string(),
                password: "12345678".to_string(),
                local_sip_port: 5060,
                register_interval_secs: 3600,
                heartbeat_interval_secs: 3600,
                heartbeat_timeout_count: 3,
                transport: Transport::Udp,
                ..Gb28181Config::default()
            },
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
            metrics: Arc::new(crate::metrics::NoopMetrics),
        };

        let platform = UdpSocket::bind("127.0.0.1:0").await.expect("platform bind");
        let platform_addr = platform.local_addr().expect("addr");
        let server_addr = server
            .sip_socket
            .as_ref()
            .expect("socket bound")
            .local_addr()
            .expect("server addr");

        let fresh_401 = "SIP/2.0 401 Unauthorized\r\nCSeq: 3 REGISTER\r\nWWW-Authenticate: Digest realm=\"3402000000\", nonce=\"dereg\", algorithm=MD5\r\nContent-Length: 0\r\n\r\n";
        let fresh_200 = "SIP/2.0 200 OK\r\nCSeq: 4 REGISTER\r\nContent-Length: 0\r\n\r\n";

        let sender = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (n, _) = platform.recv_from(&mut buf).await.expect("recv dereg 1");
            let reg1 = String::from_utf8_lossy(&buf[..n]).to_string();
            platform
                .send_to(fresh_401.as_bytes(), server_addr)
                .await
                .unwrap();
            let (n, _) = platform.recv_from(&mut buf).await.expect("recv dereg 2");
            let reg2 = String::from_utf8_lossy(&buf[..n]).to_string();
            platform
                .send_to(fresh_200.as_bytes(), server_addr)
                .await
                .unwrap();
            (reg1, reg2)
        });

        // A client that already registered (cseq consumed through 2).
        let mut client = SipDeviceClient::new(
            "34020000001320000001",
            platform_addr,
            "127.0.0.1",
            5060,
            "3402000000",
            "12345678",
            3600,
        );
        client.inc_cseq();
        client.inc_cseq();
        server.perform_deregister(&mut client, platform_addr).await;
        let (reg1, reg2) = sender.await.unwrap();
        assert!(reg1.contains("REGISTER"), "leg 1: {reg1}");
        assert!(
            reg1.contains("Expires: 0"),
            "leg 1 must carry Expires: 0: {reg1}"
        );
        assert!(
            !reg1.contains("Authorization"),
            "leg 1 is unauthenticated: {reg1}"
        );
        assert!(
            reg2.contains("Expires: 0"),
            "leg 2 must carry Expires: 0: {reg2}"
        );
        assert!(
            reg2.contains("Authorization"),
            "leg 2 answers the 401: {reg2}"
        );
        // Same dialog as the registration (the binding being removed).
        let call_id = reg1
            .lines()
            .find_map(|l| l.strip_prefix("Call-ID: "))
            .expect("Call-ID header");
        assert!(reg2.contains(&format!("Call-ID: {call_id}")));
    }

    /// A no-auth platform answers the initial REGISTER with 200
    /// directly — the registration succeeds without the challenge round
    /// (twin parity with gb28181-go's "no auth required" path; also
    /// covers a stale 200 for a superseded REGISTER after a restart).
    #[tokio::test]
    async fn register_lifecycle_accepts_direct_200() {
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config: Gb28181Config::default(),
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
            metrics: Arc::new(crate::metrics::NoopMetrics),
        };
        let platform = UdpSocket::bind("127.0.0.1:0").await.expect("platform bind");
        let platform_addr = platform.local_addr().expect("addr");
        let server_addr = server
            .sip_socket
            .as_ref()
            .expect("socket bound")
            .local_addr()
            .expect("server addr");
        let ok = "SIP/2.0 200 OK\r\nCSeq: 1 REGISTER\r\nContent-Length: 0\r\n\r\n".to_string();
        let sender = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (n, _) = platform.recv_from(&mut buf).await.expect("recv REGISTER");
            platform.send_to(ok.as_bytes(), server_addr).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        let mut client = SipDeviceClient::new(
            "34020000001320000001",
            platform_addr,
            "127.0.0.1",
            5060,
            "3402000000",
            "12345678",
            3600,
        );
        server
            .perform_register(&mut client, platform_addr)
            .await
            .expect("direct 200 must register");
        let reg = sender.await.unwrap();
        assert!(reg.contains("REGISTER"), "wire: {reg}");
        assert!(
            !reg.contains("Authorization"),
            "leg 1 is unauthenticated: {reg}"
        );
        // Exactly one leg — cseq advanced past the initial value.
        assert_eq!(client.cseq, 2);
    }

    /// A platform accepting the unauthenticated de-register outright
    /// (200 on leg 1) ends the dance in one round-trip.
    #[tokio::test]
    async fn deregister_accepts_unauth_200() {
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config: Gb28181Config::default(),
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
            metrics: Arc::new(crate::metrics::NoopMetrics),
        };
        let platform = UdpSocket::bind("127.0.0.1:0").await.expect("platform bind");
        let platform_addr = platform.local_addr().expect("addr");
        let server_addr = server
            .sip_socket
            .as_ref()
            .expect("socket bound")
            .local_addr()
            .expect("server addr");

        let ok = "SIP/2.0 200 OK\r\nCSeq: 1 REGISTER\r\nContent-Length: 0\r\n\r\n".to_string();
        let sender = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let (n, _) = platform.recv_from(&mut buf).await.expect("recv dereg");
            platform.send_to(ok.as_bytes(), server_addr).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });

        let mut client = SipDeviceClient::new(
            "34020000001320000001",
            platform_addr,
            "127.0.0.1",
            5060,
            "3402000000",
            "12345678",
            3600,
        );
        let started = std::time::Instant::now();
        server.perform_deregister(&mut client, platform_addr).await;
        let reg = sender.await.unwrap();
        assert!(reg.contains("Expires: 0"), "leg 1: {reg}");
        assert!(started.elapsed() < Duration::from_secs(1));
        // No second leg — the 200 path increments cseq exactly once
        // past the leg-1 value.
        assert_eq!(client.cseq, 2);
    }

    /// "Tolerate no-answer" (issue #62): a silent platform never blocks
    /// the shutdown path — the 2s response timeout expires and the
    /// helper simply returns.
    #[tokio::test]
    async fn deregister_tolerates_silent_platform() {
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config: Gb28181Config::default(),
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
            metrics: Arc::new(crate::metrics::NoopMetrics),
        };
        // Bind but never answer.
        let platform = UdpSocket::bind("127.0.0.1:0").await.expect("platform bind");
        let platform_addr = platform.local_addr().expect("addr");
        let recv_drain = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let _ = platform.recv_from(&mut buf).await;
        });

        let mut client = SipDeviceClient::new(
            "34020000001320000001",
            platform_addr,
            "127.0.0.1",
            5060,
            "3402000000",
            "12345678",
            3600,
        );
        let started = std::time::Instant::now();
        server.perform_deregister(&mut client, platform_addr).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(2) && elapsed < Duration::from_secs(4),
            "one 2s timeout then return, took {elapsed:?}"
        );
        recv_drain.abort();
    }

    /// The ServerHandle accessor reads the same shared slot the server
    /// task writes (Annex I plumbing).
    #[tokio::test]
    async fn server_handle_exposes_platform_protocol_version() {
        let slot = Arc::new(std::sync::Mutex::new(Some("3.0".to_string())));
        let (_shutdown_tx, shutdown_rx) = watch::channel(ShutdownMode::Init);
        let task = tokio::spawn(async move {
            let _ = shutdown_rx;
        });
        let handle = ServerHandle {
            task,
            shutdown: _shutdown_tx,
            platform_proto_ver: Arc::clone(&slot),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
        };
        assert_eq!(handle.platform_protocol_version(), Some("3.0".to_string()));
        *slot.lock().unwrap() = Some("2.0".to_string());
        assert_eq!(handle.platform_protocol_version(), Some("2.0".to_string()));
    }
    /// notifier() hands the host the same live slot the server task
    /// uses (issue #57): safe before spawn — send_* are no-ops until a
    /// platform subscribes.
    #[test]
    fn notifier_accessor_returns_live_slot() {
        let sip_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let server = Gb28181Server::with_recording_index(
            Gb28181Config::default(),
            Arc::new(crate::mock::MockFrameHub::new()),
            None,
        );
        let n1 = server.notifier();
        let n2 = server.notifier();
        assert!(Arc::ptr_eq(&n1, &n2));
        // No subscription, no socket: a no-op, not a panic.
        assert!(!n1.send_alarm("4", "5", "2026-09-15T15:30:00", "2", "test"));
        let _ = sip_socket;
    }

    // ─── audio talkback receive (GB/T 28181-2022 §9.2) ─────────────────────

    fn audio_invite_msg(call_id: &str, pt: u8) -> SipMessage {
        SipMessage {
            start_line: "INVITE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
            method: Some(SipMethod::Invite),
            status_code: None,
            uri: Some("sip:34020000001320000001@3402000000".to_string()),
            version: "SIP/2.0".to_string(),
            headers: vec![
                ("Call-ID".to_string(), call_id.to_string()),
                (
                    "From".to_string(),
                    "<sip:34020000002000000001@3402000000>;tag=plat".to_string(),
                ),
                (
                    "To".to_string(),
                    "<sip:34020000001320000001@3402000000>".to_string(),
                ),
                ("CSeq".to_string(), "1 INVITE".to_string()),
                (
                    "Via".to_string(),
                    "SIP/2.0/UDP 192.168.63.197:5060;branch=z9hG4bKaudio".to_string(),
                ),
            ],
            body: format!(
                "v=0\r\no=- 0 0 IN IP4 192.168.63.197\r\ns=Play\r\nc=IN IP4 192.168.63.197\r\nt=0 0\r\nm=audio 15062 RTP/AVP {pt}\r\na=sendonly\r\ny=777\r\n"
            ),
        }
    }

    /// audio_invite_msg parametrized on the direction attribute and media
    /// target (upstream tests point c=/m= at a local socket).
    fn audio_invite_msg_to(
        call_id: &str,
        pt: u8,
        direction: &str,
        ip: &str,
        port: u16,
    ) -> SipMessage {
        SipMessage {
            start_line: "INVITE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
            method: Some(SipMethod::Invite),
            status_code: None,
            uri: Some("sip:34020000001320000001@3402000000".to_string()),
            version: "SIP/2.0".to_string(),
            headers: vec![
                ("Call-ID".to_string(), call_id.to_string()),
                (
                    "From".to_string(),
                    "<sip:34020000002000000001@3402000000>;tag=plat".to_string(),
                ),
                (
                    "To".to_string(),
                    "<sip:34020000001320000001@3402000000>".to_string(),
                ),
                ("CSeq".to_string(), "1 INVITE".to_string()),
                (
                    "Via".to_string(),
                    format!("SIP/2.0/UDP {ip}:5060;branch=z9hG4bKaudio{call_id}"),
                ),
            ],
            body: format!(
                "v=0\r\no=- 0 0 IN IP4 {ip}\r\ns=Play\r\nc=IN IP4 {ip}\r\nt=0 0\r\nm=audio {port} RTP/AVP {pt}\r\na={direction}\r\ny=999\r\n"
            ),
        }
    }

    /// Upstream half (issue #61): with a source channel installed, an
    /// a=recvonly offer is answered a=sendonly and pushed G.711 frames
    /// leave as RTP toward the offer's c=/m= address — payload type 8,
    /// growing sequence numbers, timestamps advancing by payload length
    /// (8 kHz G.711 clock), stable SSRC, payload copied verbatim.
    #[tokio::test]
    async fn test_talkback_upstream_sends_rtp() {
        use std::sync::Mutex;
        type Collected = Arc<Mutex<Vec<(Vec<u8>, u32)>>>;
        let received: Collected = Arc::new(Mutex::new(Vec::new()));
        let sink_capture = Arc::clone(&received);
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let config = Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: 5060,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: 5060,
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            transport: Transport::Udp,
            ..Gb28181Config::default()
        };
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config,
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            metrics: Arc::new(crate::metrics::NoopMetrics),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: Some(Arc::new(move |payload: &[u8], ssrc: u32| {
                sink_capture.lock().unwrap().push((payload.to_vec(), ssrc));
            })),
            talkback_source: Some(Arc::new(Mutex::new(rx))),
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
        };
        let media = UdpSocket::bind("127.0.0.1:0").await.expect("media bind");
        let media_port = media.local_addr().expect("media addr").port();
        let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
        let peer_addr = peer.local_addr().expect("peer addr");
        let msg = audio_invite_msg_to("talk-up-1", 8, "recvonly", "127.0.0.1", media_port);
        server
            .handle_invite(&msg, peer_addr)
            .await
            .expect("handle_invite should not error");

        // 200 OK with the sendonly answer golden: the direction line sits
        // between the rtpmap and y=.
        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("timed out waiting for 200 OK")
            .expect("recv failed");
        let resp = SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8"))
            .expect("parse response");
        assert_eq!(resp.status_code.map(|c| c.code()), Some(200));
        assert!(
            resp.body
                .contains("a=rtpmap:8 PCMA/8000\r\na=sendonly\r\ny=999\r\n"),
            "sendonly answer body: {}",
            resp.body
        );

        // Push two frames; the sender drains one per 20 ms tick.
        tx.send(vec![0xD5, 0x5A, 0xA5, 0x37, 0x11, 0x22, 0x33, 0x44])
            .expect("push frame 1");
        tx.send(vec![0x0F]).expect("push frame 2");

        let mut mbuf = vec![0u8; 2048];
        let (n1, _) = tokio::time::timeout(Duration::from_secs(2), media.recv_from(&mut mbuf))
            .await
            .expect("timed out waiting for upstream RTP 1")
            .expect("media recv failed");
        let f1 = mbuf[..n1].to_vec();
        assert_eq!(n1, 12 + 8, "packet length");
        assert_eq!(f1[0], 0x80, "V/P/X/CC");
        assert_eq!(f1[1], 8, "payload type 8 (PCMA)");
        assert_eq!(&f1[12..], &[0xD5, 0x5A, 0xA5, 0x37, 0x11, 0x22, 0x33, 0x44]);
        let seq1 = u16::from_be_bytes([f1[2], f1[3]]);
        let ts1 = u32::from_be_bytes([f1[4], f1[5], f1[6], f1[7]]);
        let ssrc = u32::from_be_bytes([f1[8], f1[9], f1[10], f1[11]]);

        let (n2, _) = tokio::time::timeout(Duration::from_secs(2), media.recv_from(&mut mbuf))
            .await
            .expect("timed out waiting for upstream RTP 2")
            .expect("media recv failed");
        let f2 = mbuf[..n2].to_vec();
        let seq2 = u16::from_be_bytes([f2[2], f2[3]]);
        let ts2 = u32::from_be_bytes([f2[4], f2[5], f2[6], f2[7]]);
        assert_eq!(seq2, seq1.wrapping_add(1), "sequence increments");
        assert_eq!(
            ts2.wrapping_sub(ts1),
            8,
            "timestamp advances by payload length (one sample per byte @8 kHz)"
        );
        assert_eq!(
            u32::from_be_bytes([f2[8], f2[9], f2[10], f2[11]]),
            ssrc,
            "SSRC stable across packets"
        );
        assert_eq!(&f2[12..], &[0x0F]);
    }

    /// An offer that requires upstream audio (a=recvonly) without a
    /// source wired is refused with 488 — the mirror of the no-sink
    /// refusal (issue #61).
    #[tokio::test]
    async fn test_talkback_recvonly_without_source_returns_488() {
        let config = Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: 5060,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: 5060,
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            transport: Transport::Udp,
            ..Gb28181Config::default()
        };
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config,
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            metrics: Arc::new(crate::metrics::NoopMetrics),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: Some(Arc::new(|_payload: &[u8], _ssrc: u32| {})),
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
        };
        let media = UdpSocket::bind("127.0.0.1:0").await.expect("media bind");
        let media_port = media.local_addr().expect("media addr").port();
        let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
        let peer_addr = peer.local_addr().expect("peer addr");
        let msg = audio_invite_msg_to("talk-up-nosrc", 8, "recvonly", "127.0.0.1", media_port);
        server
            .handle_invite(&msg, peer_addr)
            .await
            .expect("handle_invite should not error");
        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("timed out waiting for 488")
            .expect("recv failed");
        let resp = SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8"))
            .expect("parse response");
        assert_eq!(resp.status_code.map(|c| c.code()), Some(488));
    }

    /// A source installed against an a=sendonly offer (platform speaks,
    /// device listens) must NOT start the upstream sender: no RTP leaves.
    #[tokio::test]
    async fn test_talkback_sendonly_offer_keeps_upstream_off() {
        use std::sync::Mutex;
        type Collected = Arc<Mutex<Vec<(Vec<u8>, u32)>>>;
        let received: Collected = Arc::new(Mutex::new(Vec::new()));
        let sink_capture = Arc::clone(&received);
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let config = Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: 5060,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: 5060,
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            transport: Transport::Udp,
            ..Gb28181Config::default()
        };
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config,
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            metrics: Arc::new(crate::metrics::NoopMetrics),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: Some(Arc::new(move |payload: &[u8], ssrc: u32| {
                sink_capture.lock().unwrap().push((payload.to_vec(), ssrc));
            })),
            talkback_source: Some(Arc::new(Mutex::new(rx))),
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
        };
        let media = UdpSocket::bind("127.0.0.1:0").await.expect("media bind");
        let media_port = media.local_addr().expect("media addr").port();
        let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
        let peer_addr = peer.local_addr().expect("peer addr");
        let msg = audio_invite_msg_to("talk-up-mute", 8, "sendonly", "127.0.0.1", media_port);
        server
            .handle_invite(&msg, peer_addr)
            .await
            .expect("handle_invite should not error");
        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("timed out waiting for 200 OK")
            .expect("recv failed");
        let resp = SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8"))
            .expect("parse response");
        assert_eq!(resp.status_code.map(|c| c.code()), Some(200));
        assert!(
            !resp.body.contains("a=sendonly"),
            "sendonly offer keeps the directionless answer: {}",
            resp.body
        );

        tx.send(vec![0xD5, 0x5A]).expect("push frame");
        let mut mbuf = vec![0u8; 2048];
        match tokio::time::timeout(Duration::from_millis(300), media.recv_from(&mut mbuf)).await {
            Ok(Ok((n, _))) => {
                panic!("upstream RTP must not flow for a=sendonly offers, got {n} bytes")
            }
            Ok(Err(e)) => panic!("media recv error: {e}"),
            Err(_) => {}
        }
        drop(tx);
    }

    /// A talkback INVITE with no sink registered is refused with 488 —
    /// receiving audio nobody consumes would be a silent black hole.
    #[tokio::test]
    async fn test_audio_invite_without_sink_returns_488() {
        let config = Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: 5060,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: 5060,
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            transport: Transport::Udp,
            ..Gb28181Config::default()
        };
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config,
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            metrics: Arc::new(crate::metrics::NoopMetrics),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "192.168.62.104".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
        };
        let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
        let peer_addr = peer.local_addr().expect("peer addr");
        let msg = audio_invite_msg("audio-nosink-1", 8);
        server
            .handle_invite(&msg, peer_addr)
            .await
            .expect("handle_invite should not error");
        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("timed out waiting for 488")
            .expect("recv failed");
        let resp = SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8"))
            .expect("parse response");
        assert_eq!(resp.status_code.map(|c| c.code()), Some(488));
    }

    /// Full talkback loop: audio INVITE → 200 OK with m=audio answer →
    /// platform streams RTP to the answered port → the sink receives the
    /// G.711 payload with the session SSRC.
    #[tokio::test]
    async fn test_audio_invite_answers_and_delivers_rtp() {
        use std::sync::Mutex;
        type Collected = Arc<Mutex<Vec<(Vec<u8>, u32)>>>;
        let received: Collected = Arc::new(Mutex::new(Vec::new()));
        let sink_capture = Arc::clone(&received);
        let config = Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: 5060,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: 5060,
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            transport: Transport::Udp,
            ..Gb28181Config::default()
        };
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let mut server = Gb28181Server {
            config,
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            metrics: Arc::new(crate::metrics::NoopMetrics),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: Some(Arc::new(move |payload: &[u8], ssrc: u32| {
                sink_capture.lock().unwrap().push((payload.to_vec(), ssrc));
            })),
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
        };
        let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
        let peer_addr = peer.local_addr().expect("peer addr");
        let msg = audio_invite_msg("audio-e2e-1", 8);
        server
            .handle_invite(&msg, peer_addr)
            .await
            .expect("handle_invite should not error");

        // 1) 200 OK with an m=audio answer carrying a receive port.
        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("timed out waiting for 200 OK")
            .expect("recv failed");
        let resp = SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8"))
            .expect("parse response");
        assert_eq!(resp.status_code.map(|c| c.code()), Some(200));
        assert!(resp.body.contains("m=audio "));
        assert!(resp.body.contains("a=rtpmap:8 PCMA/8000"));
        assert!(resp.body.contains("y=777"));
        let audio_port: u16 = resp
            .body
            .lines()
            .find(|l| l.starts_with("m=audio "))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|p| p.parse().ok())
            .expect("audio port in answer");

        // 2) Stream one RTP packet (PCMA, SSRC 777) at the answered port.
        let mut pkt = vec![
            0x80u8, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x09,
        ];
        pkt.extend_from_slice(&[0xD5u8, 0x5A, 0xA5]);
        peer.send_to(&pkt, ("127.0.0.1", audio_port))
            .await
            .expect("send rtp");

        // 3) The sink receives payload + SSRC.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            {
                let got = received.lock().unwrap();
                assert!(got.len() <= 1, "expected exactly one delivery");
                if got.len() == 1 {
                    assert_eq!(got[0].0, vec![0xD5, 0x5A, 0xA5]);
                    assert_eq!(got[0].1, 777);
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sink never received the RTP payload"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Byte-exact production NVR talkback INVITE (GoSIP / MiBeeNvr M5,
    /// tcpdump 2026-09-11, MiBeeNvr#353): no `a=rtpmap`, leading-zero
    /// decimal `y=` SSRC, `o=` session-id/version `0 0`, Via host
    /// `0.0.0.0`, quoted display names, no From tag, and no trailing
    /// CRLF after `y=` (Content-Length 144).
    fn real_nvr_talkback_invite() -> SipMessage {
        let raw = "INVITE sip:34020000001310000003@192.168.63.174:5060 SIP/2.0\r\n\
                   Via: SIP/2.0/UDP 0.0.0.0:5060;branch=z9hG4bK.Ot9zCQf07bLFwIQhoFUtpQpa1Sia4bx5;rport=\r\n\
                   CSeq: 1 INVITE\r\n\
                   From: \"34020000002000000001\" <sip:34020000002000000001@192.168.63.30>\r\n\
                   To: \"34020000001310000003\" <sip:34020000001310000003@192.168.63.174:5060>\r\n\
                   Call-ID: 4BS50Dp5zTahRVgCDQsCoDMkZzHy4hWB\r\n\
                   Contact: <sip:34020000002000000001@192.168.63.30:5060>\r\n\
                   Max-Forwards: 70\r\n\
                   Content-Type: application/sdp\r\n\
                   User-Agent: GoSIP\r\n\
                   Subject: 34020000001310000003:0200006001,34020000002000000001:0\r\n\
                   Content-Length: 144\r\n\
                   Allow: INVITE, ACK, CANCEL, REGISTER, MESSAGE, BYE, INFO, NOTIFY, OPTIONS\r\n\
                   \r\n\
                   v=0\r\n\
                   o=34020000002000000001 0 0 IN IP4 192.168.63.30\r\n\
                   s=Play\r\n\
                   c=IN IP4 192.168.63.30\r\n\
                   t=0 0\r\n\
                   m=audio 57411 RTP/AVP 8\r\n\
                   a=sendrecv\r\n\
                   y=0200006001";
        SipMessage::parse(raw).expect("parse real NVR INVITE")
    }

    async fn audio_test_server(
        device_id: &str,
        local_ip: &str,
        audio_sink: Option<Arc<dyn AudioTalkbackSink>>,
    ) -> Gb28181Server {
        let config = Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: 5060,
            device_id: device_id.to_string(),
            channel_id: device_id.to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: 5060,
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            transport: Transport::Udp,
            ..Gb28181Config::default()
        };
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        Gb28181Server {
            config,
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            metrics: Arc::new(crate::metrics::NoopMetrics),
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: local_ip.to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
        }
    }

    /// Golden full loop with the byte-exact production NVR offer: the
    /// real message must answer 200 OK (not 488 — the deployed 488s were
    /// the no-sink path, MiBeeNvr#353 ③(b) ruled out by this test) and
    /// deliver RTP payload with the leading-zero SSRC parsed as 200006001.
    #[tokio::test]
    async fn test_real_nvr_talkback_invite_full_loop() {
        use std::sync::Mutex;
        type Collected = Arc<Mutex<Vec<(Vec<u8>, u32)>>>;
        let received: Collected = Arc::new(Mutex::new(Vec::new()));
        let sink_capture = Arc::clone(&received);
        let mut server = audio_test_server(
            "34020000001310000003",
            "127.0.0.1",
            Some(Arc::new(move |payload: &[u8], ssrc: u32| {
                sink_capture.lock().unwrap().push((payload.to_vec(), ssrc));
            })),
        )
        .await;
        let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
        let peer_addr = peer.local_addr().expect("peer addr");
        server
            .handle_invite(&real_nvr_talkback_invite(), peer_addr)
            .await
            .expect("handle_invite should not error");

        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("timed out waiting for 200 OK")
            .expect("recv failed");
        let resp = SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8"))
            .expect("parse response");
        assert_eq!(resp.status_code.map(|c| c.code()), Some(200));
        assert!(resp.body.contains("m=audio "));
        assert!(resp.body.contains("a=rtpmap:8 PCMA/8000"));
        // Answer echoes the session SSRC numerically (leading zero dropped).
        assert!(resp.body.contains("y=200006001"));
        // To tag appended to the quoted display-name form.
        assert!(resp
            .get_header("To")
            .expect("To header")
            .starts_with("\"34020000001310000003\" <sip:"));
        assert!(resp.get_header("To").unwrap().contains(";tag="));
        let audio_port: u16 = resp
            .body
            .lines()
            .find(|l| l.starts_with("m=audio "))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|p| p.parse().ok())
            .expect("audio port in answer");

        // RTP with SSRC 200006001 = 0x0BEBD971.
        let mut pkt = vec![
            0x80u8, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x0B, 0xEB, 0xD9, 0x71,
        ];
        pkt.extend_from_slice(&[0xD5u8, 0x5A, 0xA5]);
        peer.send_to(&pkt, ("127.0.0.1", audio_port))
            .await
            .expect("send rtp");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            {
                let got = received.lock().unwrap();
                assert!(got.len() <= 1, "expected exactly one delivery");
                if got.len() == 1 {
                    assert_eq!(got[0].0, vec![0xD5, 0x5A, 0xA5]);
                    assert_eq!(got[0].1, 200006001);
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sink never received the RTP payload"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The codec negotiated from the offer's payload type travels with
    /// every delivered packet (`on_audio_codec`), so hosts can decode
    /// A-law vs μ-law correctly.
    #[tokio::test]
    async fn test_audio_sink_receives_negotiated_codec() {
        use std::sync::Mutex;
        struct CodecSink {
            seen: Mutex<Vec<AudioCodec>>,
        }
        impl AudioTalkbackSink for CodecSink {
            fn on_audio(&self, _payload: &[u8], _ssrc: u32) {
                panic!("codec-aware sink must not be called via on_audio");
            }
            fn on_audio_codec(&self, _payload: &[u8], _ssrc: u32, codec: AudioCodec) {
                self.seen.lock().unwrap().push(codec);
            }
        }
        let sink = Arc::new(CodecSink {
            seen: Mutex::new(Vec::new()),
        });
        let mut server =
            audio_test_server("34020000001310000003", "127.0.0.1", Some(sink.clone())).await;
        let peer = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
        let peer_addr = peer.local_addr().expect("peer addr");
        server
            .handle_invite(&real_nvr_talkback_invite(), peer_addr)
            .await
            .expect("handle_invite should not error");

        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("timed out waiting for 200 OK")
            .expect("recv failed");
        let resp = SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8"))
            .expect("parse response");
        assert_eq!(resp.status_code.map(|c| c.code()), Some(200));
        let audio_port: u16 = resp
            .body
            .lines()
            .find(|l| l.starts_with("m=audio "))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|p| p.parse().ok())
            .expect("audio port in answer");
        let pkt = [
            0x80u8, 0x08, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x0B, 0xEB, 0xD9, 0x71, 0xD5,
        ];
        peer.send_to(&pkt, ("127.0.0.1", audio_port))
            .await
            .expect("send rtp");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if sink.seen.lock().unwrap().len() == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sink never received the codec"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(sink.seen.lock().unwrap().as_slice(), [AudioCodec::Pcma]);
    }

    // ─── voice broadcast (§9.12.1) ─────────────────────────────────────────

    fn broadcast_notify_msg() -> SipMessage {
        SipMessage {
            start_line: "MESSAGE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
            method: Some(SipMethod::Message),
            status_code: None,
            uri: Some("sip:34020000001320000001@3402000000".to_string()),
            version: "SIP/2.0".to_string(),
            headers: vec![
                ("Call-ID".to_string(), "bnotify-1".to_string()),
                ("From".to_string(), "<sip:34020000002000000001@3402000000>;tag=plat".to_string()),
                ("To".to_string(), "<sip:34020000001320000001@3402000000>".to_string()),
                ("CSeq".to_string(), "1 MESSAGE".to_string()),
                ("Via".to_string(), "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKbcast1".to_string()),
                ("Content-Type".to_string(), "Application/MANSCDP+xml".to_string()),
            ],
            body: "<?xml version=\"1.0\"?>\r\n<Notify>\r\n<CmdType>Broadcast</CmdType>\r\n<SN>42</SN>\r\n<SourceID>34020000002000000001</SourceID>\r\n<TargetID>34020000001320000001</TargetID>\r\n</Notify>\r\n".to_string(),
        }
    }

    async fn broadcast_test_server(
        sink: Option<Arc<dyn AudioTalkbackSink>>,
    ) -> (Gb28181Server, Arc<UdpSocket>) {
        let config = Gb28181Config {
            enabled: true,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: 5060,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: "12345678".to_string(),
            local_sip_port: 5060,
            register_interval_secs: 60,
            heartbeat_interval_secs: 60,
            heartbeat_timeout_count: 3,
            transport: Transport::Udp,
            ..Gb28181Config::default()
        };
        let sip_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"));
        let server = Gb28181Server {
            config,
            au_hub: Arc::new(crate::mock::MockFrameHub::new()),
            metrics: Arc::new(crate::metrics::NoopMetrics),
            sip_socket: Some(Arc::clone(&sip_socket)),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            broadcast_pending: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: sink,
            talkback_source: None,
            authenticator: None,
            platform_proto_ver: Arc::new(std::sync::Mutex::new(None)),
            platform_date: Arc::new(std::sync::Mutex::new(None)),
            snapshot_executor: None,
            control_handler: None,
            config_handler: None,
            notifier: Arc::new(crate::subscribe::DeviceNotifier::new()),
            position_source: None,
            position_cancel: None,
            notifier_std_sock: None,
        };
        (server, sip_socket)
    }

    async fn recv_sip(peer: &UdpSocket) -> SipMessage {
        let mut buf = vec![0u8; 65535];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), peer.recv_from(&mut buf))
            .await
            .expect("timed out waiting for SIP")
            .expect("recv failed");
        SipMessage::parse(std::str::from_utf8(&buf[..len]).expect("utf8")).expect("parse")
    }

    /// §9.12.1 device half with a sink: 信令3 (A.2.6.11, Result OK) then
    /// the audio INVITE (s=Play / m=audio / Subject / Request-URI at the
    /// platform address); the platform's 200 completes the handshake
    /// (in-dialog ACK) and RTP reaches the sink with PCMA semantics.
    #[tokio::test]
    async fn test_broadcast_full_device_half() {
        use std::sync::Mutex;
        type Collected = Arc<Mutex<Vec<(Vec<u8>, u32)>>>;
        let received: Collected = Arc::new(Mutex::new(Vec::new()));
        let sink_capture = Arc::clone(&received);
        let sink: Arc<dyn AudioTalkbackSink> = Arc::new(move |payload: &[u8], ssrc: u32| {
            sink_capture.lock().unwrap().push((payload.to_vec(), ssrc));
        });
        let (mut server, _sip) = broadcast_test_server(Some(sink)).await;
        let platform = UdpSocket::bind("127.0.0.1:0").await.expect("platform bind");
        let platform_addr = platform.local_addr().expect("platform addr");

        let notify = crate::manscdp::parse_broadcast_notify(&broadcast_notify_msg().body)
            .expect("notify parses");
        server
            .start_broadcast(notify, platform_addr)
            .await
            .expect("start_broadcast");

        // 信令3: A.2.6.11 acknowledgement, Result OK.
        let resp3 = recv_sip(&platform).await;
        assert_eq!(resp3.method, Some(SipMethod::Message));
        assert!(resp3.body.contains("<CmdType>Broadcast</CmdType>"));
        assert!(resp3.body.contains("<SN>42</SN>"));
        assert!(resp3.body.contains("<Result>OK</Result>"));

        // 信令5: audio-only INVITE toward the announced source.
        let invite = recv_sip(&platform).await;
        assert_eq!(invite.method, Some(SipMethod::Invite));
        assert!(
            invite
                .uri
                .as_deref()
                .unwrap_or("")
                .starts_with("sip:34020000002000000001@127.0.0.1:"),
            "Request-URI targets the platform's actual address, got {:?}",
            invite.uri
        );
        let subject = invite.get_header("Subject").unwrap_or("");
        assert!(
            subject.starts_with("34020000002000000001:")
                && subject.ends_with(",34020000001320000001:0"),
            "Subject convention, got {subject}"
        );
        assert!(invite.body.contains("s=Play\r\n"));
        assert!(invite.body.contains("m=audio "));
        assert!(invite.body.contains("a=rtpmap:8 PCMA/8000\r\n"));
        let media_port: u16 = invite
            .body
            .lines()
            .find(|l| l.starts_with("m=audio "))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|p| p.parse().ok())
            .expect("media port in SDP");
        let invite_call_id = invite.get_header("Call-ID").unwrap_or("").to_string();

        // Platform 200 OK (信令13/14): the response is routed by Call-ID;
        // test the completion directly with the pending state.
        let ok200 = SipMessage {
            start_line: "SIP/2.0 200 OK".to_string(),
            method: None,
            status_code: Some(SipStatusCode::Ok),
            uri: None,
            version: "SIP/2.0".to_string(),
            headers: vec![
                ("Call-ID".to_string(), invite_call_id.clone()),
                ("From".to_string(), invite.get_header("From").unwrap_or("").to_string()),
                ("To".to_string(), "<sip:34020000002000000001@3402000000>;tag=mediasrv".to_string()),
                ("CSeq".to_string(), "1 INVITE".to_string()),
                ("Via".to_string(), "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKbcast1".to_string()),
            ],
            body: "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=Play\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\ny=12345\r\n"
                .to_string(),
        };
        let pending = server.broadcast_pending.take().expect("pending exists");
        assert_eq!(pending.call_id, invite_call_id);
        server
            .complete_broadcast(pending, ok200.clone())
            .await
            .expect("complete_broadcast");

        // 信令15: in-dialog ACK — routing headers from the INVITE, To tag
        // verbatim from the response.
        let ack = recv_sip(&platform).await;
        assert_eq!(ack.method, Some(SipMethod::Ack));
        assert_eq!(ack.get_header("Call-ID").unwrap_or(""), invite_call_id);
        assert!(ack.get_header("To").unwrap_or("").contains("tag=mediasrv"));
        assert_eq!(ack.get_header("CSeq").unwrap_or(""), "1 ACK");

        // RTP platform→device on the announced port reaches the sink
        // (header stripped, SSRC preserved, PCMA implied by the invite).
        let pkt = {
            let mut p = vec![0x80u8, 8];
            p.extend_from_slice(&1u16.to_be_bytes());
            p.extend_from_slice(&160u32.to_be_bytes());
            p.extend_from_slice(&0x0A0B0C0Du32.to_be_bytes());
            p.extend_from_slice(&[0xD5, 0x5A, 0xA5]);
            p
        };
        platform
            .send_to(&pkt, format!("127.0.0.1:{media_port}"))
            .await
            .expect("send RTP");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            received.lock().unwrap().as_slice(),
            [(vec![0xD5, 0x5A, 0xA5], 0x0A0B0C0D)]
        );

        // BYE (信令17) tears the session down via the shared dialog path.
        let bye = SipMessage {
            start_line: "BYE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
            method: Some(SipMethod::Bye),
            status_code: None,
            uri: Some("sip:34020000001320000001@3402000000".to_string()),
            version: "SIP/2.0".to_string(),
            headers: vec![
                ("Call-ID".to_string(), invite_call_id),
                (
                    "From".to_string(),
                    "<sip:34020000002000000001@3402000000>;tag=mediasrv".to_string(),
                ),
                (
                    "To".to_string(),
                    "<sip:34020000001320000001@3402000000>".to_string(),
                ),
                ("CSeq".to_string(), "2 BYE".to_string()),
                (
                    "Via".to_string(),
                    "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKbye1".to_string(),
                ),
            ],
            body: String::new(),
        };
        server
            .handle_bye(&bye, platform_addr)
            .await
            .expect("handle_bye");
        let bye_ok = recv_sip(&platform).await;
        assert_eq!(bye_ok.status_code.map(|c| c.code()), Some(200));
    }

    /// Without an audio sink the device acknowledges with Result=ERROR
    /// and never sends the audio INVITE (§9.12.1 decline path).
    #[tokio::test]
    async fn test_broadcast_declined_without_sink() {
        let (mut server, _sip) = broadcast_test_server(None).await;
        let platform = UdpSocket::bind("127.0.0.1:0").await.expect("platform bind");
        let platform_addr = platform.local_addr().expect("platform addr");
        let notify = crate::manscdp::parse_broadcast_notify(&broadcast_notify_msg().body)
            .expect("notify parses");
        server
            .start_broadcast(notify, platform_addr)
            .await
            .expect("start_broadcast");
        let resp3 = recv_sip(&platform).await;
        assert!(resp3.body.contains("<Result>ERROR</Result>"));
        assert!(server.broadcast_pending.is_none());
    }
}
