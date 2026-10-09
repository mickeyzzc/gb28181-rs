//! Device-side firmware-upgrade execution (GB/T 28181-2022 A.2.3.1.12 /
//! A.2.5.9; gb28181-go #108 twin).
//!
//! The platform orders an upgrade with a DeviceControl(DeviceUpgrade)
//! MESSAGE. The server hands the parsed command to the installed
//! [`DeviceUpgrader`] — the host owns download and flash (the standard
//! leaves the transfer to FileURL's scheme) — and reports completion
//! with an `DeviceUpgradeResult` notify carrying the same SessionID.
//! The outcome's `firmware` (the version in effect after the attempt)
//! is required by the notify; a failed upgrade carries a reason (01
//! download timeout / 02 package corrupt / 03 system error / 99 other).
//! Without an upgrader the server keeps its control reject.

use std::future::Future;
use std::pin::Pin;

use crate::manscdp::DeviceUpgradeCmd;

/// One upgrade attempt's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceUpgradeOutcome {
    /// Whether the firmware was applied.
    pub success: bool,
    /// Firmware in effect after the attempt (new on success, unchanged
    /// on failure) — required by the A.2.5.9 notify.
    pub firmware: String,
    /// A.2.5.9 UpgradeFailedReason ("01"/"02"/"03"/"99"); ignored on
    /// success.
    pub failed_reason: String,
}

/// The upgrader's asynchronous outcome.
pub type DeviceUpgradeExchange =
    Pin<Box<dyn Future<Output = anyhow::Result<DeviceUpgradeOutcome>> + Send>>;

/// Product-side upgrade executor, installed via
/// `Gb28181Server::with_device_upgrader`.
pub trait DeviceUpgrader: Send + Sync {
    /// Downloads and applies `cmd.file_url`, reporting the outcome.
    fn upgrade(&self, cmd: DeviceUpgradeCmd) -> DeviceUpgradeExchange;
}
