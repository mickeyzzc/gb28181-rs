//! MANSCDP + device-ID offline demo — no sockets, pure message layer.
//!
//! Walks the message-building and parsing surface a host integrates with:
//!
//! 1. Device IDs: `format_device_id` / `parse_device_id` round-trip and the
//!    registered type codes (`device_types`)
//! 2. Keepalive `Notify` — build the exact wire MESSAGE and parse the body
//!    back into the crate's `Notify` type
//! 3. `RecordInfo` response — two recordings listed, items + SumNum parsed
//!    back out the way a foreign NVR reads them
//! 4. `DeviceInfo` response — firmware / model fields extracted
//! 5. GB/T 28181 time strings — `format_gb_time_ms` output shape
//!
//! ```sh
//! cargo run --example manscdp_demo
//! ```

use anyhow::{bail, Context, Result};

use gb28181_rs::client::{
    build_device_info_response, build_keepalive_notify, build_recordinfo_response,
    format_gb_time_ms, RecordItem,
};
use gb28181_rs::device_id::{format_device_id, parse_device_id};
use gb28181_rs::device_types;
use gb28181_rs::manscdp::{DeviceItem, Notify};

const DEVICE_ID: &str = "34020000001320000001";
const SIP_DOMAIN: &str = "3402000000";

fn xml_field(xml: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open).map_or(0, |p| p + open.len());
    let end = xml[start..].find(&close).map_or(start, |e| start + e);
    xml[start..end].to_string()
}

fn main() -> Result<()> {
    println!("== gb28181-rs MANSCDP + device-ID demo (offline) ==\n");

    // -- 1. Device IDs ------------------------------------------------------
    let id = format_device_id("34020000", 0, 132, 1).context("format device id")?;
    if id != DEVICE_ID {
        bail!("format_device_id produced {id}, want {DEVICE_ID}");
    }
    let parts = parse_device_id(&id).context("parse device id")?;
    println!(
        "[device-id] {id} <-> region={} industry={} type={} serial={}",
        parts.region_code, parts.industry_type, parts.device_type, parts.serial
    );
    if parse_device_id("123").is_ok() {
        bail!("short ID must be rejected");
    }
    println!(
        "[device-id] known types: IPC={} NVR={} ALARM={}",
        device_types::IPC,
        device_types::NVR,
        device_types::ALARM
    );

    // -- 2. Keepalive Notify ------------------------------------------------
    let msg = build_keepalive_notify("9", DEVICE_ID, SIP_DOMAIN, "192.168.62.104", 5060, "OK", 42)?;
    let body = &msg.body;
    println!("[keepalive] wire body: {body}");
    let notify: Notify = serde_xml_rs::from_str(body).context("parse Notify body")?;
    if notify.cmd_type != "Keepalive" || notify.sn != "9" {
        bail!("Notify parse mismatch: {notify:?}");
    }
    println!(
        "[keepalive] parsed back: CmdType={} SN={} Status=OK",
        notify.cmd_type, notify.sn
    );

    // -- 3. RecordInfo response ----------------------------------------------
    let items = vec![
        RecordItem {
            device_id: DEVICE_ID.to_string(),
            name: "14-30-00".to_string(),
            file_path: "2026/08/15/14-30-00.h264".to_string(),
            address: DEVICE_ID.to_string(),
            start_time: format_gb_time_ms(1_786_804_200_000),
            end_time: format_gb_time_ms(1_786_804_500_000),
            secrecy: "0".to_string(),
            r#type: "time".to_string(),
        },
        RecordItem {
            device_id: DEVICE_ID.to_string(),
            name: "14-40-00".to_string(),
            file_path: "2026/08/15/14-40-00.h264".to_string(),
            address: DEVICE_ID.to_string(),
            start_time: format_gb_time_ms(1_786_804_800_000),
            end_time: format_gb_time_ms(1_786_805_100_000),
            secrecy: "0".to_string(),
            r#type: "time".to_string(),
        },
    ];
    let resp = build_recordinfo_response(
        "7",
        DEVICE_ID,
        SIP_DOMAIN,
        "192.168.62.104",
        5060,
        42,
        &items,
    )?;
    let body = &resp.body;
    let sum = xml_field(body, "SumNum");
    let listed = body.matches("<Item>").count();
    println!("[recordinfo] SumNum={sum}, {listed} item(s)");
    if sum != "2" || listed != 2 {
        bail!("RecordInfo response must list 2 items, got SumNum={sum} items={listed}");
    }
    let first_file = xml_field(body, "FilePath");
    println!("[recordinfo] first file: {first_file}");
    if first_file != "2026/08/15/14-30-00.h264" {
        bail!("unexpected first file path: {first_file}");
    }

    // -- 4. DeviceInfo response ----------------------------------------------
    let info = build_device_info_response(
        "3",
        DEVICE_ID,
        SIP_DOMAIN,
        "192.168.62.104",
        5060,
        42,
        &DeviceItem {
            device_id: DEVICE_ID.to_string(),
            name: "front-door".to_string(),
            manufacturer: "MiBee".to_string(),
            model: "OV5647-Cam".to_string(),
            firmware: "0.5.0".to_string(),
        },
    )?;
    let body = &info.body;
    println!(
        "[deviceinfo] {} {} {} v{}",
        xml_field(body, "Manufacturer"),
        xml_field(body, "Model"),
        xml_field(body, "DeviceID"),
        xml_field(body, "Firmware")
    );
    if xml_field(body, "Manufacturer") != "MiBee" || xml_field(body, "Firmware") != "0.5.0" {
        bail!("DeviceInfo fields mismatch: {body}");
    }

    // -- 5. GB/T 28181 time format -------------------------------------------
    let t = format_gb_time_ms(1_786_804_200_000);
    println!("[time] 1_786_804_200_000 ms -> {t} (local-offset aware)");
    if t.len() != 19 || !t.contains('T') {
        bail!("unexpected GB time shape: {t}");
    }

    println!("\nmanscdp demo: all checks passed");
    Ok(())
}
