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
