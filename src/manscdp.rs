//! MANSCDP+xml structures for GB/T 28181 protocol.
//!
//! This module provides serde-based structures for parsing and serializing
//! MANSCDP (Monitoring System Protocol and Data Protocol) XML messages
//! used in GB/T 28181-2022 for device catalog, device info, and keepalive.

use serde::{Deserialize, Serialize};

/// Deserializes an empty XML string (missing or blank attribute) into `None`.
pub(crate) fn empty_string_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.filter(|s| !s.is_empty()))
}

/// Query — platform sends to device (Catalog, DeviceInfo, Keepalive)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename = "Query")]
pub struct Query {
    #[serde(rename = "CmdType")]
    pub cmd_type: String,
    #[serde(rename = "SN")]
    pub sn: String,
    #[serde(rename = "DeviceID")]
    pub device_id: String,
    /// Start of the requested recording window (GB/T 28181 time format).
    #[serde(
        rename = "StartTime",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub start_time: Option<String>,
    /// End of the requested recording window (GB/T 28181 time format).
    #[serde(rename = "EndTime", default, deserialize_with = "empty_string_as_none")]
    pub end_time: Option<String>,
    /// Recording type filter (e.g. "time").
    #[serde(rename = "Type", default, deserialize_with = "empty_string_as_none")]
    pub r#type: Option<String>,
    /// Stream type filter (e.g. "0" for main stream).
    #[serde(
        rename = "StreamType",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub stream_type: Option<String>,
    /// Cruise-track index for the 2022 CruiseTrackQuery (A.2.4.12),
    /// echoed in the response's `<Number>`.
    #[serde(rename = "Number", default, deserialize_with = "empty_string_as_none")]
    pub number: Option<String>,
    /// Config types requested by a ConfigDownload query (A.2.4.7) —
    /// multiple types arrive "/"-separated.
    #[serde(
        rename = "ConfigType",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub config_type: Option<String>,
}

/// Query in attribute format — older platforms put CmdType/SN/DeviceID on the
/// root element instead of as child elements.
#[derive(Debug, Deserialize)]
pub struct QueryAttr {
    #[serde(rename = "@CmdType")]
    pub cmd_type: String,
    #[serde(rename = "@SN")]
    pub sn: String,
    #[serde(rename = "@DeviceID")]
    pub device_id: String,
    #[serde(
        rename = "@StartTime",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub start_time: Option<String>,
    #[serde(
        rename = "@EndTime",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub end_time: Option<String>,
    #[serde(rename = "@Type", default, deserialize_with = "empty_string_as_none")]
    pub r#type: Option<String>,
    #[serde(
        rename = "@StreamType",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub stream_type: Option<String>,
    #[serde(
        rename = "@ConfigType",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    pub config_type: Option<String>,
}

/// Response — device sends back to platform (Catalog, DeviceInfo)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename = "Response")]
pub struct Response {
    #[serde(rename = "CmdType")]
    pub cmd_type: String,
    #[serde(rename = "SN")]
    pub sn: String,
    #[serde(rename = "DeviceID")]
    pub device_id: String,
    /// SumNum for Catalog response
    #[serde(rename = "SumNum", skip_serializing_if = "Option::is_none")]
    pub sum_num: Option<u32>,
    /// DeviceList for Catalog response
    #[serde(rename = "DeviceList", skip_serializing_if = "Option::is_none")]
    pub device_list: Option<DeviceList>,
    /// Device for DeviceInfo response
    #[serde(rename = "Device", skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceItem>,
}

/// Device list container for Catalog response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceList {
    #[serde(rename = "Item")]
    pub item: Vec<ChannelItem>,
}

/// CatalogItem — per GB/T 28181-2022 Annex A.2.1 mandatory fields
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelItem {
    #[serde(rename = "DeviceID")]
    pub device_id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Manufacturer")]
    pub manufacturer: String,
    #[serde(rename = "Model")]
    pub model: String,
    #[serde(rename = "Owner")]
    pub owner: String,
    #[serde(rename = "CivilCode")]
    pub civil_code: String,
    #[serde(rename = "Address")]
    pub address: String,
    #[serde(rename = "Parental")]
    pub parental: u32,
    #[serde(rename = "ParentID")]
    pub parent_id: String,
    #[serde(rename = "SafetyWay")]
    pub safety_way: u32,
    #[serde(rename = "RegisterWay")]
    pub register_way: u32,
    #[serde(rename = "Secrecy")]
    pub secrecy: u32,
    #[serde(rename = "Status")]
    pub status: String,
    #[serde(rename = "IPAddress")]
    pub ip_address: String,
    #[serde(rename = "Port")]
    pub port: u16,
    #[serde(rename = "Longitude")]
    pub longitude: f64,
    #[serde(rename = "Latitude")]
    pub latitude: f64,
}

/// DeviceItem — for DeviceInfo response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceItem {
    #[serde(rename = "DeviceID")]
    pub device_id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Manufacturer")]
    pub manufacturer: String,
    #[serde(rename = "Model")]
    pub model: String,
    #[serde(rename = "Firmware")]
    pub firmware: String,
}

/// Notify — for Keepalive and other notifications
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename = "Notify")]
pub struct Notify {
    #[serde(rename = "CmdType")]
    pub cmd_type: String,
    #[serde(rename = "SN")]
    pub sn: String,
    #[serde(rename = "DeviceID")]
    pub device_id: String,
    #[serde(rename = "Status", skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Notify in attribute format — older platforms put CmdType/SN/DeviceID/Time/
/// Keepalive on the root element instead of as child elements.
#[derive(Debug, Deserialize)]
pub struct NotifyAttr {
    #[serde(rename = "@CmdType")]
    pub cmd_type: String,
    #[serde(rename = "@SN")]
    pub sn: String,
    #[serde(rename = "@DeviceID")]
    pub device_id: String,
    #[serde(rename = "@Time", deserialize_with = "empty_string_as_none")]
    pub time: Option<String>,
    #[serde(rename = "@Keepalive", deserialize_with = "empty_string_as_none")]
    pub keepalive: Option<String>,
}

/// Parses a `<Query>` body in either child-element (live MiBee NVR) or
/// attribute (older platforms) format, normalizing into [`Query`].
#[allow(dead_code)] // wired in R2 (dispatch from client.rs)
pub(crate) fn parse_query_dual(body: &str) -> Option<Query> {
    // Try child-element format first (matches live MiBee NVR).
    if let Ok(q) = serde_xml_rs::from_str::<Query>(body) {
        if !q.cmd_type.is_empty() {
            return Some(q);
        }
    }
    // Fallback: try attribute format.
    if let Ok(qa) = serde_xml_rs::from_str::<QueryAttr>(body) {
        if !qa.cmd_type.is_empty() {
            return Some(Query {
                cmd_type: qa.cmd_type,
                sn: qa.sn,
                device_id: qa.device_id,
                start_time: qa.start_time,
                end_time: qa.end_time,
                r#type: qa.r#type,
                stream_type: qa.stream_type,
                number: None,
                config_type: qa.config_type,
            });
        }
    }
    None
}

/// A RecordInfo query with the recording window resolved to epoch milliseconds.
///
/// `start_ms`/`end_ms` are `None` when the platform omitted the corresponding
/// time or it could not be parsed (lenient).
#[derive(Debug, Clone)]
pub struct RecordInfoQuery {
    /// SN echoed back in the response.
    pub sn: String,
    /// Device ID echoed back in the response.
    pub device_id: String,
    /// Start of the requested window in ms since the Unix epoch.
    pub start_ms: Option<u64>,
    /// End of the requested window in ms since the Unix epoch.
    pub end_ms: Option<u64>,
    /// Recording type filter (e.g. "time"), if provided.
    pub r#type: Option<String>,
    /// Stream type filter (e.g. "0"), if provided.
    pub stream_type: Option<String>,
}

/// Parse a `<Query CmdType="RecordInfo">` body in either child-element or
/// attribute format, resolving the time window to epoch milliseconds.
pub(crate) fn parse_recordinfo_query_dual(body: &str) -> Option<RecordInfoQuery> {
    let q = parse_query_dual(body)?;
    if q.cmd_type != "RecordInfo" {
        return None;
    }
    Some(RecordInfoQuery {
        sn: q.sn,
        device_id: q.device_id,
        start_ms: q.start_time.as_deref().and_then(parse_gb_time_ms),
        end_ms: q.end_time.as_deref().and_then(parse_gb_time_ms),
        r#type: q.r#type,
        stream_type: q.stream_type,
    })
}

/// Parse a GB/T 28181 time string (`YYYY-MM-DDTHH:MM:SS`) into milliseconds
/// since the Unix epoch. A trailing `Z` or `+HH:MM`/`-HH:MM` offset is
/// applied explicitly; a bare naive string (the common platform form) is
/// interpreted in the device's local timezone, matching the Go repo's
/// `time.Local` semantics. Returns `None` on any malformed input.
pub(crate) fn parse_gb_time_ms(s: &str) -> Option<u64> {
    parse_gb_time_ms_with(s, device_local_offset_secs())
}

/// Offset-aware core of [`parse_gb_time_ms`] for deterministic tests.
pub(crate) fn parse_gb_time_ms_with(s: &str, local_offset_secs: i64) -> Option<u64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let (date_part, rest) = s.split_at(10);
    let time_part = &rest[1..9]; // skip the 'T'
    let year: i64 = date_part[0..4].parse().ok()?;
    let month: i64 = date_part[5..7].parse().ok()?;
    let day: i64 = date_part[8..10].parse().ok()?;
    let hour: i64 = time_part[0..2].parse().ok()?;
    let minute: i64 = time_part[3..5].parse().ok()?;
    let second: i64 = time_part[6..8].parse().ok()?;

    // Optional trailing offset: `Z` (UTC) or `+HH:MM` / `-HH:MM`.
    let tail = &s[19..];
    let offset_minutes: i64 = if tail == "Z" {
        0
    } else if tail.is_empty() {
        // Naive local time (GB28181 platforms omit the offset).
        local_offset_secs / 60
    } else {
        let sign = if tail.starts_with('-') { -1 } else { 1 };
        let digits = tail.trim_start_matches(['+', '-']);
        if digits.len() != 5 || digits.as_bytes()[2] != b':' {
            return None;
        }
        let hh: i64 = digits[0..2].parse().ok()?;
        let mm: i64 = digits[3..5].parse().ok()?;
        sign * (hh * 60 + mm)
    };

    // Days since the Unix epoch (Howard Hinnant's civil-from-days inverse).
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    Some(secs as u64 * 1000)
}

/// Device-local UTC offset in seconds for "now", honoring /etc/localtime
/// via libc (same source Go's `time.Local` uses). Falls back to 0 (UTC).
///
/// The libc `localtime_r` path is POSIX-only; non-Unix targets report UTC
/// (offset 0) — std has no portable local-offset API, and the offset only
/// decorates MANSCDP DeviceInfo/Keepalive timestamps.
// tm_gmtoff is c_long: the widening conversion below is only "useless" on
// LP64 hosts; ILP32 targets (issue #55) need it to compile.
#[allow(clippy::useless_conversion)]
pub fn device_local_offset_secs() -> i64 {
    #[cfg(unix)]
    {
        // `as _` picks up libc::time_t from localtime_r's signature without
        // naming the alias (deprecated on 32-bit musl ahead of the 64-bit
        // time_t switch); truncation past 2038 only affects this decorative
        // timestamp, matching the previous explicit cast.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as _)
            .unwrap_or(0);
        // SAFETY: `tm` is a plain struct and `localtime_r` writes it without
        // retaining the pointer; a NULL return leaves the zeroed fallback.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&now, &mut tm).is_null() {
                0
            } else {
                // tm_gmtoff is i64 on LP64 but i32 on ILP32 targets (#55).
                i64::from(tm.tm_gmtoff)
            }
        }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Days since 1970-01-01 for a civil date, or `None` if the date is invalid.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

// ---------------------------------------------------------------------------
// GB/T 28181-2022 image snapshot (A.2.1.24 / A.2.5.7) — twin-parity wire
// types with gb28181-go (#49): the Control element is <SnapShot> with
// SnapNum/Interval/UploadURL/SessionID; the completion notify carries the
// same SessionID plus a SnapShotList of SnapShotFileID entries.
// ---------------------------------------------------------------------------

/// The snapshot payload inside an inbound DeviceControl (A.2.1.24
/// snapShotCfgType).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapShotCmd {
    /// Frames to capture, 1..=10; manual snapshot = 1.
    #[serde(rename = "SnapNum")]
    pub snap_num: u32,
    /// Per-frame interval in seconds (>=1); absent for manual snapshots.
    #[serde(rename = "Interval", default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<u32>,
    /// HTTP endpoint the device POSTs the JPEGs to.
    #[serde(rename = "UploadURL")]
    pub upload_url: String,
    /// Platform-generated session ID ([A-Za-z0-9-], 32..128 bytes),
    /// echoed in the completion notify.
    #[serde(rename = "SessionID")]
    pub session_id: String,
}

/// An inbound DeviceControl body carrying a snapshot command.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ControlSnapShot {
    #[serde(rename = "CmdType")]
    pub cmd_type: String,
    #[serde(rename = "SN")]
    pub sn: String,
    #[serde(rename = "DeviceID")]
    pub device_id: String,
    #[serde(rename = "SnapShot")]
    pub snap_shot: SnapShotCmd,
}

/// The `<SnapShotList>` node: 0..=10 uploaded-image IDs. An empty list is
/// the standard's wholly/partially-failed signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapShotList {
    #[serde(rename = "SnapShotFileID", default)]
    pub snap_shot_file_id: Vec<String>,
}

/// The completion notify (A.2.5.7): the device reports the uploaded image
/// IDs after executing a snapshot command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename = "Notify")]
pub struct UploadSnapShotFinished {
    #[serde(rename = "CmdType")]
    pub cmd_type: String,
    #[serde(rename = "SN")]
    pub sn: String,
    #[serde(rename = "DeviceID")]
    pub device_id: String,
    #[serde(rename = "SessionID")]
    pub session_id: String,
    #[serde(rename = "SnapShotList")]
    pub snap_shot_list: SnapShotList,
}

impl UploadSnapShotFinished {
    /// Serializes the notify body as a `<Notify>` document (caller wraps
    /// it in a SIP MESSAGE).
    pub fn to_xml(&self) -> anyhow::Result<String> {
        // serde-xml-rs emits an XML declaration; the Go twin's encoder does
        // not — strip it so both bodies are byte-identical on the wire.
        let out = serde_xml_rs::to_string(self)?;
        let body = out
            .strip_prefix("<?xml version=\"1.0\" encoding=\"UTF-8\"?>")
            .unwrap_or(out.as_str());
        Ok(body.to_string())
    }
}

/// Parses an inbound DeviceControl snapshot body; `None` when the body is
/// not a snapshot control.
pub fn parse_control_snapshot(body: &str) -> Option<ControlSnapShot> {
    let c = serde_xml_rs::from_str::<ControlSnapShot>(body).ok()?;
    (c.cmd_type == "DeviceControl").then_some(c)
}

/// Builds the device-side completion report.
pub fn build_upload_snapshot_finished(
    sn: u32,
    device_id: &str,
    session_id: &str,
    file_ids: &[String],
) -> UploadSnapShotFinished {
    UploadSnapShotFinished {
        cmd_type: "UploadSnapShotFinished".to_string(),
        sn: sn.to_string(),
        device_id: device_id.to_string(),
        session_id: session_id.to_string(),
        snap_shot_list: SnapShotList {
            snap_shot_file_id: file_ids.to_vec(),
        },
    }
}

// ---------------------------------------------------------------------------
// DeviceControl sub-command decode (GB/T 28181-2016 §9.3.2 / 2022 §9.3,
// issue #58). The family shares one `<Control><CmdType>DeviceControl…`
// body with exactly one sub-command child element. DragZoom (2022 拉框
// 放大/缩小) is deferred until its wire form is verified against the
// standard text; PTZCmd is bit-level decoded (§A.3-A.4, issue #57) —
// [`parse_ptz_command`] is the byte-exact inverse of the gb28181-go
// platform builders, and the golden hex table is shared across the
// twins.
// ---------------------------------------------------------------------------

/// PTZ direction bits (§A.4, byte 4 of the A5 0F command). Diagonals
/// combine two axis bits; zoom rides the high bits of the same byte.
pub const PTZ_RIGHT: u8 = 0x01;
pub const PTZ_LEFT: u8 = 0x02;
pub const PTZ_DOWN: u8 = 0x04;
pub const PTZ_UP: u8 = 0x08;
pub const PTZ_ZOOM_IN: u8 = 0x10;
pub const PTZ_ZOOM_OUT: u8 = 0x20;

/// FI lens action bits (§A.3.3 表 A.6 — low nibble of byte 4, which the
/// gb28181-go builders OR with 0x40).
pub const PTZ_FOCUS_FAR: u8 = 0x01;
pub const PTZ_FOCUS_NEAR: u8 = 0x02;
pub const PTZ_IRIS_OPEN: u8 = 0x04;
pub const PTZ_IRIS_CLOSE: u8 = 0x08;

/// §A.3.4 preset instruction (byte 4 = 0x81/0x82/0x83).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtzPresetAction {
    /// 设置预置位 (0x81).
    Set,
    /// 调用预置位 (0x82).
    Call,
    /// 删除预置位 (0x83).
    Delete,
}

/// §A.3.5 cruise instruction (byte 4 = 0x84-0x88).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtzCruiseAction {
    /// 加入巡航点 (0x84).
    AddPoint,
    /// 删除巡航点 (0x85).
    DelPoint,
    /// 设置巡航速度 (0x86).
    Speed,
    /// 设置巡航停留时间 (0x87).
    StayTime,
    /// 开始巡航 (0x88).
    Start,
}

/// A bit-level decoded PTZCmd (GB/T 28181 §A.3-A.4, issue #57): the
/// 8-byte `A5 0F` command. Structurally invalid input (hex length,
/// missing A5 start byte, checksum mismatch) surfaces as
/// [`PtzCommand::Invalid`] with the raw string preserved — the decode is
/// total and hosts keep seeing everything the platform sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtzCommand {
    /// §A.4 pan/tilt/zoom: direction bits (0 = stop) and the per-axis
    /// speeds of the acting axes (0x00-0xFF slow→fast).
    Move {
        /// Direction bits — combine [`PTZ_UP`], [`PTZ_LEFT`], … .
        bits: u8,
        pan_speed: u8,
        tilt_speed: u8,
        zoom_speed: u8,
    },
    /// §A.3.4 preset set/call/delete; the number (1-255) rides in byte 7.
    Preset { action: PtzPresetAction, preset: u8 },
    /// §A.3.5 cruise: group in byte 5, value (preset/speed/stay) in
    /// byte 7.
    Cruise {
        action: PtzCruiseAction,
        group: u8,
        value: u8,
    },
    /// §A.3.3 FI focus/iris: action bits (low nibble of byte 4 | 0x40)
    /// and the focus/iris speeds in bytes 5/6.
    Lens {
        bits: u8,
        focus_speed: u8,
        iris_speed: u8,
    },
    /// §A.3.7 auxiliary switch (wiper/light): number in byte 5.
    AuxSwitch { number: u8, on: bool },
    /// Structurally valid but unrecognized instruction code; data bytes
    /// 5-7 preserved for vendor extensions.
    Unknown { code: u8, data: [u8; 3] },
    /// Undecodable as an 8-byte A5 command; `raw` is the input as
    /// received (trimmed).
    Invalid { raw: String },
}

impl PtzCommand {
    /// Whether the command is a §A.4 stop (a Move with no direction
    /// bits set).
    #[must_use]
    pub fn is_stop(&self) -> bool {
        matches!(self, Self::Move { bits: 0, .. })
    }
}

/// Decodes the `<PTZCmd>` hex payload into a structured [`PtzCommand`].
/// Total — never fails: undecodable input yields [`PtzCommand::Invalid`].
/// Bytes 2-3 (the `0F` combination byte and the address) vary across
/// vendors and only feed the checksum.
#[must_use]
pub fn parse_ptz_command(a505_hex: &str) -> PtzCommand {
    let trimmed = a505_hex.trim();
    let invalid = || PtzCommand::Invalid {
        raw: trimmed.to_string(),
    };
    let Ok(raw) = hex::decode(trimmed) else {
        return invalid();
    };
    if raw.len() != 8 || raw[0] != 0xA5 {
        return invalid();
    }
    let sum = raw[..7].iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
    if sum != raw[7] {
        return invalid();
    }
    let code = raw[3];
    match code {
        0x00..=0x3F => PtzCommand::Move {
            bits: code,
            pan_speed: raw[4],
            tilt_speed: raw[5],
            zoom_speed: raw[6],
        },
        0x40..=0x4F => PtzCommand::Lens {
            bits: code & 0x0F,
            focus_speed: raw[4],
            iris_speed: raw[5],
        },
        0x81..=0x83 => PtzCommand::Preset {
            action: match code {
                0x81 => PtzPresetAction::Set,
                0x82 => PtzPresetAction::Call,
                _ => PtzPresetAction::Delete,
            },
            preset: raw[6],
        },
        0x84..=0x88 => PtzCommand::Cruise {
            action: match code {
                0x84 => PtzCruiseAction::AddPoint,
                0x85 => PtzCruiseAction::DelPoint,
                0x86 => PtzCruiseAction::Speed,
                0x87 => PtzCruiseAction::StayTime,
                _ => PtzCruiseAction::Start,
            },
            group: raw[4],
            value: raw[6],
        },
        0x8C | 0x8D => PtzCommand::AuxSwitch {
            number: raw[4],
            on: code == 0x8C,
        },
        _ => PtzCommand::Unknown {
            code,
            data: [raw[4], raw[5], raw[6]],
        },
    }
}

/// A decoded DeviceControl sub-command (issue #58).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceControlKind {
    /// `<IFrameCmd>Send</IFrameCmd>` — force the next encoded frame to be
    /// an IDR. Platforms send this when starting a pull or after loss.
    ForceIFrame,
    /// `<RecordCmd>` — `Record` / `StopRecord` toggles platform-requested
    /// local recording.
    Record(bool),
    /// `<GuardCmd>` — `SetGuard` / `ResetGuard` arm/disarm.
    Guard(bool),
    /// `<AlarmCmd>ResetAlarm</AlarmCmd>` — clear the active alarm.
    ResetAlarm,
    /// `<TeleBoot>Boot</TeleBoot>` — remote restart.
    TeleBoot,
    /// `<PTZCmd>` — A.3/A.4 command, bit-level decoded ([`PtzCommand`],
    /// issue #57).
    Ptz(PtzCommand),
    /// `<HomePosition>` — 看守位 control (A.2.3.1.10): auto-return to a
    /// preset after inactivity.
    HomePosition {
        /// 1 = enabled, 0 = disabled.
        enabled: u32,
        /// Auto-reset interval in seconds (absent = keep current).
        reset_time: Option<u32>,
        /// Preset index to return to, 0-255 (absent = keep current).
        preset_index: Option<u32>,
    },
}

/// An inbound DeviceControl body with a recognized sub-command.
///
/// Only the child-element form is parsed (the production-validated form
/// for controls, matching [`parse_control_snapshot`]); unknown or absent
/// sub-commands yield `None` so the server keeps its reject behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceControl {
    pub sn: String,
    pub device_id: String,
    pub kind: DeviceControlKind,
}

#[derive(Deserialize)]
struct DeviceControlBody {
    #[serde(rename = "CmdType", default)]
    cmd_type: String,
    #[serde(rename = "SN", default)]
    sn: String,
    #[serde(rename = "DeviceID", default)]
    device_id: String,
    #[serde(
        rename = "IFrameCmd",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    i_frame_cmd: Option<String>,
    #[serde(
        rename = "RecordCmd",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    record_cmd: Option<String>,
    #[serde(
        rename = "GuardCmd",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    guard_cmd: Option<String>,
    #[serde(
        rename = "AlarmCmd",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    alarm_cmd: Option<String>,
    #[serde(
        rename = "TeleBoot",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    tele_boot: Option<String>,
    #[serde(rename = "PTZCmd", default, deserialize_with = "empty_string_as_none")]
    ptz_cmd: Option<String>,
    #[serde(rename = "HomePosition", default)]
    home_position: Option<HomePositionBody>,
}

/// `<HomePosition>` body (A.2.3.1.10).
#[derive(Debug, Deserialize)]
struct HomePositionBody {
    #[serde(rename = "Enabled", default)]
    enabled: String,
    #[serde(
        rename = "ResetTime",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    reset_time: Option<String>,
    #[serde(
        rename = "PresetIndex",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    preset_index: Option<String>,
}

/// Parses an inbound DeviceControl body; `None` when the body is not a
/// DeviceControl or carries no recognized sub-command (the caller keeps
/// its control-reject behavior for those).
pub fn parse_device_control(body: &str) -> Option<DeviceControl> {
    let c: DeviceControlBody = serde_xml_rs::from_str(body).ok()?;
    if c.cmd_type != "DeviceControl" {
        return None;
    }
    let kind = if let Some(v) = c.i_frame_cmd {
        match v.as_str() {
            "Send" => Some(DeviceControlKind::ForceIFrame),
            _ => None,
        }
    } else if let Some(v) = c.record_cmd {
        match v.as_str() {
            "Record" => Some(DeviceControlKind::Record(true)),
            "StopRecord" => Some(DeviceControlKind::Record(false)),
            _ => None,
        }
    } else if let Some(v) = c.guard_cmd {
        match v.as_str() {
            "SetGuard" => Some(DeviceControlKind::Guard(true)),
            "ResetGuard" => Some(DeviceControlKind::Guard(false)),
            _ => None,
        }
    } else if let Some(v) = c.alarm_cmd {
        match v.as_str() {
            "ResetAlarm" => Some(DeviceControlKind::ResetAlarm),
            _ => None,
        }
    } else if let Some(v) = c.tele_boot {
        match v.as_str() {
            "Boot" => Some(DeviceControlKind::TeleBoot),
            _ => None,
        }
    } else if let Some(hp) = c.home_position {
        let enabled = hp.enabled.trim().parse::<u32>().ok()?;
        Some(DeviceControlKind::HomePosition {
            enabled,
            reset_time: hp.reset_time.and_then(|v| v.trim().parse::<u32>().ok()),
            preset_index: hp.preset_index.and_then(|v| v.trim().parse::<u32>().ok()),
        })
    } else {
        c.ptz_cmd
            .map(|hex| DeviceControlKind::Ptz(parse_ptz_command(&hex)))
    }?;
    Some(DeviceControl {
        sn: c.sn,
        device_id: c.device_id,
        kind,
    })
}

// ---------------------------------------------------------------------------
// DeviceConfig sub-command decode (GB/T 28181-2022 §9.3.3 / A.2.3.2,
// issue #57 minimum). The command family carries one sub-command child
// (A.2.3.2.2-12); this crate decodes the subset a fixed camera can act
// on — BasicParam, FrameMirror, AlarmReport — everything else keeps the
// control-reject behavior. 校时 is NOT part of this family (2022 §9.10.2
// does it via the REGISTER response's SIP Date header).
// ---------------------------------------------------------------------------

/// A decoded DeviceConfig sub-command (issue #57).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceConfigKind {
    /// A.2.3.2.2 基本参数配置 — device name + registration tuning. The
    /// library does NOT hot-apply these; hosts decide what sticks.
    BasicParam {
        name: Option<String>,
        expiration: Option<u64>,
        heartbeat_interval: Option<u64>,
        heartbeat_count: Option<u32>,
    },
    /// A.2.3.2.9 画面翻转配置 — 0 none, 1 horizontal, 2 vertical, 3 both
    /// (A.2.1.22 frameMirrorCfgType).
    FrameMirror(u32),
    /// A.2.3.2.10 报警上报开关配置 — motion-detection / field-detection
    /// event report switches (0 off, 1 on).
    AlarmReport {
        motion_detection: u32,
        field_detection: u32,
    },
}

/// An inbound DeviceConfig command with a recognized sub-command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConfig {
    pub sn: String,
    pub device_id: String,
    pub kind: DeviceConfigKind,
}

#[derive(Deserialize)]
struct DeviceConfigBody {
    #[serde(rename = "CmdType", default)]
    cmd_type: String,
    #[serde(rename = "SN", default)]
    sn: String,
    #[serde(rename = "DeviceID", default)]
    device_id: String,
    #[serde(rename = "BasicParam", default)]
    basic_param: Option<BasicParamBody>,
    #[serde(
        rename = "FrameMirror",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    frame_mirror: Option<String>,
    #[serde(rename = "AlarmReport", default)]
    alarm_report: Option<AlarmReportBody>,
}

#[derive(Deserialize)]
struct BasicParamBody {
    #[serde(rename = "Name", default, deserialize_with = "empty_string_as_none")]
    name: Option<String>,
    #[serde(
        rename = "Expiration",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    expiration: Option<String>,
    #[serde(
        rename = "HeartBeatInterval",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    heartbeat_interval: Option<String>,
    #[serde(
        rename = "HeartBeatCount",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    heartbeat_count: Option<String>,
}

#[derive(Deserialize)]
struct AlarmReportBody {
    #[serde(rename = "MotionDetection", default)]
    motion_detection: String,
    #[serde(rename = "FieldDetection", default)]
    field_detection: String,
}

/// Parses an inbound DeviceConfig body; `None` when the body is not a
/// DeviceConfig or carries no recognized sub-command (the caller keeps
/// the reject behavior for those).
pub fn parse_device_config(body: &str) -> Option<DeviceConfig> {
    let c: DeviceConfigBody = serde_xml_rs::from_str(body).ok()?;
    if c.cmd_type != "DeviceConfig" {
        return None;
    }
    let kind = if let Some(bp) = c.basic_param {
        Some(DeviceConfigKind::BasicParam {
            name: bp.name,
            expiration: bp.expiration.and_then(|v| v.trim().parse().ok()),
            heartbeat_interval: bp.heartbeat_interval.and_then(|v| v.trim().parse().ok()),
            heartbeat_count: bp.heartbeat_count.and_then(|v| v.trim().parse().ok()),
        })
    } else if let Some(fm) = c
        .frame_mirror
        .as_deref()
        .and_then(|v| v.trim().parse::<u32>().ok())
    {
        Some(DeviceConfigKind::FrameMirror(fm))
    } else if let Some(ar) = c.alarm_report {
        let motion_detection = ar.motion_detection.trim().parse().ok()?;
        let field_detection = ar.field_detection.trim().parse().ok()?;
        Some(DeviceConfigKind::AlarmReport {
            motion_detection,
            field_detection,
        })
    } else {
        None
    }?;
    Some(DeviceConfig {
        sn: c.sn,
        device_id: c.device_id,
        kind,
    })
}

/// Parses a `<Notify>` body in either child-element or attribute format,
/// normalizing into [`Notify`].
#[allow(dead_code)]
// wired in R2 (dispatch from client.rs)
pub(crate) fn parse_notify_dual(body: &str) -> Option<Notify> {
    // Try child-element format first (matches live MiBee NVR).
    if let Ok(n) = serde_xml_rs::from_str::<Notify>(body) {
        if !n.cmd_type.is_empty() {
            return Some(n);
        }
    }
    // Fallback: try attribute format.
    if let Ok(na) = serde_xml_rs::from_str::<NotifyAttr>(body) {
        if !na.cmd_type.is_empty() {
            return Some(Notify {
                cmd_type: na.cmd_type,
                sn: na.sn,
                device_id: na.device_id,
                status: None,
            });
        }
    }
    None
}

#[cfg(test)]
mod proptests {
    //! Property tests: the dual-format XML parsers must never panic on
    //! arbitrary input (issue #29) — malformed bodies surface as None/Err.
    use super::*;
    use proptest::prelude::*;

    proptest::proptest! {
        #[test]
        fn parse_query_dual_never_panics(body in proptest::collection::vec(any::<char>(), 0..512)) {
            let s: String = body.into_iter().collect();
            let _ = parse_query_dual(&s);
        }

        #[test]
        fn parse_notify_dual_never_panics(body in proptest::collection::vec(any::<char>(), 0..512)) {
            let s: String = body.into_iter().collect();
            let _ = parse_notify_dual(&s);
        }

        #[test]
        fn query_roundtrip_via_elements(cmd in "[A-Za-z]{1,16}", sn in "[0-9]{1,6}", id in "[0-9]{1,20}") {
            let body = format!("<Query><CmdType>{cmd}</CmdType><SN>{sn}</SN><DeviceID>{id}</DeviceID></Query>");
            let q = parse_query_dual(&body).expect("element form must parse");
            prop_assert_eq!(q.cmd_type, cmd);
            prop_assert_eq!(q.sn, sn);
            prop_assert_eq!(q.device_id, id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── DeviceControl sub-command decode (#58) ────────────────────────────

    fn control_body(sub: &str) -> String {
        format!(
            "<Control><CmdType>DeviceControl</CmdType><SN>17</SN>\
             <DeviceID>34020000001320000001</DeviceID>{sub}</Control>"
        )
    }

    #[test]
    fn device_control_iframe_cmd_send_decodes() {
        let c = parse_device_control(&control_body("<IFrameCmd>Send</IFrameCmd>"))
            .expect("IFrameCmd Send must decode");
        assert_eq!(c.sn, "17");
        assert_eq!(c.device_id, "34020000001320000001");
        assert_eq!(c.kind, DeviceControlKind::ForceIFrame);
    }

    #[test]
    fn device_control_iframe_cmd_unknown_value_is_rejected() {
        // An unrecognized IFrameCmd value must fall to the reject path,
        // never silently execute.
        assert!(parse_device_control(&control_body("<IFrameCmd>Later</IFrameCmd>")).is_none());
    }

    #[test]
    fn device_control_record_cmd_both_values() {
        let c = parse_device_control(&control_body("<RecordCmd>Record</RecordCmd>"))
            .expect("RecordCmd Record must decode");
        assert_eq!(c.kind, DeviceControlKind::Record(true));
        let c = parse_device_control(&control_body("<RecordCmd>StopRecord</RecordCmd>"))
            .expect("RecordCmd StopRecord must decode");
        assert_eq!(c.kind, DeviceControlKind::Record(false));
        assert!(parse_device_control(&control_body("<RecordCmd>Bogus</RecordCmd>")).is_none());
    }

    #[test]
    fn device_control_guard_and_alarm_and_teleboot() {
        assert_eq!(
            parse_device_control(&control_body("<GuardCmd>SetGuard</GuardCmd>"))
                .unwrap()
                .kind,
            DeviceControlKind::Guard(true)
        );
        assert_eq!(
            parse_device_control(&control_body("<GuardCmd>ResetGuard</GuardCmd>"))
                .unwrap()
                .kind,
            DeviceControlKind::Guard(false)
        );
        assert_eq!(
            parse_device_control(&control_body("<AlarmCmd>ResetAlarm</AlarmCmd>"))
                .unwrap()
                .kind,
            DeviceControlKind::ResetAlarm
        );
        assert_eq!(
            parse_device_control(&control_body("<TeleBoot>Boot</TeleBoot>"))
                .unwrap()
                .kind,
            DeviceControlKind::TeleBoot
        );
        // Unknown GuardCmd value falls to reject.
        assert!(parse_device_control(&control_body("<GuardCmd>Nope</GuardCmd>")).is_none());
    }

    #[test]
    fn device_control_ptz_cmd_decodes() {
        // A.3/A.4 command, bit-level decoded (#57).
        let c = parse_device_control(&control_body("<PTZCmd>A50F0102200000D7</PTZCmd>"))
            .expect("PTZCmd must parse");
        assert_eq!(
            c.kind,
            DeviceControlKind::Ptz(PtzCommand::Move {
                bits: PTZ_LEFT,
                pan_speed: 0x20,
                tilt_speed: 0,
                zoom_speed: 0
            })
        );
        // Undecodable hex still parses into a control — delivered as
        // Invalid with the raw string preserved (no reject path).
        let c = parse_device_control(&control_body("<PTZCmd>A50F01021F00</PTZCmd>"))
            .expect("PTZCmd must parse");
        assert_eq!(
            c.kind,
            DeviceControlKind::Ptz(PtzCommand::Invalid {
                raw: "A50F01021F00".to_string()
            })
        );
    }

    /// Golden hex table — the byte-exact outputs of the gb28181-go
    /// platform builders (BuildPTZCommand / preset / cruise / FI / aux,
    /// GB/T 28181 §A.3-A.4). The same table pins the Go twin
    /// (device.DecodePTZCommand); keep both in sync.
    #[test]
    fn parse_ptz_command_goldens() {
        let cases: &[(&str, PtzCommand)] = &[
            // BuildPTZCommand(direction, 0x20).
            (
                "A50F0100000000B5",
                PtzCommand::Move {
                    bits: 0,
                    pan_speed: 0,
                    tilt_speed: 0,
                    zoom_speed: 0,
                },
            ),
            (
                "A50F0108002000DD",
                PtzCommand::Move {
                    bits: PTZ_UP,
                    pan_speed: 0,
                    tilt_speed: 0x20,
                    zoom_speed: 0,
                },
            ),
            (
                "A50F0104002000D9",
                PtzCommand::Move {
                    bits: PTZ_DOWN,
                    pan_speed: 0,
                    tilt_speed: 0x20,
                    zoom_speed: 0,
                },
            ),
            (
                "A50F0102200000D7",
                PtzCommand::Move {
                    bits: PTZ_LEFT,
                    pan_speed: 0x20,
                    tilt_speed: 0,
                    zoom_speed: 0,
                },
            ),
            (
                "A50F0101200000D6",
                PtzCommand::Move {
                    bits: PTZ_RIGHT,
                    pan_speed: 0x20,
                    tilt_speed: 0,
                    zoom_speed: 0,
                },
            ),
            (
                "A50F010A202000FF",
                PtzCommand::Move {
                    bits: PTZ_UP | PTZ_LEFT,
                    pan_speed: 0x20,
                    tilt_speed: 0x20,
                    zoom_speed: 0,
                },
            ),
            (
                "A50F0109202000FE",
                PtzCommand::Move {
                    bits: PTZ_UP | PTZ_RIGHT,
                    pan_speed: 0x20,
                    tilt_speed: 0x20,
                    zoom_speed: 0,
                },
            ),
            (
                "A50F0106202000FB",
                PtzCommand::Move {
                    bits: PTZ_DOWN | PTZ_LEFT,
                    pan_speed: 0x20,
                    tilt_speed: 0x20,
                    zoom_speed: 0,
                },
            ),
            (
                "A50F0105202000FA",
                PtzCommand::Move {
                    bits: PTZ_DOWN | PTZ_RIGHT,
                    pan_speed: 0x20,
                    tilt_speed: 0x20,
                    zoom_speed: 0,
                },
            ),
            (
                "A50F0110000020E5",
                PtzCommand::Move {
                    bits: PTZ_ZOOM_IN,
                    pan_speed: 0,
                    tilt_speed: 0,
                    zoom_speed: 0x20,
                },
            ),
            (
                "A50F0120000020F5",
                PtzCommand::Move {
                    bits: PTZ_ZOOM_OUT,
                    pan_speed: 0,
                    tilt_speed: 0,
                    zoom_speed: 0x20,
                },
            ),
            // BuildPTZPresetCommand(action, 5).
            (
                "A50F01810000053B",
                PtzCommand::Preset {
                    action: PtzPresetAction::Set,
                    preset: 5,
                },
            ),
            (
                "A50F01820000053C",
                PtzCommand::Preset {
                    action: PtzPresetAction::Call,
                    preset: 5,
                },
            ),
            (
                "A50F01830000053D",
                PtzCommand::Preset {
                    action: PtzPresetAction::Delete,
                    preset: 5,
                },
            ),
            // BuildPTZCruiseCommand(action, 2, 7).
            (
                "A50F018402000742",
                PtzCommand::Cruise {
                    action: PtzCruiseAction::AddPoint,
                    group: 2,
                    value: 7,
                },
            ),
            (
                "A50F018502000743",
                PtzCommand::Cruise {
                    action: PtzCruiseAction::DelPoint,
                    group: 2,
                    value: 7,
                },
            ),
            (
                "A50F018602000744",
                PtzCommand::Cruise {
                    action: PtzCruiseAction::Speed,
                    group: 2,
                    value: 7,
                },
            ),
            (
                "A50F018702000745",
                PtzCommand::Cruise {
                    action: PtzCruiseAction::StayTime,
                    group: 2,
                    value: 7,
                },
            ),
            (
                "A50F018802000746",
                PtzCommand::Cruise {
                    action: PtzCruiseAction::Start,
                    group: 2,
                    value: 7,
                },
            ),
            // BuildFICommand(action, 0x40), §A.3.3.
            (
                "A50F01480040003D",
                PtzCommand::Lens {
                    bits: PTZ_IRIS_CLOSE,
                    focus_speed: 0,
                    iris_speed: 0x40,
                },
            ),
            (
                "A50F014400400039",
                PtzCommand::Lens {
                    bits: PTZ_IRIS_OPEN,
                    focus_speed: 0,
                    iris_speed: 0x40,
                },
            ),
            (
                "A50F014240000037",
                PtzCommand::Lens {
                    bits: PTZ_FOCUS_NEAR,
                    focus_speed: 0x40,
                    iris_speed: 0,
                },
            ),
            (
                "A50F014140000036",
                PtzCommand::Lens {
                    bits: PTZ_FOCUS_FAR,
                    focus_speed: 0x40,
                    iris_speed: 0,
                },
            ),
            (
                "A50F0140000000F5",
                PtzCommand::Lens {
                    bits: 0,
                    focus_speed: 0,
                    iris_speed: 0,
                },
            ),
            // BuildAuxSwitchCommand(1, on), §A.3.7.
            (
                "A50F018C01000042",
                PtzCommand::AuxSwitch {
                    number: 1,
                    on: true,
                },
            ),
            (
                "A50F018D01000043",
                PtzCommand::AuxSwitch {
                    number: 1,
                    on: false,
                },
            ),
        ];
        for (hex, want) in cases {
            let got = parse_ptz_command(hex);
            assert_eq!(&got, want, "golden {hex}");
            if let PtzCommand::Move { bits, .. } = want {
                assert_eq!(got.is_stop(), *bits == 0, "is_stop for {hex}");
            }
        }
    }

    #[test]
    fn parse_ptz_command_invalid_and_unknown() {
        for bad in [
            "A50F01",
            "A50F0100000000B500",
            "A50F0100000000B",
            "ZZ0F0100000000B5",
            "950F0100000000B5",
            "A50F0100000000FF",
        ] {
            assert_eq!(
                parse_ptz_command(bad),
                PtzCommand::Invalid {
                    raw: bad.to_string()
                },
                "input {bad}"
            );
        }
        // Lowercase hex accepted; surrounding whitespace trimmed.
        assert_eq!(
            parse_ptz_command(" a50f0108002000dd "),
            PtzCommand::Move {
                bits: PTZ_UP,
                pan_speed: 0,
                tilt_speed: 0x20,
                zoom_speed: 0
            }
        );
        // Structurally valid, unrecognized instruction code → Unknown
        // with the data bytes (checksum: A5+0F+01+99+11+22+33 = 0xB4).
        assert_eq!(
            parse_ptz_command("A50F0199112233B4"),
            PtzCommand::Unknown {
                code: 0x99,
                data: [0x11, 0x22, 0x33]
            }
        );
    }

    #[test]
    fn device_control_home_position_decodes() {
        // A.2.3.1.10 看守位 control: Enabled required, ResetTime /
        // PresetIndex optional (absent = keep current).
        let c = parse_device_control(
            "<Control><CmdType>DeviceControl</CmdType><SN>9</SN><DeviceID>d</DeviceID>\
             <HomePosition><Enabled>1</Enabled><ResetTime>300</ResetTime>\
             <PresetIndex>7</PresetIndex></HomePosition></Control>",
        )
        .expect("HomePosition must parse");
        assert_eq!(
            c.kind,
            DeviceControlKind::HomePosition {
                enabled: 1,
                reset_time: Some(300),
                preset_index: Some(7),
            }
        );
        let c = parse_device_control(
            "<Control><CmdType>DeviceControl</CmdType><SN>10</SN><DeviceID>d</DeviceID>\
             <HomePosition><Enabled>0</Enabled></HomePosition></Control>",
        )
        .expect("minimal HomePosition must parse");
        assert_eq!(
            c.kind,
            DeviceControlKind::HomePosition {
                enabled: 0,
                reset_time: None,
                preset_index: None,
            }
        );
        // Non-numeric Enabled is not a command we recognize.
        assert!(parse_device_control(
            "<Control><CmdType>DeviceControl</CmdType><SN>11</SN><DeviceID>d</DeviceID>\
             <HomePosition><Enabled>on</Enabled></HomePosition></Control>"
        )
        .is_none());
    }

    #[test]
    fn device_config_kinds_decode() {
        // A.2.3.2.2 BasicParam (every child optional).
        let c = parse_device_config(
            "<Control><CmdType>DeviceConfig</CmdType><SN>71</SN><DeviceID>d</DeviceID>\
             <BasicParam><Name>Dome</Name><Expiration>120</Expiration>\
             <HeartBeatInterval>15</HeartBeatInterval><HeartBeatCount>5</HeartBeatCount>\
             </BasicParam></Control>",
        )
        .expect("BasicParam must parse");
        assert_eq!(
            c.kind,
            DeviceConfigKind::BasicParam {
                name: Some("Dome".to_string()),
                expiration: Some(120),
                heartbeat_interval: Some(15),
                heartbeat_count: Some(5),
            }
        );
        // A.2.3.2.9 FrameMirror (A.2.1.22: 0-3).
        let c = parse_device_config(
            "<Control><CmdType>DeviceConfig</CmdType><SN>72</SN><DeviceID>d</DeviceID>\
             <FrameMirror>1</FrameMirror></Control>",
        )
        .expect("FrameMirror must parse");
        assert_eq!(c.kind, DeviceConfigKind::FrameMirror(1));
        // A.2.3.2.10 AlarmReport switches.
        let c = parse_device_config(
            "<Control><CmdType>DeviceConfig</CmdType><SN>73</SN><DeviceID>d</DeviceID>\
             <AlarmReport><MotionDetection>1</MotionDetection>\
             <FieldDetection>0</FieldDetection></AlarmReport></Control>",
        )
        .expect("AlarmReport must parse");
        assert_eq!(
            c.kind,
            DeviceConfigKind::AlarmReport {
                motion_detection: 1,
                field_detection: 0,
            }
        );
        // Unrecognized sub-commands (SVAC, OSD, …) and other CmdTypes
        // yield None — the server keeps its reject behavior.
        assert!(parse_device_config(
            "<Control><CmdType>DeviceConfig</CmdType><SN>74</SN><DeviceID>d</DeviceID>\
             <OSDConfig><OSDText>x</OSDText></OSDConfig></Control>"
        )
        .is_none());
        assert!(parse_device_config(
            "<Control><CmdType>DeviceControl</CmdType><SN>75</SN><DeviceID>d</DeviceID>\
             <IFrameCmd>Send</IFrameCmd></Control>"
        )
        .is_none());
    }

    #[test]
    fn device_control_unknown_subcommand_is_none() {
        // No recognized sub-command (e.g. DragZoom, deferred, or an empty
        // body) → None so the server keeps the control-reject behavior.
        assert!(parse_device_control(&control_body("<DragZoomIn/>")).is_none());
        assert!(parse_device_control(&control_body("")).is_none());
    }

    #[test]
    fn device_control_ignores_non_control_bodies() {
        // Snapshot controls route through parse_control_snapshot; a Keepalive
        // Notify is not a Control at all.
        let snap = "<Control><CmdType>DeviceControl</CmdType><SN>1</SN><DeviceID>d</DeviceID>\
                    <SnapShot><SnapNum>1</SnapNum><UploadURL>u</UploadURL>\
                    <SessionID>s</SessionID></SnapShot></Control>";
        assert!(parse_device_control(snap).is_none());
        assert!(
            parse_device_control("<Notify><CmdType>Keepalive</CmdType><SN>1</SN></Notify>")
                .is_none()
        );
    }

    #[test]
    fn test_channel_item_serialize() {
        let item = ChannelItem {
            device_id: "31011500991320000001".to_string(),
            name: "Camera 1".to_string(),
            manufacturer: "MiBee".to_string(),
            model: "Mibee-Cam-01".to_string(),
            owner: "Admin".to_string(),
            civil_code: "310115".to_string(),
            address: "Test Location".to_string(),
            parental: 0,
            parent_id: "31011500991320000000".to_string(),
            safety_way: 0,
            register_way: 1,
            secrecy: 0,
            status: "ON".to_string(),
            ip_address: "192.168.1.100".to_string(),
            port: 5060,
            longitude: 121.4737,
            latitude: 31.2304,
        };

        let xml = serde_xml_rs::to_string(&item).unwrap();
        assert!(xml.contains("<DeviceID>"));
        assert!(xml.contains("31011500991320000001"));
    }

    #[test]
    fn test_device_item_serialize() {
        let device = DeviceItem {
            device_id: "31011500991320000001".to_string(),
            name: "Mibee Camera".to_string(),
            manufacturer: "MiBee".to_string(),
            model: "Mibee-Cam-01".to_string(),
            firmware: "v1.0.0".to_string(),
        };

        let xml = serde_xml_rs::to_string(&device).unwrap();
        assert!(xml.contains("<DeviceID>"));
        assert!(xml.contains("<Firmware>"));
    }

    #[test]
    fn test_response_catalog_fields() {
        // Verify Response struct fields are correctly defined
        let response = Response {
            cmd_type: "Catalog".to_string(),
            sn: "123".to_string(),
            device_id: "31011500991320000001".to_string(),
            sum_num: Some(1),
            device_list: Some(DeviceList { item: vec![] }),
            device: None,
        };
        assert_eq!(response.cmd_type, "Catalog");
        assert_eq!(response.sum_num, Some(1));
        assert!(response.device_list.is_some());
        assert!(response.device.is_none());
    }

    #[test]
    fn test_parse_query_dual_child_element() {
        let xml = "<Query><CmdType>Catalog</CmdType><SN>7</SN><DeviceID>34020000001320000001</DeviceID></Query>";
        let q = parse_query_dual(xml).expect("child-element query should parse");
        assert_eq!(q.cmd_type, "Catalog");
        assert_eq!(q.sn, "7");
        assert_eq!(q.device_id, "34020000001320000001");
    }

    #[test]
    fn test_parse_query_dual_attribute_format() {
        let xml = "<Query CmdType=\"Catalog\" SN=\"7\" DeviceID=\"34020000001320000001\" />";
        let q = parse_query_dual(xml).expect("attribute query should parse");
        assert_eq!(q.cmd_type, "Catalog");
        assert_eq!(q.sn, "7");
        assert_eq!(q.device_id, "34020000001320000001");
    }

    #[test]
    fn test_parse_query_dual_garbage() {
        assert!(parse_query_dual("<Garbage/>").is_none());
    }

    #[test]
    fn test_parse_notify_dual_child_element() {
        let xml = "<Notify><CmdType>Keepalive</CmdType><SN>1</SN><DeviceID>34020000001320000001</DeviceID></Notify>";
        let n = parse_notify_dual(xml).expect("child-element notify should parse");
        assert_eq!(n.cmd_type, "Keepalive");
        assert_eq!(n.sn, "1");
        assert_eq!(n.device_id, "34020000001320000001");
    }

    #[test]
    fn test_parse_notify_dual_attribute_format() {
        let xml = "<Notify CmdType=\"Keepalive\" SN=\"1\" DeviceID=\"x\" Keepalive=\"1\" />";
        let n = parse_notify_dual(xml).expect("attribute notify should parse");
        assert_eq!(n.cmd_type, "Keepalive");
        assert_eq!(n.sn, "1");
        assert_eq!(n.device_id, "x");
    }
}

#[test]
fn test_parse_recordinfo_query_child_element() {
    // Z-suffixed (UTC) times keep this deterministic on any machine TZ;
    // naive parsing is covered by test_parse_gb_time_ms_naive_local.
    let xml = "<Query><CmdType>RecordInfo</CmdType><SN>9</SN><DeviceID>34020000001320000001</DeviceID><StartTime>2026-08-15T14:30:00Z</StartTime><EndTime>2026-08-15T15:00:00Z</EndTime><Type>time</Type><StreamType>0</StreamType></Query>";
    let q = parse_recordinfo_query_dual(xml).expect("recordinfo query should parse");
    assert_eq!(q.sn, "9");
    assert_eq!(q.device_id, "34020000001320000001");
    assert_eq!(q.start_ms, Some(1_786_804_200_000));
    assert_eq!(q.end_ms, Some(1_786_806_000_000));
    assert_eq!(q.r#type.as_deref(), Some("time"));
    assert_eq!(q.stream_type.as_deref(), Some("0"));
}

#[test]
fn test_parse_recordinfo_query_attribute_format() {
    let xml = "<Query CmdType=\"RecordInfo\" SN=\"9\" DeviceID=\"34020000001320000001\" StartTime=\"2026-08-15T14:30:00Z\" EndTime=\"2026-08-15T15:00:00Z\" />";
    let q = parse_recordinfo_query_dual(xml).expect("attribute recordinfo query should parse");
    assert_eq!(q.sn, "9");
    assert_eq!(q.device_id, "34020000001320000001");
    assert_eq!(q.start_ms, Some(1_786_804_200_000));
    assert_eq!(q.end_ms, Some(1_786_806_000_000));
}

#[test]
fn test_parse_recordinfo_query_wrong_cmdtype() {
    let xml = "<Query><CmdType>Catalog</CmdType><SN>9</SN><DeviceID>x</DeviceID></Query>";
    assert!(parse_recordinfo_query_dual(xml).is_none());
}

#[test]
fn test_parse_gb_time_ms_utc() {
    // Explicit offset (Z/±HH:MM) is TZ-independent.
    assert_eq!(
        parse_gb_time_ms("2026-08-15T14:30:00Z"),
        Some(1_786_804_200_000)
    );
    // Naive times go through device-local offset via _with(0) = UTC.
    assert_eq!(
        parse_gb_time_ms_with("2026-08-15T14:30:00", 0),
        Some(1_786_804_200_000)
    );
}

#[test]
fn test_parse_gb_time_ms_naive_local() {
    // Naive time on a +08:00 device: 14:30 local = 06:30 UTC.
    assert_eq!(
        parse_gb_time_ms_with("2026-08-15T14:30:00", 28_800),
        Some(1_786_775_400_000)
    );
}

#[test]
fn test_parse_gb_time_ms_offset() {
    // +08:00 means local 14:30 is 06:30 UTC.
    assert_eq!(
        parse_gb_time_ms("2026-08-15T14:30:00+08:00"),
        Some(1_786_775_400_000)
    );
    // -05:00 means local 14:30 is 19:30 UTC.
    assert_eq!(
        parse_gb_time_ms("2026-08-15T14:30:00-05:00"),
        Some(1_786_822_200_000)
    );
}

#[test]
fn test_parse_gb_time_ms_invalid() {
    assert!(parse_gb_time_ms("").is_none());
    assert!(parse_gb_time_ms("garbage").is_none());
    assert!(parse_gb_time_ms("2026-13-15T14:30:00").is_none());
    assert!(parse_gb_time_ms("2026-08-15T14:30:00+8:0").is_none());
}

#[cfg(test)]
mod snapshot_tests {
    //! GB/T 28181-2022 snapshot wire types — the goldens are byte-identical
    //! to the Go twin's (gb28181-go manscdp/snapshot_test.go, issue #49).

    use super::*;

    const SESSION_ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn parse_control_snapshot_golden() {
        let body = "<Control><CmdType>DeviceControl</CmdType><SN>17</SN><DeviceID>34020000001320000001</DeviceID>\
<SnapShot><SnapNum>3</SnapNum><Interval>2</Interval>\
<UploadURL>http://192.168.63.30:9090/api/gb28181/snapshot/upload</UploadURL>\
<SessionID>0123456789abcdef0123456789abcdef</SessionID></SnapShot></Control>";
        let c = parse_control_snapshot(body).expect("snapshot control parses");
        assert_eq!(c.sn, "17");
        assert_eq!(c.device_id, "34020000001320000001");
        assert_eq!(c.snap_shot.snap_num, 3);
        assert_eq!(c.snap_shot.interval, Some(2));
        assert_eq!(
            c.snap_shot.upload_url,
            "http://192.168.63.30:9090/api/gb28181/snapshot/upload"
        );
        assert_eq!(c.snap_shot.session_id, SESSION_ID);

        // Manual snapshot omits Interval.
        let manual = "<Control><CmdType>DeviceControl</CmdType><SN>1</SN><DeviceID>d</DeviceID>\
<SnapShot><SnapNum>1</SnapNum><UploadURL>http://x/u</UploadURL><SessionID>0123456789abcdef0123456789abcdef</SessionID></SnapShot></Control>";
        assert_eq!(
            parse_control_snapshot(manual).unwrap().snap_shot.interval,
            None
        );

        // Non-snapshot controls and garbage return None.
        assert!(parse_control_snapshot("<Control><CmdType>DeviceControl</CmdType><SN>1</SN><DeviceID>d</DeviceID><RecordCmd>Record</RecordCmd></Control>").is_none());
        assert!(parse_control_snapshot("<not-xml").is_none());
    }

    #[test]
    fn upload_snapshot_finished_roundtrip_golden() {
        let n = build_upload_snapshot_finished(
            18,
            "34020000001320000001",
            SESSION_ID,
            &["f-1".to_string(), "f-2".to_string()],
        );
        let xml = n.to_xml().unwrap();
        assert_eq!(
            xml,
            "<Notify><CmdType>UploadSnapShotFinished</CmdType><SN>18</SN>\
<DeviceID>34020000001320000001</DeviceID><SessionID>0123456789abcdef0123456789abcdef</SessionID>\
<SnapShotList><SnapShotFileID>f-1</SnapShotFileID><SnapShotFileID>f-2</SnapShotFileID></SnapShotList></Notify>"
        );
        // A platform (or the Go twin) re-parses byte-identically.
        let back: UploadSnapShotFinished = serde_xml_rs::from_str(&xml).unwrap();
        assert_eq!(back, n);

        // Empty list = wholly/partially failed capture/upload.
        let empty = build_upload_snapshot_finished(1, "d", SESSION_ID, &[]);
        let xml = empty.to_xml().unwrap();
        let back: UploadSnapShotFinished = serde_xml_rs::from_str(&xml).unwrap();
        assert!(back.snap_shot_list.snap_shot_file_id.is_empty());
    }
}
