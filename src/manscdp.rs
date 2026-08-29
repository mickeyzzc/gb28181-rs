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
pub fn device_local_offset_secs() -> i64 {
    #[cfg(unix)]
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as libc::time_t)
            .unwrap_or(0);
        // SAFETY: `tm` is a plain struct and `localtime_r` writes it without
        // retaining the pointer; a NULL return leaves the zeroed fallback.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&now, &mut tm).is_null() {
                0
            } else {
                tm.tm_gmtoff
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

/// Parses a `<Notify>` body in either child-element or attribute format,
/// normalizing into [`Notify`].
#[allow(dead_code)] // wired in R2 (dispatch from client.rs)
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
mod tests {
    use super::*;

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
