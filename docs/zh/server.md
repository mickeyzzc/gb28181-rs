# 服务器生命周期：启动、停机与传输层

## 构造不做 I/O

构造函数只存配置——不开 socket、不 panic：

```rust
use std::sync::Arc;
use gb28181_rs::{Gb28181Config, Gb28181Server, MockFrameHub};

let server = Gb28181Server::new(config, Arc::new(MockFrameHub::new()));
let server = Gb28181Server::with_recording_index(
    config, au_hub, Some(recording_index),
);
```

bind 发生在 `spawn()`：

```rust
// 实例方法形态:
let mut handle = server.spawn().await?;

// 关联函数形态（等价）:
let mut handle = Gb28181Server::start(config, au_hub, None).await?;
```

`spawn()` 按 `config.transport` 分流：
- **UDP**（默认）——bind `0.0.0.0:<local_sip_port>` UDP。
- **TCP**——同端口起 TCP listener，逐连接处理带帧 SIP。

## 服务器在跑什么

- REGISTER 生命周期：digest 认证（MD5 与 SHA-256、`qop=auth`、
  CSPRNG cnonce）、到期重注册、失败重试与退避。
- 按 `heartbeat_interval_secs` 发保活（MESSAGE）。
- 目录 / 设备信息 / 设备状态应答。
- INVITE 驱动的直播、回放、下载（见
  [直播推流](live-streaming.md)与
  [录像回放](recording-playback.md)两篇）。

注册重试与退避对停机敏感：请求停机时挂起的退避立即中止。

## 优雅停机

`ServerHandle` 提供两种停止：

```rust
handle.shutdown().await; // 优雅：收发循环停止、在途媒体任务收尾、清理执行
handle.abort();           // 立即：tokio 任务 abort（最后手段）
```

handle 也实现了 `Future`（等待服务器自行退出），典型监管方：

```rust
tokio::select! {
    _ = tokio::signal::ctrl_c() => handle.shutdown().await?,
    _ = &mut handle => { /* 服务器自己退出了 */ }
}
```

停机有集成测试覆盖：UDP 与 TCP 服务器都能及时停止收发循环，生命
周期测试断言构造函数对合法输入永不 panic。

## 日志

本库只经 [`log`] 门面输出——宿主初始化一个 logger（`env_logger`、
`tracing`……）即可看到注册、INVITE、媒体事件。不直接写 stdout/stderr。

[`log`]: https://docs.rs/log

## 语音对讲（audio-only INVITE，接收侧）

GB/T 28181-2022 §9.2 语音对讲：平台向设备发起 **audio-only INVITE**
（只有 `m=audio`、没有 `m=video`），随后把 G.711 RTP 流推给设备。0.7.0
起本库实现接收侧：

```rust
use std::sync::Arc;
use gb28181_rs::{Gb28181Config, Gb28181Server};

// 闭包 Fn(&[u8], u32) 开箱即用（有 blanket impl）：
let sink = |payload: &[u8], ssrc: u32| {
    // payload = 一个 RTP 包的 G.711 字节（A 律 payload type 8，
    // μ 律 payload type 0）。拷贝进通道交给音频输出线程——
    // 这里跑在媒体任务上，必须轻量。
};

let server = Gb28181Server::new(config, hub)
    .with_audio_sink(Arc::new(sink));
```

行为细节：

- offer 解析：audio-only offer 会让 `parse_invite` 给出
  `media_kind: MediaKind::Audio` 与 `audio_codec`
  （`AudioCodec::Pcma`/`Pcmu`）；带 `m=video` 的混合 offer 走原视频推流路径。
- 应答通告一个临时 UDP 接收端口，回显 offer 的 payload type
  （`a=rtpmap:8 PCMA/8000` / `0 PCMU/8000`），`y=` 行带会话 SSRC——
  字节级稳定，golden 测试约束。
- 收到的每个 RTP 包去掉 12 字节固定头（含 CSRC 列表）后，负载连同
  包内 SSRC 交给 sink。
- 未注册 sink、或 offer 为非 G.711 / TCP 媒体时，INVITE 被 **488** 拒绝。
- 对讲会话与视频会话共用同一个会话槽位：视频 INVITE 会回收进行中的
  对讲，反之亦然；BYE 走共享的媒体任务清理路径终结接收。

设备麦克风音频的上行（发送侧）不在本修订内。
