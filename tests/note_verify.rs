//! Device-side incoming Note verification (issue #41): when the
//! installed RegisterAuthenticator overrides `verify_incoming_note`,
//! platform→device requests are checked before dispatch. Default policy
//! rejects a bad Note with 403; Warn serves with a log line; a good or
//! absent Note always passes (mixed-mode Digest platforms).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use gb28181_rs::authenticator::{IncomingNotePolicy, RegisterAuthenticator};
use gb28181_rs::config::Gb28181Config;
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::sip::SipMessage;
use gb28181_rs::Gb28181Server;
use tokio::net::UdpSocket;

/// Accepts the REGISTER flow silently and verifies incoming Notes with
/// one rule: "good" passes, anything else fails.
struct StubAuth;

impl RegisterAuthenticator for StubAuth {
    fn initial_authorization(&self) -> String {
        String::new()
    }

    fn authorize_with_challenge(&self, _www_authenticate: &str) -> Result<String> {
        Ok(String::new())
    }

    fn verify_ok(&self, _security_info: &str) -> Result<()> {
        Ok(())
    }

    fn verify_incoming_note(
        &self,
        _method: &str,
        _from: &str,
        _to: &str,
        _call_id: &str,
        _date: &str,
        note: &str,
        _body: &str,
    ) -> Result<()> {
        if note.is_empty() {
            return Ok(()); // mixed-mode tolerance, mirrors the platform side
        }
        if note == "good" {
            Ok(())
        } else {
            Err(anyhow!("stub: bad note"))
        }
    }
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

fn options_request(call_id: &str, note: &str, date: &str) -> String {
    let mut out = format!(
        "OPTIONS sip:34020000001320000001@3402000000 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKnv{call_id}\r\n\
         From: <sip:34020000002000000001@3402000000>;tag=plat{call_id}\r\n\
         To: <sip:34020000001320000001@3402000000>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 OPTIONS\r\n\
         Max-Forwards: 70\r\n"
    );
    if !note.is_empty() {
        out.push_str(&format!("Note: {note}\r\n"));
    }
    if !date.is_empty() {
        out.push_str(&format!("Date: {date}\r\n"));
    }
    out.push_str("Content-Length: 0\r\n\r\n");
    out
}

async fn start_server(
    policy: IncomingNotePolicy,
) -> (UdpSocket, std::net::SocketAddr, gb28181_rs::ServerHandle) {
    let platform = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let platform_port = platform.local_addr().unwrap().port();

    let handle = Gb28181Server::with_recording_index(
        Gb28181Config {
            enabled: true,
            local_sip_port: 0,
            platform_sip_address: "127.0.0.1".to_string(),
            platform_sip_port: platform_port,
            device_id: "34020000001320000001".to_string(),
            channel_id: "34020000001320000001".to_string(),
            sip_domain: "3402000000".to_string(),
            password: String::new(),
            register_interval_secs: 3600,
            heartbeat_interval_secs: 3600,
            heartbeat_timeout_count: 3,
            incoming_note_policy: policy,
            ..Gb28181Config::default()
        },
        Arc::new(MockFrameHub::new()),
        None,
    )
    .with_register_authenticator({
        let hook: Arc<dyn RegisterAuthenticator> = Arc::new(StubAuth);
        Some(hook)
    })
    .spawn()
    .await
    .expect("spawn");

    // Quiet the register lifecycle: with an authenticator installed the
    // flow demands the full dance — 401 challenge, then 200 OK (the stub
    // accepts any challenge).
    let (reg, peer) = recv_sip(&platform).await;
    assert_eq!(
        reg.method.map(|m| m.to_string()).as_deref(),
        Some("REGISTER"),
        "first message must be REGISTER"
    );
    platform
        .send_to(
            response(
                "401 Unauthorized",
                &reg,
                &[(
                    "WWW-Authenticate",
                    "Digest realm=\"3402000000\", nonce=\"note-verify\", algorithm=MD5".to_string(),
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

    (platform, peer, handle)
}

async fn expect_status(platform: &UdpSocket, want: u16) {
    let (msg, _) = recv_sip(platform).await;
    let got = msg.status_code.map(|c| c.code()).unwrap_or(0);
    assert_eq!(got, want, "status = {got}, want {want}: {msg:?}");
}

#[tokio::test]
async fn incoming_note_reject_policy_answers_403_on_bad_note() {
    let (platform, peer, mut handle) = start_server(IncomingNotePolicy::Reject).await;

    platform
        .send_to(
            options_request("nr1", "garbage", "2026-09-09T00:00:00.000").as_bytes(),
            peer,
        )
        .await
        .unwrap();
    expect_status(&platform, 403).await;
    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn incoming_note_reject_policy_accepts_good_note() {
    let (platform, peer, mut handle) = start_server(IncomingNotePolicy::Reject).await;

    platform
        .send_to(
            options_request("nr2", "good", "2026-09-09T00:00:00.000").as_bytes(),
            peer,
        )
        .await
        .unwrap();
    expect_status(&platform, 200).await;
    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn incoming_note_absent_note_passes() {
    let (platform, peer, mut handle) = start_server(IncomingNotePolicy::Reject).await;

    platform
        .send_to(options_request("nr3", "", "").as_bytes(), peer)
        .await
        .unwrap();
    expect_status(&platform, 200).await;
    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn incoming_note_warn_policy_logs_but_serves() {
    let (platform, peer, mut handle) = start_server(IncomingNotePolicy::Warn).await;

    platform
        .send_to(
            options_request("nw1", "garbage", "2026-09-09T00:00:00.000").as_bytes(),
            peer,
        )
        .await
        .unwrap();
    expect_status(&platform, 200).await;
    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn incoming_note_off_policy_skips_verification() {
    let (platform, peer, mut handle) = start_server(IncomingNotePolicy::Off).await;

    platform
        .send_to(
            options_request("no1", "garbage", "2026-09-09T00:00:00.000").as_bytes(),
            peer,
        )
        .await
        .unwrap();
    expect_status(&platform, 200).await;
    let _ = handle.shutdown().await;
}
