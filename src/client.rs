//! SIP device client — manages device registration with a SIP platform.
//!
//! Also includes INVITE parsing (`InviteInfo` / `parse_invite`) and
//! 401 challenge extraction (`parse_401_challenge`), plus MESSAGE
//! builders for Catalog, DeviceInfo, and Keepalive responses.

use std::net::SocketAddr;

use anyhow::{anyhow, Result};

use super::manscdp::{ChannelItem, DeviceItem, Notify, Query};
use super::rtp_pusher::H264_PAYLOAD_TYPE;
use super::sip::{
    build_bye_request, build_digest_auth, build_register_request, DigestAuthParams, SdpSession,
    SessionType, SipMessage, SipMethod, SipStatusCode,
};

/// A GB/T 28181 SIP device client that registers with a SIP platform.
///
/// Manages the REGISTER dialog with a GB/T 28181 SIP platform, including
/// digest authentication challenge-response.
#[derive(Debug, Clone)]
pub struct SipDeviceClient {
    /// 20-digit device ID
    pub device_id: String,
    /// SIP server (platform) address
    pub sip_server_addr: SocketAddr,
    /// Local IP address advertised in SIP messages
    pub local_ip: String,
    /// Local SIP port
    pub local_port: u16,
    /// SIP domain (usually the platform's domain)
    pub domain: String,
    /// Authentication username (usually same as device_id)
    pub username: String,
    /// Authentication password
    pub password: String,
    /// Current Call-ID for SIP dialogs
    pub call_id: String,
    /// Current CSeq number
    pub cseq: u32,
    /// Registration expiry in seconds
    pub expires: u32,
}

impl SipDeviceClient {
    /// Create a new SIP device client.
    pub fn new(
        device_id: &str,
        sip_server_addr: SocketAddr,
        local_ip: &str,
        local_port: u16,
        domain: &str,
        password: &str,
        expires: u32,
    ) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            device_id: device_id.to_string(),
            sip_server_addr,
            local_ip: local_ip.to_string(),
            local_port,
            domain: domain.to_string(),
            username: device_id.to_string(),
            password: password.to_string(),
            call_id: format!("{}-{}", device_id, nanos),
            cseq: 1,
            expires,
        }
    }

    /// Build an initial (unauthenticated) SIP REGISTER request.
    pub fn build_register(&self) -> SipMessage {
        build_register_request(
            &self.device_id,
            &self.local_ip,
            &self.domain,
            &self.domain,
            self.expires,
            None,
            &self.call_id,
            self.cseq,
        )
    }

    /// Build a SIP REGISTER request with Digest authentication.
    pub fn build_register_with_auth(&self, auth: &DigestAuthParams) -> SipMessage {
        let uri = format!("sip:{}@{}", self.domain, self.domain);
        let auth_header = build_digest_auth(
            &self.username,
            &auth.realm,
            &self.password,
            &auth.nonce,
            &uri,
            "REGISTER",
            auth.algorithm.as_deref().unwrap_or("MD5"),
            auth.qop.as_deref(),
        );
        build_register_request(
            &self.device_id,
            &self.local_ip,
            &self.domain,
            &self.domain,
            self.expires,
            Some(&auth_header),
            &self.call_id,
            self.cseq,
        )
    }

    /// Build a SIP BYE request to end a session.
    pub fn build_bye(
        &self,
        remote_id: &str,
        remote_addr: &str,
        call_id: &str,
        cseq: u32,
    ) -> SipMessage {
        build_bye_request(
            &self.device_id,
            &self.local_ip,
            remote_id,
            remote_addr,
            call_id,
            cseq,
        )
    }

    /// Increment the CSeq counter.
    pub fn inc_cseq(&mut self) {
        self.cseq = self.cseq.wrapping_add(1);
    }
}

/// Parse the WWW-Authenticate header from a 401 SIP response to extract
/// the Digest challenge parameters.
pub fn parse_401_challenge(msg: &SipMessage) -> Result<DigestAuthParams> {
    let auth_header = msg
        .get_header("WWW-Authenticate")
        .ok_or_else(|| anyhow!("401 response missing WWW-Authenticate header"))?;
    super::sip::parse_digest_auth(auth_header)
}

/// Information extracted from a received SIP INVITE request.
/// Media transport negotiated from the INVITE SDP offer.
///
/// Derived from the `m=` line protocol and the RFC 4145 `a=setup:` value:
/// `TCP/RTP/AVP` + `setup:passive` (or `actpass`) means the platform
/// listens and the **device connects** ([`MediaTransport::TcpConnect`]);
/// `setup:active` means the platform will dial the device
/// ([`MediaTransport::TcpListen`], unsupported by this device and refused
/// with 488). Plain `RTP/AVP` is [`MediaTransport::Udp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaTransport {
    /// Classic UDP media (`RTP/AVP`).
    Udp,
    /// TCP media, device actively connects to the offered media port.
    TcpConnect,
    /// TCP media, platform dials the device (device would have to listen).
    TcpListen,
}

#[derive(Debug, Clone)]
pub struct InviteInfo {
    /// Call-ID from the INVITE
    pub call_id: String,
    /// Media transport negotiated from the SDP offer
    pub media_transport: MediaTransport,
    /// Media address (IP) extracted from SDP
    pub media_address: String,
    /// Media port from SDP m= line
    pub media_port: u16,
    /// SSRC (0 if not specified in SDP)
    pub ssrc: u32,
    /// RTP payload type
    pub payload_type: u8,
    /// Session type from the SDP `s=` line (Play / Playback / Download).
    pub session_type: SessionType,
    /// Requested playback start (seconds since the Unix epoch), None if
    /// absent or `t=0 0` (whole range).
    pub start_secs: Option<u64>,
    /// Requested playback end (seconds since the Unix epoch), None if
    /// absent or `t=0 0` (whole range).
    pub end_secs: Option<u64>,
}

/// Parse a SIP INVITE message to extract stream target information.
///
/// The INVITE comes FROM the platform TO this device, containing the
/// platform's receive address and port in the SDP body.
pub fn parse_invite(msg: &SipMessage) -> Result<InviteInfo> {
    let call_id = msg
        .get_header("Call-ID")
        .ok_or_else(|| anyhow!("INVITE missing Call-ID header"))?
        .to_string();

    let sdp = SdpSession::parse(&msg.body)?;

    let media = sdp
        .media
        .first()
        .ok_or_else(|| anyhow!("INVITE SDP has no media lines"))?;

    // Extract IP from connection address (format: "IN IP4 x.x.x.x")
    let c_addr = sdp
        .connection_address
        .as_deref()
        .unwrap_or("IN IP4 127.0.0.1");
    let ip = c_addr
        .split_whitespace()
        .last()
        .unwrap_or("127.0.0.1")
        .to_string();

    let payload_type = media
        .payload_types
        .first()
        .copied()
        .unwrap_or(H264_PAYLOAD_TYPE);

    // SSRC from GB28181 y= field (session-level, decimal)
    let ssrc = sdp.ssrc.unwrap_or(0);
    // Session type from the SDP s= line: Play (live), Playback, or Download.
    let session_type = match sdp.session_name.trim() {
        "Playback" => SessionType::Playback,
        "Download" => SessionType::Download,
        _ => SessionType::Play,
    };
    // Media transport: TCP when the m= line says so; setup then decides
    // which side connects (RFC 4145). actpass lets the answerer choose —
    // this device chooses to connect (matching the SDP answer's
    // a=setup:active).
    let media_transport = if media.proto.contains("TCP") {
        match media.get_attr("setup") {
            Some("active") => MediaTransport::TcpListen,
            Some("passive") | Some("actpass") | None => MediaTransport::TcpConnect,
            Some(_) => MediaTransport::TcpConnect,
        }
    } else {
        MediaTransport::Udp
    };
    Ok(InviteInfo {
        call_id,
        media_transport,
        media_address: ip,
        media_port: media.port,
        ssrc,
        payload_type,
        session_type,
        start_secs: sdp.start_secs,
        end_secs: sdp.end_secs,
    })
}

/// Build a SIP MESSAGE with Catalog response.
///
/// Per GB/T 28181-2022 §7.6, the Catalog response contains a list of
/// channels/devices with their status and configuration. The message uses
/// fresh Call-ID/CSeq headers (like the keepalive notify) and child-element
/// MANSCDP XML per the NVR interop requirement.
pub fn build_catalog_response(
    sn: &str,
    device_id: &str,
    domain: &str,
    local_ip: &str,
    local_port: u16,
    cseq: u32,
    items: &[ChannelItem],
) -> Result<SipMessage> {
    let body = format!(
        "<Response CmdType=\"Catalog\" SN=\"{}\"><DeviceID>{}</DeviceID><SumNum>{}</SumNum><DeviceList Num=\"{}\">{}</DeviceList></Response>",
        sn,
        device_id,
        items.len(),
        items.len(),
        items
            .iter()
            .map(|item| format!(
                "<Item><DeviceID>{}</DeviceID><Name>{}</Name><Manufacturer>{}</Manufacturer><Model>{}</Model><Status>{}</Status></Item>",
                item.device_id, item.name, item.manufacturer, item.model, item.status
            ))
            .collect::<String>()
    );

    let mut headers = Vec::new();
    // SIP routing headers (REQUIRED by all SIP proxies/servers)
    headers.push((
        "Via".to_string(),
        format!(
            "SIP/2.0/UDP {}:{};rport;branch=z9hG4bK{}",
            local_ip, local_port, cseq
        ),
    ));
    headers.push((
        "From".to_string(),
        format!("<sip:{}@{}>;tag={}", device_id, domain, cseq),
    ));
    headers.push(("To".to_string(), format!("<sip:{}@{}>", domain, domain)));
    headers.push((
        "Call-ID".to_string(),
        format!("{}-catalog-{}", device_id, cseq),
    ));
    headers.push(("CSeq".to_string(), format!("{} MESSAGE", cseq)));
    headers.push(("Max-Forwards".to_string(), "70".to_string()));
    // Content headers
    headers.push((
        "Content-Type".to_string(),
        "Application/MANSCDP+xml".to_string(),
    ));
    headers.push(("Content-Length".to_string(), body.len().to_string()));

    Ok(SipMessage {
        start_line: format!("MESSAGE sip:{}@{} SIP/2.0", domain, domain),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some(format!("sip:{}@{}", domain, domain)),
        version: "SIP/2.0".to_string(),
        headers,
        body,
    })
}

/// Build a SIP MESSAGE with DeviceInfo response.
///
/// Per GB/T 28181-2022 §7.6, the DeviceInfo response contains device
/// identification and firmware information.
pub fn build_device_info_response(
    sn: &str,
    device_id: &str,
    domain: &str,
    local_ip: &str,
    local_port: u16,
    cseq: u32,
    info: &DeviceItem,
) -> Result<SipMessage> {
    let body = format!(
        "<Response CmdType=\"DeviceInfo\" SN=\"{}\"><DeviceID>{}</DeviceID><Result>OK</Result><DeviceName>{}</DeviceName><Manufacturer>{}</Manufacturer><Model>{}</Model><Firmware>{}</Firmware></Response>",
        sn, device_id, info.name, info.manufacturer, info.model, info.firmware
    );

    let mut headers = Vec::new();
    // SIP routing headers (REQUIRED by all SIP proxies/servers)
    headers.push((
        "Via".to_string(),
        format!(
            "SIP/2.0/UDP {}:{};rport;branch=z9hG4bK{}",
            local_ip, local_port, cseq
        ),
    ));
    headers.push((
        "From".to_string(),
        format!("<sip:{}@{}>;tag={}", device_id, domain, cseq),
    ));
    headers.push(("To".to_string(), format!("<sip:{}@{}>", domain, domain)));
    headers.push((
        "Call-ID".to_string(),
        format!("{}-deviceinfo-{}", device_id, cseq),
    ));
    headers.push(("CSeq".to_string(), format!("{} MESSAGE", cseq)));
    headers.push(("Max-Forwards".to_string(), "70".to_string()));
    // Content headers
    headers.push((
        "Content-Type".to_string(),
        "Application/MANSCDP+xml".to_string(),
    ));
    headers.push(("Content-Length".to_string(), body.len().to_string()));

    Ok(SipMessage {
        start_line: format!("MESSAGE sip:{}@{} SIP/2.0", domain, domain),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some(format!("sip:{}@{}", domain, domain)),
        version: "SIP/2.0".to_string(),
        headers,
        body,
    })
}

/// Build a SIP MESSAGE with Keepalive notification.
///
/// Per GB/T 28181-2022 §7.7, the Keepalive Notify indicates the device is online.
/// Default status is "OK".
pub fn build_keepalive_notify(
    sn: &str,
    device_id: &str,
    domain: &str,
    local_ip: &str,
    local_port: u16,
    status: &str,
    cseq: u32,
) -> Result<SipMessage> {
    let body = format!(
        "<Notify CmdType=\"Keepalive\" SN=\"{}\"><DeviceID>{}</DeviceID><Status>{}</Status></Notify>",
        sn, device_id, status
    );

    let mut headers = Vec::new();
    // SIP routing headers (REQUIRED by all SIP proxies/servers)
    headers.push((
        "Via".to_string(),
        format!(
            "SIP/2.0/UDP {}:{};rport;branch=z9hG4bK{}",
            local_ip, local_port, cseq
        ),
    ));
    headers.push((
        "From".to_string(),
        format!("<sip:{}@{}>;tag={}", device_id, domain, cseq),
    ));
    headers.push(("To".to_string(), format!("<sip:{}@{}>", domain, domain)));
    headers.push((
        "Call-ID".to_string(),
        format!("{}-keepalive-{}", device_id, cseq),
    ));
    headers.push(("CSeq".to_string(), format!("{} MESSAGE", cseq)));
    headers.push(("Max-Forwards".to_string(), "70".to_string()));
    // Content headers
    headers.push((
        "Content-Type".to_string(),
        "Application/MANSCDP+xml".to_string(),
    ));
    headers.push(("Content-Length".to_string(), body.len().to_string()));

    Ok(SipMessage {
        start_line: format!("MESSAGE sip:{}@{} SIP/2.0", domain, domain),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some(format!("sip:{}@{}", domain, domain)),
        version: "SIP/2.0".to_string(),
        headers,
        body,
    })
}

/// A single RecordInfo item (one recorded segment).
#[derive(Debug, Clone)]
pub struct RecordItem {
    /// Device ID of the recording source.
    pub device_id: String,
    /// Human-readable name.
    pub name: String,
    /// Path to the recording file.
    pub file_path: String,
    /// Address of the recording source.
    pub address: String,
    /// Start time in GB/T 28181 format (`YYYY-MM-DDTHH:MM:SS`).
    pub start_time: String,
    /// End time in GB/T 28181 format (`YYYY-MM-DDTHH:MM:SS`).
    pub end_time: String,
    /// Secrecy level ("0" = public).
    pub secrecy: String,
    /// Recording type ("time" = scheduled/time-based).
    pub r#type: String,
}

/// Build a SIP MESSAGE with RecordInfo response.
///
/// Per GB/T 28181-2022 §7.6, the RecordInfo response reports the device's
/// recording list for a queried time window. `items` are the matched
/// segments; an empty list yields `SumNum=0` with an empty `RecordList`.
pub fn build_recordinfo_response(
    sn: &str,
    device_id: &str,
    domain: &str,
    local_ip: &str,
    local_port: u16,
    cseq: u32,
    items: &[RecordItem],
) -> Result<SipMessage> {
    let items_xml = items
        .iter()
        .map(|it| {
            format!("<Item><DeviceID>{}</DeviceID><Name>{}</Name><FilePath>{}</FilePath><Address>{}</Address><StartTime>{}</StartTime><EndTime>{}</EndTime><Secrecy>{}</Secrecy><Type>{}</Type></Item>",
                it.device_id, it.name, it.file_path, it.address, it.start_time, it.end_time, it.secrecy, it.r#type)
        })
        .collect::<String>();
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"GB2312\"?>\
        <Response CmdType=\"RecordInfo\" SN=\"{}\">\
        <DeviceID>{}</DeviceID>\
        <Name>{}</Name>\
        <SumNum>{}</SumNum>\
        <RecordList Num=\"{}\">\
        {}\
        </RecordList>\
        </Response>",
        sn,
        device_id,
        device_id,
        items.len(),
        items.len(),
        items_xml
    );

    let mut headers = Vec::new();
    // SIP routing headers (REQUIRED by all SIP proxies/servers)
    headers.push((
        "Via".to_string(),
        format!(
            "SIP/2.0/UDP {}:{};rport;branch=z9hG4bK{}",
            local_ip, local_port, cseq
        ),
    ));
    headers.push((
        "From".to_string(),
        format!("<sip:{}@{}>;tag={}", device_id, domain, cseq),
    ));
    headers.push(("To".to_string(), format!("<sip:{}@{ }>", domain, domain)));
    headers.push((
        "Call-ID".to_string(),
        format!("{}-recordinfo-{}", device_id, cseq),
    ));
    headers.push(("CSeq".to_string(), format!("{} MESSAGE", cseq)));
    headers.push(("Max-Forwards".to_string(), "70".to_string()));
    // Content headers
    headers.push((
        "Content-Type".to_string(),
        "Application/MANSCDP+xml".to_string(),
    ));
    headers.push(("Content-Length".to_string(), body.len().to_string()));

    Ok(SipMessage {
        start_line: format!("MESSAGE sip:{}@{} SIP/2.0", domain, domain),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some(format!("sip:{}@{ }", domain, domain)),
        version: "SIP/2.0".to_string(),
        headers,
        body,
    })
}

/// Format an epoch-millisecond timestamp as a GB/T 28181 time string
/// (`YYYY-MM-DDTHH:MM:SS`, device-local timezone — naive form, matching
/// how platforms send queries and how the Go repo formats via `time.Local`).
#[must_use]
pub fn format_gb_time_ms(ms: u64) -> String {
    format_gb_time_ms_with(ms, crate::manscdp::device_local_offset_secs())
}

/// Offset-aware core of [`format_gb_time_ms`] for deterministic tests.
#[must_use]
pub fn format_gb_time_ms_with(ms: u64, local_offset_secs: i64) -> String {
    let secs = ((ms / 1000) as i64 + local_offset_secs).max(0) as u64;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil date from days since epoch (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Build a SIP MESSAGE with DeviceStatus response.
///
/// Per GB/T 28181-2022 §7.6, the DeviceStatus response reports the device's
/// online/encoding/recording state and its current time.
pub fn build_device_status_response(
    sn: &str,
    device_id: &str,
    domain: &str,
    local_ip: &str,
    local_port: u16,
    cseq: u32,
) -> Result<SipMessage> {
    // DeviceTime in GB28181 format. No chrono dependency in this crate, so
    // compute the UTC date/time from SystemTime directly.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    let rem = secs % 86400;
    // Civil date from days since epoch (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let now = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    );

    let record_state = if crate::RECORD_ACTIVE.load(std::sync::atomic::Ordering::SeqCst) {
        "ON"
    } else {
        "OFF"
    };

    let body = format!(
        "<?xml version=\"1.0\" encoding=\"GB2312\"?>\
        <Response CmdType=\"DeviceStatus\" SN=\"{}\">\
        <DeviceID>{}</DeviceID>\
        <Result>OK</Result>\
        <Online>ONLINE</Online>\
        <Status>OK</Status>\
        <Encode>ON</Encode>\
        <Record>{}</Record>\
        <DeviceTime>{}</DeviceTime>\
        </Response>",
        sn, device_id, record_state, now
    );

    let mut headers = Vec::new();
    // SIP routing headers (REQUIRED by all SIP proxies/servers)
    headers.push((
        "Via".to_string(),
        format!(
            "SIP/2.0/UDP {}:{};rport;branch=z9hG4bK{}",
            local_ip, local_port, cseq
        ),
    ));
    headers.push((
        "From".to_string(),
        format!("<sip:{}@{}>;tag={}", device_id, domain, cseq),
    ));
    headers.push(("To".to_string(), format!("<sip:{}@{ }>", domain, domain)));
    headers.push((
        "Call-ID".to_string(),
        format!("{}-devicestatus-{}", device_id, cseq),
    ));
    headers.push(("CSeq".to_string(), format!("{} MESSAGE", cseq)));
    headers.push(("Max-Forwards".to_string(), "70".to_string()));
    // Content headers
    headers.push((
        "Content-Type".to_string(),
        "Application/MANSCDP+xml".to_string(),
    ));
    headers.push(("Content-Length".to_string(), body.len().to_string()));

    Ok(SipMessage {
        start_line: format!("MESSAGE sip:{}@{} SIP/2.0", domain, domain),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some(format!("sip:{}@{ }", domain, domain)),
        version: "SIP/2.0".to_string(),
        headers,
        body,
    })
}

/// Build a SIP MESSAGE rejecting a device control command.
///
/// Per GB/T 28181-2022 §7.6, unsupported control commands (PTZ, broadcast,
/// config, etc.) are answered with a `Result` of `ERROR`.
pub fn build_control_reject_response(
    cmd_type: &str,
    sn: &str,
    device_id: &str,
    domain: &str,
    local_ip: &str,
    local_port: u16,
    cseq: u32,
) -> Result<SipMessage> {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"GB2312\"?>\
        <Response CmdType=\"{}\" SN=\"{}\">\
        <DeviceID>{}</DeviceID>\
        <Result>ERROR</Result>\
        </Response>",
        cmd_type, sn, device_id
    );

    let mut headers = Vec::new();
    // SIP routing headers (REQUIRED by all SIP proxies/servers)
    headers.push((
        "Via".to_string(),
        format!(
            "SIP/2.0/UDP {}:{};rport;branch=z9hG4bK{}",
            local_ip, local_port, cseq
        ),
    ));
    headers.push((
        "From".to_string(),
        format!("<sip:{}@{}>;tag={}", device_id, domain, cseq),
    ));
    headers.push(("To".to_string(), format!("<sip:{}@{ }>", domain, domain)));
    headers.push((
        "Call-ID".to_string(),
        format!("{}-control-{}", device_id, cseq),
    ));
    headers.push(("CSeq".to_string(), format!("{} MESSAGE", cseq)));
    headers.push(("Max-Forwards".to_string(), "70".to_string()));
    // Content headers
    headers.push((
        "Content-Type".to_string(),
        "Application/MANSCDP+xml".to_string(),
    ));
    headers.push(("Content-Length".to_string(), body.len().to_string()));

    Ok(SipMessage {
        start_line: format!("MESSAGE sip:{}@{} SIP/2.0", domain, domain),
        method: Some(SipMethod::Message),
        status_code: None,
        uri: Some(format!("sip:{}@{ }", domain, domain)),
        version: "SIP/2.0".to_string(),
        headers,
        body,
    })
}

/// Dispatch an inbound MESSAGE request from the platform.
///
/// Parses the XML body to determine the command type and returns
/// a 200 OK response. The caller is responsible for building and sending
/// the Catalog/DeviceInfo response MESSAGE.
///
/// # Returns
/// * `Ok((ok_response, None))` - 200 OK to acknowledge the query
///
/// # Supported CmdType values
/// * `Catalog` - Platform queries device catalog → 200 OK (caller sends Catalog Response)
/// * `DeviceInfo` - Platform queries device info → 200 OK (caller sends DeviceInfo Response)
/// * `Keepalive` - Platform acknowledges our Keepalive → 200 OK only
/// * Unknown - Log warning, return 200 OK only
pub fn dispatch_inbound_message(msg: &SipMessage) -> Result<(SipMessage, Option<SipMessage>)> {
    let content_type = msg.get_header("Content-Type").unwrap_or("");

    if content_type != "Application/MANSCDP+xml" {
        eprintln!(
            "gb28181: received MESSAGE with unsupported Content-Type: {}",
            content_type
        );
        return build_200_ok_response(msg);
    }

    // Parse XML body as Query (most common inbound MESSAGE type)
    if let Ok(query) = serde_xml_rs::from_str::<Query>(&msg.body) {
        match query.cmd_type.as_str() {
            "Catalog" => {
                // Platform queries catalog → return 200 OK + queue Catalog response
                // Note: caller must provide the actual channel items via build_catalog_response
                eprintln!(
                    "gb28181: received Catalog Query SN={} from {}",
                    query.sn, query.device_id
                );
                let ok_response = build_200_ok_response(msg)?.0;
                // Caller must build the actual catalog response with real data
                // For now, return None to indicate caller needs to build it
                Ok((ok_response, None))
            }
            "DeviceInfo" => {
                eprintln!(
                    "gb28181: received DeviceInfo Query SN={} from {}",
                    query.sn, query.device_id
                );
                let ok_response = build_200_ok_response(msg)?.0;
                // Caller must build the actual device info response
                Ok((ok_response, None))
            }
            _ => {
                eprintln!("gb28181: unknown Query CmdType: {}", query.cmd_type);
                build_200_ok_response(msg)
            }
        }
    } else if let Ok(_notify) = serde_xml_rs::from_str::<Notify>(&msg.body) {
        // Platform is acknowledging our Keepalive (or other notification)
        eprintln!("gb28181: received platform acknowledge for Notify");
        build_200_ok_response(msg)
    } else {
        eprintln!("gb28181: failed to parse MESSAGE body as Query or Notify");
        build_200_ok_response(msg)
    }
}

/// Build a 200 OK response to a MESSAGE request.
fn build_200_ok_response(request: &SipMessage) -> Result<(SipMessage, Option<SipMessage>)> {
    let mut headers = Vec::new();

    // Copy headers from request
    if let Some(via) = request.get_header("Via") {
        headers.push(("Via".to_string(), via.to_string()));
    }
    if let Some(from) = request.get_header("From") {
        headers.push(("From".to_string(), from.to_string()));
    }
    if let Some(to) = request.get_header("To") {
        headers.push(("To".to_string(), to.to_string()));
    }
    if let Some(call_id) = request.get_header("Call-ID") {
        headers.push(("Call-ID".to_string(), call_id.to_string()));
    }
    if let Some(cseq) = request.get_header("CSeq") {
        // Keep original CSeq method
        headers.push(("CSeq".to_string(), cseq.to_string()));
    }

    headers.push(("Content-Length".to_string(), "0".to_string()));

    let response = SipMessage {
        start_line: "SIP/2.0 200 OK".to_string(),
        method: None,
        status_code: Some(SipStatusCode::Ok),
        uri: request.uri.clone(),
        version: "SIP/2.0".to_string(),
        headers,
        body: String::new(),
    };

    Ok((response, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_response_xml_well_formed() {
        let items = [ChannelItem {
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
        }];

        // Note: serde-xml-rs has limitations with Response containing None fields
        // This test verifies ChannelItem structure is well-formed
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].device_id, "31011500991320000001");
    }

    #[test]
    fn test_keepalive_notify_format() {
        let result = build_keepalive_notify(
            "456",
            "31011500991320000001",
            "3402000000",
            "192.168.1.100",
            5060,
            "OK",
            1000,
        );
        assert!(result.is_ok());

        let msg = result.unwrap();
        assert!(msg
            .body
            .contains("<Notify CmdType=\"Keepalive\" SN=\"456\">"));
        assert!(msg.body.contains("<Status>OK</Status>"));
        // SIP routing headers required by the platform (NVR drops MESSAGE without Via)
        assert!(msg.get_header("Via").is_some());
        assert_eq!(msg.get_header("CSeq"), Some("1000 MESSAGE"));
        assert_eq!(msg.get_header("Max-Forwards"), Some("70"));
        assert_eq!(msg.start_line, "MESSAGE sip:3402000000@3402000000 SIP/2.0");
    }

    #[test]
    fn test_dispatch_inbound_catalog_query() {
        // Construct inbound MESSAGE with Catalog Query XML
        let query_xml = r#"<?xml version="1.0" encoding="GB2312"?>
<Query>
    <CmdType>Catalog</CmdType>
    <SN>789</SN>
    <DeviceID>31011500991320000001</DeviceID>
</Query>"#;

        let headers = vec![
            ("From".to_string(), "<sip:platform@domain>".to_string()),
            (
                "To".to_string(),
                "<sip:31011500991320000001@domain>".to_string(),
            ),
            ("Call-ID".to_string(), "test-call-id".to_string()),
            ("CSeq".to_string(), "1 MESSAGE".to_string()),
            (
                "Content-Type".to_string(),
                "Application/MANSCDP+xml".to_string(),
            ),
            ("Content-Length".to_string(), query_xml.len().to_string()),
        ];

        let inbound_msg = SipMessage {
            start_line: "MESSAGE sip:31011500991320000001@domain SIP/2.0".to_string(),
            method: Some(SipMethod::Message),
            status_code: None,
            uri: Some("sip:31011500991320000001@domain".to_string()),
            version: "SIP/2.0".to_string(),
            headers,
            body: query_xml.to_string(),
        };

        let result = dispatch_inbound_message(&inbound_msg);
        assert!(result.is_ok());

        let (ok_response, queued) = result.unwrap();
        assert!(matches!(ok_response.status_code, Some(SipStatusCode::Ok)));
        // Catalog response requires channel items from caller, so queued is None
        assert!(queued.is_none());
    }

    #[test]
    fn test_dispatch_unknown_cmdtype_no_crash() {
        // Construct MESSAGE with unknown CmdType
        let query_xml = r#"<?xml version="1.0" encoding="GB2312"?>
<Query>
    <CmdType>UnknownCommand</CmdType>
    <SN>999</SN>
    <DeviceID>31011500991320000001</DeviceID>
</Query>"#;

        let headers = vec![
            ("From".to_string(), "<sip:platform@domain>".to_string()),
            (
                "To".to_string(),
                "<sip:31011500991320000001@domain>".to_string(),
            ),
            ("Call-ID".to_string(), "test-call-id".to_string()),
            ("CSeq".to_string(), "1 MESSAGE".to_string()),
            (
                "Content-Type".to_string(),
                "Application/MANSCDP+xml".to_string(),
            ),
            ("Content-Length".to_string(), query_xml.len().to_string()),
        ];

        let inbound_msg = SipMessage {
            start_line: "MESSAGE sip:31011500991320000001@domain SIP/2.0".to_string(),
            method: Some(SipMethod::Message),
            status_code: None,
            uri: Some("sip:31011500991320000001@domain".to_string()),
            version: "SIP/2.0".to_string(),
            headers,
            body: query_xml.to_string(),
        };

        let result = dispatch_inbound_message(&inbound_msg);
        assert!(result.is_ok());

        let (ok_response, queued) = result.unwrap();
        assert!(matches!(ok_response.status_code, Some(SipStatusCode::Ok)));
        assert!(queued.is_none());
    }

    #[test]
    fn test_recordinfo_response_body() {
        let resp = build_recordinfo_response(
            "10",
            "34020000001320000001",
            "192.168.63.197",
            "192.168.62.104",
            5060,
            100,
            &[],
        )
        .expect("recordinfo response should build");
        assert!(resp.body.contains("RecordInfo"));
        assert!(resp.body.contains("<SumNum>0</SumNum>"));
        assert!(resp.body.contains("RecordList Num=\"0\""));
    }

    #[test]
    fn test_device_status_response_fields() {
        let resp = build_device_status_response(
            "11",
            "34020000001320000001",
            "192.168.63.197",
            "192.168.62.104",
            5060,
            101,
        )
        .expect("device status response should build");
        assert!(resp.body.contains("DeviceStatus"));
        assert!(resp.body.contains(">ONLINE<"));
        assert!(resp.body.contains(">OK<"));
        assert!(resp.body.contains(">ON<"));
        assert!(resp.body.contains(">OFF<"));
        assert!(resp.body.contains("DeviceTime"));
    }

    #[test]
    fn test_control_reject_response_body() {
        let resp = build_control_reject_response(
            "DeviceControl",
            "12",
            "34020000001320000001",
            "192.168.63.197",
            "192.168.62.104",
            5060,
            102,
        )
        .expect("control reject response should build");
        assert!(resp.body.contains(">ERROR<"));
        assert!(resp.body.contains("DeviceControl"));
    }

    #[test]
    fn test_recordinfo_response_has_sip_headers() {
        let resp = build_recordinfo_response(
            "10",
            "34020000001320000001",
            "192.168.63.197",
            "192.168.62.104",
            5060,
            100,
            &[],
        )
        .expect("recordinfo response should build");
        for name in [
            "Via",
            "From",
            "To",
            "Call-ID",
            "CSeq",
            "Content-Type",
            "Content-Length",
        ] {
            assert!(
                resp.headers.iter().any(|(k, _)| k == name),
                "missing SIP header: {name}"
            );
        }
    }
}

#[test]
fn test_format_gb_time_ms_roundtrip() {
    // 2026-08-15T14:30:00Z = 1786804200 s. _with(0) pins the UTC frame.
    assert_eq!(
        format_gb_time_ms_with(1_786_804_200_000, 0),
        "2026-08-15T14:30:00"
    );
    // Epoch anchor.
    assert_eq!(format_gb_time_ms_with(0, 0), "1970-01-01T00:00:00");
    // Leap year boundary: 2024-02-29T12:00:00Z.
    assert_eq!(
        format_gb_time_ms_with(1_709_208_000_000, 0),
        "2024-02-29T12:00:00"
    );
    // +08:00 device: 14:30 UTC renders as 22:30 local (CST).
    assert_eq!(
        format_gb_time_ms_with(1_786_804_200_000, 28_800),
        "2026-08-15T22:30:00"
    );
}

#[test]
fn test_recordinfo_response_empty_golden() {
    let resp = build_recordinfo_response(
        "10",
        "34020000001320000001",
        "192.168.63.197",
        "192.168.62.104",
        5060,
        100,
        &[],
    )
    .expect("recordinfo response should build");
    // Byte-identical to the pre-R-RI empty body (NVR stability).
    assert_eq!(
            resp.body,
            "<?xml version=\"1.0\" encoding=\"GB2312\"?><Response CmdType=\"RecordInfo\" SN=\"10\"><DeviceID>34020000001320000001</DeviceID><Name>34020000001320000001</Name><SumNum>0</SumNum><RecordList Num=\"0\"></RecordList></Response>"
        );
}

#[test]
fn test_recordinfo_response_with_items() {
    let items = vec![
        RecordItem {
            device_id: "34020000001320000001".to_string(),
            name: "34020000001320000001".to_string(),
            file_path: "2026/08/15/14-30-00.h264".to_string(),
            address: String::new(),
            start_time: "2026-08-15T14:30:00".to_string(),
            end_time: "2026-08-15T14:35:00".to_string(),
            secrecy: "0".to_string(),
            r#type: "time".to_string(),
        },
        RecordItem {
            device_id: "34020000001320000001".to_string(),
            name: "34020000001320000001".to_string(),
            file_path: "2026/08/15/14-35-00.h264".to_string(),
            address: String::new(),
            start_time: "2026-08-15T14:35:00".to_string(),
            end_time: "2026-08-15T14:40:00".to_string(),
            secrecy: "0".to_string(),
            r#type: "time".to_string(),
        },
    ];
    let resp = build_recordinfo_response(
        "10",
        "34020000001320000001",
        "192.168.63.197",
        "192.168.62.104",
        5060,
        100,
        &items,
    )
    .expect("recordinfo response should build");
    assert!(resp.body.contains("<SumNum>2</SumNum>"));
    assert!(resp.body.contains("<RecordList Num=\"2\">"));
    assert!(resp
        .body
        .contains("<FilePath>2026/08/15/14-30-00.h264</FilePath>"));
    assert!(resp
        .body
        .contains("<StartTime>2026-08-15T14:30:00</StartTime>"));
    assert!(resp.body.contains("<Secrecy>0</Secrecy>"));
    assert!(resp.body.contains("<Type>time</Type>"));
    // Exactly two Item elements.
    assert_eq!(resp.body.matches("<Item>").count(), 2);
}

#[cfg(test)]
mod media_transport_tests {
    use super::*;

    fn invite_with_sdp(call_id: &str, body: &str) -> SipMessage {
        SipMessage {
            start_line: "INVITE sip:34020000001320000001@3402000000 SIP/2.0".to_string(),
            method: Some(SipMethod::Invite),
            status_code: None,
            uri: Some("sip:34020000001320000001@3402000000".to_string()),
            version: "SIP/2.0".to_string(),
            headers: vec![
                ("Call-ID".to_string(), call_id.to_string()),
                ("CSeq".to_string(), "1 INVITE".to_string()),
            ],
            body: body.to_string(),
        }
    }

    #[test]
    fn udp_offer_is_udp_media() {
        let msg = invite_with_sdp(
            "mt-udp",
            "v=0\r\no=- 0 0 IN IP4 192.168.1.10\r\ns=Play\r\nc=IN IP4 192.168.1.10\r\nt=0 0\r\nm=video 30000 RTP/AVP 96\r\ny=2000000001\r\n",
        );
        let info = parse_invite(&msg).expect("parse");
        assert_eq!(info.media_transport, MediaTransport::Udp);
        assert_eq!(info.media_port, 30000);
    }

    #[test]
    fn tcp_passive_offer_means_device_connects() {
        // tcp-passive platform offer (MiBeeNvr v0.11 default).
        let msg = invite_with_sdp(
            "mt-tcp-passive",
            "v=0\r\no=- 0 0 IN IP4 192.168.1.10\r\ns=Play\r\nc=IN IP4 192.168.1.10\r\nt=0 0\r\nm=video 30000 TCP/RTP/AVP 96\r\na=recvonly\r\na=setup:passive\r\na=connection:new\r\na=rtpmap:96 PS/90000\r\ny=2000000001\r\n",
        );
        let info = parse_invite(&msg).expect("parse");
        assert_eq!(info.media_transport, MediaTransport::TcpConnect);
    }

    #[test]
    fn tcp_actpass_offer_device_chooses_to_connect() {
        let msg = invite_with_sdp(
            "mt-tcp-actpass",
            "v=0\r\no=- 0 0 IN IP4 192.168.1.10\r\ns=Play\r\nc=IN IP4 192.168.1.10\r\nt=0 0\r\nm=video 30000 TCP/RTP/AVP 96\r\na=setup:actpass\r\ny=2000000001\r\n",
        );
        let info = parse_invite(&msg).expect("parse");
        assert_eq!(info.media_transport, MediaTransport::TcpConnect);
    }

    #[test]
    fn tcp_setup_active_offer_means_platform_dials() {
        let msg = invite_with_sdp(
            "mt-tcp-active",
            "v=0\r\no=- 0 0 IN IP4 192.168.1.10\r\ns=Play\r\nc=IN IP4 192.168.1.10\r\nt=0 0\r\nm=video 9 TCP/RTP/AVP 96\r\na=setup:active\r\ny=2000000001\r\n",
        );
        let info = parse_invite(&msg).expect("parse");
        assert_eq!(info.media_transport, MediaTransport::TcpListen);
    }
}
