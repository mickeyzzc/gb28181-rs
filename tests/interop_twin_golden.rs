//! Cross-language wire-format consistency with the Go twin library
//! (mickeyzzc/gb28181-go, PR #14 / v0.2.0).
//!
//! The MiBee ecosystem runs Rust devices (mibee-eye-raspi-rs, notebook-cam)
//! against Go platforms; the two muxers MUST emit identical bytes for
//! identical inputs or every wire-level quirk becomes a per-language bug
//! class (the 2026-08 mid-PES regression was exactly this). These goldens
//! are the verbatim output of `gb28181-go/device.MuxH264ToPS` and pin this
//! library to them; the Go library pins itself to Rust's output in the
//! reciprocal golden test, so the pair can only drift in lockstep-visible
//! ways (issue #10).
//!
//! Regeneration procedure (when a wire change is INTENTIONAL): dump the new
//! bytes from gb28181-go (see the twin-check scratch in its tmp/), update
//! both libraries' goldens in the same change, and re-run the hardware
//! interop loop against the local NVR.

use gb28181_rs::ps::mux_h264_to_ps;

fn lcg_fill(n: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(n);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..n {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        b.push(1 + ((x >> 16) % 255) as u8);
    }
    b
}

/// Small keyframe (SPS+PPS+IDR) — byte-for-byte identity with the Go twin
/// (pack header + PSM + single timestamped PES).
#[test]
fn golden_keyframe_matches_go_twin() {
    let sps: Vec<u8> = vec![0x67, 0x42, 0x80, 0x28, 0xDA, 0x01, 0xE0, 0x08];
    let pps: Vec<u8> = vec![0x68, 0xCE, 0x3C, 0x80];
    let idr: Vec<u8> = vec![0x65, 0x88, 0x84, 0x00, 0x4B, 0x00, 0x01, 0x00];
    let ps = mux_h264_to_ps(&[&sps, &pps, &idr], true, 90_000, 90_000);

    const GOLDEN: &str = "000001ba440016fc8401009c43f8000001bb000b01000000041be000000000000001e0002bc00a310005bf21110005bf21000000000167428028da01e00800000168ce3c80000001658884004b000100";
    assert_eq!(hex(&ps), GOLDEN);
}

/// Small P-frame — byte-for-byte identity with the Go twin.
#[test]
fn golden_pframe_matches_go_twin() {
    let idr: Vec<u8> = vec![0x65, 0x88, 0x84, 0x00, 0x4B, 0x00, 0x01, 0x00];
    let ps = mux_h264_to_ps(&[&idr], false, 180_000, 180_000);

    const GOLDEN: &str = "000001ba44002df90401009c43f8000001e00019c00a31000b7e4111000b7e410000000001658884004b000100";
    assert_eq!(hex(&ps), GOLDEN);
}

/// Large (200KB) access unit — the split structure must match the Go twin:
/// total length, the declared PES_packet_length of every chunk, and the
/// continuation-PES header shape (no timestamps, zero hdrlen, gap byte).
#[test]
fn golden_large_au_split_matches_go_twin() {
    let mut idr = vec![0x65u8];
    idr.extend(lcg_fill(199_999)); // 200000-byte ES → 4 PES chunks
    let ps = mux_h264_to_ps(&[&idr], false, 90_000, 90_000);

    assert_eq!(ps.len(), 200_064, "total muxed length");

    // Walk by declared lengths, collecting (declared, first 9 bytes).
    let mut chunks: Vec<(usize, [u8; 9])> = Vec::new();
    let mut pos = 0usize;
    while pos + 9 <= ps.len() {
        if ps[pos] != 0 || ps[pos + 1] != 0 || ps[pos + 2] != 1 || ps[pos + 3] != 0xE0 {
            pos += 1;
            continue;
        }
        let declared = ((ps[pos + 4] as usize) << 8) | ps[pos + 5] as usize;
        let mut head = [0u8; 9];
        head.copy_from_slice(&ps[pos..pos + 9]);
        chunks.push((declared, head));
        pos += 6 + declared;
    }

    let declared: Vec<usize> = chunks.iter().map(|(d, _)| *d).collect();
    assert_eq!(
        declared,
        vec![65_013, 65_003, 65_003, 5_007],
        "per-chunk PES_packet_length"
    );

    // First PES carries PTS+DTS: flags 0xC0, hdrlen 0x0A, PTS prefix 0x31.
    assert_eq!(
        chunks[0].1,
        [0x00, 0x00, 0x01, 0xE0, 0xFD, 0xF5, 0xC0, 0x0A, 0x31]
    );
    // Continuation PES: flags 0x00, hdrlen 0x00, gap 0x00 (zero hdrlen also
    // reads correctly at the standard byte-8 position).
    for c in &chunks[1..3] {
        assert_eq!(c.1, [0x00, 0x00, 0x01, 0xE0, 0xFD, 0xEB, 0x00, 0x00, 0x00]);
    }
    assert_eq!(
        chunks[3].1,
        [0x00, 0x00, 0x01, 0xE0, 0x13, 0x8F, 0x00, 0x00, 0x00]
    );
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
