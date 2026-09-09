//! End-to-end GB35114 A-level integration: a real `Gb28181Server` driven
//! by the real `security35114::Authenticator` against a fake platform that
//! verifies the device's sign1, seals the VKEK, and checks the Note header
//! of the first keepalive.
#![cfg(feature = "gb35114")]

use std::sync::Arc;
use std::time::Duration;

use gb28181_rs::authenticator::RegisterAuthenticator;
use gb28181_rs::config::Gb28181Config;
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::security35114::{
    encrypt_vkek, load_identity, parse_auth_authorization, sign_auth_payload, verify_message,
    verify_note_header, Authenticator, Options, RandomEncoding,
};
use gb28181_rs::sip::SipMessage;
use gb28181_rs::Gb28181Server;

const DEVICE_ID: &str = "34020000001320000001";
const SERVER_ID: &str = "34020000002000000001";
const RANDOM1: &str = "PRAIIbutDbd5x/NKsbwwYw==";
const VKEK: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/src/security35114/testdata/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

async fn recv_sip(platform: &tokio::net::UdpSocket) -> (SipMessage, std::net::SocketAddr) {
    let mut buf = vec![0u8; 65535];
    let (n, peer) = tokio::time::timeout(Duration::from_secs(10), platform.recv_from(&mut buf))
        .await
        .expect("datagram within 10s")
        .expect("recv");
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    (SipMessage::parse(&text).expect("parse"), peer)
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

#[tokio::test]
async fn server_gb35114_unidirection_handshake_and_keepalive() -> anyhow::Result<()> {
    let dev = load_identity(&fixture("device_cert.pem"), &fixture("device_key.pem"))?;
    let platform_identity =
        load_identity(&fixture("platform_cert.pem"), &fixture("platform_key.pem"))?;
    let auth = Arc::new(Authenticator::new(Options::new(
        dev.clone(),
        DEVICE_ID,
        SERVER_ID,
    ))?);

    let platform = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let platform_port = platform.local_addr()?.port();

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
    .with_register_authenticator({
        let hook: Arc<dyn RegisterAuthenticator> = auth.clone();
        Some(hook)
    })
    .spawn()
    .await?;

    // --- REGISTER #1: Capability announcement ---
    let (reg1, peer) = recv_sip(&platform).await;
    assert!(
        reg1.get_header("Authorization")
            .unwrap_or("")
            .starts_with("Capability "),
        "first REGISTER carries Capability: {:?}",
        reg1.get_header("Authorization")
    );
    platform
        .send_to(
            response(
                "401 Unauthorized",
                &reg1,
                &[(
                    "WWW-Authenticate",
                    format!("Unidirection algorithm=\"A:SM2;H:SM3;S:SM4/OFB/PKCS5;SI:SM3-SM2\", random1=\"{RANDOM1}\""),
                )],
            )
            .as_bytes(),
            peer,
        )
        .await?;

    // --- REGISTER #2: platform verifies sign1 with the device certificate ---
    let (reg2, _) = recv_sip(&platform).await;
    let authz = reg2.get_header("Authorization").unwrap_or("");
    let aa = parse_auth_authorization(authz)?;
    let payload = sign_auth_payload(
        &aa.random1,
        &aa.random2,
        SERVER_ID,
        RandomEncoding::ConcatWireStrings,
    );
    verify_message(&dev.certificate, &payload, &aa.sign1).expect("platform verifies device sign1");

    // --- 200 OK with the sealed VKEK ---
    let crypt = encrypt_vkek(&dev.certificate.public_key, &VKEK)?;
    platform
        .send_to(
            response(
                "200 OK",
                &reg2,
                &[(
                    "SecurityInfo",
                    format!("Unidirection cryptkey=\"{crypt}\", algorithm=\"A:SM2;H:SM3\""),
                )],
            )
            .as_bytes(),
            peer,
        )
        .await?;

    // --- VKEK negotiated, first keepalive carries a valid Note ---
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while auth.vkek().is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(auth.vkek().is_some(), "VKEK negotiated after 200 OK");

    let mut keepalive = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while keepalive.is_none() && tokio::time::Instant::now() < deadline {
        let (msg, _) = recv_sip(&platform).await;
        if msg
            .get_header("CSeq")
            .map(|c| c.contains("MESSAGE"))
            .unwrap_or(false)
        {
            keepalive = Some(msg);
        }
    }
    let keepalive = keepalive.expect("keepalive MESSAGE within 10s");
    let date = keepalive.get_header("Date").unwrap_or("");
    let note = keepalive.get_header("Note").unwrap_or("");
    assert!(!date.is_empty(), "keepalive carries Date");
    assert!(
        note.starts_with("Digest nonce=\""),
        "keepalive carries Note: {note}"
    );
    verify_note_header(
        note,
        "MESSAGE",
        keepalive.get_header("From").unwrap_or(""),
        keepalive.get_header("To").unwrap_or(""),
        keepalive.get_header("Call-ID").unwrap_or(""),
        date,
        &VKEK,
        &keepalive.body,
        gb28181_rs::security35114::VkekEncoding::Raw,
    )
    .expect("keepalive Note verifies");

    tokio::time::timeout(Duration::from_secs(3), handle.shutdown()).await??;
    let _ = platform_identity; // available for bidirection extensions of this test
    Ok(())
}

/// End-to-end device-side downstream Note verification (issue #41):
/// after the A-level handshake, a platform request signed with the
/// negotiated VKEK is served; the same Note over a tampered body draws
/// 403 under the default reject policy.
#[tokio::test]
async fn server_gb35114_verifies_downstream_note() -> anyhow::Result<()> {
    let dev = load_identity(&fixture("device_cert.pem"), &fixture("device_key.pem"))?;
    let auth = Arc::new(Authenticator::new(Options::new(
        dev.clone(),
        DEVICE_ID,
        SERVER_ID,
    ))?);

    let platform = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let platform_port = platform.local_addr()?.port();

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
            heartbeat_interval_secs: 3600,
            heartbeat_timeout_count: 3,
            ..Gb28181Config::default()
        },
        Arc::new(MockFrameHub::new()),
        None,
    )
    .with_register_authenticator({
        let hook: Arc<dyn RegisterAuthenticator> = auth.clone();
        Some(hook)
    })
    .spawn()
    .await?;

    // --- handshake (same dance as the test above, quiet heartbeats) ---
    let (reg1, peer) = recv_sip(&platform).await;
    platform
        .send_to(
            response(
                "401 Unauthorized",
                &reg1,
                &[(
                    "WWW-Authenticate",
                    format!("Unidirection algorithm=\"A:SM2;H:SM3;S:SM4/OFB/PKCS5;SI:SM3-SM2\", random1=\"{RANDOM1}\""),
                )],
            )
            .as_bytes(),
            peer,
        )
        .await?;
    let (reg2, _) = recv_sip(&platform).await;
    let crypt = encrypt_vkek(&dev.certificate.public_key, &VKEK)?;
    platform
        .send_to(
            response(
                "200 OK",
                &reg2,
                &[(
                    "SecurityInfo",
                    format!("Unidirection cryptkey=\"{crypt}\", algorithm=\"A:SM2;H:SM3\""),
                )],
            )
            .as_bytes(),
            peer,
        )
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while auth.vkek().is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(auth.vkek().is_some(), "VKEK negotiated after 200 OK");

    // --- signed downstream request is served ---
    let from = format!("<sip:{SERVER_ID}@3402000000>;tag=platnv");
    let to = format!("<sip:{DEVICE_ID}@3402000000>");
    let signed_options = |call_id: &str, body: &str| {
        let date = gb28181_rs::security35114::format_date(std::time::SystemTime::now());
        let note = gb28181_rs::security35114::build_note_header(
            "OPTIONS",
            &from,
            &to,
            call_id,
            &date,
            &VKEK,
            body,
            gb28181_rs::security35114::VkekEncoding::Raw,
        );
        format!(
            "OPTIONS sip:{DEVICE_ID}@3402000000 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKnv{call_id}\r\n\
             From: {from}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 OPTIONS\r\n\
             Max-Forwards: 70\r\n\
             Note: {note}\r\n\
             Date: {date}\r\n\
             Content-Length: 0\r\n\
             \r\n"
        )
    };

    platform
        .send_to(signed_options("nv-good", "").as_bytes(), peer)
        .await?;
    // Skip the device's own outbound traffic (keepalive MESSAGEs) until a
    // response arrives.
    let ok_resp = loop {
        let (msg, _) = recv_sip(&platform).await;
        if msg.status_code.is_some() {
            break msg;
        }
    };
    assert_eq!(
        ok_resp.status_code.map(|c| c.code()),
        Some(200),
        "signed OPTIONS must be served: {ok_resp:?}"
    );

    // --- same Note over a tampered body draws 403 ---
    platform
        .send_to(signed_options("nv-bad", "<tampered/>").as_bytes(), peer)
        .await?;
    let forbidden = loop {
        let (msg, _) = recv_sip(&platform).await;
        if msg.status_code.is_some() {
            break msg;
        }
    };
    assert_eq!(
        forbidden.status_code.map(|c| c.code()),
        Some(403),
        "tampered OPTIONS must be rejected: {forbidden:?}"
    );

    tokio::time::timeout(Duration::from_secs(3), handle.shutdown()).await??;
    Ok(())
}
