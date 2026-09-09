# 日志与 tracing 兼容

本库经标准 [`log`](https://crates.io/crates/log) 0.4 facade 打日志——
这是对照原生 `tracing` 迁移评估后的刻意选择（issue #36）。本篇是决策
记录 + 两类宿主的接线方法。

## 为什么保留 `log` facade

- **它是兼容面最宽的选择。** 基于 `log` 的库在纯 log 宿主
  （`env_logger`、`simple_logger`……）与 tracing 宿主下都开箱即用
  ——`tracing` 通过 `tracing-log` 桥自动吸收 `log` 记录。反过来不
  成立：原生 `tracing` 的库在纯 log 宿主下默认静默，除非宿主自己装
  桥。
- **库的诊断是事件，不是 span。** 本库发出的都是点事件（注册状态转
  移、INVITE 生命周期、心跳失败）。`tracing` 能增加的价值——跟随会
  话生命周期的 span——需要把 span 上下文穿透引擎公开 API
  （`Gb28181Server` 内部自起任务），一次 API 变更却没有事件级的收
  益。只有出现真实宿主需求再重评（企业化 Epic 上跟踪）。

## 纯 log 宿主

初始化任意 `log` 实现：

```rust
env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
    .init();
```

## tracing 宿主

`tracing-subscriber` 加 LogTracer 桥即可把库的记录收为 tracing 事件
——宿主除初始化外零改动：

```rust
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

tracing_log::LogTracer::init().expect("install log bridge");
tracing_subscriber::registry()
    .with(tracing_subscriber::fmt::layer())
    .with(tracing_subscriber::EnvFilter::from_default_env())
    .init();
```

记录按库发出的 `INFO`/`WARN`/`ERROR`/`DEBUG` 级别到达；与其他 crate
一样用 `gb28181_rs=debug` 风格指令过滤。

## 库会发出什么

| 级别 | 含义 |
|---|---|
| `error` | 子系统失败并降级（如心跳发送错误、socket 收包错误） |
| `warn` | 可恢复异常：注册重试、INVITE 重传重放、GB35114 Note 拒绝、严格模式示例值警告 |
| `info` | 生命周期转移：监听启动、注册成功、INVITE/BYE、停机 |
| `debug` | 每条消息级细节（未处理方法、周期性重注册 tick） |

全部与正确性无关——生产可调高过滤级别静默运行；复现互调问题时再开
`debug`。
