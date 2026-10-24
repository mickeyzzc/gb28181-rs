# 快速开始：最小可编译设备

`gb28181-rs` 是 Rust 的 GB/T 28181 设备侧实现。本页用一个约三十行的
示例把设备服务器跑起来：注册、心跳、点播推流的骨架一次到位。

## 安装

```bash
cargo add gb28181-rs@0.12.0
```

示例还需要一个异步运行时（服务器是 async 的）：

```toml
[dependencies]
gb28181-rs = "=0.12.0"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

## 最小示例

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

要点：

- `Gb28181Server::new` 只存配置、不做 I/O；`spawn()` 才绑定 SIP 端口并
  启动注册 / 保活 / 媒体任务（[服务器生命周期](gb28181-rs-server.md)）。
- `MockFrameHub` 是内置的帧 hub（有界、写满即丢），用于把链路先跑通；
  生产管线实现自己的 `FrameSource`（[直播推流接缝](gb28181-rs-live-streaming.md)）。
- 示例值均为文档保留段：`192.0.2.x` 地址、`3402000000…` 段国标 ID。

## 下一步

- [配置参考](gb28181-rs-configuration.md)：全字段与默认值。
- [直播推流](gb28181-rs-live-streaming.md) / [录像与回放](gb28181-rs-recording-playback.md)：媒体面接缝。
- [MANSCDP](gb28181-rs-manscdp.md)与[PS 封装](gb28181-rs-psmux.md)：可独立使用的子模块。
- 仓库内置 `cargo run --example device_demo`：进程内假平台全流程互操作演示。
