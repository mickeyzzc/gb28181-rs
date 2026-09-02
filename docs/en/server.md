# Server lifecycle: start, shutdown, transports

## Construction is I/O-free

Constructors only store configuration — no sockets, no panics:

```rust
use std::sync::Arc;
use gb28181_rs::{Gb28181Config, Gb28181Server, MockFrameHub};

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
  [live-streaming](live-streaming.md) and
  [recording-playback](recording-playback.md) guides).

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
