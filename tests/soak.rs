//! Soak: long-run register + keepalive + INVITE/BYE cycles against a
//! fake platform, asserting no file-descriptor growth (the leak proxy
//! for per-session media sockets and tasks). Opt-in — normal CI stays
//! fast:
//!
//! ```text
//! cargo test --test soak -- --ignored                  # default 50 cycles
//! GB28181_SOAK_CYCLES=200 cargo test --test soak -- --ignored
//! ```

use std::sync::Arc;
use std::time::Duration;

use gb28181_rs::config::Gb28181Config;
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::sip::SipMessage;
use gb28181_rs::Gb28181Server;
use tokio::net::UdpSocket;

const DEVICE_ID: &str = "34020000001320000001";

fn soak_cycles() -> usize {
    std::env::var("GB28181_SOAK_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
}

fn fd_count() -> Option<usize> {
    std::fs::read_dir("/proc/self/fd").map(|d| d.count()).ok()
}

async fn recv_sip(platform: &UdpSocket) -> (SipMessage, std::net::SocketAddr) {
    let mut buf = vec![0u8; 65535];
    let (n, peer) = tokio::time::timeout(Duration::from_secs(10), platform.recv_from(&mut buf))
        .await
        .expect("datagram within 10s")
        .expect("recv");
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    (SipMessage::parse(&text).expect("parse"), peer)
}

/// Reads until a RESPONSE arrives (skips the device's own outbound
/// keepalive MESSAGEs).
async fn recv_response(platform: &UdpSocket) -> SipMessage {
    loop {
        let (msg, _) = recv_sip(platform).await;
        if msg.status_code.is_some() {
            return msg;
        }
    }
}

fn response(status: &str, req: &SipMessage, extra: &[(&str, String)]) -> String {
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

fn invite_wire(call_id: &str, media_port: u16) -> String {
    format!(
        "INVITE sip:{DEVICE_ID}@3402000000 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKsoak{call_id}\r\n\
         From: <sip:34020000002000000001@3402000000>;tag=soak{call_id}\r\n\
         To: <sip:{DEVICE_ID}@3402000000>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Max-Forwards: 70\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: 130\r\n\
         \r\n\
         v=0\r\n\
         o=34020000002000000001 0 0 IN IP4 127.0.0.1\r\n\
         s=Play\r\n\
         c=IN IP4 127.0.0.1\r\n\
         t=0 0\r\n\
         m=video {media_port} RTP/AVP 96\r\n\
         y=777\r\n"
    )
}

fn bye_wire(call_id: &str) -> String {
    format!(
        "BYE sip:{DEVICE_ID}@3402000000 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKsoakbye{call_id}\r\n\
         From: <sip:34020000002000000001@3402000000>;tag=soak{call_id}\r\n\
         To: <sip:{DEVICE_ID}@3402000000>;tag=dev{call_id}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 2 BYE\r\n\
         Max-Forwards: 70\r\n\
         Content-Length: 0\r\n\
         \r\n"
    )
}

#[tokio::test]
#[ignore = "soak: opt-in long-run (cargo test --test soak -- --ignored)"]
async fn soak_register_keepalive_invite_cycles() {
    let platform = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let platform_port = platform.local_addr().unwrap().port();
    let media = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let media_port = media.local_addr().unwrap().port();

    let mut handle = Gb28181Server::with_recording_index(
        Gb28181Config {
            enabled: true,
            local_sip_port: 0,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: platform_port,
            device_id: DEVICE_ID.to_string(),
            channel_id: DEVICE_ID.to_string(),
            sip_domain: "3402000000".to_string(),
            password: String::new(),
            register_interval_secs: 3600,
            heartbeat_interval_secs: 1,
            heartbeat_timeout_count: 3,
            ..Gb28181Config::default()
        },
        Arc::new(MockFrameHub::new()),
        None,
    )
    .spawn()
    .await
    .expect("spawn");

    // --- REGISTER: challenge the first request, accept the retry ---
    let (reg1, peer) = recv_sip(&platform).await;
    assert_eq!(
        reg1.method.map(|m| m.to_string()).as_deref(),
        Some("REGISTER")
    );
    platform
        .send_to(
            response(
                "401 Unauthorized",
                &reg1,
                &[(
                    "WWW-Authenticate",
                    "Digest realm=\"3402000000\", nonce=\"soak\", algorithm=MD5".to_string(),
                )],
            )
            .as_bytes(),
            peer,
        )
        .await
        .unwrap();
    let (reg2, _) = recv_sip(&platform).await;
    platform
        .send_to(response("200 OK", &reg2, &[]).as_bytes(), peer)
        .await
        .unwrap();

    let before = fd_count();

    // --- INVITE/BYE cycles over the registered, keepalive-flowing session ---
    let cycles = soak_cycles();
    for i in 0..cycles {
        let call_id = format!("soak-{i}");
        platform
            .send_to(invite_wire(&call_id, media_port).as_bytes(), peer)
            .await
            .unwrap();
        let ok = recv_response(&platform).await;
        assert_eq!(
            ok.status_code.map(|c| c.code()),
            Some(200),
            "cycle {i}: INVITE must be answered 200: {ok:?}"
        );

        platform
            .send_to(bye_wire(&call_id).as_bytes(), peer)
            .await
            .unwrap();
        let bye_ok = recv_response(&platform).await;
        assert_eq!(
            bye_ok.status_code.map(|c| c.code()),
            Some(200),
            "cycle {i}: BYE must be answered 200: {bye_ok:?}"
        );
    }

    // Settle: let the media task and socket teardown finish.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let after = fd_count();
    if let (Some(before), Some(after)) = (before, after) {
        println!("soak: {cycles} cycles, fds {before} → {after}");
        assert!(
            after <= before + 8,
            "descriptors grew by {} across {cycles} INVITE/BYE cycles — a media socket or task leaks",
            after - before
        );
    }

    tokio::time::timeout(Duration::from_secs(3), handle.shutdown())
        .await
        .unwrap()
        .unwrap();
}
