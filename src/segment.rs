//! Reference recording-segment format: parse a segment file back into
//! access units paired with presentation timestamps.
//!
//! The segment is a bare Annex-B H.264 bytestream. NALUs are grouped into
//! access units by splitting on AUD (access unit delimiter) boundaries when
//! present, otherwise on IDR boundaries. PTS offsets come from the per-frame
//! `<segment>.ts.jsonl` sidecar; if the sidecar is missing, a nominal 25 fps
//! cadence is assumed.
//!
//! This is the on-disk format the crate's playback path ([`crate::playback`])
//! reads; hosts that record with it get RecordInfo/playback support without
//! a format shim.

use crate::frame::Nalu;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Nominal frame rate used when the PTS sidecar is missing.
const NOMINAL_FPS: u64 = 25;

/// One decoded access unit from a recording segment.
#[derive(Debug, Clone)]
pub struct RecordedAu {
    /// NALU payloads (without start codes) belonging to this access unit.
    pub nalus: Vec<Vec<u8>>,
    /// Presentation offset from the start of the segment.
    pub pts_offset: Duration,
    /// True if this access unit is a key frame (contains an IDR slice).
    pub is_key_frame: bool,
}

/// H.264 Annex-B splitter (start-code scan only — no slice parsing).
struct Parser;

impl Parser {
    /// Returns indices of all start code positions in `data`.
    ///
    /// Matches both 4-byte (0x00000001) and 3-byte (0x000001) start codes.
    /// For 4-byte codes the position points to the first zero byte (the
    /// extra `0x00` prefix); for 3-byte codes it points to the first `0x00`.
    fn find_start_codes(data: &[u8]) -> Vec<usize> {
        if data.len() < 3 {
            return Vec::new();
        }

        let mut positions = Vec::new();
        let mut i = 0;

        while i < data.len() - 2 {
            // Look for 0x000001 pattern (core of both 3-byte and 4-byte codes).
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                // Check if preceded by 0x00 → 4-byte start code at i - 1.
                if i > 0 && data[i - 1] == 0 {
                    positions.push(i - 1);
                } else {
                    positions.push(i);
                }
                i += 3;
                continue;
            }
            i += 1;
        }

        positions
    }

    /// Splits Annex-B data into individual NAL units.
    fn parse(data: &[u8]) -> Vec<Nalu> {
        if data.is_empty() {
            return Vec::new();
        }

        let positions = Self::find_start_codes(data);
        if positions.is_empty() {
            return Vec::new();
        }

        let mut nalus = Vec::with_capacity(positions.len());

        for i in 0..positions.len() {
            let pos = positions[i];

            // Determine NALU data start: skip the start code bytes.
            let nalu_start = if pos + 4 <= data.len()
                && data[pos] == 0
                && data[pos + 1] == 0
                && data[pos + 2] == 0
                && data[pos + 3] == 1
            {
                pos + 4
            } else {
                pos + 3
            };

            if nalu_start >= data.len() {
                break;
            }

            // End of NALU: next start code or end of data.
            let nalu_end = if i + 1 < positions.len() {
                positions[i + 1]
            } else {
                data.len()
            };

            let nalu_data = &data[nalu_start..nalu_end];
            if nalu_data.is_empty() {
                continue;
            }

            let nalu_type = nalu_data[0] & 0x1F;

            nalus.push(Nalu {
                nalu_type,
                data: nalu_data.to_vec(),
                is_idr: nalu_type == 5,
                is_sps: nalu_type == 7,
                is_pps: nalu_type == 8,
                is_aud: nalu_type == 9,
            });
        }

        nalus
    }
}

/// Path to the per-frame sidecar for a segment file.
#[must_use]
pub fn sidecar_path(segment_path: &Path) -> PathBuf {
    let mut os = segment_path.as_os_str().to_owned();
    os.push(".ts.jsonl");
    PathBuf::from(os)
}

/// Read a segment file into access units.
///
/// # Errors
///
/// Returns an I/O error if the segment file cannot be read.
pub fn read_segment(path: &Path) -> std::io::Result<Vec<RecordedAu>> {
    let data = fs::read(path)?;
    let nalus = Parser::parse(&data);
    let groups = group_aus(&nalus);
    let pts = load_pts(path, groups.len());
    Ok(groups
        .into_iter()
        .enumerate()
        .map(|(i, (nalus, is_key))| RecordedAu {
            nalus,
            pts_offset: pts[i],
            is_key_frame: is_key,
        })
        .collect())
}

/// Group a flat NALU stream into access units.
///
/// Each VCL slice NALU (type 1-5) starts a new access unit; non-slice NALUs
/// (SPS/PPS/AUD) that precede a slice are attached to that slice's access
/// unit. This mirrors the live path, where each encoder frame is one access
/// unit. Returns `(nalus, is_key_frame)` per group.
#[must_use]
pub fn group_aus(nalus: &[Nalu]) -> Vec<(Vec<Vec<u8>>, bool)> {
    let mut groups: Vec<(Vec<Vec<u8>>, bool)> = Vec::new();
    let mut pending: Vec<Vec<u8>> = Vec::new();
    for nalu in nalus {
        let is_slice = (1..=5).contains(&nalu.nalu_type);
        if is_slice {
            let mut group = std::mem::take(&mut pending);
            group.push(nalu.data.clone());
            groups.push((group, nalu.is_idr));
        } else {
            pending.push(nalu.data.clone());
        }
    }
    groups
}

/// Load per-frame PTS offsets (ms from segment start) from the sidecar.
///
/// Falls back to a nominal 25 fps cadence if the sidecar is missing or has
/// fewer entries than `count`.
#[must_use]
pub fn load_pts(path: &Path, count: usize) -> Vec<Duration> {
    let sidecar = sidecar_path(path);
    let mut pts = Vec::with_capacity(count);
    if let Ok(content) = fs::read_to_string(&sidecar) {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(ms) = parse_pts_ms(trimmed) {
                pts.push(Duration::from_millis(ms));
            }
            if pts.len() >= count {
                break;
            }
        }
    }
    if pts.len() < count {
        // Fallback: nominal cadence.
        pts = (0..count)
            .map(|i| Duration::from_millis(i as u64 * 1000 / NOMINAL_FPS))
            .collect();
    }
    pts
}

/// Parse a single `{"pts_ms":N}` sidecar line.
#[must_use]
fn parse_pts_ms(line: &str) -> Option<u64> {
    let line = line.trim();
    if !line.starts_with('{') || !line.ends_with('}') {
        return None;
    }
    let inner = &line[1..line.len() - 1];
    let mut parts = inner.splitn(2, ':');
    let key = parts.next()?.trim().trim_matches('"');
    if key != "pts_ms" {
        return None;
    }
    let val = parts.next()?.trim().trim_matches('"');
    val.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mibee_rec_reader_{}_{}", std::process::id(), n));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    fn nalu(nalu_type: u8, data: Vec<u8>) -> Nalu {
        Nalu {
            nalu_type,
            data,
            is_idr: nalu_type == 5,
            is_sps: nalu_type == 7,
            is_pps: nalu_type == 8,
            is_aud: nalu_type == 9,
        }
    }

    /// Build an Annex-B bytestream from NALU payloads.
    fn annex_b(nalus: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nalus {
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(n);
        }
        out
    }

    #[test]
    fn test_group_aus_by_aud() {
        // AUD + SPS + PPS + IDR = AU1; AUD + non-IDR = AU2.
        let nalus = vec![
            nalu(9, vec![0x69, 0xf0]),
            nalu(7, vec![0x67, 0x42]),
            nalu(8, vec![0x68, 0xce]),
            nalu(5, vec![0x65, 0x88]),
            nalu(9, vec![0x69, 0xf0]),
            nalu(1, vec![0x61, 0x88]),
        ];
        let groups = group_aus(&nalus);
        assert_eq!(groups.len(), 2);
        assert!(groups[0].1, "first AU is key frame");
        assert!(!groups[1].1, "second AU is not key frame");
        // First AU has 4 NALUs (AUD+SPS+PPS+IDR).
        assert_eq!(groups[0].0.len(), 4);
        assert_eq!(groups[1].0.len(), 2);
    }

    #[test]
    fn test_group_aus_by_idr_without_aud() {
        // No AUDs: split on IDR.
        let nalus = vec![
            nalu(7, vec![0x67, 0x42]),
            nalu(8, vec![0x68, 0xce]),
            nalu(5, vec![0x65, 0x88]),
            nalu(1, vec![0x61, 0x88]),
            nalu(5, vec![0x65, 0x99]),
        ];
        let groups = group_aus(&nalus);
        assert_eq!(groups.len(), 3);
        assert!(groups[0].1, "first AU (SPS+PPS+IDR) is key");
        assert!(!groups[1].1, "second AU (non-IDR) is not key");
        assert!(groups[2].1, "third AU (IDR) is key");
        assert_eq!(groups[0].0.len(), 3);
        assert_eq!(groups[1].0.len(), 1);
        assert_eq!(groups[2].0.len(), 1);
    }

    #[test]
    fn test_read_segment_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("0000.h264");

        // Two AUs: IDR (SPS+PPS+IDR) and a non-IDR.
        let au1 = vec![vec![0x67, 0x42], vec![0x68, 0xce], vec![0x65, 0x88]];
        let au2 = vec![vec![0x61, 0x88]];
        let mut bytes = annex_b(&au1);
        bytes.extend_from_slice(&annex_b(&au2));
        fs::write(&path, &bytes).unwrap();

        // Sidecar with explicit pts.
        let sidecar = sidecar_path(&path);
        fs::write(&sidecar, "{\"pts_ms\":0}\n{\"pts_ms\":40}\n").unwrap();

        let aus = read_segment(&path).unwrap();
        assert_eq!(aus.len(), 2);
        assert!(aus[0].is_key_frame);
        assert!(!aus[1].is_key_frame);
        assert_eq!(aus[0].pts_offset, Duration::from_millis(0));
        assert_eq!(aus[1].pts_offset, Duration::from_millis(40));
        // NALU payloads round-trip without start codes.
        assert_eq!(aus[0].nalus, au1);
        assert_eq!(aus[1].nalus, au2);
    }

    #[test]
    fn test_read_segment_sidecar_missing_fallback() {
        let dir = temp_dir();
        let path = dir.join("0000.h264");
        let au1 = vec![vec![0x65, 0x88]];
        let au2 = vec![vec![0x61, 0x88]];
        let au3 = vec![vec![0x61, 0x89]];
        let mut bytes = annex_b(&au1);
        bytes.extend_from_slice(&annex_b(&au2));
        bytes.extend_from_slice(&annex_b(&au3));
        fs::write(&path, &bytes).unwrap();
        // No sidecar written.

        let aus = read_segment(&path).unwrap();
        assert_eq!(aus.len(), 3);
        // Nominal 25 fps: 0, 40, 80 ms.
        assert_eq!(aus[0].pts_offset, Duration::from_millis(0));
        assert_eq!(aus[1].pts_offset, Duration::from_millis(40));
        assert_eq!(aus[2].pts_offset, Duration::from_millis(80));
    }

    #[test]
    fn test_parse_pts_ms() {
        assert_eq!(parse_pts_ms("{\"pts_ms\":123}"), Some(123));
        assert_eq!(parse_pts_ms("{\"pts_ms\":0}"), Some(0));
        assert_eq!(parse_pts_ms("garbage"), None);
        assert_eq!(parse_pts_ms("{\"other\":1}"), None);
    }
}
