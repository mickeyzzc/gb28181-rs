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
- **X-GB-Ver protocol version** (2022 Annex I, opt-in) — set `protocol_version` in the config (conventionally `"3.0"` = 2022, `"2.0"` = 2016) and every REGISTER carries the `X-GB-Ver` header, initial and authenticated alike; the platform's version, when its response announces one, is exposed via `ServerHandle::platform_protocol_version()`. Unset (default) omits the header — byte-identical to the pre-2022 wire form. Semantics ported from the Go twin (gb28181-go #78)
- **MANSCDP XML** — Catalog / DeviceInfo / DeviceStatus / RecordInfo / Keepalive, element **and** attribute forms; inbound bodies accepted as UTF-8 **or** GB2312/GBK/GB18030, outbound GB2312-declared bodies wire-encoded correctly
- **GB/T 28181-2022 information queries** — HomePositionQuery / CruiseTrackListQuery / CruiseTrackQuery / PTZPosition / SDCardStatus answered with the minimal valid Response (A.2.4.10-14 / A.2.6.12-16): required `SumNum` zero, optional capability blocks omitted, CruiseTrackQuery track index echoed — semantics and goldens mirror gb28181-go #78
- **Subscriptions (SUBSCRIBE/NOTIFY)** — the device answers platform SUBSCRIBEs (Catalog / Alarm / MobilePosition) with a 200 OK echoing `Expires`, keeps per-event bookkeeping with renewal/expiry, and sends SIP NOTIFYs on the subscription dialog: `DeviceNotifier` hands the host `send_alarm` / `send_catalog_change` / `send_mobile_position` (no-ops until subscribed), and an optional `MobilePositionSource` drives periodic position reports on the SUBSCRIBE `Interval` (default 5s). Wire shapes mirror the Go twin platform side (subscribe.go / handleNotify)
- **Media push** — H.264/H.265 NALUs → MPEG-2 Program Stream → RTP (UDP + RTP-over-TCP framed), SSRC handling, bounded PES splitting for large access units; RTP timestamps derived from real capture time (any frame rate)
- **DeviceControl sub-commands** (2016 §9.3.2 / 2022 §9.3) — `IFrameCmd` (force IDR: platforms send it when starting a pull or after loss), `RecordCmd` (Record/StopRecord), `GuardCmd` (SetGuard/ResetGuard), `AlarmCmd` (ResetAlarm), `TeleBoot` (Boot) and `PTZCmd` (bit-level decoded: §A.3-A.4 movement direction/speed bits, presets, cruise, FI focus/iris, auxiliary switches — `parse_ptz_command`, the byte-exact inverse of the gb28181-go platform builders with a shared golden hex table; undecodable hex arrives as `PtzCommand::Invalid` with the raw string preserved) decode from the shared Control body and fire on a `DeviceControlHandler` host seam (no-op defaults, install only what your hardware supports); unrecognized kinds keep the spec-compliant control reject. UDP transport; DragZoom decode is deferred until its 2022 wire form is verified
- **DeviceConfig minimum + ConfigDownload** (2022 §9.3.3 / A.2.3.2, A.2.4.7) — the decodable subset lands with host callbacks (`DeviceConfigHandler`): `BasicParam` (name + registration tuning — hosts decide what sticks), `FrameMirror` (0-3 flip modes) and `AlarmReport` (motion/field event report switches); a handler-accepted command answers `Result=OK`, everything else keeps the historical reject. ConfigDownload queries answer the minimal valid Response with the `BasicParam` block (name + expiry + heartbeat tuning from the live config) when requested. Note: 2022 校时 is NOT a DeviceConfig sub-command — §9.10.2 does it via the REGISTER response's SIP Date header. `HomePosition` joins the DeviceControl family (A.2.3.1.10: Enabled/ResetTime/PresetIndex → `on_home_position`)
- **Voice broadcast (receive, §9.12.1)** — the Broadcast notification (A.2.5.5) is acknowledged (A.2.6.11, OK iff an audio sink is installed) and, with a sink, the device sends the audio-only INVITE toward the announced source (s=Play / m=audio / `<source>:<ssrc>,<device>:0` Subject, Request-URI at the platform's actual address), ACKs the 200, and receives the platform's G.711 RTP through the shared media slot — BYE tears it down. Single-slot sessions; 5s INVITE answer window; declined with Result=ERROR when no sink
- **Voice talkback (receive)** — audio-only INVITE (GB/T 28181-2022 §9.2): G.711 A/μ-law RTP received on an ephemeral port and delivered to an `AudioTalkbackSink` (closure-friendly; override `on_audio_codec` to also get the negotiated A-law/μ-law variant). Handles the offer forms real stacks send — payload-type-only PCMA with no `a=rtpmap`, leading-zero decimal `y=` SSRC (locked by byte-exact production-capture golden tests). Non-G.711 or no-sink offers are refused with 488
- **Live + playback + download** — INVITE-driven live sessions; RecordInfo queries and paced playback/download from recorded segments, with SIP INFO playback control (play/pause/speed)
- **Playback/download completion notify** — a session that ends naturally sends the in-dialog §9.4.2 `MediaStatus: Play Finished` / `Download Finished` SIP INFO (MANSRTSP-style body) so the platform tears the fetch down immediately instead of waiting out stall timeouts; BYE-side teardown stays silent. UDP transport

- **GB 35114 A-level security, both sides** *(opt-in, `gb35114` feature)* — device-side SM2-certificate mutual authentication over the REGISTER flow (`with_register_authenticator`) and platform-side challenge/verify/Note state machine (`security35114::Platform`); VKEK negotiation inside the `cryptkey` SM2 DER envelope, keyed-SM3 `Note`-header integrity; golden fixtures shared with the Go twin prove cross-implementation interop
- **Reference segment format** — bare Annex-B H.264 + `.ts.jsonl` per-frame timestamp sidecar ([`segment`](src/segment.rs))

Not included (by design): platform/UAS role, SIP over TLS/WebSocket, GB 35114 B/C levels (those require SVAC hardware media per GB/T 25724).

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

`Gb28181Config` is serde-friendly (TOML/JSON) and can be re-exported into the host's own config struct. Address defaults follow the spec's example values (`platform_sip_address = 192.168.1.1`, `device_id = 34020000001320000001`, ports 5060) — **always set them explicitly in production**; the server logs a warning at startup when the example defaults are still in effect. The SIP password has **no default**: it starts empty, and the server warns when it is unset or still the well-known example `12345678`.

Identity fields (all optional, all neutral by default — this library never advertises a product/vendor name on the wire):

| Field | Default | Used in |
|---|---|---|
| `user_agent` | `gb28181-rs/<version>` | SIP `User-Agent` on REGISTER |
| `device_name` | `Camera <device_id>` | Catalog/DeviceInfo `Name` |
| `manufacturer` | `Unknown` | Catalog/DeviceInfo `Manufacturer` |
| `model` | `Unknown` | Catalog/DeviceInfo `Model` |
| `firmware` | crate version | DeviceInfo `Firmware` |

`enabled` is a host convenience switch — the library never reads it; the host gates `start()` on it.

## GB35114 A-level security (v0.8.0 device / v0.9.0 platform, opt-in)

[GB 35114-2017](https://openstd.samr.gov.cn/bzgk/std/newGbInfo?hcno=B7F5589329EF98B32F0EB8ACEC341C81) layers SM2-certificate security on top of GB/T 28181. This crate implements **A-level** only — levels B/C additionally require SVAC media (GB/T 25724, a hardware codec), which is out of scope by design. Enable with the `gb35114` feature (needs Rust ≥ 1.85; the crate's MSRV stays 1.80 without it):

```toml
[dependencies]
gb28181-rs = { version = "0.9", features = ["gb35114"] }
```

```rust
use gb28181_rs::authenticator::RegisterAuthenticator as _;
use gb28181_rs::security35114::{load_certificate, load_identity, Authenticator, Options};

let dev = load_identity(&cert_pem, &key_pem)?;          // SM2 SEC1 or PKCS#8
let platform = load_certificate(&platform_cert_pem)?;   // verifies sign2
let auth = Authenticator::new(Options::new(dev, device_id, server_id))?;

let server = Gb28181Server::with_recording_index(cfg, hub, None)
    .with_register_authenticator(Some(Arc::new(auth)))  // replaces Digest auth
    .spawn()
    .await?;
```

The handshake follows the published standard text cross-checked against real captures: `Capability` announcement → 401 with `random1` → signed re-REGISTER (`sign1` = SM2 over random2‖random1‖serverID) → 200 OK `SecurityInfo` carrying the SM2-sealed VKEK (`cryptkey`, DER C1‖C3‖C2 envelope) and, for `Bidirection`, the platform's `sign2`. After registration every outgoing request (keepalive, …) carries `Date` + `Note: Digest nonce="…",algorithm=SM3` keyed by the VKEK. Crypto comes from the RustCrypto `sm2`/`sm3` crates (pure Rust — `aarch64-musl` cross-compile friendly); the golden tests share their certificates, vectors, and interop fixtures with the Go twin (`gb28181-go/security35114`), including gmsm-produced signature/envelope samples that must verify and decrypt here.

Two points are ambiguous across implementations and therefore configurable ([`RandomEncoding`](src/security35114/mod.rs), [`Sign2Order`](src/security35114/mod.rs)): the random representation inside the signed payload, and the R1/R2 operand order of `sign2`. Defaults match the standard text. Downstream `Note` verification: after the handshake, platform→device requests carrying a `Note` are verified on the device side against the VKEK with a ±5-minute `Date` freshness window — a bad signature draws 403 by default (`Gb28181Config::incoming_note_policy`, with `warn`/`off` for rollout), and `Note`-less requests keep passing (mixed-mode Digest platforms).

### Platform side (UAS, v0.9.0)

`security35114::Platform` is the mirror-image state machine for GB/T 28181 platforms, at API parity with the Go twin's `security35114.Platform` (wire-compatible — a Go-built platform has completed live Bidirection handshakes with a Rust-built device and vice versa). It issues `Bidirection`/`Unidirection` challenges, verifies device `sign1` against the (pre-provisioned or `cnonce`-announced) certificate, seals the VKEK, signs `sign2`, and verifies every subsequent `Note`. This crate ships no UAS SIP server (device/UAC role only) — wire it into any platform stack:

```rust
use gb28181_rs::security35114::{Platform, PlatformConfig};

let mut cfg = PlatformConfig::new(server_id);            // 20-digit platform ID
cfg.identity = Some(platform_identity);                  // SM2 signing identity — sign2
cfg.device_certs.insert(device_id.to_string(), dev_cert); // or trust the cnonce announcement
let platform = Platform::new(cfg)?;

let www_auth = platform.challenge(&device_id, &capability_authorization)?; // 401 value
let security_info = platform.verify_register(&device_id, &authorization)?; // 200 OK value
platform.verify_note(&device_id, &note, "MESSAGE", &from, &to, &call_id, &date, &body)?;
```

`Platform` is concurrency-safe, keys sessions by device ID, keeps the previous VKEK verifying while a device re-registers, answers SIP-over-UDP retransmissions of the completed REGISTER idempotently, and rejects stale `random1`, scheme confusion, unknown/mismatched certificates, and foreign server IDs with the [`PlatformError`](src/security35114/platform.rs) sentinel enum (`err.downcast_ref::<PlatformError>()` on the `anyhow` chain) for 4xx mapping. The in-module loopback tests drive it against the real device-side `Authenticator` — handshake, VKEK agreement on both sides, and `Note` tamper detection.

### GB28181-2022 snapshot wire types (issue #49 twin)

`manscdp` parses the inbound `DeviceControl` snapshot command (`parse_control_snapshot`: `SnapShot` with `SnapNum`/`Interval`/`UploadURL`/`SessionID`, A.2.1.24) and builds the device-side `UploadSnapShotFinished` completion report (`build_upload_snapshot_finished`, A.2.5.7) — goldens byte-identical to the Go twin.

## Documentation

Topic guides now live in the MiBee documentation hub — the single
source of truth for library manuals, bilingual:

> **https://www.mlsbs.top/docs/mibeelibs**

Manual changes go there by PR (review flow in the hub repo's GOVERNANCE).
[`docs/README.md`](docs/README.md) keeps the redirect.
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
