//! Playback / Download media task (GB/T 28181 §7.4.1).
//!
//! Streams recorded segments to the platform as PS-over-RTP, mirroring the
//! live path (`run_media_task`) but reading from the recording index instead
//! of AuHub. `Playback` paces frames to wall-clock time relative to the
//! requested range start; `Download` sends as fast as possible.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, Mutex};

use crate::manscdp::{empty_string_as_none, parse_gb_time_ms};
use crate::ps::mux_h264_to_ps;
use crate::rtp_pusher::RtpPusher;
use crate::segment::read_segment;
use crate::server::{build_rtp_packet_raw, frame_rtp_over_tcp, PS_PAYLOAD_TYPE};
use crate::{RecordingSource, SegmentMeta};

/// Maximum RTP payload size for PS data (matches the live path).
const MAX_RTP_PAYLOAD: usize = 1400;

/// A playback control command received via SIP INFO `PlaybackControl`.
///
/// `Pause` stops pushing (the session stays alive); `Play` resumes,
/// optionally seeking to `start_ms` (absolute wall-clock ms) and/or setting
/// the pacing `speed` multiplier. Unknown `ControlValue`s are dropped by the
/// parser (the server logs and still answers 200 OK).
#[derive(Debug, Clone, PartialEq)]
pub enum PlaybackControl {
    /// Stop pushing frames; the session remains active until PLAY or BYE.
    Pause,
    /// Resume pushing, optionally seeking and/or changing speed.
    Play {
        /// Seek target (absolute wall-clock ms), if the platform supplied one.
        start_ms: Option<u64>,
        /// Pacing multiplier (e.g. 2.0 = double speed), if supplied.
        speed: Option<f64>,
    },
}

/// Child-element form of a `<Control>` body (matches live MiBee NVR).
#[derive(Debug, Deserialize)]
#[serde(rename = "Control")]
struct ControlBody {
    #[serde(rename = "CmdType")]
    cmd_type: String,
    #[serde(rename = "Info")]
    info: ControlInfo,
}

/// The `<Info>` block of a PlaybackControl, parsed leniently.
#[derive(Debug, Deserialize)]
struct ControlInfo {
    #[serde(rename = "ControlValue", default)]
    control_value: String,
    #[serde(
        rename = "StartTime",
        default,
        deserialize_with = "empty_string_as_none"
    )]
    start_time: Option<String>,
    // EndTime is tolerated in the XML but not used for control (serde
    // ignores unknown fields), so it is intentionally not a struct field.
    #[serde(rename = "Speed", default, deserialize_with = "empty_string_as_none")]
    speed: Option<String>,
    #[serde(rename = "Scale", default, deserialize_with = "empty_string_as_none")]
    scale: Option<String>,
}

/// Parse a SIP INFO `PlaybackControl` body into a [`PlaybackControl`].
///
/// Lenient: tolerates `Scale` in place of `Speed`, missing optional fields,
/// and an absent `Info` block. Returns `None` when the body is not a
/// PlaybackControl or carries an unknown `ControlValue` (caller logs + 200 OK).
pub(crate) fn parse_playback_control(body: &str) -> Option<PlaybackControl> {
    let ctl = serde_xml_rs::from_str::<ControlBody>(body).ok()?;
    if ctl.cmd_type != "PlaybackControl" {
        return None;
    }
    let info = ctl.info;
    let speed = info
        .speed
        .or(info.scale)
        .as_deref()
        .and_then(|s| s.trim().parse::<f64>().ok());
    match info.control_value.trim().to_ascii_uppercase().as_str() {
        "PAUSE" => Some(PlaybackControl::Pause),
        "PLAY" => Some(PlaybackControl::Play {
            start_ms: info.start_time.as_deref().and_then(parse_gb_time_ms),
            speed,
        }),
        _ => None,
    }
}

/// One frame scheduled for playback, with its absolute wall-clock offset.
struct Frame {
    /// Absolute wall-clock ms (segment start + AU pts offset).
    abs_ms: u64,
    /// True if this is a key frame (IDR).
    is_key: bool,
    /// NALU payloads (without start codes).
    nalus: Vec<Vec<u8>>,
}

/// Mutable state of an in-progress playback stream.
struct PlaybackState {
    frames: Vec<Frame>,
    idx: usize,
    pacing_base: tokio::time::Instant,
    base_ms: Option<u64>,
    pts: u64,
    speed: f64,
    paused: bool,
}

/// Build the ordered list of frames to emit for `[start_ms, end_ms]`, applying
/// the keyframe fast-forward (parity with the Go repo): in-range non-keyframe
/// AUs are dropped until the first keyframe is reached.
fn build_frames(
    source: &dyn RecordingSource,
    segments: &[SegmentMeta],
    start_ms: u64,
    end_ms: u64,
) -> Vec<Frame> {
    let mut frames = Vec::new();
    for seg in segments {
        // Skip segments entirely before the requested start; stop once past
        // the requested end (segments are ordered by start time).
        if seg.end_ms < start_ms {
            continue;
        }
        if seg.start_ms > end_ms {
            break;
        }
        let path = source.resolve_path(&seg.file);
        let aus = match read_segment(&path) {
            Ok(aus) => aus,
            Err(e) => {
                eprintln!(
                    "gb28181: playback: failed to read segment {}: {e}",
                    path.display()
                );
                continue;
            }
        };
        // Fast-forward: skip AUs whose pts_offset is before the requested
        // start within this segment.
        let skip_before_ms = start_ms.saturating_sub(seg.start_ms);
        let mut sent_keyframe = false;
        for au in aus {
            let au_ms = au.pts_offset.as_millis() as u64;
            if au_ms < skip_before_ms {
                continue;
            }
            let abs_ms = seg.start_ms + au_ms;
            if abs_ms > end_ms {
                break;
            }
            if !sent_keyframe {
                if !au.is_key_frame {
                    continue;
                }
                sent_keyframe = true;
            }
            frames.push(Frame {
                abs_ms,
                is_key: au.is_key_frame,
                nalus: au.nalus,
            });
        }
    }
    frames
}

/// Apply a control command to the playback state.
fn apply_control(
    state: &mut PlaybackState,
    source: &dyn RecordingSource,
    segments: &[SegmentMeta],
    end_ms: u64,
    control: PlaybackControl,
) {
    match control {
        PlaybackControl::Pause => state.paused = true,
        PlaybackControl::Play { start_ms, speed } => {
            state.paused = false;
            if let Some(speed) = speed {
                state.speed = speed;
            }
            if let Some(seek) = start_ms {
                // Seek: rebuild the frame list from the new position and
                // restart iteration (keyframe fast-forward applies).
                state.frames = build_frames(source, segments, seek, end_ms);
                state.idx = 0;
                state.pts = 0;
            }
            // Re-anchor pacing so a resume/seek does not burst.
            state.pacing_base = tokio::time::Instant::now();
            state.base_ms = None;
        }
    }
}

/// Stream recorded segments as PS-over-RTP to the platform.
///
/// `segments` must already be the result of `lookup(start_ms, end_ms)`.
/// Frames whose `pts_offset` falls before `start_ms` (within a segment) are
/// skipped (fast-forward); frames past `end_ms` stop the stream. When `paced`
/// is true (Playback), each frame is emitted on a wall-clock schedule anchored
/// at task start; when false (Download), frames are sent back-to-back. Control
/// commands arrive on `ctl` (PAUSE/PLAY/seek/speed) and are applied live.
#[allow(clippy::too_many_arguments)]
pub async fn run_playback_task(
    source: Arc<dyn RecordingSource>,
    segments: Vec<SegmentMeta>,
    start_ms: u64,
    end_ms: u64,
    media_socket: Arc<UdpSocket>,
    media_tcp_conn: Option<Arc<Mutex<TcpStream>>>,
    ssrc: u32,
    device_id: &str,
    remote_addr: SocketAddr,
    paced: bool,
    mut ctl: mpsc::Receiver<PlaybackControl>,
) -> Result<()> {
    let mut rtp_pusher = RtpPusher::new(remote_addr, ssrc, PS_PAYLOAD_TYPE);
    let mut state = PlaybackState {
        frames: build_frames(source.as_ref(), &segments, start_ms, end_ms),
        idx: 0,
        pacing_base: tokio::time::Instant::now(),
        base_ms: None,
        pts: 0,
        speed: 1.0,
        paused: false,
    };
    let mut ctl_open = true;

    println!("gb28181: playback task started for device {device_id} (paced={paced})");

    loop {
        if state.paused {
            // Paused: watch only the control channel until PLAY or BYE.
            match ctl.recv().await {
                Some(c) => apply_control(&mut state, source.as_ref(), &segments, end_ms, c),
                None => break, // channel closed (BYE) -> end
            }
            continue;
        }

        if state.idx >= state.frames.len() {
            break;
        }

        let abs_ms = state.frames[state.idx].abs_ms;
        let base = *state.base_ms.get_or_insert(abs_ms);
        let offset = abs_ms - base;
        let target =
            state.pacing_base + Duration::from_millis((offset as f64 / state.speed) as u64);

        if paced {
            let now = tokio::time::Instant::now();
            if target > now {
                let sleep = tokio::time::sleep(target - now);
                tokio::pin!(sleep);
                tokio::select! {
                    _ = &mut sleep => {}
                    c = ctl.recv(), if ctl_open => {
                        match c {
                            Some(c) => {
                                apply_control(
                                    &mut state,
                                    source.as_ref(),
                                    &segments,
                                    end_ms,
                                    c,
                                );
                                continue;
                            }
                            None => ctl_open = false,
                        }
                    }
                }
            }
        } else if let Ok(c) = ctl.try_recv() {
            // Download: check for control without blocking.
            apply_control(&mut state, source.as_ref(), &segments, end_ms, c);
            continue;
        }

        // Send the current frame.
        let frame = &state.frames[state.idx];
        let nalu_slices: Vec<&[u8]> = frame.nalus.iter().map(|n| n.as_slice()).collect();
        state.pts += 3000;
        let ps_data = mux_h264_to_ps(&nalu_slices, frame.is_key, state.pts, state.pts);

        let chunk_count = ps_data.len().div_ceil(MAX_RTP_PAYLOAD);
        for (i, chunk) in ps_data.chunks(MAX_RTP_PAYLOAD).enumerate() {
            let is_last = i == chunk_count - 1;
            let rtp_packet = build_rtp_packet_raw(
                ssrc,
                PS_PAYLOAD_TYPE,
                rtp_pusher.sequence_number,
                rtp_pusher.timestamp,
                chunk,
                is_last,
            );
            rtp_pusher.sequence_number = rtp_pusher.sequence_number.wrapping_add(1);

            if let Some(conn) = &media_tcp_conn {
                let mut conn = conn.lock().await;
                let frame = frame_rtp_over_tcp(&rtp_packet);
                if let Err(e) = conn.write_all(&frame).await {
                    eprintln!("gb28181: failed to send RTP packet over TCP: {e}");
                    return Ok(());
                }
            } else if let Err(e) = media_socket.send_to(&rtp_packet, remote_addr).await {
                eprintln!("gb28181: failed to send RTP packet: {e}");
                return Ok(());
            }
        }
        rtp_pusher.increment_timestamp(3000);
        state.idx += 1;
    }

    println!("gb28181: playback task ended for device {device_id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::segment::sidecar_path;

    static TEMP_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mibee-playback-test-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// A control receiver whose sender is dropped: the task runs with no
    /// control input (equivalent to a session that never receives INFO).
    fn no_control() -> mpsc::Receiver<PlaybackControl> {
        let (tx, rx) = mpsc::channel(8);
        drop(tx);
        rx
    }

    /// Write a synthetic segment (Annex-B NALUs + per-frame `.ts.jsonl`
    /// sidecar, matching the recording writer's format) and return its meta.
    fn write_segment(root: &Path, name: &str, aus: &[(bool, Vec<Vec<u8>>, u64)]) -> SegmentMeta {
        let path = root.join(name);
        let mut bytes = Vec::new();
        for (_, nalus, _) in aus {
            for n in nalus {
                bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                bytes.extend_from_slice(n);
            }
        }
        fs::write(&path, bytes).expect("write segment");
        let mut ts = String::new();
        for (_, _, pts_ms) in aus {
            ts.push_str(&format!("{{\"pts_ms\":{pts_ms}}}\n"));
        }
        fs::write(sidecar_path(&path), ts).expect("write sidecar");
        SegmentMeta {
            file: name.to_string(),
            start_ms: 1_000_000,
            end_ms: 1_000_000 + 100,
        }
    }

    /// Fake recording source rooted at a temp dir.
    struct TestSource {
        segments: Vec<SegmentMeta>,
        root: PathBuf,
    }

    impl RecordingSource for TestSource {
        fn lookup(&self, _start_ms: u64, _end_ms: u64) -> Vec<SegmentMeta> {
            self.segments.clone()
        }
        fn resolve_path(&self, file: &str) -> PathBuf {
            self.root.join(file)
        }
    }

    /// Receive `count` RTP packets on a loopback socket, returning arrival
    /// offsets (ms from `start`) and payloads.
    async fn receive_rtp(
        receiver: &UdpSocket,
        start: tokio::time::Instant,
        count: usize,
    ) -> Vec<(u128, Vec<u8>)> {
        let mut buf = vec![0u8; 2048];
        let mut packets = Vec::new();
        for _ in 0..count {
            let (len, _) =
                tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut buf))
                    .await
                    .expect("timed out waiting for RTP packet")
                    .expect("recv failed");
            packets.push((start.elapsed().as_millis(), buf[..len].to_vec()));
        }
        packets
    }

    fn has_psm(packet: &[u8]) -> bool {
        packet.windows(4).any(|w| w == [0x00, 0x00, 0x01, 0xBB])
    }

    fn has_nalu(packet: &[u8], nalu: &[u8]) -> bool {
        packet.windows(nalu.len()).any(|w| w == nalu)
    }

    #[tokio::test]
    async fn test_playback_paced_sends_rtp() {
        let dir = temp_dir();
        let seg = write_segment(
            &dir,
            "0000.h264",
            &[
                (true, vec![vec![0x65, 0x88]], 0),
                (false, vec![vec![0x61, 0x88]], 40),
                (false, vec![vec![0x61, 0x89]], 80),
            ],
        );
        let source = Arc::new(TestSource {
            segments: vec![seg.clone()],
            root: dir.clone(),
        });
        let receiver = UdpSocket::bind("127.0.0.1:0").await.expect("bind receiver");
        let remote = receiver.local_addr().expect("receiver addr");
        let media_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind media"));

        let start = tokio::time::Instant::now();
        let task = tokio::spawn(run_playback_task(
            source,
            vec![seg.clone()],
            seg.start_ms,
            seg.end_ms,
            media_socket,
            None,
            12345,
            "dev",
            remote,
            true,
            no_control(),
        ));
        let packets = receive_rtp(&receiver, start, 3).await;
        task.await.expect("task join").expect("task ok");

        // First packet immediate; second ~40ms; third ~80ms (±50ms tolerance).
        assert!(packets[0].0 < 50, "first packet should be immediate");
        let d1 = packets[1].0 as i64;
        assert!((d1 - 40).abs() <= 50, "second packet at ~40ms, got {d1}ms");
        let d2 = packets[2].0 as i64;
        assert!((d2 - 80).abs() <= 50, "third packet at ~80ms, got {d2}ms");
        // First packet is a keyframe (PSM present).
        assert!(has_psm(&packets[0].1), "keyframe packet must carry PSM");
    }

    #[tokio::test]
    async fn test_playback_download_no_pacing() {
        let dir = temp_dir();
        let seg = write_segment(
            &dir,
            "0000.h264",
            &[
                (true, vec![vec![0x65, 0x88]], 0),
                (false, vec![vec![0x61, 0x88]], 40),
                (false, vec![vec![0x61, 0x89]], 80),
            ],
        );
        let source = Arc::new(TestSource {
            segments: vec![seg.clone()],
            root: dir.clone(),
        });
        let receiver = UdpSocket::bind("127.0.0.1:0").await.expect("bind receiver");
        let remote = receiver.local_addr().expect("receiver addr");
        let media_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind media"));

        let start = tokio::time::Instant::now();
        let task = tokio::spawn(run_playback_task(
            source,
            vec![seg.clone()],
            seg.start_ms,
            seg.end_ms,
            media_socket,
            None,
            12345,
            "dev",
            remote,
            false,
            no_control(),
        ));
        let packets = receive_rtp(&receiver, start, 3).await;
        task.await.expect("task join").expect("task ok");

        // Download sends back-to-back: all three packets arrive quickly.
        assert!(
            packets[2].0 < 200,
            "download must not pace, third packet at {}ms",
            packets[2].0
        );
    }

    /// Go-parity: a mid-GOP start must fast-forward to the NEXT keyframe —
    /// the first emitted frame carries PSM and skipped AUs never hit the wire.
    #[tokio::test]
    async fn test_playback_fast_forwards_to_keyframe() {
        let dir = temp_dir();
        let mut seg = write_segment(
            &dir,
            "0000.h264",
            &[
                (true, vec![vec![0x65, 0x88]], 0),
                (false, vec![vec![0x61, 0x88]], 40),
                (false, vec![vec![0x61, 0x89]], 80),
                (true, vec![vec![0x65, 0x99]], 120),
                (false, vec![vec![0x61, 0xAA]], 160),
            ],
        );
        seg.end_ms = seg.start_ms + 200;
        let source = Arc::new(TestSource {
            segments: vec![seg.clone()],
            root: dir.clone(),
        });
        let receiver = UdpSocket::bind("127.0.0.1:0").await.expect("bind receiver");
        let remote = receiver.local_addr().expect("receiver addr");
        let media_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind media"));

        // Request starts 60ms in: lands mid-GOP between the two IDRs.
        let start_ms = seg.start_ms + 60;
        let task = tokio::spawn(run_playback_task(
            source,
            vec![seg.clone()],
            start_ms,
            seg.end_ms,
            media_socket,
            None,
            12345,
            "dev",
            remote,
            false,
            no_control(),
        ));
        let packets = receive_rtp(&receiver, tokio::time::Instant::now(), 2).await;
        task.await.expect("task join").expect("task ok");

        // First emitted packet is the IDR at 120ms (PSM present), and the
        // skipped mid-GOP AUs (40/80ms) never reach the wire.
        assert!(
            has_psm(&packets[0].1),
            "first sent frame must be a keyframe"
        );
        assert!(
            has_nalu(&packets[0].1, &[0x65, 0x99]),
            "IDR at 120ms must be first"
        );
        assert!(
            !has_nalu(&packets[0].1, &[0x61, 0x89]),
            "mid-GOP AU at 80ms must be skipped"
        );
        assert!(
            has_nalu(&packets[1].1, &[0x61, 0xAA]),
            "AU at 160ms follows the IDR"
        );
    }

    #[tokio::test]
    async fn test_playback_stops_at_range_end() {
        let dir = temp_dir();
        let seg = write_segment(
            &dir,
            "0000.h264",
            &[
                (true, vec![vec![0x65, 0x88]], 0),
                (false, vec![vec![0x61, 0x88]], 40),
                (false, vec![vec![0x61, 0x89]], 80),
            ],
        );
        let source = Arc::new(TestSource {
            segments: vec![seg.clone()],
            root: dir.clone(),
        });
        let receiver = UdpSocket::bind("127.0.0.1:0").await.expect("bind receiver");
        let remote = receiver.local_addr().expect("receiver addr");
        let media_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind media"));

        // Request ends 40ms into the segment: only the first AU is emitted.
        let end_ms = seg.start_ms + 40;
        let task = tokio::spawn(run_playback_task(
            source,
            vec![seg.clone()],
            seg.start_ms,
            end_ms,
            media_socket,
            None,
            12345,
            "dev",
            remote,
            false,
            no_control(),
        ));
        let packets = receive_rtp(&receiver, tokio::time::Instant::now(), 1).await;
        task.await.expect("task join").expect("task ok");

        assert!(has_psm(&packets[0].1), "only the keyframe AU is in range");
    }

    // ── PlaybackControl parser variants ─────────────────────────────────

    #[test]
    fn test_parse_playback_control_pause() {
        let body = "<Control><CmdType>PlaybackControl</CmdType><SN>1</SN><DeviceID>34020000001320000001</DeviceID><Info><ControlValue>PAUSE</ControlValue></Info></Control>";
        assert_eq!(parse_playback_control(body), Some(PlaybackControl::Pause));
    }

    #[test]
    fn test_parse_playback_control_play_no_extras() {
        let body = "<Control><CmdType>PlaybackControl</CmdType><Info><ControlValue>PLAY</ControlValue></Info></Control>";
        assert_eq!(
            parse_playback_control(body),
            Some(PlaybackControl::Play {
                start_ms: None,
                speed: None,
            })
        );
    }

    #[test]
    fn test_parse_playback_control_play_with_start_and_speed() {
        let body = "<Control><CmdType>PlaybackControl</CmdType><Info><ControlValue>PLAY</ControlValue><StartTime>2026-08-15T14:30:00Z</StartTime><EndTime>2026-08-15T14:35:00Z</EndTime><Speed>2</Speed></Info></Control>";
        let ctl = parse_playback_control(body).expect("parse");
        match ctl {
            PlaybackControl::Play { start_ms, speed } => {
                assert_eq!(start_ms, Some(1_786_804_200_000));
                assert_eq!(speed, Some(2.0));
            }
            PlaybackControl::Pause => panic!("expected Play"),
        }
    }

    #[test]
    fn test_parse_playback_control_scale_fallback() {
        // Some platforms send Scale instead of Speed.
        let body = "<Control><CmdType>PlaybackControl</CmdType><Info><ControlValue>PLAY</ControlValue><Scale>4</Scale></Info></Control>";
        let ctl = parse_playback_control(body).expect("parse");
        match ctl {
            PlaybackControl::Play { speed, .. } => assert_eq!(speed, Some(4.0)),
            PlaybackControl::Pause => panic!("expected Play"),
        }
    }

    #[test]
    fn test_parse_playback_control_unknown_value() {
        let body = "<Control><CmdType>PlaybackControl</CmdType><Info><ControlValue>FOO</ControlValue></Info></Control>";
        assert_eq!(parse_playback_control(body), None);
    }

    #[test]
    fn test_parse_playback_control_not_playback_cmd() {
        let body = "<Control><CmdType>DeviceControl</CmdType><Info><ControlValue>PLAY</ControlValue></Info></Control>";
        assert_eq!(parse_playback_control(body), None);
    }

    #[test]
    fn test_parse_playback_control_missing_info() {
        let body = "<Control><CmdType>PlaybackControl</CmdType></Control>";
        assert_eq!(parse_playback_control(body), None);
    }

    // ── PlaybackControl E2E ────────────────────────────────────────────

    /// Receive one RTP packet within `timeout`, or None if none arrives.
    async fn try_receive_rtp(receiver: &UdpSocket, timeout: Duration) -> Option<Vec<u8>> {
        let mut buf = vec![0u8; 2048];
        tokio::time::timeout(timeout, receiver.recv_from(&mut buf))
            .await
            .ok()
            .and_then(|r| r.ok())
            .map(|(len, _)| buf[..len].to_vec())
    }

    /// A long synthetic recording: 10 frames at 40ms spacing (400ms total).
    fn write_long_segment(root: &Path, name: &str) -> SegmentMeta {
        let mut aus = Vec::new();
        for i in 0..10 {
            let is_key = i == 0;
            aus.push((is_key, vec![vec![0x65, 0x88 + i as u8]], i * 40));
        }
        write_segment(root, name, &aus)
    }

    #[tokio::test]
    async fn test_playback_pause_stops_and_play_resumes() {
        let dir = temp_dir();
        let seg = write_long_segment(&dir, "0000.h264");
        let source = Arc::new(TestSource {
            segments: vec![seg.clone()],
            root: dir.clone(),
        });
        let receiver = UdpSocket::bind("127.0.0.1:0").await.expect("bind receiver");
        let remote = receiver.local_addr().expect("receiver addr");
        let media_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind media"));
        let (ctl_tx, ctl_rx) = mpsc::channel::<PlaybackControl>(8);

        let task = tokio::spawn(run_playback_task(
            source,
            vec![seg.clone()],
            seg.start_ms,
            seg.end_ms,
            media_socket,
            None,
            12345,
            "dev",
            remote,
            true,
            ctl_rx,
        ));

        // First frame arrives immediately.
        let first = try_receive_rtp(&receiver, Duration::from_secs(2))
            .await
            .expect("first frame");
        assert!(has_psm(&first), "first frame is a keyframe");

        // PAUSE: no RTP for 300ms.
        ctl_tx
            .send(PlaybackControl::Pause)
            .await
            .expect("send pause");
        assert!(
            try_receive_rtp(&receiver, Duration::from_millis(300))
                .await
                .is_none(),
            "no RTP while paused"
        );

        // PLAY: streaming resumes.
        ctl_tx
            .send(PlaybackControl::Play {
                start_ms: None,
                speed: None,
            })
            .await
            .expect("send play");
        let resumed = try_receive_rtp(&receiver, Duration::from_secs(2))
            .await
            .expect("resumed frame");
        assert!(has_psm(&resumed), "resumed frame is a keyframe");

        drop(ctl_tx);
        task.await.expect("task join").expect("task ok");
    }

    #[tokio::test]
    async fn test_playback_seek_jumps_to_new_position() {
        let dir = temp_dir();
        // Two segments with distinct NALU content.
        let seg1 = write_segment(
            &dir,
            "0000.h264",
            &[
                (true, vec![vec![0x65, 0x11]], 0),
                (false, vec![vec![0x61, 0x12]], 40),
            ],
        );
        let mut seg2 = write_segment(
            &dir,
            "0001.h264",
            &[
                (true, vec![vec![0x65, 0x22]], 0),
                (false, vec![vec![0x61, 0x23]], 40),
            ],
        );
        seg2.start_ms = seg1.end_ms + 1;
        seg2.end_ms = seg2.start_ms + 100;
        let source = Arc::new(TestSource {
            segments: vec![seg1.clone(), seg2.clone()],
            root: dir.clone(),
        });
        let receiver = UdpSocket::bind("127.0.0.1:0").await.expect("bind receiver");
        let remote = receiver.local_addr().expect("receiver addr");
        let media_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind media"));
        let (ctl_tx, ctl_rx) = mpsc::channel::<PlaybackControl>(8);

        let task = tokio::spawn(run_playback_task(
            source,
            vec![seg1.clone(), seg2.clone()],
            seg1.start_ms,
            seg2.end_ms,
            media_socket,
            None,
            12345,
            "dev",
            remote,
            true,
            ctl_rx,
        ));

        // First frame is seg1's keyframe.
        let first = try_receive_rtp(&receiver, Duration::from_secs(2))
            .await
            .expect("first frame");
        assert!(has_nalu(&first, &[0x65, 0x11]), "first frame from seg1");

        // Seek to seg2's start: next frame is seg2's keyframe (PSM + content).
        ctl_tx
            .send(PlaybackControl::Play {
                start_ms: Some(seg2.start_ms),
                speed: None,
            })
            .await
            .expect("send seek");
        let seeked = try_receive_rtp(&receiver, Duration::from_secs(2))
            .await
            .expect("seeked frame");
        assert!(has_psm(&seeked), "seeked frame carries PSM");
        assert!(
            has_nalu(&seeked, &[0x65, 0x22]),
            "seeked frame is seg2's keyframe"
        );

        drop(ctl_tx);
        task.await.expect("task join").expect("task ok");
    }

    #[tokio::test]
    async fn test_playback_speed_multiplier_paces_faster() {
        let dir = temp_dir();
        // 5 frames at 100ms spacing.
        let mut seg = write_segment(
            &dir,
            "0000.h264",
            &[
                (true, vec![vec![0x65, 0x88]], 0),
                (false, vec![vec![0x61, 0x89]], 100),
                (false, vec![vec![0x61, 0x8A]], 200),
                (false, vec![vec![0x61, 0x8B]], 300),
                (false, vec![vec![0x61, 0x8C]], 400),
            ],
        );
        // Keep all five frames in range (write_segment defaults to +100ms).
        seg.end_ms = seg.start_ms + 500;
        let source = Arc::new(TestSource {
            segments: vec![seg.clone()],
            root: dir.clone(),
        });
        let receiver = UdpSocket::bind("127.0.0.1:0").await.expect("bind receiver");
        let remote = receiver.local_addr().expect("receiver addr");
        let media_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind media"));
        let (ctl_tx, ctl_rx) = mpsc::channel::<PlaybackControl>(8);

        let start = tokio::time::Instant::now();
        let task = tokio::spawn(run_playback_task(
            source,
            vec![seg.clone()],
            seg.start_ms,
            seg.end_ms,
            media_socket,
            None,
            12345,
            "dev",
            remote,
            true,
            ctl_rx,
        ));

        // First frame immediate, then set 4x speed.
        let _first = try_receive_rtp(&receiver, Duration::from_secs(2))
            .await
            .expect("first frame");
        ctl_tx
            .send(PlaybackControl::Play {
                start_ms: None,
                speed: Some(4.0),
            })
            .await
            .expect("send speed");

        // Next two frames: 100ms spacing at 4x = 25ms gaps (±15ms).
        let p1 = try_receive_rtp(&receiver, Duration::from_secs(2))
            .await
            .expect("frame 1");
        let t1 = start.elapsed().as_millis() as i64;
        let p2 = try_receive_rtp(&receiver, Duration::from_secs(2))
            .await
            .expect("frame 2");
        let t2 = start.elapsed().as_millis() as i64;
        let gap = t2 - t1;
        assert!((gap - 25).abs() <= 15, "4x speed gap ~25ms, got {gap}ms");
        assert!(has_nalu(&p1, &[0x61, 0x89]), "frame 1 content");
        assert!(has_nalu(&p2, &[0x61, 0x8A]), "frame 2 content");

        drop(ctl_tx);
        task.await.expect("task join").expect("task ok");
    }
}
