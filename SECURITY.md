# Security Policy

## Supported versions

Only the latest tagged release and the current `main` branch receive
security fixes. Older tags are end-of-life — the library is consumed via
git pins / crates.io, upgrade is a version bump.

| Version | Supported |
|---------|-----------|
| latest tag | ✅ |
| `main` | ✅ (fixes land here first, PR-only) |
| older tags | ❌ end-of-life |

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

- Prefer a private [GitHub security advisory](https://github.com/mickeyzzc/gb28181-rs/security/advisories/new).
- Alternatively email the maintainer (see the GitHub profile); include
  `gb28181-rs security` in the subject.

Include reproduction details (SIP trace, MANSCDP body, capture) when
possible. You will get an acknowledgement within 7 days. Urgent fixes are
released as patch versions out of band; otherwise they ship with the next
capability package (merge ≠ release — see `CONTRIBUTING.md`).

## Scope

Security-relevant surfaces maintained by this library:

- SIP message parsing from **untrusted** platforms/peers, REGISTER
  challenge handling, INVITE/BYE/MESSAGE transaction handling.
- MANSCDP XML codec (GB2312/GBK transcoding included).
- RTP/PS muxing and demuxing of media payloads.
- SDP generation/parsing.
- `security35114` (feature `gb35114`): SM2 certificate authentication
  material handling — certificates and keys are read from caller-supplied
  paths and never logged.
- `Platform` (UAS) surfaces: challenge/verify state machine and Note
  verification.

Out of scope: consumers' credential storage, SIP-over-TLS deployment
choices, media encryption beyond GB 35114 level A (levels B/C need SVAC
hardware and are not implemented).

## Safe harbor

Fuzzing and penetration testing against your own deployments, and
submitting crashers found by property/fuzz testing, are welcome — please
still report anything that survives the in-repo test corpus privately
first.
