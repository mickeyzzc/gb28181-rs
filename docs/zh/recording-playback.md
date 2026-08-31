# 录像、录像查询与回放

实现一个 trait——[`RecordingSource`]——平台就能查录像、看回放。其余
一切（RecordInfo 应答格式、回放节奏、下载、SIP INFO 回放控制）都是
库的事。

## RecordingSource 接缝

```rust
use gb28181_rs::{RecordingSource, SegmentMeta};

struct MyRecordingIndex { /* 你的索引：SQLite、边车扫描…… */ }

impl RecordingSource for MyRecordingIndex {
    /// 与闭区间 [start_ms, end_ms] 有重叠的全部录像段。
    fn lookup(&self, start_ms: u64, end_ms: u64) -> Vec<SegmentMeta> {
        // 查你的索引，映射为 SegmentMeta
    }
    /// 把相对路径 `file` 解析为可读的绝对路径。
    /// 录像根目录与进程 cwd 不同时必须覆写。
    fn resolve_path(&self, file: &str) -> std::path::PathBuf { /* ... */ }
}

pub struct SegmentMeta {
    pub file: String,     // 相对录像根目录的路径
    pub start_ms: u64,    // 起始墙钟时间，unix 毫秒
    pub end_ms: u64,      // 结束墙钟时间，unix 毫秒
}
```

构造时注册：

```rust
let server = Gb28181Server::with_recording_index(
    config,
    au_hub,
    Some(std::sync::Arc::new(MyRecordingIndex { /* ... */ })),
);
```

## 参考录像段格式

录像段**读取**用库的参考格式（出身项目的录像格式）：Annex-B 裸
H.264 文件 + 每帧一条毫秒时间戳的 `<segment>.ts.jsonl` 边车。按这个
格式录像的宿主零格式适配即得回放能力：

```rust
use gb28181_rs::{read_segment, RecordedAu};

pub struct RecordedAu {
    pub nalus: Vec<Vec<u8>>,   // 不含起始码的 NAL 负载
    pub pts_offset: Duration,  // 相对段起的呈现偏移
    pub is_key_frame: bool,
}

let aus = read_segment(std::path::Path::new("recordings/20260831-10.h264"))?;
```

自己构造该格式时的辅助函数：`sidecar_path()`（段路径 → 边车路径）、
`group_aus()`（Annex-B NAL → 访问单元）、`load_pts()`。

## 平台侧看到什么

- **RecordInfo**——带时间范围的 MESSAGE 查询；服务器调 `lookup()`
  并以录像段清单应答。
- **回放 INVITE**——SDP `s=Playback` 带时间范围；服务器读取命中段并
  **按实时节奏**推 RTP/PS（按时间戳的节奏，不是全速）。
- **下载 INVITE**——下载语义的 SDP；服务器全速推流。
- **SIP INFO 回放控制**——暂停、恢复、按绝对时间拖动、倍速以 INFO
  到达，驱动节奏化推流器。

## 录像状态

`DeviceStatus` 用一个共享标志回答 `<Record>ON/OFF`，由你的录像写入
方维护：

```rust
gb28181_rs::set_record_active(true);  // 开始录像
// ... 写入方停止 ...
gb28181_rs::set_record_active(false);
```

## 没有平台怎么验证

```sh
cargo run --example playback_demo
# RecordInfo 查询、节奏化回放、PAUSE + 2 倍速 PLAY 控制、BYE——
# 针对合成的 Annex-B + 边车录像段，本机回环完成。
```
