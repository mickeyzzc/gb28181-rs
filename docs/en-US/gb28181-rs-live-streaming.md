# Live streaming: the FrameSource seam

The server never talks to a capture pipeline directly. When the platform
INVITEs a live stream, the server asks the host's [`FrameSource`] for a
subscription and pushes whatever arrives into the RTP/PS media path.

## Install

```bash
cargo add gb28181-rs@0.12.0
```

## Data model

Frames are plain data with no start codes:

```rust
pub struct Nalu {
    pub nalu_type: u8,   // first byte & 0x1F
    pub data: Vec<u8>,   // payload WITHOUT Annex-B start code
    pub is_idr: bool,    // type == 5
    pub is_sps: bool,    // type == 7
    pub is_pps: bool,    // type == 8
    pub is_aud: bool,    // type == 9
}

pub struct AccessUnit {
    pub nalus: Vec<Nalu>,
    pub timestamp: Instant,   // capture time — drives RTP PTS deltas
    pub is_key_frame: bool,
}
```

## Implementing FrameSource

```rust
use gb28181_rs::{FrameSource, FrameSubscription};

struct MyFrameHub { /* your fan-out hub */ }

impl FrameSource for MyFrameHub {
    fn subscribe_with_capacity(&self, capacity: usize) -> FrameSubscription {
        // hand out an id + the receiving end of a bounded channel
    }
    fn unsubscribe(&self, id: u64) {
        // remove the subscriber and close its channel
    }
}
```

The two semantic requirements (matching the reference hub the server was
extracted from):

- **Bounded, drop-on-full** — the producer must never block on a slow
  subscriber. Small capacities (2–8) are correct: the RTP pacer is
  real-time, backlog is worthless.
- **`unsubscribe` closes the channel** — the server's media task exits on
  channel close.

## The INVITE lifecycle

1. Platform sends `INVITE` with SDP (`s=Play`, media port, SSRC, TCP/UDP).
2. Server responds `200 OK` with its SDP answer, subscribes on your hub.
3. Your pipeline pushes access units; the server muxes each AU to MPEG-PS,
   packetizes to RTP, and sends to the platform's media port.
4. PTS deltas are derived from `AccessUnit::timestamp` (90 kHz clock,
   clamped to a sane range) — you do not manage timestamps.
5. `BYE` (or shutdown) unsubscribes; your channel close propagates.

```mermaid
sequenceDiagram
    participant P as Platform
    participant S as Gb28181Server
    participant H as FrameSource (host)
    P->>S: INVITE (SDP: s=Play, media port, SSRC)
    S-->>P: 200 OK (SDP answer)
    S->>H: subscribe_with_capacity() (bounded channel)
    loop while streaming
        H->>S: AccessUnit (NAL without start codes + capture time)
        S->>P: RTP/PS (MPEG-PS mux, 90 kHz PTS)
    end
    P->>S: BYE
    S->>H: unsubscribe(id) (closes the channel, media task exits)
    S-->>P: 200 OK
```

A ready-made `MockFrameHub` (bounded, drop-on-full) exists for wiring
tests and demos before a real pipeline exists.

## Verifying without a platform

Run the in-process interop demo — a hand-written fake platform registers
the real device server, queries the catalog, INVITEs a stream, demuxes
RTP/PS back to NAL units, and BYEs, with digest verified on the platform
side:

```sh
cargo run --example device_demo
```
