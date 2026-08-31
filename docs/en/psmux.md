# PS muxing and RTP, standalone

The media path is usable without the SIP server: mux H.264/H.265 NAL
units into MPEG-2 Program Stream, packetize to RTP, or parse a PS stream
back into NAL units.

## Mux

```rust
use gb28181_rs::{mux_h264_to_ps, mux_h265_to_ps};

// NALU payloads WITHOUT Annex-B start codes; PTS/DTS on the 90 kHz clock.
let ps: Vec<u8> = mux_h264_to_ps(&[&sps, &pps, &idr], true, 90_000, 90_000);
let ps265: Vec<u8> = mux_h265_to_ps(&[&vps, &sps, &pps, &idr], true, pts, dts);
```

Key frames emit the full pack header + program stream map (PSM) preamble
that GB28181 demuxers expect before the PES payload; non-key frames
carry the pack header + PES only. Access units larger than the PES
length field's sane range are split into multiple bounded PES packets.

## Parse

```rust
use gb28181_rs::{parse_ps_to_h264, parse_ps_to_nal_units, parse_ps_pack_header, parse_pes_packet};

let frames: Vec<Vec<u8>> = parse_ps_to_h264(&ps)?;   // Annex-B frames (start codes restored)
let nalus: Vec<Vec<u8>> = parse_ps_to_nal_units(&ps)?; // raw NAL payloads
```

## RTP packetization

```rust
use std::net::SocketAddr;
use gb28181_rs::RtpPusher;

let mut pusher = RtpPusher::new(
    SocketAddr::from(([192, 0, 2, 10], 30000)), // platform media port
    0x1234_5678,                                // SSRC from the INVITE's SDP
    96,                                         // dynamic payload type (PS)
);
let packet = pusher.build_rtp_packet(&ps);      // fragments oversized PS automatically
pusher.increment_timestamp(3600);               // advance the RTP clock yourself between AUs
```

## Byte-level guarantees

PS output is a **wire contract**: this crate and its Go twin
(`gb28181-go`) must produce byte-identical streams. Golden tests pin
hex constants for a small keyframe, a P frame, and a >64 KB access unit
split into four PES packets. If you change any muxing path in either
repo, those tests go red first.

```sh
cargo run --example ps_mux
# Offline: mux → parse-back round-trip and oversized-frame splitting.
```
