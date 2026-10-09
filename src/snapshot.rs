//! Device-side snapshot command execution (GB/T 28181-2022 A.2.1.24 /
//! A.2.5.7).
//!
//! The platform orders captures with a DeviceConfig(SnapShotConfig)
//! MESSAGE (A.2.1.24 rides the device-config channel — twin of
//! gb28181-go #107). The server answers 200 plus the A.2.6.8 OK
//! response, hands the parsed command to the installed
//! [`SnapshotExecutor`], and reports completion with an
//! UploadSnapShotFinished notify carrying the same SessionID. The
//! executor owns the product side: capture JPEG frames and POST each
//! body to the command's `upload_url` **verbatim** (the URL already
//! carries the session parameter — the receiving platform owns that
//! contract); the returned IDs become the notify's SnapShotFileID list,
//! and an empty list reports the exchange as wholly/partially failed
//! (A.2.5.7). Without an executor the server rejects the configuration
//! with Result=ERROR on the same channel.

use std::future::Future;
use std::pin::Pin;

/// One platform-issued snapshot command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotCommand {
    /// Frames to capture, 1..=10.
    pub snap_num: u32,
    /// Per-frame interval in seconds (>=1) when the platform asked for a
    /// timed burst; `None` for a manual single shot.
    pub interval: Option<u32>,
    /// HTTP endpoint to POST each JPEG to, used verbatim.
    pub upload_url: String,
    /// Platform-generated session ID, echoed by the server in the
    /// completion notify.
    pub session_id: String,
}

/// The executor's asynchronous outcome: one ID per successfully
/// uploaded file (empty = failed exchange).
pub type SnapshotExchange = Pin<Box<dyn Future<Output = anyhow::Result<Vec<String>>> + Send>>;

/// Product-side snapshot executor, installed via
/// `Gb28181Server::with_snapshot_executor`.
pub trait SnapshotExecutor: Send + Sync {
    /// Captures and uploads `cmd.snap_num` frames, returning one ID per
    /// successfully uploaded file.
    fn execute(&self, cmd: SnapshotCommand) -> SnapshotExchange;
}
