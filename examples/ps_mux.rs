//! Offline PS mux/demux demo — no network, no camera, deterministic output.
//!
//! Builds synthetic H.264 and H.265 access units, muxes them into MPEG-PS
//! with the GB/T 28181 framing, prints the byte layout, then parses the
//! stream back and verifies the NAL units round-trip intact — including the
//! oversized-frame case where the muxer must split across bounded PES
//! packets (the twin-library contract pinned by the interop goldens).
//!
//! ```sh
//! cargo run --example ps_mux
//! ```

use gb28181_rs::ps::{self, MAX_PES_CHUNK_BYTES, STREAM_TYPE_H264, STREAM_TYPE_H265};

/// Synthetic NAL: a type header byte plus filler that can never be mistaken
/// for a start code (filler bytes stay outside 0x00–0x06).
fn nalu(header: u8, size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| if i == 0 { header } else { (i % 200 + 7) as u8 })
        .collect()
}

fn pes_packet_count(ps: &[u8]) -> usize {
    ps.windows(4)
        .filter(|w| **w == [0x00, 0x00, 0x01, 0xE0])
        .count()
}

fn psm_stream_type(ps: &[u8]) -> Option<u8> {
    // This library's twin contract (pinned against gb28181-go and the NVR)
    // uses 0x000001BB for the Program Stream Map.
    let pos = ps.windows(4).position(|w| *w == [0x00, 0x00, 0x01, 0xBB])?;
    // PSM payload starts after start code (4) + stream id (0xBB) + length (2):
    // [0]=version, [1:3]=program_stream_info_length, [3:5]=
    // elementary_stream_map_length, then 4-byte entries.
    let psm = &ps[pos + 6..];
    let info_len = usize::from(psm[1]) << 8 | usize::from(psm[2]);
    // Skip the 2-byte elementary_stream_map_length before the entries.
    let entry = &psm[3 + info_len + 2..];
    Some(entry[0])
}

fn main() {
    println!("== gb28181-rs PS mux demo ==\n");

    // -- 1. H.264 keyframe access unit: SPS + PPS + IDR ------------------
    let sps = nalu(0x67, 27);
    let pps = nalu(0x68, 9);
    let idr = nalu(0x65, 40_000);
    let nalus: Vec<&[u8]> = vec![&sps, &pps, &idr];

    let ps = ps::mux_h264_to_ps(&nalus, true, 0, 0);
    println!(
        "H.264 keyframe (SPS+PPS+IDR, {} ES bytes) -> {} PS bytes",
        sps.len() + pps.len() + idr.len(),
        ps.len()
    );
    println!("  head: {}", hex::encode(&ps[..48.min(ps.len())]));
    println!(
        "  PES packets: {}, PSM announces stream_type {:#04x} (H.264 = {:#04x})",
        pes_packet_count(&ps),
        psm_stream_type(&ps).unwrap(),
        STREAM_TYPE_H264
    );

    let recovered = ps::parse_ps_to_nal_units(&ps).expect("parse back");
    let types: Vec<u8> = recovered.iter().map(|n| n[0] & 0x1F).collect();
    println!("  demuxed NAL types: {:?} (7=SPS 8=PPS 5=IDR)", types);
    assert_eq!(recovered.len(), nalus.len(), "NAL count must round-trip");
    assert_eq!(recovered[0], sps, "SPS bytes must round-trip");
    assert_eq!(recovered[1], pps, "PPS bytes must round-trip");
    assert_eq!(recovered[2], idr, "IDR bytes must round-trip");
    println!("  round-trip: OK\n");

    // -- 2. H.265 keyframe (GB/T 28181-2022 stream_type 0x24) ------------
    let vps = nalu(0x40, 32);
    let sps265 = nalu(0x42, 12);
    let idr265 = nalu(0x26, 30_000);
    let h265_nalus: Vec<&[u8]> = vec![&vps, &sps265, &idr265];
    let ps265 = ps::mux_h265_to_ps(&h265_nalus, true, 0, 0);
    println!("H.265 keyframe (VPS+SPS+IDR) -> {} PS bytes", ps265.len());
    println!(
        "  PSM announces stream_type {:#04x} (H.265 = {:#04x})",
        psm_stream_type(&ps265).unwrap(),
        STREAM_TYPE_H265
    );
    let rec265 = ps::parse_ps_to_nal_units(&ps265).expect("parse back");
    assert_eq!(rec265.len(), h265_nalus.len());
    for (got, want) in rec265.iter().zip(&h265_nalus) {
        assert_eq!(got, want);
    }
    println!("  round-trip: OK\n");

    // -- 3. Oversized frame: bounded PES splitting ------------------------
    let big = nalu(0x65, 200_000);
    let ps_big = ps::mux_h264_to_ps(&[&big], true, 0, 0);
    let chunks = pes_packet_count(&ps_big);
    println!(
        "200000-byte frame -> {} PS bytes in {} PES packets",
        ps_big.len(),
        chunks
    );
    println!(
        "  PES chunk ceiling: {} bytes (MAX_PES_CHUNK_BYTES)",
        MAX_PES_CHUNK_BYTES
    );
    assert!(chunks >= 4, "a 200 KB frame cannot fit one PES");
    let rec_big = ps::parse_ps_to_nal_units(&ps_big).expect("parse back");
    assert_eq!(rec_big.len(), 1, "split PES must reassemble to one NAL");
    assert_eq!(rec_big[0], big, "reassembly must be byte-exact");
    println!("  reassembly: OK\n");

    println!("all PS mux/demux checks passed");
}
