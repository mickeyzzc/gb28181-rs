//! RTP packetization pacing (issue #34): fragmenting PS payloads into
//! MTU-sized RTP packets — the steady-state per-frame cost of the push
//! path. Run with `cargo bench --bench rtp`.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use gb28181_rs::rtp_pusher::RtpPusher;

fn bench_rtp(c: &mut Criterion) {
    // ~120 KB of PS data per keyframe (matches benches/ps.rs) fragments
    // into ~85 MTU packets.
    let ps_frame: Vec<u8> = vec![0x5a; 120 * 1024];

    c.bench_function("rtp/fragment_keyframe_ps", |b| {
        b.iter_batched(
            || RtpPusher::new("127.0.0.1:0".parse().unwrap(), 1, 96),
            |mut pusher| pusher.build_rtp_packet(&ps_frame),
            BatchSize::SmallInput,
        )
    });

    // One MTU payload — the P-frame steady state.
    let p_frame: Vec<u8> = vec![0x5a; 1200];
    c.bench_function("rtp/fragment_pframe_ps", |b| {
        b.iter_batched(
            || RtpPusher::new("127.0.0.1:0".parse().unwrap(), 1, 96),
            |mut pusher| pusher.build_rtp_packet(&p_frame),
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(benches, bench_rtp);
criterion_main!(benches);
