# Changelog

All notable changes to this project are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project follows [semantic versioning](https://semver.org/). Wire-format
golden strings are contracts — any golden change is a breaking change.

Releases are capability packages: merges accumulate on `main` silently and
ship with the next tag (merge ≠ release). Only urgent security fixes are
released out of band.

## [Unreleased]

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
