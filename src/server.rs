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

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch, Mutex};

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
use super::sip::{build_invite_response, SessionType, SipMessage, SipMethod, SipStatusCode};
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
#[derive(Debug)]
#[must_use = "dropping the handle leaves the server unsupervised; await it or call shutdown()"]
pub struct ServerHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown: watch::Sender<bool>,
}

impl ServerHandle {
    /// Request a graceful shutdown and wait for the server task to finish.
    ///
    /// Stops the SIP recv/accept loop and the keepalive task, aborts any
    /// active media/playback task, and unsubscribes from the frame source.
    /// Sending a REGISTER with `Expires: 0` (SIP de-registration) is the
    /// host's responsibility and NOT performed here.
    pub async fn shutdown(&mut self) -> Result<()> {
        // Ignore a send error: every receiver may already be dropped.
        let _ = self.shutdown.send(true);
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
}

impl<F: Fn(&[u8], u32) + Send + Sync> AudioTalkbackSink for F {
    fn on_audio(&self, payload: &[u8], ssrc: u32) {
        self(payload, ssrc)
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
    /// Detected local IP advertised in Contact headers.
    local_ip: String,
    /// Optional source of recorded-segment metadata for RecordInfo queries.
    recording_index: Option<Arc<dyn RecordingSource>>,
    /// Control channel for an active playback session (SIP INFO PlaybackControl).
    playback_ctl: Option<mpsc::Sender<PlaybackControl>>,
    /// Audio talkback sink (audio-only INVITE receive). `None` = talkback
    /// INVITEs are refused with 488.
    audio_sink: Option<Arc<dyn AudioTalkbackSink>>,
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
    shutdown: &mut watch::Receiver<bool>,
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
            local_ip: String::new(),
            recording_index,
            playback_ctl: None,
            audio_sink: None,
        }
    }

    /// Attach the audio talkback sink (receive half of GB/T 28181-2022
    /// §9.2 voice talkback). Without it, audio-only INVITEs are refused
    /// with 488.
    #[must_use]
    pub fn with_audio_sink(mut self, sink: Arc<dyn AudioTalkbackSink>) -> Self {
        self.audio_sink = Some(sink);
        self
    }

    /// Bind the SIP socket and run this server (instance flavor of
    /// [`Gb28181Server::start`]).
    ///
    /// Branches on `config.transport` exactly like `start`. Returns a
    /// [`ServerHandle`] for graceful shutdown.
    pub async fn spawn(self) -> Result<ServerHandle> {
        match self.config.transport {
            Transport::Udp => self.spawn_udp().await,
            Transport::Tcp => self.spawn_tcp().await,
        }
    }

    async fn spawn_udp(mut self) -> Result<ServerHandle> {
        let sip_addr = format!("0.0.0.0:{}", self.config.local_sip_port);
        let sip_socket = UdpSocket::bind(&sip_addr)
            .await
            .context(format!("gb28181: failed to bind SIP socket on {sip_addr}"))?;
        self.sip_socket = Some(Arc::new(sip_socket));
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
        self.config.warn_on_example_defaults();

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            if let Err(e) = self.run_udp(&mut shutdown_rx).await {
                log::error!("gb28181: server error: {e}");
            }
        });

        Ok(ServerHandle {
            task: handle,
            shutdown: shutdown_tx,
        })
    }

    /// Common post-bind path for TCP.
    async fn run_tcp_bound(self, listener: TcpListener) -> Result<ServerHandle> {
        let local_sip_port = self.config.local_sip_port;
        log::info!("gb28181: listening on SIP port {local_sip_port} (TCP)");
        self.config.warn_on_example_defaults();

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let au_hub = self.au_hub;
        let config = self.config;
        let recording_index = self.recording_index;
        let audio_sink = self.audio_sink;

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
        })
    }

    /// Main UDP server loop.
    async fn run_udp(&mut self, shutdown: &mut watch::Receiver<bool>) -> Result<()> {
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
            tokio::select! {
                result = self.perform_register(&mut sip_client, platform_sip_addr) => {
                    match result {
                        Ok(()) => {
                            registered = true;
                            log::info!(
                                "gb28181: registered with platform {} (attempt {}/{})",
                                platform_sip_addr,
                                attempt,
                                MAX_REG_ATTEMPTS
                            );
                            break;
                        }
                        Err(e) => {
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
            tokio::spawn(async move {
                if let Err(e) = run_keepalive(
                    sip_socket_for_keepalive,
                    platform_sip_addr,
                    &keepalive_device_id,
                    &keepalive_domain,
                    &keepalive_local_ip,
                    keepalive_local_port,
                    keepalive_interval_secs,
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
                                tokio::spawn(async move {
                                    if let Err(e) = run_keepalive(
                                        sip_socket_for_keepalive,
                                        platform_sip_addr,
                                        &keepalive_device_id,
                                        &keepalive_domain,
                                        &keepalive_local_ip,
                                        keepalive_local_port,
                                        keepalive_interval_secs,
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
        if let Some(task) = self.media_task.take() {
            task.abort();
        }
        self.media_socket = None;
        self.media_tcp_conn = None;
        self.invite_info = None;
        self.playback_ctl = None;
        Ok(())
    }

    /// Perform REGISTER lifecycle.
    async fn perform_register(
        &mut self,
        client: &mut SipDeviceClient,
        platform_addr: SocketAddr,
    ) -> Result<()> {
        // Step 1: Send initial REGISTER
        let register = client.build_register();
        let initial_cseq = client.cseq;
        self.send_sip_message(&register, platform_addr).await?;

        // Step 2: Wait for the 401 challenge OF THIS REQUEST. A late
        // response from a previous cycle (stale 200 OK / old-nonce 401)
        // must be skipped, not consumed — otherwise cycles go one-off
        // (issue #11).
        let msg = self
            .receive_register_response(initial_cseq, Duration::from_secs(5))
            .await?;
        if msg.status_code != Some(SipStatusCode::Unauthorized) {
            bail!("Expected 401 Unauthorized, got {:?}", msg.status_code);
        }

        // Step 3: Parse challenge and send authenticated REGISTER
        let auth = parse_401_challenge(&msg)?;
        client.inc_cseq();
        let authed_cseq = client.cseq;
        let authed_register = client.build_register_with_auth(&auth);
        self.send_sip_message(&authed_register, platform_addr)
            .await?;

        // Step 4: Wait for the 200 OK OF THIS REQUEST (skip stale).
        let msg = self
            .receive_register_response(authed_cseq, Duration::from_secs(5))
            .await?;
        if msg.status_code != Some(SipStatusCode::Ok) {
            bail!("Expected 200 OK, got {:?}", msg.status_code);
        }

        client.inc_cseq();
        Ok(())
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
            Some(SipMethod::Subscribe) | Some(SipMethod::Notify) | Some(SipMethod::Options) => {
                log::warn!(
                    "gb28181: received {}, responding 200 OK",
                    msg.method.map(|m| m.to_string()).unwrap_or_default()
                );
                let ok_response = build_error_response(msg, 200, "OK");
                self.send_sip_message(&ok_response, peer_addr).await?;
            }
            _ => {
                // Check if this is a response to our keepalive
                if msg.status_code == Some(SipStatusCode::Ok) {
                    *keepalive_failures = 0; // Reset failure counter on OK
                } else if msg.status_code.is_some() && msg.status_code != Some(SipStatusCode::Ok) {
                    *keepalive_failures += 1;
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
                tokio::spawn(async move {
                    if let Err(e) = run_media_task(
                        async_rx,
                        media_socket_clone,
                        media_task_conn,
                        ssrc,
                        &device_id,
                        media_dest,
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

        let sdp = build_audio_sdp_answer(media_port, invite_info.ssrc, &device_ip, codec);
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
            _media_addr: invite_info.media_address,
            _media_port: invite_info.media_port,
        });

        // RTP receive loop: strip the fixed 12-byte header (+ CSRC list)
        // and hand the G.711 payload to the sink. Lives on the shared
        // media_task slot, so BYE / dialog recycle aborts it.
        let session_ssrc = invite_info.ssrc;
        let recv_socket = Arc::clone(&media_socket);
        let media_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                match recv_socket.recv_from(&mut buf).await {
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
                        sink.on_audio(
                            &buf[header_len..len],
                            if ssrc != 0 { ssrc } else { session_ssrc },
                        );
                    }
                    Err(e) => {
                        log::warn!("gb28181: talkback recv error: {e}");
                        break;
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
fn build_audio_sdp_answer(
    media_port: u16,
    ssrc: u32,
    device_ip: &str,
    codec: AudioCodec,
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
         y={ssrc}\r\n",
        codec.name()
    )
}

/// Build a SIP error response.
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
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
    let mut sn = 1u32;
    let mut cseq = 1000u32;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let sn_str = sn.to_string();
                let notify = build_keepalive_notify(
                    &sn_str, device_id, domain, local_ip, local_port, "OK", cseq,
                )?;

                let data = serialize_wire(&notify);
                if let Err(e) = sip_socket.send_to(&data, platform_addr).await {
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
) -> Result<()> {
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
    shutdown: &mut watch::Receiver<bool>,
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
        sip_socket: Some(placeholder_udp),
        tcp_conn: Some(write_half),
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        local_ip: local_ip.clone(),
        recording_index,
        playback_ctl: None,
        audio_sink,
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

    // -- local IP probe retry (boot network race) ---------------------------

    /// A probe that fails twice with ENETUNREACH (the boot race: no route to
    /// the platform yet) and succeeds on the third call must succeed overall
    /// after exactly three attempts.
    #[tokio::test]
    async fn test_local_ip_probe_retries_transient_failures() {
        let (_tx, mut rx) = watch::channel(false);
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
        let (_tx, mut rx) = watch::channel(false);
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
        let (tx, mut rx) = watch::channel(false);
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let scripted = Arc::clone(&calls);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = tx.send(true);
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
        let (_tx, mut rx) = watch::channel(false);
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
        let sdp = build_audio_sdp_answer(40000, 777, "192.168.62.104", AudioCodec::Pcma);
        assert_eq!(
            sdp,
            "v=0\r\no=- 0 0 IN IP4 192.168.62.104\r\ns=Play\r\nc=IN IP4 192.168.62.104\r\nt=0 0\r\nm=audio 40000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\ny=777\r\n"
        );
    }

    #[test]
    fn test_build_audio_sdp_answer_pcmu() {
        let sdp = build_audio_sdp_answer(40001, 778, "192.168.62.104", AudioCodec::Pcmu);
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
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: Some(Arc::new(source)),
        playback_ctl: None,
        audio_sink: None,
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
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
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
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
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
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: Some(Arc::new(source)),
        playback_ctl: None,
        audio_sink: None,
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
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
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
        sip_socket: Some(sip_socket),
        tcp_conn: None,
        media_socket: None,
        media_tcp_conn: None,
        media_task: None,
        subscriber_id: None,
        invite_info: None,
        local_ip: "192.168.62.104".to_string(),
        recording_index: None,
        playback_ctl: None,
        audio_sink: None,
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
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
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
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
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
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
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
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            local_ip: "192.168.62.104".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: None,
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
            sip_socket: Some(sip_socket),
            tcp_conn: None,
            media_socket: None,
            media_tcp_conn: None,
            media_task: None,
            subscriber_id: None,
            invite_info: None,
            local_ip: "127.0.0.1".to_string(),
            recording_index: None,
            playback_ctl: None,
            audio_sink: Some(Arc::new(move |payload: &[u8], ssrc: u32| {
                sink_capture.lock().unwrap().push((payload.to_vec(), ssrc));
            })),
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
}
