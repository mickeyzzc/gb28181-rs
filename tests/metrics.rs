//! MetricsHooks observability seam (#30): a registration against a fake
//! platform must drive the hooks — hosts bridge them to Prometheus or any
//! backend without the library taking a dependency.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gb28181_rs::config::Gb28181Config;
use gb28181_rs::metrics::MetricsHooks;
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::sip::SipMessage;
use gb28181_rs::Gb28181Server;

#[derive(Default)]
struct CountingHooks {
    register_attempt: AtomicU32,
    register_ok: AtomicU32,
    register_fail: AtomicU32,
    keepalive_fail: AtomicU32,
    ps_bytes_out: AtomicU32,
    rtp_packets_out: AtomicU32,
}

impl MetricsHooks for CountingHooks {
    fn register_attempt(&self) {
        self.register_attempt.fetch_add(1, Ordering::Relaxed);
    }
    fn register_ok(&self) {
        self.register_ok.fetch_add(1, Ordering::Relaxed);
    }
    fn register_fail(&self) {
        self.register_fail.fetch_add(1, Ordering::Relaxed);
    }
    fn keepalive_fail(&self) {
        self.keepalive_fail.fetch_add(1, Ordering::Relaxed);
    }
    fn ps_bytes_out(&self, bytes: u64) {
        self.ps_bytes_out.fetch_add(bytes as u32, Ordering::Relaxed);
    }
    fn rtp_packets_out(&self, packets: u64) {
        self.rtp_packets_out
            .fetch_add(packets as u32, Ordering::Relaxed);
    }
}

fn config(platform_port: u16) -> Gb28181Config {
    Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".into(),
        platform_sip_port: platform_port,
        device_id: "34020000001320000001".into(),
        channel_id: "34020000001310000001".into(),
        sip_domain: "34020000002000000001".into(),
        password: String::new(),
        local_sip_port: 0,
        register_interval_secs: 3600,
        heartbeat_interval_secs: 3600,
        ..Default::default()
    }
}

async fn recv_sip(platform: &tokio::net::UdpSocket) -> (SipMessage, std::net::SocketAddr) {
    let mut buf = vec![0u8; 65535];
    let (n, peer) = tokio::time::timeout(Duration::from_secs(10), platform.recv_from(&mut buf))
        .await
        .expect("datagram within 10s")
        .expect("recv");
    (
        SipMessage::parse(&String::from_utf8_lossy(&buf[..n])).expect("parse"),
        peer,
    )
}

fn reply(status: &str, req: &SipMessage, extra: &[(&str, String)]) -> String {
    let mut out = format!(
        "SIP/2.0 {status}\r\nVia: {}\r\nFrom: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: {}\r\n",
        req.get_header("Via").unwrap_or(""),
        req.get_header("From").unwrap_or(""),
        req.get_header("To").unwrap_or(""),
        req.get_header("Call-ID").unwrap_or(""),
        req.get_header("CSeq").unwrap_or(""),
    );
    for (k, v) in extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("Content-Length: 0\r\n\r\n");
    out
}

#[tokio::test]
async fn registration_drives_metrics_hooks() -> anyhow::Result<()> {
    let platform = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let platform_port = platform.local_addr()?.port();
    let hooks = Arc::new(CountingHooks::default());

    let mut server = Gb28181Server::new(config(platform_port), Arc::new(MockFrameHub::new()))
        .with_metrics(hooks.clone())
        .spawn()
        .await?;

    // The device library always runs the two-step REGISTER: challenge the
    // first request, accept the digest-authed retry.
    let (req1, peer) = recv_sip(&platform).await;
    assert_eq!(req1.method, Some(gb28181_rs::sip::SipMethod::Register));
    platform
        .send_to(
            reply(
                "401 Unauthorized",
                &req1,
                &[(
                    "WWW-Authenticate",
                    "Digest realm=\"34020000002000000001\", nonce=\"metrics-nonce\", algorithm=MD5"
                        .to_string(),
                )],
            )
            .as_bytes(),
            peer,
        )
        .await?;
    let (req2, _) = recv_sip(&platform).await;
    platform
        .send_to(reply("200 OK", &req2, &[]).as_bytes(), peer)
        .await?;

    // The hooks fire once registration completes.
    for _ in 0..100 {
        if hooks.register_ok.load(Ordering::Relaxed) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        hooks.register_attempt.load(Ordering::Relaxed) >= 1,
        "register_attempt must fire"
    );
    assert_eq!(
        hooks.register_ok.load(Ordering::Relaxed),
        1,
        "register_ok must fire exactly once"
    );
    assert_eq!(hooks.register_fail.load(Ordering::Relaxed), 0);

    server.shutdown().await?;
    Ok(())
}
