# Configuration reference

`Gb28181Config` is a plain serde struct — TOML, JSON, or programmatic
construction all work. The TOML shape is identical to the `[gb28181]`
section used by the library's origin project, so hosts can re-export the
struct directly into their own config files.

## Full field reference

```rust
use gb28181_rs::config::Transport;

let mut config = Gb28181Config::default();
config.platform_sip_address = "192.0.2.10".to_string();
config.platform_sip_port = 5060;
config.device_id = "34020000001320000001".to_string();
config.password = "secret".to_string();
config.transport = Transport::Tcp;
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | `bool` | `false` | **Host-side switch only.** The library never reads it; the host gates `Gb28181Server::spawn()` on it. |
| `platform_sip_address` | `String` | `"192.168.1.1"` | SIP server (platform) address. |
| `platform_sip_port` | `u16` | `5060` | SIP server (platform) port. |
| `device_id` | `String` | `"34020000001320000001"` | 20-digit GB/T 28181 device ID (spec-example default). |
| `channel_id` | `String` | `"34020000001320000001"` | 20-digit channel ID reported in the catalog. |
| `sip_domain` | `String` | `"3402000000"` | SIP domain (usually the platform's center code). |
| `password` | `String` | `"12345678"` | SIP digest-auth password shared with the platform. |
| `local_sip_port` | `u16` | `5060` | Local SIP listening port (UDP socket or TCP listener). |
| `register_interval_secs` | `u64` | `60` | REGISTER refresh interval. |
| `heartbeat_interval_secs` | `u64` | `60` | Keepalive (MESSAGE) interval. |
| `heartbeat_timeout_count` | `u32` | `3` | Missed keepalives before the platform declares timeout. |
| `transport` | `Transport` | `Udp` | `udp` or `tcp` (serde is lowercase; unknown values are parse errors). |
| `user_agent` | `Option<String>` | `None` | SIP `User-Agent`; `None` → neutral `gb28181-rs/<version>`. |
| `device_name` | `Option<String>` | `None` | Catalog/DeviceInfo name; `None` → `Camera <device_id>`. |
| `manufacturer` | `Option<String>` | `None` | `None` → `Unknown`. |
| `model` | `Option<String>` | `None` | `None` → `Unknown`. |
| `firmware` | `Option<String>` | `None` | `None` → the crate version. |

An equivalent TOML section:

```toml
[gb28181]
enabled = true
platform_sip_address = "192.0.2.10"
platform_sip_port = 5060
device_id = "34020000001320000001"
channel_id = "34020000001310000001"
sip_domain = "3402000000"
password = "secret"
local_sip_port = 5060
transport = "udp"
user_agent = "my-host/1.0 (gb28181-rs)"
device_name = "Front gate camera"
manufacturer = "Acme"
model = "Cam-X"
firmware = "1.2.3"
```

## Identity fields and neutrality

The five identity fields never leak a product or vendor name by default —
this is pinned by the library-hygiene test suite. Resolve the effective
value with the `effective_*()` methods:

```rust
let cfg = Gb28181Config::default();
assert_eq!(cfg.effective_manufacturer(), "Unknown");
assert!(cfg.effective_user_agent().starts_with("gb28181-rs/"));
assert_eq!(cfg.effective_device_name(), format!("Camera {}", cfg.device_id));
```

`Default::default()` and an empty TOML section produce identical values
(pinned by test), so programmatic and file-driven hosts see the same
defaults.

## The example-default warning

The spec-documentation defaults (`192.168.1.1`, `12345678`, the example
device ID) are kept for wire-format stability with existing host configs.
If a running server still sees them, `warn_on_example_defaults()` logs a
warning — the server calls it automatically after binding. Two devices
sharing the spec-example ID collide on the platform; a mis-loaded config
silently targeting the example platform is the other failure this
catches.

## Device IDs

`device_id` and `channel_id` are 20-digit codes:

```
[8-digit region (GB/T 2260)][2-digit industry][3-digit type][7-digit serial]
```

Build and inspect them programmatically:

```rust
use gb28181_rs::{format_device_id, parse_device_id, device_types};

let id = format_device_id("34020000", 0, device_types::IPC, 42)?; // 34020000001320000042
let parts = parse_device_id(&id)?; // region_code, industry_type, device_type, serial
```

Known type constants: `IPC` (111), `NVR` (118), `DECODER` (121),
`ALARM` (122), `AUDIO` (134).
