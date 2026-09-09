//! Control-signaling authentication (GB35114 §9.4): every non-REGISTER
//! request carries a Date header and a Note header holding a keyed SM3
//! digest over the message. The digest input is the plain concatenation
//! `method ‖ From ‖ To ‖ Call-ID ‖ Date ‖ VKEK ‖ body` with no separators.

use anyhow::{anyhow, bail, Result};
use base64::Engine;
use sm3::{Digest, Sm3};

use super::VkekEncoding;

/// Renders the GB35114 Date header form (ISO 8601 with millisecond
/// precision, UTC).
pub fn format_date(t: std::time::SystemTime) -> String {
    let millis = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let secs = millis.div_euclid(1000);
    let ms = millis.rem_euclid(1000);
    // Civil-from-days conversion (Howard Hinnant's algorithm) — avoids a
    // chrono dependency for one header.
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Parses a Date header rendered by [`format_date`] back to
/// `SystemTime` — the freshness anchor for incoming Note verification.
pub fn parse_note_date(date: &str) -> Result<std::time::SystemTime> {
    let parse_field = |s: Option<&str>| -> Option<i64> { s?.parse().ok() };
    // YYYY-MM-DDTHH:MM:SS.mmm (UTC, no zone suffix)
    let bytes = date.as_bytes();
    let bad = || anyhow!("malformed GB35114 Date header: {date}");
    if bytes.len() != 23
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
    {
        return Err(bad());
    }
    let year: i64 = parse_field(date.get(0..4)).ok_or_else(bad)?;
    let month: i64 = parse_field(date.get(5..7)).ok_or_else(bad)?;
    let day: i64 = parse_field(date.get(8..10)).ok_or_else(bad)?;
    let hour: i64 = parse_field(date.get(11..13)).ok_or_else(bad)?;
    let minute: i64 = parse_field(date.get(14..16)).ok_or_else(bad)?;
    let second: i64 = parse_field(date.get(17..19)).ok_or_else(bad)?;
    let millis: i64 = parse_field(date.get(20..23)).ok_or_else(bad)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(bad());
    }
    // Days-from-civil (Howard Hinnant) — the inverse of format_date.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    Ok(std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::from_millis(
            secs.saturating_mul(1000).saturating_add(millis).max(0) as u64
        ))
}

/// Computes the SM3 digest over the Note-header input.
#[allow(clippy::too_many_arguments)] // mirrors the wire-signature fields
pub fn digest_payload(
    method: &str,
    from: &str,
    to: &str,
    call_id: &str,
    date: &str,
    vkek: &[u8],
    body: &str,
    enc: VkekEncoding,
) -> Vec<u8> {
    let mut hasher = Sm3::new();
    hasher.update(method.as_bytes());
    hasher.update(from.as_bytes());
    hasher.update(to.as_bytes());
    hasher.update(call_id.as_bytes());
    hasher.update(date.as_bytes());
    match enc {
        VkekEncoding::Raw => hasher.update(vkek),
        VkekEncoding::Base64String => hasher.update(
            base64::engine::general_purpose::STANDARD
                .encode(vkek)
                .as_bytes(),
        ),
    }
    hasher.update(body.as_bytes());
    hasher.finalize().to_vec()
}

/// Builds the Note header for an outgoing request:
/// `Digest nonce="<base64 SM3>",algorithm=SM3`.
#[allow(clippy::too_many_arguments)] // mirrors the wire-signature fields
pub fn build_note_header(
    method: &str,
    from: &str,
    to: &str,
    call_id: &str,
    date: &str,
    vkek: &[u8],
    body: &str,
    enc: VkekEncoding,
) -> String {
    let nonce = base64::engine::general_purpose::STANDARD.encode(digest_payload(
        method, from, to, call_id, date, vkek, body, enc,
    ));
    format!("Digest nonce=\"{nonce}\",algorithm=SM3")
}

/// Extracts nonce and algorithm from a Note header.
pub fn parse_note_header(note: &str) -> Result<(String, String)> {
    let note = note.trim();
    let rest = note
        .strip_prefix("Digest ")
        .ok_or_else(|| anyhow!("not a Digest Note header: {note}"))?;
    // Two bare params: nonce="<b64>",algorithm=SM3 — but accept quoted
    // nonce and either order.
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("nonce=") {
            let nonce = v.trim_matches('"');
            if !nonce.is_empty() {
                let algo = rest
                    .split(',')
                    .find_map(|p| p.trim().strip_prefix("algorithm=").map(|s| s.to_string()))
                    .unwrap_or_default();
                if algo.is_empty() {
                    bail!("Note header missing algorithm: {note}");
                }
                return Ok((nonce.to_string(), algo));
            }
        }
    }
    bail!("Note header missing nonce: {note}")
}

/// Recomputes the digest and compares it to the received nonce.
#[allow(clippy::too_many_arguments)]
pub fn verify_note_header(
    note: &str,
    method: &str,
    from: &str,
    to: &str,
    call_id: &str,
    date: &str,
    vkek: &[u8],
    body: &str,
    enc: VkekEncoding,
) -> Result<()> {
    let (nonce, algorithm) = parse_note_header(note)?;
    if !algorithm.eq_ignore_ascii_case("SM3") {
        bail!("unsupported Note algorithm {algorithm:?} (want SM3)");
    }
    let want = base64::engine::general_purpose::STANDARD
        .decode(nonce.as_bytes())
        .map_err(|e| anyhow!("nonce is not base64: {e}"))?;
    let got = digest_payload(method, from, to, call_id, date, vkek, body, enc);
    if got.len() == want.len() && got.iter().zip(&want).all(|(a, b)| a == b) {
        Ok(())
    } else {
        bail!("Note digest mismatch")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security35114::VkekEncoding::{Base64String, Raw};
    use std::time::{Duration, UNIX_EPOCH};

    // Same inputs and hex goldens as the Go twin
    // (gb28181-go/security35114/integrity_test.go).
    const NOTE_FROM: &str = "<sip:34020000001320000001@3402000000>;tag=1465468922";
    const NOTE_TO: &str = "<sip:34020000002000000001@3402000000>";
    const NOTE_CALL_ID: &str = "1465470512602@192.168.1.100";
    const NOTE_BODY: &str = "<?xml version=\"1.0\"?>\r\n<Notify>\r\n<CmdType>Keepalive</CmdType>\r\n<SN>1</SN>\r\n<DeviceID>34020000001320000001</DeviceID>\r\n<Status>OK</Status>\r\n</Notify>\r\n";
    const GOLDEN_DATE: &str = "2024-01-31T14:40:49.583";
    const GOLDEN_VKEK: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn digest_payload_vkek_raw_golden() {
        assert_eq!(
            hex(&digest_payload(
                "MESSAGE",
                NOTE_FROM,
                NOTE_TO,
                NOTE_CALL_ID,
                GOLDEN_DATE,
                &GOLDEN_VKEK,
                NOTE_BODY,
                Raw
            )),
            "95b9b4c1ff6900fb5b7981049ab4a6b017b1d917108b4db59762e3a82cad4210"
        );
    }

    #[test]
    fn digest_payload_vkek_base64_golden() {
        assert_eq!(
            hex(&digest_payload(
                "MESSAGE",
                NOTE_FROM,
                NOTE_TO,
                NOTE_CALL_ID,
                GOLDEN_DATE,
                &GOLDEN_VKEK,
                NOTE_BODY,
                Base64String
            )),
            "a16ab94a92cb27ff970ba37b882744fd831cde8829a3a0367def9d3fa437bfdc"
        );
    }

    #[test]
    fn note_header_build_parse_verify() {
        let note = build_note_header(
            "MESSAGE",
            NOTE_FROM,
            NOTE_TO,
            NOTE_CALL_ID,
            GOLDEN_DATE,
            &GOLDEN_VKEK,
            NOTE_BODY,
            Raw,
        );
        assert!(note.starts_with("Digest nonce=\""));
        assert!(note.ends_with("\",algorithm=SM3"));

        let (nonce, algo) = parse_note_header(&note).unwrap();
        assert_eq!(algo, "SM3");
        assert!(!nonce.is_empty());

        verify_note_header(
            &note,
            "MESSAGE",
            NOTE_FROM,
            NOTE_TO,
            NOTE_CALL_ID,
            GOLDEN_DATE,
            &GOLDEN_VKEK,
            NOTE_BODY,
            Raw,
        )
        .unwrap();
        assert!(verify_note_header(
            &note,
            "MESSAGE",
            NOTE_FROM,
            NOTE_TO,
            NOTE_CALL_ID,
            GOLDEN_DATE,
            &GOLDEN_VKEK,
            &format!("{NOTE_BODY}x"),
            Raw
        )
        .is_err());
        let mut wrong = GOLDEN_VKEK;
        wrong[0] ^= 0xFF;
        assert!(verify_note_header(
            &note,
            "MESSAGE",
            NOTE_FROM,
            NOTE_TO,
            NOTE_CALL_ID,
            GOLDEN_DATE,
            &wrong,
            NOTE_BODY,
            Raw
        )
        .is_err());
        assert!(verify_note_header(
            &note,
            "MESSAGE",
            NOTE_FROM,
            NOTE_TO,
            NOTE_CALL_ID,
            GOLDEN_DATE,
            &GOLDEN_VKEK,
            NOTE_BODY,
            Base64String
        )
        .is_err());
    }

    #[test]
    fn parse_note_header_errors() {
        assert!(parse_note_header("Digest nonce=\"xx\"").is_err());
        assert!(parse_note_header("Basic realm=\"x\"").is_err());
    }

    #[test]
    fn format_date_golden() {
        let t = UNIX_EPOCH + Duration::from_millis(1_706_712_049_583);
        assert_eq!(format_date(t), GOLDEN_DATE);
    }

    #[test]
    fn parse_note_date_roundtrip() {
        let t = UNIX_EPOCH + Duration::from_millis(1_706_712_049_583);
        let back = parse_note_date(&format_date(t)).unwrap();
        let diff = back
            .duration_since(t)
            .or_else(|_| t.duration_since(back))
            .unwrap();
        assert!(
            diff.as_millis() == 0,
            "roundtrip must be lossless: {diff:?}"
        );
    }

    #[test]
    fn parse_note_date_rejects_malformed() {
        assert!(parse_note_date("").is_err());
        assert!(parse_note_date("not-a-date").is_err());
        assert!(parse_note_date("2024-01-31T08:00:49.58").is_err()); // short millis
        assert!(parse_note_date("2024-13-01T08:00:49.583").is_err()); // month 13
        assert!(parse_note_date("2024-01-31T25:00:49.583").is_err()); // hour 25
    }
}
