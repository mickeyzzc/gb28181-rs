[中文](README.zh-CN.md) | **English**

# gb28181-rs

[![CI](https://github.com/mickeyzzc/gb28181-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/mickeyzzc/gb28181-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Language: Rust](https://img.shields.io/badge/language-Rust-dea584.svg)
![Tests](https://img.shields.io/badge/tests-107%20passing-brightgreen.svg)

**GB/T 28181-2016/2022 设备端（UAC）Rust 库** —— 让摄像头或媒体源以国标方式注册到 SIP 平台并向其推流。

手写 SIP（不依赖任何 SIP 框架）、MANSCDP XML 编解码、RTP/PS 媒体推送，以及完整的设备服务端：直播、回放、下载。代码从 [mibee-eye-raspi-rs](https://github.com/Mi-Bee-Studio) 的生产实现逐字抽取，经真实国标平台打磨（摘要认证 URI 匹配、Via branch 唯一性、本机 IP 探测、MANSCDP 属性/元素双形式、TCP 传输、回放控制）。

## 功能

- **SIP 信令** —— 手写 GB/T 28181 子集的解析/序列化（REGISTER + 摘要认证、INVITE、MESSAGE、BYE、ACK、OPTIONS），支持 UDP 与 TCP
- **注册生命周期** —— 401 摘要挑战（MD5 + SHA-256，qop=auth）、周期性重注册、保活心跳与超时判定
- **MANSCDP XML** —— Catalog / DeviceInfo / DeviceStatus / RecordInfo / Keepalive，元素与属性双形式，GB2312/GBK/GB18030/UTF-8 编码
- **媒体推送** —— H.264/H.265 NALU → MPEG-2 PS → RTP（UDP + RTP over TCP 封帧），SSRC 处理，大帧有界 PES 分片
- **直播 + 回放 + 下载** —— INVITE 驱动的直播会话；RecordInfo 查询与按帧节奏的回放/下载，SIP INFO 回放控制（播放/暂停/倍速）
- **参考录像段格式** —— 裸 Annex-B H.264 + 每帧 `.ts.jsonl` 时间戳 sidecar（见 [`segment`](src/segment.rs)）

设计上不包含：平台端（UAS）角色、SIP over TLS/WebSocket。

## 使用

```toml
[dependencies]
gb28181-rs = "0.5.0"
# git 替代方式: gb28181-rs = { git = "https://github.com/mickeyzzc/gb28181-rs.git", tag = "v0.5.0" }
```

本 crate 与采集、存储实现解耦，宿主注入两个接缝：

```rust
use gb28181_rs::{FrameSource, FrameSubscription, RecordingSource, SegmentMeta,
                 Gb28181Config, Gb28181Server, set_record_active};

// 1) 直播帧源：在采集管线的帧中心上实现 FrameSource。
struct MyFrameHub { /* ... */ }
impl FrameSource for MyFrameHub {
    fn subscribe_with_capacity(&self, capacity: usize) -> FrameSubscription { /* ... */ }
    fn unsubscribe(&self, id: u64) { /* ... */ }
}

// 2) 录像源：在录像索引上实现 RecordingSource。
struct MyRecordings { /* ... */ }
impl RecordingSource for MyRecordings {
    fn lookup(&self, start_ms: u64, end_ms: u64) -> Vec<SegmentMeta> { /* ... */ }
}

// 3) 启动设备服务端。
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config: Gb28181Config = toml::from_str(&std::fs::read_to_string("config.toml")?)?;
    let server = Gb28181Server::start(
        config,
        std::sync::Arc::new(MyFrameHub { /* ... */ }),
        Some(std::sync::Arc::new(MyRecordings { /* ... */ })),
    ).await?;
    server.await
}
```

本地录像运行期间调用 `set_record_active(true)`，DeviceStatus 会回报 `<Record>ON</Record>`。

测试可使用现成的 [`MockFrameHub`](src/mock.rs)（有界通道、满则丢弃语义的 `FrameSource` 实现）。

## 示例

[`examples/`](examples/) 提供可运行的演示：

```sh
# 离线字节级演示：H.264/H.265 封装为 PS、解析回读、超大帧 PES 分片，
# 确定性输出，无需网络。
cargo run --example ps_mux

# 进程内完整互通演示：手写的假平台（SIP 服务端 + RTP 接收端）注册真实
# 设备服务端、查询目录、INVITE 拉流、把 RTP/PS 还原为 NAL 单元、BYE 挂断
# —— 摘要应答在平台侧重新计算并校验。全部通过后以 0 退出。
cargo run --example device_demo

# 录像回放：RecordInfo 查询、按 PTS 节奏的回放 INVITE（s=Playback、
# t=<起> <止>）、RTP/PS 还原、PlaybackControl 暂停 / 恢复（2 倍速）、BYE
# —— 录像文件为合成的 Annex-B + PTS 边车分片。
cargo run --example playback_demo

# 离线消息层：设备编号格式/解析往返、保活 Notify 构建与解析、
# RecordInfo/DeviceInfo 应答、国标时间串。
cargo run --example manscdp_demo
```

`device_demo` 兼作整机冒烟测试（REGISTER + 401 摘要、目录、INVITE/ACK、RTP/PS 媒体、BYE），无需任何硬件。`playback_demo` 端到端覆盖录像媒体路径（全程断言），`manscdp_demo` 覆盖纯消息层 —— 无 socket、无需硬件。

## 开发

本项目严格执行 **TDD**，见 [CONTRIBUTING.md](CONTRIBUTING.md)。CI 强制 `rustfmt`、`clippy -D warnings`（同时编译 examples）与全量测试（107 个）；`main` 分支受保护（仅 PR 合入，CI 必过）。

## 状态

v0.5.0 —— API 面（`FrameSource`、`RecordingSource`、配置）趋于稳定但尚未冻结。在 [Mi-Bee Studio](https://github.com/Mi-Bee-Studio) 每日对 MiBee NVR 国标平台生产验证。

## 许可

MIT —— 见 [LICENSE](LICENSE)。代码抽取自 Mi-Bee Studio 摄像头项目；互操作修复统一记录在共享 issue 跟踪仓。
