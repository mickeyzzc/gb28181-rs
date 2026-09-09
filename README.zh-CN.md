[中文](README.zh-CN.md) | **English**

# gb28181-rs

[![CI](https://github.com/mickeyzzc/gb28181-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/mickeyzzc/gb28181-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Language: Rust](https://img.shields.io/badge/language-Rust-dea584.svg)
![Tests](https://img.shields.io/badge/tests-133%20passing-brightgreen.svg)

**GB/T 28181-2016/2022 设备端（UAC）Rust 库** —— 让摄像头或媒体源以国标方式注册到 SIP 平台并向其推流。

手写 SIP（不依赖任何 SIP 框架）、MANSCDP XML 编解码、RTP/PS 媒体推送，以及完整的设备服务端：直播、回放、下载。代码从 [mibee-eye-raspi-rs](https://github.com/Mi-Bee-Studio) 的生产实现抽取，经真实国标平台打磨（摘要认证 URI 匹配、Via branch 唯一性、本机 IP 探测、MANSCDP 属性/元素双形式、TCP 传输、回放控制）。

## 功能

- **SIP 信令** —— 手写 GB/T 28181 子集的解析/序列化（REGISTER + 摘要认证、INVITE、MESSAGE、BYE、ACK、OPTIONS），支持 UDP 与 TCP
- **注册生命周期** —— 401 摘要挑战（MD5 + SHA-256，qop=auth）、周期性重注册、保活心跳与超时判定
- **MANSCDP XML** —— Catalog / DeviceInfo / DeviceStatus / RecordInfo / Keepalive，元素与属性双形式；入站报文接受 UTF-8 **或** GB2312/GBK/GB18030，出站声明 GB2312 的报文按声明正确编码
- **媒体推送** —— H.264/H.265 NALU → MPEG-2 PS → RTP（UDP + RTP over TCP 封帧），SSRC 处理，大帧有界 PES 分片；RTP 时间戳取自真实采集时间（任意帧率）
- **语音对讲（接收侧）** —— audio-only INVITE（GB/T 28181-2022 §9.2）：临时端口接收 G.711 A/μ 律 RTP 并交付 `AudioTalkbackSink`（闭包即用）；非 G.711 或未注册 sink 的 offer 以 488 拒绝
- **直播 + 回放 + 下载** —— INVITE 驱动的直播会话；RecordInfo 查询与按帧节奏的回放/下载，SIP INFO 回放控制（播放/暂停/倍速）
- **GB 35114 A 级安全，设备/平台两侧**（可选，`gb35114` feature）—— 设备侧基于 SM2 数字证书的 REGISTER 双向认证（`with_register_authenticator`）+ 平台侧挑战/验签/Note 校验状态机（`security35114::Platform`）；`cryptkey` SM2 DER 信封内的 VKEK 协商、keyed-SM3 `Note` 头完整性；与 Go 孪生库共享 golden 夹具，证明跨实现互通
- **参考录像段格式** —— 裸 Annex-B H.264 + 每帧 `.ts.jsonl` 时间戳 sidecar（见 [`segment`](src/segment.rs)）

设计上不包含：平台端（UAS）角色、SIP over TLS/WebSocket、GB 35114 B/C 级（依赖 GB/T 25724 SVAC 硬件媒体）。

## 使用

```toml
[dependencies]
gb28181-rs = "0.6.0"
# git 替代方式: gb28181-rs = { git = "https://github.com/mickeyzzc/gb28181-rs.git", tag = "v0.6.0" }
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

// 3) 启动设备服务端。停机是优雅的：收发循环、保活任务与进行中的
//    媒体任务都会停止。
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config: Gb28181Config = toml::from_str(&std::fs::read_to_string("config.toml")?)?;
    // 身份标识由宿主配置、默认中性（见下）。
    config.user_agent = Some("my-host/1.0 (gb28181-rs)".to_string());

    let mut server = Gb28181Server::start(
        config,
        std::sync::Arc::new(MyFrameHub { /* ... */ }),
        Some(std::sync::Arc::new(MyRecordings { /* ... */ })),
    ).await?;
    // ... 运行你的应用；退出时：
    server.shutdown().await
}
```

本地录像运行期间调用 `set_record_active(true)`，DeviceStatus 会回报 `<Record>ON</Record>`。

本 crate 通过标准 [`log`](https://crates.io/crates/log) facade 输出日志 —— 宿主需初始化一个 logger（`env_logger`、`tracing` 等）才能看到输出；不初始化则库保持静默。

测试可使用现成的 [`MockFrameHub`](src/mock.rs)（有界通道、满则丢弃语义的 `FrameSource` 实现）。

### 配置

`Gb28181Config` 对 serde 友好（TOML/JSON），可直接重导出到宿主自己的配置结构体。连接类默认值沿用国标示例值（`platform_sip_address = 192.168.1.1`、`device_id = 34020000001320000001`、`password = 12345678`、端口 5060）—— **生产环境务必显式设置**；示例默认值仍生效时，服务端启动会输出警告日志。

身份字段（均可选、默认中性 —— 本库绝不在线上协议中替你宣传任何产品/厂商名）：

| 字段 | 默认值 | 用途 |
|---|---|---|
| `user_agent` | `gb28181-rs/<版本>` | REGISTER 的 SIP `User-Agent` |
| `device_name` | `Camera <device_id>` | Catalog/DeviceInfo 的 `Name` |
| `manufacturer` | `Unknown` | Catalog/DeviceInfo 的 `Manufacturer` |
| `model` | `Unknown` | Catalog/DeviceInfo 的 `Model` |
| `firmware` | crate 版本号 | DeviceInfo 的 `Firmware` |

`enabled` 是宿主侧开关 —— 本库从不读取它，由宿主决定是否调用 `start()`。

## GB35114 A 级安全（v0.8.0 设备侧 / v0.9.0 平台侧，可选）

[GB 35114-2017](https://openstd.samr.gov.cn/bzgk/std/newGbInfo?hcno=B7F5589329EF98B32F0EB8ACEC341C81) 在 GB/T 28181 之上叠加基于 SM2 数字证书的安全层。本库只实现 **A级** —— B/C 级额外依赖 SVAC 媒体（GB/T 25724，硬件编解码器），设计上不在范围内。通过 `gb35114` feature 开启（需要 Rust ≥ 1.85；不开 feature 时 crate 的 MSRV 仍为 1.80）：

```toml
[dependencies]
gb28181-rs = { version = "0.9", features = ["gb35114"] }
```

```rust
use gb28181_rs::authenticator::RegisterAuthenticator as _;
use gb28181_rs::security35114::{load_certificate, load_identity, Authenticator, Options};

let dev = load_identity(&cert_pem, &key_pem)?;          // SM2 SEC1 或 PKCS#8
let platform = load_certificate(&platform_cert_pem)?;   // 用于验签 sign2
let auth = Authenticator::new(Options::new(dev, device_id, server_id))?;

let server = Gb28181Server::with_recording_index(cfg, hub, None)
    .with_register_authenticator(Some(Arc::new(auth)))  // 取代摘要认证
    .spawn()
    .await?;
```

握手流程按标准文本并以真实抓包交叉校准：`Capability` 能力宣告 → 401 携带 `random1` → 带 `sign1` 的重注册（SM2 签名 random2‖random1‖serverID）→ 200 OK `SecurityInfo` 携带 SM2 封装的 VKEK（`cryptkey`，DER C1‖C3‖C2 信封）；`Bidirection` 模式另含平台 `sign2`。注册成功后，所有外出请求（保活等）携带以 VKEK 为密钥的 `Date` + `Note: Digest nonce="…",algorithm=SM3`。密码学来自 RustCrypto 的 `sm2`/`sm3` crate（纯 Rust，`aarch64-musl` 交叉编译友好）；golden 测试与 Go 孪生库（`gb28181-go/security35114`）共享证书、向量与互通夹具——包括 gmsm 产出的签名/信封样本必须在本库验签、解封成功。

两处跨实现歧义点做成可配置项（[`RandomEncoding`](src/security35114/mod.rs)、[`Sign2Order`](src/security35114/mod.rs)）：签名负载中随机数的表示形式、`sign2` 的 R1/R2 操作数顺序。默认值遵循标准文本。下行 `Note` 校验：握手完成后，平台发往设备的带 `Note` 请求会在设备侧用 VKEK 验签并检查 `Date` ±5 分钟新鲜度窗口——签名不符默认回 403（`Gb28181Config::incoming_note_policy` 可选 `warn`/`off` 便于灰度），无 `Note` 的请求照旧放行（兼容混跑的 Digest 平台）。

### 平台侧（UAS，v0.9.0）

`security35114::Platform` 是面向 GB/T 28181 平台的镜像状态机，与 Go 孪生库的 `security35114.Platform` **API 对等、线格式互通**（Go 建平台 × Rust 设备、Rust 平台 × Go 设备均已实机完成 Bidirection 握手）。它下发 `Bidirection`/`Unidirection` 挑战、用预置（或 `cnonce` 宣告的）设备证书验签 `sign1`、封装 VKEK、签名 `sign2`，并校验后续所有 `Note`。本 crate 不含 UAS SIP 服务器（纯设备/UAC 角色）——请接入任意平台协议栈：

```rust
use gb28181_rs::security35114::{Platform, PlatformConfig};

let mut cfg = PlatformConfig::new(server_id);             // 20 位平台 ID
cfg.identity = Some(platform_identity);                   // 平台 SM2 签名身份 —— sign2
cfg.device_certs.insert(device_id.to_string(), dev_cert); // 或信任 cnonce 宣告
let platform = Platform::new(cfg)?;

let www_auth = platform.challenge(&device_id, &capability_authorization)?; // 401 值
let security_info = platform.verify_register(&device_id, &authorization)?; // 200 OK 值
platform.verify_note(&device_id, &note, "MESSAGE", &from, &to, &call_id, &date, &body)?;
```

`Platform` 并发安全，会话按设备 ID 索引；设备重注册期间旧 VKEK 继续可验；对 SIP-over-UDP 重传的已完成 REGISTER 幂等返回同一 `SecurityInfo`；对过期 `random1`、方案错配、未知/不匹配证书、外来 server ID 以 [`PlatformError`](src/security35114/platform.rs) 哨兵枚举拒绝（在 `anyhow` 链上 `err.downcast_ref::<PlatformError>()`），便于上层映射 4xx。模块内回环测试用真实设备侧 `Authenticator` 驱动——握手、双方 VKEK 一致、`Note` 篡改检测。

### GB28181-2022 抓拍线格式类型（issue #49 孪生对齐）

`manscdp` 解析入站 `DeviceControl` 抓拍命令（`parse_control_snapshot`：`SnapShot` 携带 `SnapNum`/`Interval`/`UploadURL`/`SessionID`，A.2.1.24），并构造设备侧 `UploadSnapShotFinished` 完成上报（`build_upload_snapshot_finished`，A.2.5.7）——golden 与 Go 孪生库逐字节一致。

## 文档

专题教程在 [`docs/zh/`](docs/zh/) —— 每篇在 `docs/en/` 下有英文对照版：

| 教程 | 内容 |
|---|---|
| [配置](docs/zh/configuration.md) | `Gb28181Config` 全字段、身份默认值、示例值警告、设备 ID 结构 |
| [直播推流](docs/zh/live-streaming.md) | `FrameSource` 接缝、`Nalu`/`AccessUnit` 形态、INVITE 生命周期、PTS 推导 |
| [录像回放](docs/zh/recording-playback.md) | `RecordingSource`、`SegmentMeta`、参考录像段格式、RecordInfo/回放/下载/回放控制 |
| [PS 封装与 RTP](docs/zh/psmux.md) | 独立使用 `mux_h264_to_ps`/`mux_h265_to_ps`、解析、`RtpPusher`、字节级金串保证 |
| [消息与字符集](docs/zh/manscdp.md) | 消息类型、元素/属性双形态、UTF-8/GB18030 线上字符集、设备 ID |
| [服务器生命周期](docs/zh/server.md) | 构造与 bind、UDP/TCP 传输、优雅停机、重试退避、日志 |
| [日志与 tracing](docs/zh/logging.md) | `log` facade 决策、tracing 桥接、发出的级别 |

## 库卫生（v0.6.0 加固）

v0.6.0 把本 crate 打磨为可放心嵌入的中性基础库。[`tests/library_hygiene.rs`](tests/library_hygiene.rs) 中的回归测试逐条锁定以下保证：

- **消费方不可触达 panic** —— 构造函数不做 I/O、绝不 panic；`format_device_id` 返回 `Result`。
- **无硬编码 SIP 端口** —— REGISTER/BYE 的 Via 与 Contact 宣告配置的 `local_sip_port`（此前写死 5060）。
- **字符集正确的 MANSCDP** —— 入站 GB2312/GBK/GB18030 报文正常解码（此前被直接丢弃）；出站声明 GB2312 的报文按声明编码（ASCII 输出与旧版逐字节一致，wire golden 契约不变）。
- **XML 转义** —— 宿主提供的字符串（名称、路径）插值前转义。
- **随机且唯一的 SIP 标识** —— Via branch、From/To tag、摘要 `cnonce` 均来自 CSPRNG，不再用计数器/时钟推导。
- **`log` facade** —— 库代码零 `println!`/`eprintln!`。
- **优雅停机** —— `ServerHandle::shutdown()` 停止运行循环、保活与媒体任务。
- **库代码零品牌、零实验室地址**（配置示例默认值除外，启动时告警）。

### 0.5.x → 0.6.0 破坏性变更

- `Gb28181Server::start` 返回 `ServerHandle`（原来是 `JoinHandle<()>`）；`handle.await` 依然可用，新增 `handle.shutdown().await`。
- `build_register_request` / `build_bye_request` 增加 `local_port` 参数（REGISTER 另有 `from_tag`/`user_agent`）。
- `format_device_id` 返回 `Result<String>`（原来遇错 panic）。
- Catalog/DeviceInfo 身份默认值由厂商字符串改为中性值 —— 需要原值请设置上述配置字段。
- 库输出从 stdout/stderr 迁移到 `log` facade。

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

本项目严格执行 **TDD**，见 [CONTRIBUTING.md](CONTRIBUTING.md)。CI 强制 `rustfmt`、`clippy -D warnings`（同时编译 examples）与全量测试（133 个）；`main` 分支受保护（仅 PR 合入，CI 必过）。

## 状态

v0.6.0 —— API 面（`FrameSource`、`RecordingSource`、配置）趋于稳定但尚未冻结。在 [Mi-Bee Studio](https://github.com/Mi-Bee-Studio) 每日对 MiBee NVR 国标平台生产验证。

## 许可

MIT —— 见 [LICENSE](LICENSE)。代码抽取自 Mi-Bee Studio 摄像头项目；互操作修复统一记录在共享 issue 跟踪仓。
