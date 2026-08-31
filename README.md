# gb28181-rs

**English** | [中文](README.zh-CN.md)

[![CI](https://github.com/mickeyzzc/gb28181-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/mickeyzzc/gb28181-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Language: Rust](https://img.shields.io/badge/language-Rust-dea584.svg)
![Tests](https://img.shields.io/badge/tests-133%20passing-brightgreen.svg)

GB/T 28181-2016/2022 **device-side (UAC)** library for Rust — register a camera or media source with a GB28181 SIP platform and stream to it.

Hand-written SIP (no SIP framework), MANSCDP XML codec, RTP/PS media push, and a full device server with live streaming, playback, and download. Extracted from the production implementation in [mibee-eye-raspi-rs](https://github.com/Mi-Bee-Studio), hardened against real GB28181 platforms (digest-auth URI matching, Via branch uniqueness, local-IP detection, MANSCDP attribute/element dual forms, TCP transport, playback control).

## Features

- **SIP signaling** — hand-written parser/serializer for the GB/T 28181 subset (REGISTER + digest auth, INVITE, MESSAGE, BYE, ACK, OPTIONS), UDP and TCP
- **Registration lifecycle** — REGISTER with 401 digest challenge (MD5 + SHA-256, qop=auth), periodic re-register, keepalive heartbeat with timeout
- **MANSCDP XML** — Catalog / DeviceInfo / DeviceStatus / RecordInfo / Keepalive, element **and** attribute forms; inbound bodies accepted as UTF-8 **or** GB2312/GBK/GB18030, outbound GB2312-declared bodies wire-encoded correctly
- **Media push** — H.264/H.265 NALUs → MPEG-2 Program Stream → RTP (UDP + RTP-over-TCP framed), SSRC handling, bounded PES splitting for large access units; RTP timestamps derived from real capture time (any frame rate)
- **Live + playback + download** — INVITE-driven live sessions; RecordInfo queries and paced playback/download from recorded segments, with SIP INFO playback control (play/pause/speed)
- **Reference segment format** — bare Annex-B H.264 + `.ts.jsonl` per-frame timestamp sidecar ([`segment`](src/segment.rs))

Not included (by design): platform/UAS role, SIP over TLS/WebSocket.

## Usage

```toml
[dependencies]
gb28181-rs = "0.6.0"
# git alternative: gb28181-rs = { git = "https://github.com/mickeyzzc/gb28181-rs.git", tag = "v0.6.0" }
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

// 3) Run the device server. Shutdown is graceful: the recv/accept loop,
//    keepalive task, and any active media task all stop.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config: Gb28181Config = toml::from_str(&std::fs::read_to_string("config.toml")?)?;
    // Identity is host-configurable and neutral by default (see below).
    config.user_agent = Some("my-host/1.0 (gb28181-rs)".to_string());

    let mut server = Gb28181Server::start(
        config,
        std::sync::Arc::new(MyFrameHub { /* ... */ }),
        Some(std::sync::Arc::new(MyRecordings { /* ... */ })),
    ).await?;
    // ... run your app; on exit:
    server.shutdown().await
}
```

While local recording runs, call `set_record_active(true)` so DeviceStatus reports `<Record>ON</Record>`.

The crate logs through the standard [`log`](https://crates.io/crates/log) facade — initialize a logger (`env_logger`, `tracing`, …) in the host to see output; without one the library stays silent.

A ready-made [`MockFrameHub`](src/mock.rs) implements `FrameSource` with bounded-channel, drop-on-full semantics for tests.

### Configuration

`Gb28181Config` is serde-friendly (TOML/JSON) and can be re-exported into the host's own config struct. Connection defaults follow the spec's example values (`platform_sip_address = 192.168.1.1`, `device_id = 34020000001320000001`, `password = 12345678`, ports 5060) — **always set them explicitly in production**; the server logs a warning at startup when the example defaults are still in effect.

Identity fields (all optional, all neutral by default — this library never advertises a product/vendor name on the wire):

| Field | Default | Used in |
|---|---|---|
| `user_agent` | `gb28181-rs/<version>` | SIP `User-Agent` on REGISTER |
| `device_name` | `Camera <device_id>` | Catalog/DeviceInfo `Name` |
| `manufacturer` | `Unknown` | Catalog/DeviceInfo `Manufacturer` |
| `model` | `Unknown` | Catalog/DeviceInfo `Model` |
| `firmware` | crate version | DeviceInfo `Firmware` |

`enabled` is a host convenience switch — the library never reads it; the host gates `start()` on it.

## Documentation

Topic guides live under [`docs/en/`](docs/en/) — each has a Chinese counterpart under `docs/zh/`:

| Guide | Covers |
|---|---|
| [Configuration](docs/en/configuration.md) | every `Gb28181Config` field, identity defaults, the example-value warning, device-ID structure |
| [Live streaming](docs/en/live-streaming.md) | the `FrameSource` seam, `Nalu`/`AccessUnit` shapes, INVITE lifecycle, PTS derivation |
| [Recording & playback](docs/en/recording-playback.md) | `RecordingSource`, `SegmentMeta`, the reference segment format, RecordInfo/playback/download/control |
| [PS muxing & RTP](docs/en/psmux.md) | standalone `mux_h264_to_ps`/`mux_h265_to_ps`, parsing, `RtpPusher`, byte-level golden guarantees |
| [MANSCDP & charsets](docs/en/manscdp.md) | message types, dual element/attribute forms, UTF-8/GB18030 wire charsets, device IDs |
| [Server lifecycle](docs/en/server.md) | constructors vs binding, UDP/TCP transports, graceful shutdown, retry/backoff, logging |

## Library hygiene (v0.6.0 hardening)

v0.6.0 made the crate safe to embed as a neutral foundation library. The regression tests in [`tests/library_hygiene.rs`](tests/library_hygiene.rs) pin each guarantee:

- **No panics reachable by consumers** — constructors do no I/O and never panic; `format_device_id` returns `Result`.
- **No hardcoded SIP port** — REGISTER/BYE Via & Contact advertise the configured `local_sip_port` (was hardcoded 5060).
- **Charset-correct MANSCDP** — inbound GB2312/GBK/GB18030 bodies are decoded (previously dropped); outbound bodies declaring GB2312 are encoded to match the declaration (ASCII output is byte-identical to before, so wire goldens are preserved).
- **XML escaping** — host-supplied strings (names, paths) are escaped before interpolation.
- **Random, unique SIP identifiers** — Via branches, From/To tags, and digest `cnonce` come from the CSPRNG instead of counters/clock time.
- **`log` facade** — no `println!`/`eprintln!` in library code.
- **Graceful shutdown** — `ServerHandle::shutdown()` stops the run loop, keepalive, and media tasks.
- **No branding, no lab addresses** in library code (config example defaults excepted, warned at startup).

### Breaking changes, 0.5.x → 0.6.0

- `Gb28181Server::start` returns `ServerHandle` (was `JoinHandle<()>`); `handle.await` still works, `handle.shutdown().await` is new.
- `build_register_request` / `build_bye_request` take `local_port` (+ `from_tag`/`user_agent` for REGISTER).
- `format_device_id` returns `Result<String>` (was panicking `String`).
- Catalog/DeviceInfo identity defaults changed from vendor strings to neutral ones — set the config fields above to restore custom values.
- Library output moved from stdout/stderr to the `log` facade.

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

# Recording playback: RecordInfo query, paced playback INVITE (s=Playback,
# t=<start> <end>), RTP/PS reassembly, PlaybackControl PAUSE / PLAY (Speed 2),
# BYE — against a synthetic Annex-B + PTS-sidecar segment.
cargo run --example playback_demo

# Offline message layer: device-ID format/parse round-trip, keepalive Notify
# build+parse, RecordInfo/DeviceInfo responses, GB/T 28181 time strings.
cargo run --example manscdp_demo
```

`device_demo` doubles as an executable smoke test of the whole stack (REGISTER + 401 digest, catalog, INVITE/ACK, RTP/PS media, BYE) without any hardware. `playback_demo` covers the recorded-media path end-to-end (asserted), and `manscdp_demo` the pure message layer — no sockets, no hardware.

## Development

This project follows strict **TDD** — see [CONTRIBUTING.md](CONTRIBUTING.md). CI enforces `rustfmt`, `clippy -D warnings` (which also compiles the examples), and the full test suite (133 tests); `main` is protected (PR-only merges, CI required).

## Status

v0.6.0 — API surfaces (`FrameSource`, `RecordingSource`, config) are settling but not yet frozen. Production-tested daily at [Mi-Bee Studio](https://github.com/Mi-Bee-Studio) against the MiBee NVR GB28181 platform.

## License

MIT — see [LICENSE](LICENSE). Extracted from Mi-Bee Studio camera projects; interoperability fixes tracked in the shared camera issue tracker.
