# Server lifecycle: start, shutdown, transports

This page covers constructing, starting, and gracefully shutting down `Gb28181Server`, plus transport selection, with an overview of the device-side protocol exchange. Configuration details live in [configuration](gb28181-rs-configuration.md); live streaming and playback in [live streaming](gb28181-rs-live-streaming.md) and [recording playback](gb28181-rs-recording-playback.md).

## Protocol Exchange at a Glance

Typical interaction after the device comes online: REGISTER (digest auth) → periodic keep-alive → Catalog / DeviceInfo queries → INVITE for live → PS over RTP media → BYE.

```mermaid
sequenceDiagram
    participant P as SIP platform
    participant S as Gb28181Server
    S->>P: REGISTER (digest auth)
    P-->>S: 200 OK (expiry)
    loop every heartbeat_interval_secs
        S->>P: MESSAGE Keepalive
        P-->>S: 200 OK
    end
    P->>S: MESSAGE Catalog / DeviceInfo query
    S-->>P: MESSAGE response
    P->>S: INVITE (SDP offer)
    S-->>P: 200 OK (SDP answer)
    P->>S: ACK
    S->>P: RTP (PS stream)
    P->>S: BYE
    S-->>P: 200 OK
```

## Install

```bash
cargo add gb28181-rs@0.12.0
```

## Construction is I/O-free

Constructors only store configuration — no sockets, no panics:

```rust
use std::sync::Arc;
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::{Gb28181Config, Gb28181Server};

let server = Gb28181Server::new(config, Arc::new(MockFrameHub::new()));
let server = Gb28181Server::with_recording_index(
    config, au_hub, Some(recording_index),
);
```

Binding happens in `spawn()`:

```rust
// Instance flavor:
let mut handle = server.spawn().await?;

// Associated-function flavor (equivalent):
let mut handle = Gb28181Server::start(config, au_hub, None).await?;
```

`spawn()` branches on `config.transport`:
- **UDP** (default) — binds `0.0.0.0:<local_sip_port>` UDP.
- **TCP** — binds a TCP listener on the same port and handles framed SIP
  per connection.

## What the server runs

- REGISTER lifecycle with digest auth (MD5 and SHA-256, `qop=auth`,
  CSPRNG cnonce), re-REGISTER on expiry, and retry/backoff on failure.
- Keepalive (MESSAGE) on `heartbeat_interval_secs`.
- Catalog / DeviceInfo / DeviceStatus answering.
- INVITE-driven live media, playback, download (see the
  [live-streaming](gb28181-rs-live-streaming.md) and
  [recording-playback](gb28181-rs-recording-playback.md) guides).

Registration retries and backoff are shutdown-aware: a pending backoff
aborts immediately when shutdown is requested.

## Graceful shutdown

`ServerHandle` offers two stops:

```rust
handle.shutdown().await; // graceful: loops stop, in-flight media task finishes, cleanup runs
handle.abort();           // immediate: tokio task abort (last resort)
```

The handle also implements `Future` (awaits server exit), so a typical
supervisor is:

```rust
tokio::select! {
    _ = tokio::signal::ctrl_c() => handle.shutdown().await?,
    _ = &mut handle => { /* server exited on its own */ }
}
```

Shutdown is covered by integration tests: UDP and TCP servers both stop
their receive loops promptly, and lifecycle tests assert constructors
never panic on valid input.

## Logging

The crate logs through the [`log`] facade only — initialize a logger
(`env_logger`, `tracing`, ...) in the host to see registration, INVITE,
and media events. Nothing writes to stdout/stderr directly.

[`log`]: https://docs.rs/log

## Voice talkback (audio-only INVITE, receive half)

GB/T 28181-2022 §9.2 voice talkback: the platform sends an **audio-only
INVITE** (`m=audio` with no `m=video`) and streams G.711 RTP to the device.
Since 0.7.0 the server implements the receive half:

```rust
use std::sync::Arc;
use gb28181_rs::{Gb28181Config, Gb28181Server, AudioTalkbackSink};

// A closure Fn(&[u8], u32) works out of the box (blanket impl):
let sink = |payload: &[u8], ssrc: u32| {
    // payload = one RTP packet's G.711 bytes (A-law, payload type 8, or
    // μ-law, payload type 0). Copy into a channel to your audio output
    // thread — this runs on the media task and must stay cheap.
};

let server = Gb28181Server::new(config, hub)
    .with_audio_sink(Arc::new(sink));
```

Behavior:

- Offer parsing: `parse_invite` reports `media_kind: MediaKind::Audio` and
  `audio_codec` (`AudioCodec::Pcma`/`Pcmu`) for audio-only offers; mixed
  offers with an `m=video` line keep the video-push path.
- The answer advertises an ephemeral UDP receive port and mirrors the
  offered payload type (`a=rtpmap:8 PCMA/8000` / `0 PCMU/8000`), with the
  session SSRC in the `y=` line — byte-stable, golden-tested.
- Each received RTP packet's payload (12-byte header + CSRCs stripped) is
  handed to the sink with the packet's SSRC.
- Without a sink, or for non-G.711 / TCP-media offers, the INVITE is
  refused with **488**.
- The talkback dialog occupies the single-dialog slot shared with video
  sessions: a video INVITE recycles an active talkback and vice versa;
  BYE tears the receiver down through the shared media-task cleanup.

Sending the device's microphone audio back (the send half) is not part of
this revision.

## 2022 device-side closure (v0.12.0)

Since v0.12.0 every GB/T 28181-2022 platform→device control, config,
and intercom flow has a decoded wire path plus a host seam. Install
only what your hardware honors — unhandled commands keep the
historical explicit reject.

**DeviceControl** — `with_control_handler` installs a
`DeviceControlHandler`; decoded commands arrive as typed values:

```rust
server.with_control_handler(MyControl {
    // IFrameCmd Send, RecordCmd, Guard/Alarm/TeleBoot …
    // PTZCmd arrives pre-decoded: PtzCommand (move/lens/preset/…)
    // DragZoom arrives as manscdp::DragZoom (six required integers)
});
```

**DeviceConfig** — `with_config_handler` receives `DeviceConfig`
(BasicParam all-optional children, FrameMirror 0–3, AlarmReport
switches); ConfigDownload and HomePosition are answered from the
running configuration.

**Subscriptions & notifications** — `server.notifier()` (take it
before spawn) exposes `DeviceNotifier`: `send_alarm` /
`send_catalog_change` / `send_mobile_position`, safe no-ops while the
platform has not subscribed. `with_position_source` feeds the periodic
MobilePosition loop.

**Intercom** — `with_talkback_source(mpsc::Receiver<Vec<u8>>)` is the
device→platform upstream (20 ms G.711 frames, law defaults to PCMA and
re-negotiates per offer); the AudioTalkbackSink (see talkback above)
now also receives the voice-broadcast downcall — installing it enables
full-duplex intercom plus broadcast reception (§9.12.1: notify → ack →
audio INVITE back-call → RTP).

**Lifecycle** — `shutdown_with_deregister()` sends REGISTER
`Expires: 0` with the full digest dance before stopping (2 s budget,
falls back to a plain shutdown on timeout; TCP transport keeps the
fast stop). `handle.platform_protocol_version()` reports the peer's
X-GB-Ver; `platform_date_unix()` exposes the SIP-Date observation
seam — the host decides whether to act on clock drift.
