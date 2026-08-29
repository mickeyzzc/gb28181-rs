# gb28181-rs

**English** | [中文](README.zh-CN.md)

[![CI](https://github.com/mickeyzzc/gb28181-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/mickeyzzc/gb28181-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Language: Rust](https://img.shields.io/badge/language-Rust-dea584.svg)
![Tests](https://img.shields.io/badge/tests-104%20passing-brightgreen.svg)

GB/T 28181-2016/2022 **device-side (UAC)** library for Rust — register a camera or media source with a GB28181 SIP platform and stream to it.

Hand-written SIP (no SIP framework), MANSCDP XML codec, RTP/PS media push, and a full device server with live streaming, playback, and download. Extracted verbatim from the production implementation in [mibee-eye-raspi-rs](https://github.com/Mi-Bee-Studio), hardened against real GB28181 platforms (digest-auth URI matching, Via branch uniqueness, local-IP detection, MANSCDP attribute/element dual forms, TCP transport, playback control).

## Features

- **SIP signaling** — hand-written parser/serializer for the GB/T 28181 subset (REGISTER + digest auth, INVITE, MESSAGE, BYE, ACK, OPTIONS), UDP and TCP
- **Registration lifecycle** — REGISTER with 401 digest challenge (MD5 + SHA-256, qop=auth), periodic re-register, keepalive heartbeat with timeout
- **MANSCDP XML** — Catalog / DeviceInfo / DeviceStatus / RecordInfo / Keepalive, element **and** attribute forms, GB2312/GBK/GB18030/UTF-8 bodies
- **Media push** — H.264/H.265 NALUs → MPEG-2 Program Stream → RTP (UDP + RTP-over-TCP framed), SSRC handling, bounded PES splitting for large access units
- **Live + playback + download** — INVITE-driven live sessions; RecordInfo queries and paced playback/download from recorded segments, with SIP INFO playback control (play/pause/speed)
- **Reference segment format** — bare Annex-B H.264 + `.ts.jsonl` per-frame timestamp sidecar ([`segment`](src/segment.rs))

Not included (by design): platform/UAS role, SIP over TLS/WebSocket.

## Usage

```toml
[dependencies]
gb28181-rs = { git = "https://github.com/mickeyzzc/gb28181-rs.git", tag = "v0.4.0" }
```

The crate is transport- and storage-agnostic. Your host injects two seams:

```rust
use gb28181_rs::{FrameSource, FrameSubscription, RecordingSource, SegmentMeta,
                 Gb28181Config, Gb28181Server, set_record_active};

// 1) Live frames: implement FrameSource over your capture pipeline's hub.
struct MyFrameHub { /* ... */ }
impl FrameSource for MyFrameHub {
    fn subscribe_with_capacity(&self, capacity: usize) -> FrameSubscription { /* ... */ }
    fn unsubscribe(&self, id: u64) { /* ... */ }
}

// 2) Recordings: implement RecordingSource over your recording index.
struct MyRecordings { /* ... */ }
impl RecordingSource for MyRecordings {
    fn lookup(&self, start_ms: u64, end_ms: u64) -> Vec<SegmentMeta> { /* ... */ }
}

// 3) Run the device server.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config: Gb28181Config = toml::from_str(&std::fs::read_to_string("config.toml")?)?;
    let server = Gb28181Server::start(
        config,
        std::sync::Arc::new(MyFrameHub { /* ... */ }),
        Some(std::sync::Arc::new(MyRecordings { /* ... */ })),
    ).await?;
    server.await
}
```

While local recording runs, call `set_record_active(true)` so DeviceStatus reports `<Record>ON</Record>`.

A ready-made [`MockFrameHub`](src/mock.rs) implements `FrameSource` with bounded-channel, drop-on-full semantics for tests.

## Examples

Runnable demos live in [`examples/`](examples/):

```sh
# Offline byte-level demo: mux H.264/H.265 to PS, parse back, oversized-frame
# PES splitting — deterministic, no network.
cargo run --example ps_mux

# Full in-process interop: a hand-written fake platform (SIP server + RTP
# receiver) registers the real device server, queries the catalog, INVITEs a
# live stream, demuxes RTP/PS back to NAL units, and BYEs — with the digest
# response verified on the platform side. Exits 0 on success.
cargo run --example device_demo
```

`device_demo` doubles as an executable smoke test of the whole stack (REGISTER + 401 digest, catalog, INVITE/ACK, RTP/PS media, BYE) without any hardware.

## Development

This project follows strict **TDD** — see [CONTRIBUTING.md](CONTRIBUTING.md). CI enforces `rustfmt`, `clippy -D warnings` (which also compiles the examples), and the full test suite (104 tests); `main` is protected (PR-only merges, CI required).

## Status

v0.4.0 — API surfaces (`FrameSource`, `RecordingSource`, config) are settling but not yet frozen. Production-tested daily at [Mi-Bee Studio](https://github.com/Mi-Bee-Studio) against the MiBee NVR GB28181 platform.

## License

MIT — see [LICENSE](LICENSE). Extracted from Mi-Bee Studio camera projects; interoperability fixes tracked in the shared camera issue tracker.
