# Logging & tracing compatibility

The library logs through the standard [`log`](https://crates.io/crates/log)
0.4 facade — a deliberate choice evaluated against a native `tracing`
migration (issue #36). This guide is the decision record plus the wiring
for both host kinds.

## Why the `log` facade stays

- **It is the widest compatible choice.** A `log`-based library works
  unchanged with plain-log hosts (`env_logger`, `simple_logger`, …) AND
  tracing-based hosts — `tracing` absorbs `log` records automatically
  through the `tracing-log` bridge. The reverse is not true: a native
  `tracing` library is silent for plain-log hosts unless they install a
  bridge themselves.
- **Library diagnostics are events, not spans.** Everything this crate
  emits is a point event (registration transitions, INVITE lifecycle,
  keepalive failures). The value `tracing` would add — spans following
  the session lifecycle — requires plumbing span context through the
  engine's public API (`Gb28181Server` spawns its tasks internally), an
  API change with no event-level payoff. Revisit only if a host need
  surfaces (tracked on the enterprise roadmap Epic).

## Plain-log hosts

Initialize any `log` implementation:

```rust
env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
    .init();
```

## tracing-based hosts

`tracing-subscriber` with the LogTracer bridge captures the library's
records as tracing events — no code change in the host beyond init:

```rust
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

tracing_log::LogTracer::init().expect("install log bridge");
tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(tracing_subscriber::EnvFilter::from_default_env())
    .init();
```

Records arrive at the `INFO`/`WARN`/`ERROR`/`DEBUG` levels the library
emits; filter with `gb28181_rs=debug` style directives like any other
crate.

## What the library emits

| Level | Meaning |
|---|---|
| `error` | a subsystem failed and degraded (e.g. keepalive send error, socket recv error) |
| `warn` | recoverable anomalies: registration retry, INVITE retransmission replay, GB35114 Note rejection, strict-mode example-value warnings |
| `info` | lifecycle transitions: listener started, registered, INVITE/BYE, shutdown |
| `debug` | per-message detail (unhandled methods, periodic re-registration ticks) |

None of it is required for correctness — run silent in production by
raising the filter; turn `debug` on when reproducing interop issues.
