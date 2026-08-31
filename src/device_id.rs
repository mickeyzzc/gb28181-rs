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
///
/// # Errors
/// `Err` when `center_code` is not exactly 8 ASCII digits (this is a public
/// API — bad input is returned, never panicked on).
pub fn format_device_id(
    center_code: &str,
    industry: u8,
    dev_type: u16,
    serial: u32,
) -> Result<String> {
    if center_code.len() != 8 || !center_code.chars().all(|c| c.is_ascii_digit()) {
        bail!(
            "center_code must be exactly 8 ASCII digits, got {:?} ({} chars)",
            center_code,
            center_code.len()
        );
    }
    Ok(format!(
        "{}{:02}{:03}{:07}",
        center_code, industry, dev_type, serial
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_golden() {
        // The default device/channel ID used across this workspace:
        // region 34020000, industry 00, type 132, serial 0000001.
        assert_eq!(
            format_device_id("34020000", 0, 132, 1).expect("valid center"),
            "34020000001320000001"
        );
    }

    #[test]
    fn format_zero_pads_each_field() {
        assert_eq!(
            format_device_id("11000000", 6, 5, 42).expect("valid center"),
            "11000000060050000042"
        );
    }

    #[test]
    fn format_then_parse_roundtrip() {
        for (industry, dev_type, serial) in
            [(0u8, 132u16, 2000001u32), (0, 111, 7), (6, 118, 9999999)]
        {
            let id =
                format_device_id("34020000", industry, dev_type, serial).expect("valid center");
            let parts = parse_device_id(&id).expect("roundtrip parses");
            assert_eq!(parts.region_code, "34020000");
            assert_eq!(parts.industry_type, industry);
            assert_eq!(parts.device_type, dev_type);
            assert_eq!(parts.serial, serial);
        }
    }

    #[test]
    fn format_with_ipc_type_code() {
        let id =
            format_device_id("34020000", 0, u16::from(device_types::IPC), 1).expect("valid center");
        assert_eq!(id, "34020000001110000001");
        assert_eq!(
            parse_device_id(&id).unwrap().device_type,
            u16::from(device_types::IPC)
        );
    }

    #[test]
    fn parse_golden() {
        let parts = parse_device_id("34020000001320000001").unwrap();
        assert_eq!(
            parts,
            DeviceIdParts {
                region_code: "34020000".to_string(),
                industry_type: 0,
                device_type: 132,
                serial: 1,
            }
        );
    }

    #[test]
    fn parse_rejects_wrong_length() {
        assert!(parse_device_id("3402000000132000000").is_err()); // 19 digits
        assert!(parse_device_id("340200000013200000011").is_err()); // 21 digits
        assert!(parse_device_id("").is_err());
    }

    #[test]
    fn parse_rejects_non_digits() {
        assert!(parse_device_id("3402000000132000000a").is_err());
        assert!(parse_device_id("3402000A0013200000001").is_err());
    }

    /// Regression: a bad center code is a returned error, not a panic —
    /// consumer-reachable APIs must not panic on input.
    #[test]
    fn format_rejects_bad_center_code_without_panicking() {
        assert!(format_device_id("3402000", 0, 132, 1).is_err());
        assert!(format_device_id("3402000A", 0, 132, 1).is_err());
        assert!(format_device_id("", 0, 132, 1).is_err());
    }
}
