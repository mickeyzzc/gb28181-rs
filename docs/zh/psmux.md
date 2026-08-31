# PS 封装与 RTP：独立使用

媒体路径可以脱离 SIP 服务器单独使用：把 H.264/H.265 NAL 封成
MPEG-2 节目流、打包 RTP，或把 PS 流解析回 NAL。

## 封装

```rust
use gb28181_rs::{mux_h264_to_ps, mux_h265_to_ps};

// NAL 负载不含 Annex-B 起始码；PTS/DTS 为 90 kHz 时钟值。
let ps: Vec<u8> = mux_h264_to_ps(&[&sps, &pps, &idr], true, 90_000, 90_000);
let ps265: Vec<u8> = mux_h265_to_ps(&[&vps, &sps, &pps, &idr], true, pts, dts);
```

关键帧输出 GB28181 解复用器要求的完整 pack header + 节目流映射
（PSM）前导，再接 PES 负载；非关键帧只有 pack header + PES。超出 PES
长度字段合理范围的访问单元会切成多个有界 PES 包。

## 解析

```rust
use gb28181_rs::{parse_ps_to_h264, parse_ps_to_nal_units, parse_ps_pack_header, parse_pes_packet};

let frames: Vec<Vec<u8>> = parse_ps_to_h264(&ps)?;     // Annex-B 帧（起始码已还原）
let nalus: Vec<Vec<u8>> = parse_ps_to_nal_units(&ps)?; // 裸 NAL 负载
```

## RTP 打包

```rust
use std::net::SocketAddr;
use gb28181_rs::RtpPusher;

let mut pusher = RtpPusher::new(
    SocketAddr::from(([192, 0, 2, 10], 30000)), // 平台媒体端口
    0x1234_5678,                                // INVITE SDP 里的 SSRC
    96,                                         // 动态负载类型（PS）
);
let packet = pusher.build_rtp_packet(&ps);      // 超长 PS 自动分片
pusher.increment_timestamp(3600);               // 帧间自行推进 RTP 时钟
```

## 字节级保证

PS 输出是**线上契约**：本库与 Go 孪生库（`gb28181-go`）必须产出逐字
节相同的流。金串测试把小关键帧、P 帧、>64 KB 访问单元四段 PES 切分
都钉成十六进制常量。无论改哪个库的封装路径，这些测试都会先红。

```sh
cargo run --example ps_mux
# 离线：封装 → 解回往返与超大帧切分。
```
