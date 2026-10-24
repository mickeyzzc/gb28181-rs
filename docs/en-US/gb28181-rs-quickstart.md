# Quick start: a minimal compilable device

`gb28181-rs` is the Rust GB/T 28181 device-side implementation. This page
brings a device server up in about thirty lines: registration, keepalive,
and the skeleton for INVITE-driven streaming.

## Install

```bash
cargo add gb28181-rs@0.12.0
```

The example also needs an async runtime (the server is async):

```toml
[dependencies]
gb28181-rs = "=0.12.0"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

## Minimal example

```rust
use std::sync::Arc;

use gb28181_rs::config::Gb28181Config;
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::server::Gb28181Server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Gb28181Config {
        platform_sip_address: "192.0.2.10".to_string(),
        platform_sip_port: 5060,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001310000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "secret".to_string(),
        ..Default::default()
    };
    let hub = Arc::new(MockFrameHub::new());
    let server = Gb28181Server::new(config, hub);
    let _handle = server.spawn().await?;
    Ok(())
}
```

Notes:

- `Gb28181Server::new` only stores configuration and performs no I/O;
  `spawn()` binds the SIP socket and starts the registration / keepalive /
  media tasks ([server lifecycle](gb28181-rs-server.md)).
- `MockFrameHub` is the built-in frame hub (bounded, drop-on-full) for
  wiring the path first; production pipelines implement their own
  `FrameSource` ([live-streaming seam](gb28181-rs-live-streaming.md)).
- Example values use documentation-safe ranges: `192.0.2.x` addresses,
  GB IDs from the `3402000000…` example block.

## Next steps

- [Configuration reference](gb28181-rs-configuration.md): every field and default.
- [Live streaming](gb28181-rs-live-streaming.md) /
  [recording and playback](gb28181-rs-recording-playback.md): media seams.
- [MANSCDP](gb28181-rs-manscdp.md) and [PS mux](gb28181-rs-psmux.md):
  independently usable submodules.
- The repo ships `cargo run --example device_demo`: an in-process fake
  platform running the full interop flow.
