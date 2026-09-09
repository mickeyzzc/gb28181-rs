//! PS encapsulation throughput (issue #34): muxing a realistic keyframe
//! NAL set to Program Stream and parsing it back. Run with
//! `cargo bench --bench ps`.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use gb28181_rs::ps::{mux_h264_to_ps, parse_ps_to_h264};

/// A keyframe-shaped NAL set: SPS, PPS, then a ~120 KB IDR split into
/// slices — the per-frame cost the camera hot path pays.
fn keyframe_nalus() -> Vec<Vec<u8>> {
    let mut nalus: Vec<Vec<u8>> = vec![vec![
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0x01, 0x6c, 0x80, 0x00, 0x00,
        0x03, 0x00, 0x80, 0x00, 0x00, 0x1e, 0x07, 0x8c, 0x18, 0xcb,
    ]];
    nalus.push(vec![0x68, 0xeb, 0xec, 0xb2, 0x2c]);
    let mut idr = vec![0x65];
    idr.extend_from_slice(&vec![0x5a; 120 * 1024]);
    nalus.push(idr);
    nalus
}

fn bench_ps(c: &mut Criterion) {
    let nalus = keyframe_nalus();
    let refs: Vec<&[u8]> = nalus.iter().map(|n| n.as_slice()).collect();

    c.bench_function("ps/mux_h264_keyframe", |b| {
        b.iter(|| mux_h264_to_ps(&refs, true, 90_000, 90_000))
    });

    let ps = mux_h264_to_ps(&refs, true, 90_000, 90_000);
    c.bench_function("ps/parse_keyframe_to_h264", |b| {
        b.iter(|| parse_ps_to_h264(&ps).expect("parse"))
    });

    // Round-trip pairs: mux then demux, the camera→NVR data path shape.
    c.bench_function("ps/roundtrip_keyframe", |b| {
        b.iter_batched(
            || nalus.clone(),
            |nalus| {
                let refs: Vec<&[u8]> = nalus.iter().map(|n| n.as_slice()).collect();
                let ps = mux_h264_to_ps(&refs, true, 90_000, 90_000);
                parse_ps_to_h264(&ps).expect("parse")
            },
            BatchSize::LargeInput,
        )
    });
}

criterion_group!(benches, bench_ps);
criterion_main!(benches);
