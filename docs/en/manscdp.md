# MANSCDP messages, charsets, and device IDs

The MESSAGE layer (MANSCDP-XML: catalog queries, keepalives, device
info/status, record queries) is exported as plain data types plus codec
helpers, so hosts can build tooling on it without running a server.

## Message types

```rust
use gb28181_rs::{Notify, Query, Response, DeviceList, DeviceItem, ChannelItem};
```

- `Notify` — device→platform notifications (keepalive, alarm push).
- `Query` — platform→device queries (catalog, device info, record info).
- `Response` — device→platform answers.
- `DeviceList` / `DeviceItem` / `ChannelItem` — catalog payloads.

The server constructs and answers these internally; the types are
re-exported for hosts that decode/encode MESSAGE bodies themselves
(archive tooling, test harnesses, proxies). The XML codec emits both the
element form and the attribute form that real platforms demand.

## Wire charsets

MANSCDP bodies on the wire are UTF-8 **or** GB2312/GBK/GB18030 — real
platforms send both. The codec helpers handle detection and encoding:

```rust
use gb28181_rs::charset::{decode_wire_body, encode_wire_body};

// Inbound: strict UTF-8 first, GB18030 as fallback — never lossy.
let text = decode_wire_body(&raw_message_bytes);

// Outbound: ASCII passes through byte-identically (golden-stable);
// non-ASCII Chinese text is encoded as GB18030 per GB/T 28181 practice.
let bytes = encode_wire_body("前门摄像头");
```

## Device IDs

20-digit national-standard IDs:
`[8-digit region (GB/T 2260)][2-digit industry][3-digit type][7-digit serial]`.

```rust
use gb28181_rs::{format_device_id, parse_device_id, device_types, DeviceIdParts};

let id = format_device_id("34020000", 0, device_types::IPC, 42)?;
let DeviceIdParts { region_code, industry_type, device_type, serial } = parse_device_id(&id)?;
```

Type constants: `IPC` = 111, `NVR` = 118, `DECODER` = 121, `ALARM` =
122, `AUDIO` = 134. Both functions return errors on malformed input —
they never panic.

## Time handling

GB/T 28181 record queries use Beijing-local time strings. The library
converts using the host's configured local offset
(`manscdp::device_local_offset_secs()`); hosts running in UTC should
ensure their platform expects UTC or configure the offset accordingly.

## Verifying offline

```sh
cargo run --example manscdp_demo
# Device-ID round-trips, keepalive build+parse, RecordInfo/DeviceInfo
# responses, GB/T 28181 time strings — no network involved.
```
