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
