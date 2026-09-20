# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project follows [semantic versioning](https://semver.org/). Wire-format
golden strings are contracts — any golden change is a breaking change.

Releases are capability packages: merges accumulate on `main` silently and
ship with the next tag (merge ≠ release). Only urgent security fixes are
released out of band.

## [Unreleased]

## [v0.12.0] — 2026-09-20

The GB/T 28181-2022 device-side closure package (#57/#58), twin of
gb28181-go v0.11.0 — every platform→device control, config, and
intercom flow now has a decoded wire path and a host seam — plus the
ILP32 unlock, three fixes, and a hardened CI floor.

- `fix(manscdp)` compile on ILP32 targets (#56, fixes #55): widen
  `tm_gmtoff` to `i64` in `device_local_offset_secs` (identity on
  LP64, required on ILP32). Unblocks 32-bit consumers; the new
  `ilp32-check` CI job (armv7 musl, mirrors windows-check) keeps it
  fixed.

- `feat(manscdp)` DeviceControl sub-command decode + host seam (#63):
  `parse_device_control` (IFrameCmd/RecordCmd/GuardCmd/AlarmCmd/
  TeleBoot/PTZCmd passthrough) + `DeviceControlHandler` — no handler
  installed keeps the historical reject.

- `feat(manscdp)` PTZCmd bit-level decode (#67): golden byte table
  shared with the go twin, generated from the platform-side
  constructors; `DeviceControlKind::Ptz(String)` evolved in place to
  `Ptz(PtzCommand)`.

- `feat(server)` X-GB-Ver protocol version negotiation (Annex I, #68):
  `protocol_version` config (opt-in; None omits the header and keeps
  the wire byte-identical), stamped on both REGISTER legs;
  `ServerHandle::platform_protocol_version()`.

- `feat(manscdp)` GB/T 28181-2022 information-query minimal responses
  (#65): the five query pairs (HomePosition/CruiseTrackList/
  CruiseTrack/PTZPosition/SDCardStatus) answer with minimal valid
  empty-capability responses, goldens pinned to the standard text.

- `feat(playback)` §9.4.2 MediaStatus INFO on natural session end
  (#64): `MediaEndInfo` snapshots the dialog at INVITE; the INFO fires
  where the task drains naturally — BYE (server abort) stays silent,
  pinned both directions.

- `feat(subscribe)` SUBSCRIBE/NOTIFY framework, device side (#66):
  per-event `SubscriptionRegistry` + host `DeviceNotifier`
  (`send_alarm`/`send_catalog_change`/`send_mobile_position`,
  unsubscribed = safe no-op) + `with_position_source` periodic
  reporting. The notifier sends through a blocking std clone of the
  SIP socket — host threads may sit outside the tokio runtime.

- `feat(server)` `DeviceNotifier` exposed to hosts (#70):
  `Gb28181Server::notifier()` (pre-spawn, safe when unsubscribed).

- `feat(server)` SIP-Date time sync observation (#71): parses the
  three RFC 3261 Date forms, exposes `platform_date_unix()` —
  observation only, the host decides whether to act.

- `feat(manscdp)` DragZoom control decode + `on_drag_zoom` seam (#72):
  2022 §A.2.3.1.8/9 line format, six required integer children,
  missing child = explicit reject (same strict read as the go twin).

- `feat(server)` graceful deregistration on shutdown (#73):
  `shutdown_with_deregister` — REGISTER `Expires: 0` with the full 401
  dance (GB35114 authenticator included), Call-ID reuse per RFC 3261
  §10.2.2, 2 s short timeout; all-failures degrade to a warn. TCP
  transport keeps the fast stop (documented); SIPS stays a documented
  exclusion.

- `feat(server)` talkback upstream — device→platform G.711 RTP sender
  (#74): `with_talkback_source`, 20 ms pacing, PT 8/0; golden answer
  bytes unchanged, `a=recvonly` offer without a source is refused 488.

- `feat(talkback)` codec-aware sink delivery + real-NVR goldens (#53):
  the sink receives the negotiated law per session.

- `fix(client)` classify server-answered queries as known (#76): the
  thin dispatcher warned "unknown Query CmdType" on every keepalive
  DeviceStatus poll while the rich dispatcher was answering §7.6
  properly — known query families no longer warn, genuinely unknown
  ones still do.

- `fix(server)` accept a direct 200 OK on the initial REGISTER (#75):
  RFC 3261 §10.2 allows registration without auth; the go twin always
  accepted. Covers stale-200-across-instances too.

- `feat` voice broadcast device half (§9.12.1, #77): A.2.5.5 notify →
  A.2.6.11 ack (OK only with a sink) → audio INVITE back-call
  (`s=Play`, Subject `<source>:<ssrc>,<device>:0`, Request-URI = the
  platform's real address) → RTP feeds the `AudioTalkbackSink`; BYE
  tears down through the existing path. Loopback tests pin the full
  wire set + the refusal path.

- `feat(server)` expose the negotiated upstream talkback law (#78):
  `TalkbackSource { rx, law }` shared slot, builder getter available
  pre-spawn; the law defaults to PCMA and re-negotiates per offer.

- `ci` coverage gate, 80% → 85% lines (#79, #81): the cargo-llvm-cov
  report (issue #33) becomes a gate — measured baseline 92.5% lines.

- `test` property suite extended to the MANSCDP/PTZ/digest/date
  surfaces (#80): device control/config/snapshot parses never panic on
  arbitrary bytes; PTZ decode is byte-faithful both ways (Invalid
  preserves the trimmed input; every generated valid A5+checksum
  command decodes); digest-auth and SIP Date parsing never panic.

- `ci` repo hygiene gate (#52) and `docs` manual migration to the doc
  hub (#54) round out the batch.

## [v0.11.0] — 2026-09-09

The device-snapshot capability package (mibee-eye-raspi#28): one
complete user-valuable feature with tests and bilingual docs.

- `feat(device)` snapshot command execution (mibee-eye-raspi#28 / GB/T
  28181-2022 A.2.1.24 + A.2.5.7): a DeviceControl(SnapShot) MESSAGE is
  answered 200, handed to the new `snapshot::SnapshotExecutor` seam
  (`with_snapshot_executor`), and completes asynchronously with an
  UploadSnapShotFinished notify echoing the SessionID plus one
  SnapShotFileID per uploaded file — an empty list reports the exchange
  as wholly/partially failed. The executor owns the product side
  (capture + POST each JPEG body to the command's `upload_url`
  verbatim). UDP transport only; over TCP — and without an executor —
  the historical control-reject behavior is kept.

- `fix(config)` no default SIP password (#26): `password` now defaults to
  empty instead of the spec-example `12345678` — a mis-loaded host config
  can no longer silently authenticate with a publicly documented value.
  `check_example_defaults()` flags an empty password (digest auth cannot
  succeed) and a password explicitly set to the well-known example value;
  strict mode (`strict_example_defaults`) refuses both at startup. Hosts
  that relied on the serde default must set the password explicitly.

## [v0.10.0] — 2026-09-09

- `docs` logging & tracing compatibility guide (#36): the decision
  record for keeping the `log` facade (widest host compatibility —
  `tracing` absorbs `log` automatically; native tracing would silence
  plain-log hosts), the LogTracer bridge wiring, and the emitted-level
  reference. Bilingual (docs/en + docs/zh).

- `test` soak harness (#35): `cargo test --test soak -- --ignored`
  drives INVITE→200/BYE→200 cycles (default 50,
  `GB28181_SOAK_CYCLES` scales) over a registered, keepalive-flowing
  session, asserting no descriptor growth across per-session media
  socket/task teardown. `#[ignore]`-gated so normal CI stays fast.

- `bench` criterion benches (#34): `ps` (keyframe mux/demux/roundtrip),
  `rtp` (keyframe vs P-frame PS fragmentation), `sip` (REGISTER and
  catalog MESSAGE parse/serialize/roundtrip). Run via `cargo bench`;
  dev-dependency only, no MSRV impact.

- `feat(gb35114)` device-side downstream `Note` verification (#41): after
  the A-level handshake, platform→device requests carrying a Note are
  verified against the negotiated VKEK with a ±5-minute Date freshness
  window (the replay guard — the digest alone is self-consistent).
  Failure behavior is `Gb28181Config::incoming_note_policy`: 403 under
  the default `reject`, log-only `warn` for rollout observation, `off`.
  Note-less requests keep passing (mixed-mode Digest platforms),
  mirroring the platform-side verifier. New
  `RegisterAuthenticator::verify_incoming_note` trait method (default
  accepts); the security35114 Authenticator overrides it. Verified
  end-to-end against a fake platform signing with the real crypto.
- `feat(manscdp)` GB28181-2022 snapshot wire types (`SnapShot` control,
  `UploadSnapShotFinished` notify) — twin parity with gb28181-go (#40)
- `feat(metrics)` library-neutral `MetricsHooks` observability seam (#39)
- `test` property tests for the untrusted-input parsers (#38)

## [v0.9.0] — 2026-09-08

GB 35114 platform (UAS) side: `security35114::Platform` challenge /
verify / Note-verification state machine, sharing fixtures and golden
vectors with the Go twin. (#28)

## [v0.8.0] — 2026-09-08

**Added** `security35114` module behind the `gb35114` feature: GB 35114
A-level device security — SM2 certificate mutual authentication for
REGISTER and the keyed-SM3 Note integrity header. Only level A; B/C
(SVAC media) are out of scope. (#27)

## [v0.7.0] — 2026-09-02

- **Added** talkback receive half: audio-only INVITE demuxed to an
  `AudioTalkbackSink` (G.711). (#25)
- **Fixed** local-IP route probe retried to survive the boot network race. (#24)

## [v0.6.0] — 2026-08-31

**Changed (breaking)** library hardening: no-panic constructors
(`Result` returns), configurable SIP port and identity, GB2312 codec for
MANSCDP, graceful shutdown. (#22)

## [v0.5.1] — 2026-08-30

- **Fixed** publish packaging: libraries are lockless — the test-generated
  `Cargo.lock` is dropped before `cargo publish`. (#21)
- **Added** crates.io release workflow on `vX.Y.Z` tags (#20), README badge
  and install snippet (#19), playback and MANSCDP-offline examples (#18).

## [v0.5.0] — 2026-08-30

**Fixed** signaling robustness: cached 200 OK re-sent on INVITE
retransmission; REGISTER refreshed at half-expiry. (#17, issues #18/#19)

## [v0.4.0] — 2026-08-29

**Added** runnable examples and closed the accompanying test-coverage
gaps. (#16)

## [v0.3.0] — 2026-08-29

- **Added** H.265 PS muxing (PSM stream_type 0x24, framing shared with
  H.264). (#14)
- **Added** cross-language twin goldens: the mux wire format is pinned to
  gb28181-go's output. (#15)
- **Fixed** portability: POSIX `localtime_r` cfg-guarded; MSRV and Windows
  verified in CI. (#13)

## [v0.2.0] — 2026-08-29

**Fixed** PS mux: access units larger than 64 KB split across bounded
continuation PES packets. (#12)

## [v0.1.3] — 2026-08-29

**Fixed** REGISTER responses matched by CSeq — stale-cycle responses are
skipped. (#6)

## [v0.1.2] — 2026-08-28

**Fixed** TCP `$` framing header is 4 bytes, not 3. (#3)

## [v0.1.1] — 2026-08-28

**Fixed** INVITE SDP honors TCP media offers. (#2)

## [v0.1.0] — 2026-08-28

Initial release: GB/T 28181-2016/2022 device (UAC) library — SIP
signaling, SDP, RTP/PS muxing, MANSCDP codec, keepalive/catalog.

[Unreleased]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.9.0...HEAD
[v0.9.0]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.8.0...v0.9.0
[v0.8.0]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.7.0...v0.8.0
[v0.7.0]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.6.0...v0.7.0
[v0.6.0]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.5.1...v0.6.0
[v0.5.1]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.5.0...v0.5.1
[v0.5.0]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.4.0...v0.5.0
[v0.4.0]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.3.0...v0.4.0
[v0.3.0]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.2.0...v0.3.0
[v0.2.0]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.1.3...v0.2.0
[v0.1.3]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.1.2...v0.1.3
[v0.1.2]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.1.1...v0.1.2
[v0.1.1]: https://github.com/mickeyzzc/gb28181-rs/compare/v0.1.0...v0.1.1
[v0.1.0]: https://github.com/mickeyzzc/gb28181-rs/releases/tag/v0.1.0
