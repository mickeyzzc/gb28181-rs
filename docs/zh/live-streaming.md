# 直播推流：FrameSource 接缝

服务器从不直接对接采集管线。平台 INVITE 点播时，服务器向宿主的
[`FrameSource`] 要一个订阅，把到达的帧推入 RTP/PS 媒体路径。

## 数据模型

帧是不带起始码的纯数据：

```rust
pub struct Nalu {
    pub nalu_type: u8,   // 首字节 & 0x1F
    pub data: Vec<u8>,   // 裸负载，不含 Annex-B 起始码
    pub is_idr: bool,    // type == 5
    pub is_sps: bool,    // type == 7
    pub is_pps: bool,    // type == 8
    pub is_aud: bool,    // type == 9
}

pub struct AccessUnit {
    pub nalus: Vec<Nalu>,
    pub timestamp: Instant,   // 采集时刻——RTP PTS 增量由此推导
    pub is_key_frame: bool,
}
```

## 实现 FrameSource

```rust
use gb28181_rs::{FrameSource, FrameSubscription};

struct MyFrameHub { /* 你的扇出 hub */ }

impl FrameSource for MyFrameHub {
    fn subscribe_with_capacity(&self, capacity: usize) -> FrameSubscription {
        // 发放一个 id + 有界 channel 的接收端
    }
    fn unsubscribe(&self, id: u64) {
        // 移除订阅者并关闭其 channel
    }
}
```

两条语义要求（与服务器抽取自的参考 hub 一致）：

- **有界、写满即丢**——生产者绝不能被慢订阅者阻塞。小容量（2–8）
  是正确的：RTP 推流是实时的，积压没有价值。
- **`unsubscribe` 关闭 channel**——服务器的媒体任务靠 channel 关闭退出。

## INVITE 生命周期

1. 平台发 `INVITE`，SDP 携带（`s=Play`、媒体端口、SSRC、TCP/UDP）。
2. 服务器回 `200 OK` 与 SDP 应答，并在你的 hub 上订阅。
3. 你的管线推访问单元；服务器把每个 AU 封成 MPEG-PS、打包 RTP、
   发往平台媒体端口。
4. PTS 增量由 `AccessUnit::timestamp` 推导（90 kHz 时钟，钳制在合理
   范围）——时间戳不需要你管理。
5. `BYE`（或停机）取消订阅，channel 关闭向下传播。

内置 `MockFrameHub`（有界、写满即丢）供真实管线就绪前跑通通信令。

## 没有平台怎么验证

跑进程内互操作演示——手写假平台注册真实设备服务、查目录、INVITE
点播、把 RTP/PS 解复用回 NAL、BYE，平台侧还校验 digest 应答：

```sh
cargo run --example device_demo
```
