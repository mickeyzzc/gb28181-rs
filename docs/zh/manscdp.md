# MANSCDP 消息、字符集与设备 ID

MESSAGE 层（MANSCDP-XML：目录查询、保活、设备信息/状态、录像查询）
以纯数据类型 + 编解码辅助函数的形式导出，宿主可以不跑服务器、在它
之上构建自己的工具。

## 消息类型

```rust
use gb28181_rs::{Notify, Query, Response, DeviceList, DeviceItem, ChannelItem};
```

- `Notify`——设备→平台通知（保活、告警上报）。
- `Query`——平台→设备查询（目录、设备信息、录像信息）。
- `Response`——设备→平台应答。
- `DeviceList` / `DeviceItem` / `ChannelItem`——目录负载。

服务器内部负责构造与应答这些消息；类型 re-export 出来，供需要自行
编解码 MESSAGE 体的宿主使用（归档工具、测试台、代理）。XML 编解码
同时支持真机平台要求的元素式与属性式双形态。

## 线上字符集

线上 MANSCDP 体是 UTF-8 **或** GB2312/GBK/GB18030——真实平台两种都
发。编解码辅助函数负责识别与编码：

```rust
use gb28181_rs::charset::{decode_wire_body, encode_wire_body};

// 入站：先严格 UTF-8，GB18030 兜底——永不丢字。
let text = decode_wire_body(&raw_message_bytes);

// 出站：ASCII 逐字节直通（保证金串稳定）；非 ASCII 中文按 GB/T 28181
// 惯例编码为 GB18030。
let bytes = encode_wire_body("前门摄像头");
```

## 设备 ID

20 位国标编码：
`[8 位行政区划（GB/T 2260）][2 位行业][3 位类型][7 位序号]`。

```rust
use gb28181_rs::{format_device_id, parse_device_id, device_types, DeviceIdParts};

let id = format_device_id("34020000", 0, device_types::IPC, 42)?;
let DeviceIdParts { region_code, industry_type, device_type, serial } = parse_device_id(&id)?;
```

类型常量：`IPC` = 111、`NVR` = 118、`DECODER` = 121、`ALARM` = 122、
`AUDIO` = 134。两个函数对畸形输入返回错误——绝不 panic。

## 时间处理

GB/T 28181 录像查询用北京本地时间字符串。库按宿主配置的本地偏移
（`manscdp::device_local_offset_secs()`）换算；以 UTC 运行的宿主应确认
平台接受 UTC，或相应配置偏移。

## 离线验证

```sh
cargo run --example manscdp_demo
# 设备 ID 往返、保活构造+解析、RecordInfo/DeviceInfo 应答、
# GB/T 28181 时间串——全程无网络。
```
