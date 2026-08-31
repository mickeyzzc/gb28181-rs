# Recording, RecordInfo, and playback

Implement one trait — [`RecordingSource`] — and the platform can query
recordings and play them back. Everything else (RecordInfo response
shapes, playback pacing, download, SIP INFO playback control) is the
library's job.

## The RecordingSource seam

```rust
use gb28181_rs::{RecordingSource, SegmentMeta};

struct MyRecordingIndex { /* your index: SQLite, sidecar scan, ... */ }

impl RecordingSource for MyRecordingIndex {
    /// All segments overlapping the inclusive [start_ms, end_ms] range.
    fn lookup(&self, start_ms: u64, end_ms: u64) -> Vec<SegmentMeta> {
        // query your index, map rows to SegmentMeta
    }
    /// Resolve a relative `file` to an absolute readable path.
    /// Override when your recording root differs from the process cwd.
    fn resolve_path(&self, file: &str) -> std::path::PathBuf { /* ... */ }
}

pub struct SegmentMeta {
    pub file: String,     // path relative to the recording root
    pub start_ms: u64,    // wall-clock start, unix milliseconds
    pub end_ms: u64,      // wall-clock end, unix milliseconds
}
```

Register it at construction:

```rust
let server = Gb28181Server::with_recording_index(
    config,
    au_hub,
    Some(std::sync::Arc::new(MyRecordingIndex { /* ... */ })),
);
```

## The reference segment format

Segment *reading* uses the crate's reference format (the format the
origin project records in): a bare Annex-B H.264 file plus a per-frame
`<segment>.ts.jsonl` sidecar of millisecond timestamps. Hosts that
record in this format get playback with zero format adaptation:

```rust
use gb28181_rs::{read_segment, RecordedAu};

pub struct RecordedAu {
    pub nalus: Vec<Vec<u8>>,   // payloads without start codes
    pub pts_offset: Duration,  // presentation offset from segment start
    pub is_key_frame: bool,
}

let aus = read_segment(std::path::Path::new("recordings/20260831-10.mp4.h264"))?;
```

Helpers if you build the format yourself: `sidecar_path()` (segment path
→ sidecar path), `group_aus()` (Annex-B NALUs → access units), and
`load_pts()`.

## What the platform sees

- **RecordInfo** — a MESSAGE query with a time range; the server calls
  `lookup()` and answers with the segment list.
- **Playback INVITE** — SDP with `s=Playback` and a time range; the
  server reads matching segments and pushes RTP/PS **paced to real time**
  (the timestamps' pace, not full speed).
- **Download INVITE** — SDP with `y=<ssrc>` semantics of a download; the
  server pushes at full speed.
- **PlaybackControl over SIP INFO** — pause, resume, seek by absolute
  time, and speed changes arrive as INFO and steer the paced pusher.

## Recording status

`DeviceStatus` answers `<Record>ON</Record>` or `OFF` from a shared
flag your recording writer maintains:

```rust
gb28181_rs::set_record_active(true);  // recording started
// ... writer stops ...
gb28181_rs::set_record_active(false);
```

## Verifying without a platform

```sh
cargo run --example playback_demo
# RecordInfo query, paced playback, PAUSE + 2x PLAY control, BYE —
# against a synthetic Annex-B + sidecar segment, on localhost.
```
