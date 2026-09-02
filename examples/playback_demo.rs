//! GB/T 28181 recording playback demo — RecordInfo + playback INVITE +
//! PS-over-RTP reassembly + PlaybackControl (pause / resume) + BYE.
//!
//! Extends the in-process fake-platform pattern of `device_demo` to the
//! recorded-media path. A synthetic segment (Annex-B file + `.ts.jsonl` PTS
//! sidecar, the crate's reference recording format) is written to a temp
//! directory and served through a `RecordingSource`; the fake platform then
//! walks the full NVR playback flow:
//!
//! 1. REGISTER → 401 Digest challenge → verified Authorization
//! 2. RecordInfo query (MANSCDP) → response must list the segment
//! 3. Playback INVITE (`s=Playback`, `t=<start> <end>`, `y=` SSRC)
//!    → 200 OK → ACK
//! 4. RTP/PS stream reassembled back to NAL units, paced by the segment PTS
//! 5. SIP INFO `PlaybackControl` PAUSE → frames must stop
//! 6. SIP INFO `PlaybackControl` PLAY (Speed 2) → frames must resume
//! 7. BYE → 200 OK, exit 0
//!
//! ```sh
//! cargo run --example playback_demo
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use md5::{Digest, Md5};
use tokio::net::UdpSocket;

use gb28181_rs::client::format_gb_time_ms;
use gb28181_rs::config::{Gb28181Config, Transport};
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::ps;
use gb28181_rs::server::Gb28181Server;
use gb28181_rs::sip::{parse_digest_auth, SipMessage, SipMethod, SipStatusCode};
use gb28181_rs::{RecordingSource, SegmentMeta};

const PLATFORM_ID: &str = "34020000002000000001";
const DEVICE_ID: &str = "34020000001320000001";
const SIP_DOMAIN: &str = "3402000000";
const PASSWORD: &str = "12345678";
const NONCE: &str = "play0123456789ab";
const SSRC: u32 = 778;
const CALL_ID: &str = "playback0001";

const FRAME_MS: u64 = 100;
const FRAME_COUNT: u64 = 30;
const GOP: u64 = 15;
/// Frames to receive before pausing, and total frames required to pass.
const FRAMES_BEFORE_PAUSE: usize = 8;

const DEMO_TIMEOUT: Duration = Duration::from_secs(60);

fn md5_hex(s: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Synthetic recording: Annex-B file + `.ts.jsonl` PTS sidecar
// ---------------------------------------------------------------------------

struct DemoRecording {
    root: PathBuf,
    meta: SegmentMeta,
}

impl DemoRecording {
    /// Write `FRAME_COUNT` access units at `FRAME_MS` cadence, starting
    /// `RECORD_AGE_MS` in the past (so the wall-clock window is real).
    fn write(record_age_ms: u64) -> Result<Self> {
        let root = std::env::temp_dir().join("gb28181-rs-playback-demo");
        std::fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
        let seg_path = root.join("demo-clip.h264");
        let sidecar = gb28181_rs::segment::sidecar_path(&seg_path);

        let start_ms = now_ms() - record_age_ms;
        let mut annexb = Vec::new();
        let mut pts_lines = String::new();
        for i in 0..FRAME_COUNT {
            let key = i % GOP == 0;
            let mut au = Vec::new();
            if key {
                au.extend_from_slice(&[0, 0, 0, 1, 0x67]); // SPS
                au.extend_from_slice(&[0x64, 0x00, 0x1f, 0xac]);
                au.extend_from_slice(&[0, 0, 0, 1, 0x68]); // PPS
                au.extend_from_slice(&[0xee, 0x3c, 0x80]);
                au.extend_from_slice(&[0, 0, 0, 1, 0x65]); // IDR
                au.extend(std::iter::repeat(0xAA).take(1200));
            } else {
                au.extend_from_slice(&[0, 0, 0, 1, 0x41]); // P slice
                au.extend(std::iter::repeat(0xBB).take(400));
            }
            annexb.extend_from_slice(&au);
            pts_lines.push_str(&format!("{{\"pts_ms\":{}}}\n", i * FRAME_MS));
        }
        std::fs::write(&seg_path, &annexb).context("write segment file")?;
        std::fs::write(&sidecar, pts_lines).context("write PTS sidecar")?;
        println!(
            "[record] {} access units @ {FRAME_MS} ms -> {} ({} bytes) + sidecar",
            FRAME_COUNT,
            seg_path.display(),
            annexb.len()
        );

        Ok(Self {
            root,
            meta: SegmentMeta {
                file: "demo-clip.h264".to_string(),
                start_ms,
                end_ms: start_ms + (FRAME_COUNT - 1) * FRAME_MS + FRAME_MS,
            },
        })
    }
}

impl RecordingSource for DemoRecording {
    fn lookup(&self, start_ms: u64, end_ms: u64) -> Vec<SegmentMeta> {
        // Inclusive-overlap test against the single demo segment.
        if start_ms <= self.meta.end_ms && end_ms >= self.meta.start_ms {
            vec![self.meta.clone()]
        } else {
            Vec::new()
        }
    }

    fn resolve_path(&self, file: &str) -> PathBuf {
        self.root.join(file)
    }
}

// ---------------------------------------------------------------------------
// Platform-side SIP crafting (raw wire text, like a foreign NVR)
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
    s.push_str(&format!("Content-Length: 0\r\n\r\n{body}"));
    s
}

fn sip_request(method: &str, target: &str, call_id: &str, cseq: &str, body: &str) -> String {
    let via = format!("SIP/2.0/UDP 127.0.0.1;branch=z9hG4bK{method}play");
    let mut s = format!(
        "{method} sip:{target}@{SIP_DOMAIN} SIP/2.0\r\n\
         Via: {via}\r\n\
         From: <sip:{PLATFORM_ID}@{SIP_DOMAIN}>;tag=platplay\r\n\
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
            "{ctype_header}: {ctype}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
            ctype_header = "Content-Type"
        ));
    }
    s
}

fn playback_sdp(rtp_port: u16, start_s: u64, end_s: u64) -> String {
    format!(
        "v=0\r\n\
         o={PLATFORM_ID} 0 0 IN IP4 127.0.0.1\r\n\
         s=Playback\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t={start_s} {end_s}\r\n\
         m=video {rtp_port} RTP/AVP 96\r\n\
         a=recvonly\r\n\
         y={SSRC:010}\r\n"
    )
}

fn playback_control_body(control_value: &str, speed: Option<f64>) -> String {
    let speed_xml = speed
        .map(|s| format!("<Speed>{s}</Speed>"))
        .unwrap_or_default();
    format!(
        "<Control><CmdType>PlaybackControl</CmdType><DeviceID>{DEVICE_ID}</DeviceID>\
         <Info><ControlValue>{control_value}</ControlValue>{speed_xml}</Info></Control>"
    )
}

fn xml_field(xml: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open).map_or(0, |p| p + open.len());
    let end = xml[start..].find(&close).map_or(start, |e| start + e);
    xml[start..end].to_string()
}

fn count_items(xml: &str) -> usize {
    xml.matches("<Item>").count()
}

// ---------------------------------------------------------------------------
// The fake platform loop
// ---------------------------------------------------------------------------

struct DemoStats {
    rtp_packets: usize,
    frames: usize,
    paused: bool,
    frames_at_pause: usize,
    frames_while_paused: usize,
}

#[allow(clippy::too_many_lines)]
async fn run_platform(
    sip: Arc<UdpSocket>,
    rtp: Arc<UdpSocket>,
    window_start_ms: u64,
    window_end_ms: u64,
) -> Result<DemoStats> {
    let mut sip_buf = vec![0u8; 65535];
    let mut rtp_buf = vec![0u8; 65535];
    let mut ps_reassembly: Vec<u8> = Vec::new();
    let mut stats = DemoStats {
        rtp_packets: 0,
        frames: 0,
        paused: false,
        frames_at_pause: 0,
        frames_while_paused: 0,
    };

    let mut registered = false;
    let mut invite_ok = false;
    let mut bye_sent = false;
    let mut bye_ok = false;
    let mut info_seq: u32 = 0;
    let deadline = Instant::now();

    loop {
        let rtp_enabled = invite_ok && !bye_ok;
        tokio::select! {
            recv = sip.recv_from(&mut sip_buf) => {
                let (n, peer) = recv.context("platform SIP socket")?;
                let text = std::str::from_utf8(&sip_buf[..n]).context("non-UTF8 SIP")?;
                let msg = match SipMessage::parse(text) {
                    Ok(m) => m,
                    Err(e) => {
                        if std::env::var_os("DEMO_DEBUG").is_some() {
                            eprintln!("[platform] unparseable SIP: {e}: {text}");
                        }
                        continue;
                    }
                };

                if msg.method == Some(SipMethod::Register) {
                    if let Some(auth) = msg.get_header("Authorization") {
                        let params = parse_digest_auth(auth).context("parse Authorization")?;
                        let ha1 = md5_hex(&format!("{DEVICE_ID}:{SIP_DOMAIN}:{PASSWORD}"));
                        let ha2 = md5_hex(&format!("REGISTER:{}", params.uri));
                        let expected = md5_hex(&format!("{}:{}:{ha2}", ha1, params.nonce));
                        if params.response != expected {
                            bail!("device digest response mismatch");
                        }
                        println!("[platform] REGISTER digest VERIFIED");
                        let resp = sip_response(&msg, 200, "OK", &[], "");
                        sip.send_to(resp.as_bytes(), peer).await?;
                        if !registered {
                            registered = true;
                            println!("[platform] device registered — sending RecordInfo query");
                            let query = format!(
                                "<Query><CmdType>RecordInfo</CmdType><SN>7</SN>\
                                 <DeviceID>{DEVICE_ID}</DeviceID>\
                                 <StartTime>{}</StartTime><EndTime>{}</EndTime></Query>",
                                format_gb_time_ms(window_start_ms),
                                format_gb_time_ms(window_end_ms),
                            );
                            let q = sip_request("MESSAGE", DEVICE_ID, "recinfo001", "1 MESSAGE", &query);
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
                    let resp = sip_response(&msg, 200, "OK", &[], "");
                    sip.send_to(resp.as_bytes(), peer).await?;
                    if msg.body.contains("<Response CmdType=\"RecordInfo\"") {
                        let items = count_items(&msg.body);
                        let sum = xml_field(&msg.body, "SumNum");
                        println!("[platform] RecordInfo: SumNum={sum}, {items} item(s)");
                        if items != 1 || sum != "1" {
                            bail!("RecordInfo must list exactly one recording, got {items} (SumNum={sum})");
                        }
                        println!("[platform] sending playback INVITE (s=Playback, t={} {})",
                                 window_start_ms / 1000, window_end_ms / 1000);
                        let invite = sip_request(
                            "INVITE",
                            DEVICE_ID,
                            CALL_ID,
                            "1 INVITE",
                            &playback_sdp(
                                rtp.local_addr()?.port(),
                                window_start_ms / 1000,
                                window_end_ms / 1000,
                            ),
                        );
                        sip.send_to(invite.as_bytes(), ("127.0.0.1", 15060u16)).await?;
                    }
                } else if msg.method.is_none() && msg.status_code == Some(SipStatusCode::Ok) {
                    let cseq = msg.get_header("CSeq").unwrap_or_default().to_string();
                    if cseq.contains("INVITE") && !invite_ok {
                        invite_ok = true;
                        println!("[platform] playback INVITE accepted — sending ACK");
                        let ack = sip_request("ACK", DEVICE_ID, CALL_ID, "1 ACK", "");
                        sip.send_to(ack.as_bytes(), ("127.0.0.1", 15060u16)).await?;
                    } else if cseq.contains("INFO") {
                        // PlaybackControl acknowledged by the device.
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
                    let nalus = ps::parse_ps_to_nal_units(&ps_reassembly)
                        .context("demux PS frame")?;
                    if nalus.is_empty() {
                        bail!("demuxed frame has no NAL units");
                    }
                    stats.frames += 1;
                    ps_reassembly.clear();
                    if stats.paused {
                        stats.frames_while_paused += 1;
                    }
                    if stats.frames <= 3 || stats.frames % 10 == 0 {
                        println!("[platform] playback frame #{}", stats.frames);
                    }

                    if stats.frames == FRAMES_BEFORE_PAUSE && !stats.paused {
                        stats.paused = true;
                        stats.frames_at_pause = stats.frames;
                        info_seq += 1;
                        println!("[platform] sending PlaybackControl PAUSE");
                        let info = sip_request(
                            "INFO",
                            DEVICE_ID,
                            CALL_ID,
                            &format!("{info_seq} INFO"),
                            &playback_control_body("PAUSE", None),
                        );
                        sip.send_to(info.as_bytes(), ("127.0.0.1", 15060u16)).await?;
                        // Give the paused stream a moment to prove it stopped.
                        tokio::time::sleep(Duration::from_millis(900)).await;
                        println!(
                            "[platform] while paused: {} extra frames (must be 0)",
                            stats.frames_while_paused
                        );
                        if stats.frames_while_paused > 0 {
                            bail!("PAUSE did not stop the stream");
                        }
                        info_seq += 1;
                        println!("[platform] sending PlaybackControl PLAY (Speed 2)");
                        let info = sip_request(
                            "INFO",
                            DEVICE_ID,
                            CALL_ID,
                            &format!("{info_seq} INFO"),
                            &playback_control_body("PLAY", Some(2.0)),
                        );
                        sip.send_to(info.as_bytes(), ("127.0.0.1", 15060u16)).await?;
                        stats.paused = false;
                    }

                    if stats.frames + stats.frames_while_paused >= (FRAME_COUNT as usize) - 2
                        && !bye_sent
                    {
                        bye_sent = true;
                        println!("[platform] playback complete — sending BYE");
                        let bye = sip_request("BYE", DEVICE_ID, CALL_ID, "1 BYE", "");
                        sip.send_to(bye.as_bytes(), ("127.0.0.1", 15060u16)).await?;
                    }
                }
            }
        }
        if bye_ok {
            return Ok(stats);
        }
        if deadline.elapsed() > DEMO_TIMEOUT {
            bail!("platform loop exceeded {DEMO_TIMEOUT:?}");
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    println!("== gb28181-rs playback demo: RecordInfo + paced playback + pause/resume ==\n");

    let recording = DemoRecording::write(120_000)?;
    let window_start = recording.meta.start_ms;
    let window_end = recording.meta.end_ms + 1000;

    let platform_sip = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    let platform_rtp = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
    println!(
        "[platform] fake NVR on 127.0.0.1 (SIP {}, RTP {})",
        platform_sip.local_addr()?.port(),
        platform_rtp.local_addr()?.port()
    );

    let config = Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: platform_sip.local_addr()?.port(),
        device_id: DEVICE_ID.to_string(),
        channel_id: DEVICE_ID.to_string(),
        sip_domain: SIP_DOMAIN.to_string(),
        password: PASSWORD.to_string(),
        local_sip_port: 15060,
        register_interval_secs: 3600,
        heartbeat_interval_secs: 3600,
        heartbeat_timeout_count: 3,
        transport: Transport::Udp,
        ..Gb28181Config::default()
    };

    let hub = Arc::new(MockFrameHub::new());
    let server = Gb28181Server::start(config, hub, Some(Arc::new(recording))).await?;
    println!(
        "[device]  gb28181-rs {} starting with recording index (SIP UDP :15060)",
        env!("CARGO_PKG_VERSION")
    );

    let sip = Arc::clone(&platform_sip);
    let rtp = Arc::clone(&platform_rtp);
    let stats = tokio::time::timeout(
        DEMO_TIMEOUT,
        run_platform(sip, rtp, window_start, window_end),
    )
    .await
    .context("demo timed out")??;

    println!(
        "\n== summary: {} RTP packets -> {} frames (paused cleanly at #{}, {} while paused), BYE acked ==",
        stats.rtp_packets, stats.frames, stats.frames_at_pause, stats.frames_while_paused
    );
    if stats.frames_at_pause == 0 {
        bail!("never reached the pause point");
    }
    println!("playback demo: all checks passed");
    // Keep the server handle alive across the demo and let it exit cleanly
    // with the demo timeout instead of being dropped unsupervised.
    server.abort();

    Ok(())
}
