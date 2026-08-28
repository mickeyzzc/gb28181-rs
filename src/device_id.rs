//! GB/T 28181 device ID formatting and parsing.
//!
//! GB/T 28181 device IDs are 20-digit codes with the structure:
//!   [center 8 digits] [industry 2 digits] [type 3 digits] [serial 7 digits]
//!
//! Region codes follow GB/T 2260 (administrative division codes of China).
//! Type codes identify the device type (111 = IPC, 118 = NVR, etc.).

use anyhow::{bail, Context, Result};

/// Components of a parsed 20-digit GB/T 28181 device ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdParts {
    /// 8-digit administrative region code (GB/T 2260)
    pub region_code: String,
    /// 2-digit industry type code
    pub industry_type: u8,
    /// 3-digit device type code
    pub device_type: u16,
    /// 7-digit serial number
    pub serial: u32,
}

/// Format a 20-digit GB/T 28181 device ID.
///
/// The standard format is: [8 center chars][2 industry chars][3 type chars][7 serial chars] = 20 chars
///
/// # Arguments
/// * `center_code` - 8-character administrative center code
/// * `industry` - 2-digit industry type code
/// * `dev_type` - 3-digit device type code
/// * `serial` - 7-digit serial number
pub fn format_device_id(center_code: &str, industry: u8, dev_type: u16, serial: u32) -> String {
    assert_eq!(center_code.len(), 8, "center_code must be 8 digits");
    format!("{}{:02}{:03}{:07}", center_code, industry, dev_type, serial)
}

/// Parse a 20-digit GB/T 28181 device ID into its components.
pub fn parse_device_id(id: &str) -> Result<DeviceIdParts> {
    if id.len() != 20 {
        bail!(
            "Device ID must be exactly 20 digits, got {} chars",
            id.len()
        );
    }
    if !id.chars().all(|c| c.is_ascii_digit()) {
        bail!("Device ID must contain only ASCII digits");
    }
    let region_code = id[0..8].to_string();
    let industry_type: u8 = id[8..10]
        .parse()
        .context("Failed to parse industry type from digits 9-10")?;
    let device_type: u16 = id[10..13]
        .parse()
        .context("Failed to parse device type from digits 11-13")?;
    let serial: u32 = id[13..20]
        .parse()
        .context("Failed to parse serial from digits 14-20")?;
    Ok(DeviceIdParts {
        region_code,
        industry_type,
        device_type,
        serial,
    })
}

/// Standard device type codes used in GB/T 28181.
pub mod device_types {
    /// Front-end device (IPC, camera)
    pub const IPC: u8 = 111;
    /// NVR / DVR
    pub const NVR: u8 = 118;
    /// Decoder device
    pub const DECODER: u8 = 121;
    /// Alarm device
    pub const ALARM: u8 = 122;
    /// Audio device
    pub const AUDIO: u8 = 134;
}
