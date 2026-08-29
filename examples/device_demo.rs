//! In-process GB/T 28181 interop demo — one binary, both sides of the wire.
//!
//! A minimal hand-written "platform" (SIP server + RTP receiver, what a
//! foreign NVR does) talks to the real [`gb28181_rs::server::Gb28181Server`]
//! streaming synthetic H.264 access units from a [`MockFrameHub`].
//!
//! Covered flow:
//! 1. REGISTER → 401 Digest challenge → REGISTER with Authorization
//!    (the platform **verifies the digest response** it receives)
//! 2. Catalog query → channel list parsed from the MANSCDP response
//! 3. INVITE (SDP offer, `y=` SSRC) → 200 OK → ACK
//! 4. RTP/PS live stream reassembled and demuxed back to NAL units
//! 5. BYE → 200 OK, exit 0
//!
//! The whole flow is asserted, so this doubles as an executable smoke test:
//!
//! ```sh
//! cargo run --example device_demo
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use md5::{Digest, Md5};
use tokio::net::UdpSocket;

use gb28181_rs::config::{Gb28181Config, Transport};
use gb28181_rs::frame::{AccessUnit, Nalu};
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::ps;
use gb28181_rs::server::Gb28181Server;
use gb28181_rs::sip::{parse_digest_auth, SipMessage, SipMethod, SipStatusCode};

const PLATFORM_ID: &str = "34020000002000000001";
const DEVICE_ID: &str = "34020000001320000001";
const SIP_DOMAIN: &str = "3402000000";
const PASSWORD: &str = "12345678";
const NONCE: &str = "demo0123456789ab";
const SSRC: u32 = 777;
const FRAMES_TO_RECEIVE: usize = 25;
const GOP_SIZE: u64 = 12;
const DEMO_TIMEOUT: Duration = Duration::from_secs(30);

fn md5_hex(s: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// Synthetic video source
// ---------------------------------------------------------------------------

fn nalu_of(nalu_type: u8, size: usize) -> Nalu {
    Nalu {
        nalu_type,
        data: (0..size)
            .map(|i| {
                if i == 0 {
                    nalu_type
                } else {
                    (i % 200 + 7) as u8
                }
            })
            .collect(),
        is_idr: nalu_type == 5,
        is_sps: nalu_type == 7,
        is_pps: nalu_type == 8,
        is_aud: nalu_type == 9,
    }
}

fn synth_au(frame_no: u64) -> AccessUnit {
    let key = frame_no % GOP_SIZE == 0;
    let mut nalus = vec![nalu_of(9, 4)]; // AUD
    if key {
        nalus.push(nalu_of(7, 27)); // SPS
        nalus.push(nalu_of(8, 9)); // PPS
        nalus.push(nalu_of(5, 24_000)); // IDR
    } else {
        nalus.push(nalu_of(1, 2_400)); // non-reference slice
    }
    AccessUnit {
        nalus,
        timestamp: Instant::now(),
        is_key_frame: key,
    }
}

async fn produce_frames(hub: Arc<MockFrameHub>) {
    for frame_no in 0.. {
        hub.write(synth_au(frame_no));
        tokio::time::sleep(Duration::from_millis(40)).await; // 25 fps
    }
}

// ---------------------------------------------------------------------------
// Platform-side SIP message crafting (raw wire text, like a foreign NVR)
// ---------------------------------------------------------------------------

fn sip_response(
    req: &SipMessage,
    code: u32,
    reason: &str,
    extra_headers: &[(&str, &str)],
    body: &str,
) -> String {
    let mut s = format!(
        "SIP/2.0 {code} {reason}\r\n\
         Via: {}\r\n\
         From: {}\r\n\
         To: {}\r\n\
         Call-ID: {}\r\n\
         CSeq: {}\r\n",
        req.get_header("Via").unwrap_or_default(),
        req.get_header("From").unwrap_or_default(),
        req.get_header("To").unwrap_or_default(),
        req.get_header("Call-ID").unwrap_or("demo"),
        req.get_header("CSeq").unwrap_or("1 REGISTER"),
    );
    for (name, value) in extra_headers {
        s.push_str(&format!("{name}: {value}\r\n"));
    }
    if body.is_empty() {
        s.push_str("Content-Length: 0\r\n\r\n");
    } else {
        s.push_str(&format!(
            "Content-Type: Application/MANSCDP+xml\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
    }
    s
}

fn sip_request(method: &str, target: &str, call_id: &str, cseq: &str, body: &str) -> String {
    let platform_via = format!("SIP/2.0/UDP 127.0.0.1;branch=z9hG4bK{method}demo");
    let mut s = format!(
        "{method} sip:{target}@{SIP_DOMAIN} SIP/2.0\r\n\
         Via: {platform_via}\r\n\
         From: <sip:{PLATFORM_ID}@{SIP_DOMAIN}>;tag=platdemo\r\n\
         To: <sip:{target}@{SIP_DOMAIN}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq}\r\n\
         Max-Forwards: 70\r\n",
    );
    if body.is_empty() {
        s.push_str("Content-Length: 0\r\n\r\n");
    } else {
        let ctype = if method == "INVITE" {
            "application/sdp"
        } else {
            "Application/MANSCDP+xml"
        };
        s.push_str(&format!(
            "Content-Type: {ctype}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
    }
    s
}

fn invite_sdp(rtp_port: u16) -> String {
    format!(
        "v=0\r\n\
         o={PLATFORM_ID} 0 0 IN IP4 127.0.0.1\r\n\
         s=Play\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=video {rtp_port} RTP/AVP 96\r\n\
         a=recvonly\r\n\
         y={SSRC:010}\r\n"
    )
}

fn xml_field(xml: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open).map_or(0, |p| p + open.len());
    let end = xml[start..].find(&close).map_or(start, |e| start + e);
    xml[start..end].to_string()
}

/// Extract `(DeviceID, Status)` pairs from a Catalog response body.
fn catalog_channels(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(pos) = rest.find("<Item>") {
        rest = &rest[pos + "<Item>".len()..];
        let end = rest.find("</Item>").unwrap_or(rest.len());
        let item = &rest[..end];
        out.push((xml_field(item, "DeviceID"), xml_field(item, "Status")));
    }
    out
}

// ---------------------------------------------------------------------------
// The fake platform loop
// ---------------------------------------------------------------------------

struct DemoStats {
    rtp_packets: usize,
    frames: usize,
    keyframes: usize,
    video_bytes: usize,
}

async fn run_platform(sip: Arc<UdpSocket>, rtp: Arc<UdpSocket>) -> Result<DemoStats> {
    let mut sip_buf = vec![0u8; 65535];
    let mut rtp_buf = vec![0u8; 65535];
    let mut ps_reassembly: Vec<u8> = Vec::new();
    let mut stats = DemoStats {
        rtp_packets: 0,
        frames: 0,
        keyframes: 0,
        video_bytes: 0,
    };

    let mut registered = false;
    let mut invite_ok = false;
    let mut bye_sent = false;
    let mut bye_ok = false;
    let call_id = "democall0001";

    loop {
        let rtp_enabled = invite_ok && !bye_ok;
        tokio::select! {
            recv = sip.recv_from(&mut sip_buf) => {
                let (n, peer) = recv.context("platform SIP socket")?;
                let text = std::str::from_utf8(&sip_buf[..n]).context("non-UTF8 SIP")?;
                let msg = match SipMessage::parse(text) {
                    Ok(m) => m,
                    Err(e) => {
                        if std::env::var("DEMO_DEBUG").is_ok() {
                            eprintln!("[platform] unparseable SIP: {e}: {text}");
                        }
                        continue;
                    }
                };
                if std::env::var("DEMO_DEBUG").is_ok() {
                    eprintln!(
                        "[platform] SIP from {peer}: method={:?} status={:?} ctype={:?} body_len={} body_head={:?}",
                        msg.method,
                        msg.status_code,
                        msg.get_header("Content-Type"),
                        msg.body.len(),
                        msg.body.chars().take(80).collect::<String>()
                    );
                }

                // -- requests from the device --------------------------------
                if msg.method == Some(SipMethod::Register) {
                    if let Some(auth) = msg.get_header("Authorization") {
                        // Verify the digest response like a real platform would.
                        let params = parse_digest_auth(auth).context("parse Authorization")?;
                        let ha1 = md5_hex(&format!("{DEVICE_ID}:{SIP_DOMAIN}:{PASSWORD}"));
                        let ha2 = md5_hex(&format!("REGISTER:{}", params.uri));
                        let expected = md5_hex(&format!("{}:{}:{ha2}", ha1, params.nonce));
                        if params.response != expected {
                            bail!("device digest response mismatch: got {} want {}", params.response, expected);
                        }
                        println!("[platform] REGISTER digest VERIFIED (uri={}, response={})", params.uri, &params.response[..12]);
                        let resp = sip_response(&msg, 200, "OK", &[], "");
                        sip.send_to(resp.as_bytes(), peer).await?;
                        if !registered {
                            registered = true;
                            println!("[platform] device registered — sending Catalog query");
                            let query = "<Query><CmdType>Catalog</CmdType><SN>1</SN>\
                                         <DeviceID>".to_string() + DEVICE_ID + "</DeviceID></Query>";
                            let q = sip_request("MESSAGE", DEVICE_ID, "catalog0001", "1 MESSAGE", &query);
                            sip.send_to(q.as_bytes(), ("127.0.0.1", 15060u16)).await?;
                        }
                    } else {
                        println!("[platform] REGISTER without auth — challenging 401");
                        let challenge = format!(
                            "Digest realm=\"{SIP_DOMAIN}\", nonce=\"{NONCE}\", algorithm=MD5"
                        );
                        let resp = sip_response(
                            &msg,
                            401,
                            "Unauthorized",
                            &[("WWW-Authenticate", &challenge)],
                            "",
                        );
                        sip.send_to(resp.as_bytes(), peer).await?;
                    }
                } else if msg.method == Some(SipMethod::Message) {
                    // 200 OK for keepalive notifies and for catalog response
                    // notifications alike.
                    let resp = sip_response(&msg, 200, "OK", &[], "");
                    sip.send_to(resp.as_bytes(), peer).await?;
                    // MANSCDP responses use attribute-style roots
                    // (`<Response CmdType="Catalog" ...>`); extract the
                    // channel items like a foreign NVR would.
                    if msg.body.contains("<Response CmdType=\"Catalog\"") {
                        let items = catalog_channels(&msg.body);
                        println!("[platform] Catalog: {} channel(s)", items.len());
                        for (id, status) in &items {
                            println!("[platform]   {id} status={status}");
                        }
                        if items.len() != 1 || items[0].0 != DEVICE_ID {
                            bail!("catalog must list exactly the demo channel {DEVICE_ID}");
                        }
                        println!("[platform] sending INVITE (live, UDP media)");
                        let invite = sip_request(
                            "INVITE",
                            DEVICE_ID,
                            call_id,
                            "1 INVITE",
                            &invite_sdp(rtp.local_addr()?.port()),
                        );
                        sip.send_to(invite.as_bytes(), ("127.0.0.1", 15060u16))
                            .await?;
                    } else if msg.body.contains("<Notify") {
                        println!("[platform] keepalive notify acknowledged");
                    }
                } else if msg.method.is_none() && msg.status_code == Some(SipStatusCode::Ok) {
                    let cseq = msg.get_header("CSeq").unwrap_or_default().to_string();
                    if cseq.contains("INVITE") && !invite_ok {
                        invite_ok = true;
                        println!("[platform] INVITE accepted (SDP answer {} bytes) — sending ACK", msg.body.len());
                        let ack = sip_request("ACK", DEVICE_ID, call_id, "1 ACK", "");
                        sip.send_to(ack.as_bytes(), ("127.0.0.1", 15060u16)).await?;
                    } else if cseq.contains("BYE") {
                        bye_ok = true;
                    }
                }
            }

            recv = rtp.recv_from(&mut rtp_buf), if rtp_enabled => {
                let (n, _) = recv.context("platform RTP socket")?;
                let b = &rtp_buf[..n];
                if b.len() < 12 {
                    continue;
                }
                let version = b[0] >> 6;
                let payload_type = b[1] & 0x7F;
                let marker = b[1] & 0x80 != 0;
                let ssrc = u32::from_be_bytes([b[8], b[9], b[10], b[11]]);
                if version != 2 || payload_type != 96 || ssrc != SSRC {
                    bail!("bad RTP header: v={version} pt={payload_type} ssrc={ssrc}");
                }
                stats.rtp_packets += 1;
                ps_reassembly.extend_from_slice(&b[12..]);
                if marker {
                    // Last fragment of the access unit: demux PS back to NALs.
                    let nalus = ps::parse_ps_to_nal_units(&ps_reassembly)
                        .context("demux PS frame")?;
                    let is_key = nalus.iter().any(|n| n[0] & 0x1F == 5);
                    if stats.frames < 3 || (stats.frames + 1) % 10 == 0 {
                        let types: Vec<u8> = nalus.iter().map(|n| n[0] & 0x1F).collect();
                        println!(
                            "[platform] frame #{}: {} NALs {:?} key={} ({} PS bytes)",
                            stats.frames + 1,
                            nalus.len(),
                            types,
                            is_key,
                            ps_reassembly.len()
                        );
                    }
                    if is_key {
                        stats.keyframes += 1;
                    }
                    if nalus.is_empty() {
                        bail!("demuxed frame has no NAL units");
                    }
                    stats.frames += 1;
                    stats.video_bytes += ps_reassembly.len();
                    ps_reassembly.clear();

                    if stats.frames >= FRAMES_TO_RECEIVE && !bye_sent {
                        bye_sent = true;
                        println!("[platform] {FRAMES_TO_RECEIVE} frames received — sending BYE");
                        let bye = sip_request("BYE", DEVICE_ID, call_id, "1 BYE", "");
                        sip.send_to(bye.as_bytes(), ("127.0.0.1", 15060u16)).await?;
                    }
                }
            }
        }
        if bye_ok {
            return Ok(stats);
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    println!("== gb28181-rs device demo: in-process platform <-> device ==\n");

    let platform_sip = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let platform_rtp = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let sip_addr: SocketAddr = platform_sip.local_addr()?;
    println!(
        "[platform] fake NVR on 127.0.0.1 (SIP {}, RTP {})",
        sip_addr.port(),
        platform_rtp.local_addr()?.port()
    );

    let config = Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: sip_addr.port(),
        device_id: DEVICE_ID.to_string(),
        channel_id: DEVICE_ID.to_string(),
        sip_domain: SIP_DOMAIN.to_string(),
        password: PASSWORD.to_string(),
        local_sip_port: 15060,
        register_interval_secs: 3600,
        heartbeat_interval_secs: 5,
        heartbeat_timeout_count: 3,
        transport: Transport::Udp,
    };

    let hub = Arc::new(MockFrameHub::new());
    tokio::spawn(produce_frames(Arc::clone(&hub)));

    Gb28181Server::start(config, hub, None).await?;
    println!(
        "[device]  gb28181-rs {} starting (SIP UDP :15060)",
        env!("CARGO_PKG_VERSION")
    );

    let stats = tokio::time::timeout(DEMO_TIMEOUT, run_platform(platform_sip, platform_rtp))
        .await
        .context("demo timed out")??;

    println!(
        "\n== summary: {} RTP packets -> {} frames ({} keyframes, {} video bytes), BYE acked ==",
        stats.rtp_packets, stats.frames, stats.keyframes, stats.video_bytes
    );
    if stats.keyframes == 0 {
        bail!("no keyframe received — GOP never fired?");
    }
    println!("device demo: all checks passed");
    Ok(())
}
