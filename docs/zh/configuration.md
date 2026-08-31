# 配置参考

`Gb28181Config` 是普通 serde 结构体——TOML、JSON、代码构造都可以。
TOML 形态与库的出身项目的 `[gb28181]` 配置节完全一致，宿主可以把
结构体直接 re-export 进自己的配置文件体系。

## 全字段参考

```rust
use gb28181_rs::config::Transport;

let mut config = Gb28181Config::default();
config.platform_sip_address = "192.0.2.10".to_string();
config.platform_sip_port = 5060;
config.device_id = "34020000001320000001".to_string();
config.password = "secret".to_string();
config.transport = Transport::Tcp;
```

| 字段 | 类型 | 默认值 | 含义 |
|---|---|---|---|
| `enabled` | `bool` | `false` | **仅宿主侧开关**。库自身从不读取；宿主用它决定是否调用 `Gb28181Server::spawn()`。 |
| `platform_sip_address` | `String` | `"192.168.1.1"` | SIP 服务器（平台）地址。 |
| `platform_sip_port` | `u16` | `5060` | SIP 服务器（平台）端口。 |
| `device_id` | `String` | `"34020000001320000001"` | 20 位国标设备 ID（规范文档示例值）。 |
| `channel_id` | `String` | `"34020000001320000001"` | 目录中上报的 20 位通道 ID。 |
| `sip_domain` | `String` | `"3402000000"` | SIP 域（通常是平台中心编码）。 |
| `password` | `String` | `"12345678"` | 与平台约定的 SIP 摘要认证密码。 |
| `local_sip_port` | `u16` | `5060` | 本地 SIP 监听端口（UDP socket 或 TCP listener）。 |
| `register_interval_secs` | `u64` | `60` | REGISTER 刷新间隔。 |
| `heartbeat_interval_secs` | `u64` | `60` | 保活（MESSAGE）间隔。 |
| `heartbeat_timeout_count` | `u32` | `3` | 平台侧判定离线的连续丢保活次数。 |
| `transport` | `Transport` | `Udp` | `udp` 或 `tcp`（serde 小写；未知值直接解析报错）。 |
| `user_agent` | `Option<String>` | `None` | SIP `User-Agent`；`None` → 中性 `gb28181-rs/<版本>`。 |
| `device_name` | `Option<String>` | `None` | 目录/DeviceInfo 名称；`None` → `Camera <设备ID>`。 |
| `manufacturer` | `Option<String>` | `None` | `None` → `Unknown`。 |
| `model` | `Option<String>` | `None` | `None` → `Unknown`。 |
| `firmware` | `Option<String>` | `None` | `None` → crate 版本号。 |

等价的 TOML 配置节：

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
device_name = "前门摄像头"
manufacturer = "Acme"
model = "Cam-X"
firmware = "1.2.3"
```

## 身份字段与中性默认

五个身份字段的默认值**不携带任何产品/厂商名**——这一点由库卫生测试
钉死。用 `effective_*()` 方法取生效值：

```rust
let cfg = Gb28181Config::default();
assert_eq!(cfg.effective_manufacturer(), "Unknown");
assert!(cfg.effective_user_agent().starts_with("gb28181-rs/"));
assert_eq!(cfg.effective_device_name(), format!("Camera {}", cfg.device_id));
```

`Default::default()` 与空 TOML 节产出完全相同的值（有测试钉死），
代码构造与文件驱动的宿主看到同一套默认。

## 示例默认值警告

规范文档的示例值（`192.168.1.1`、`12345678`、示例设备 ID）为了线上
报文格式稳定而保留。如果运行中的服务器仍看到它们，
`warn_on_example_defaults()` 会打警告——服务器在 bind 之后自动调用。
两台设备共用示例 ID 会在平台上撞号；配置没加载成功而静默指向示例
平台，是它拦的另一类事故。

## 设备 ID

`device_id` 与 `channel_id` 是 20 位编码：

```
[8 位行政区划（GB/T 2260）][2 位行业][3 位类型][7 位序号]
```

代码里构造与解析：

```rust
use gb28181_rs::{format_device_id, parse_device_id, device_types};

let id = format_device_id("34020000", 0, device_types::IPC, 42)?; // 34020000001320000042
let parts = parse_device_id(&id)?; // region_code, industry_type, device_type, serial
```

已知类型常量：`IPC`（111）、`NVR`（118）、`DECODER`（121）、
`ALARM`（122）、`AUDIO`（134）。
