//! Device-side snapshot command execution (issue #28 / GB/T 28181-2022
//! A.2.1.24 + A.2.5.7): a DeviceControl(SnapShot) MESSAGE is answered 200,
//! handed to the installed executor, and completes asynchronously with an
//! UploadSnapShotFinished notify echoing the SessionID. Without an
//! executor the historical control-reject behavior is preserved.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use gb28181_rs::authenticator::RegisterAuthenticator;
use gb28181_rs::config::Gb28181Config;
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::sip::SipMessage;
use gb28181_rs::snapshot::{SnapshotCommand, SnapshotExchange, SnapshotExecutor};
use gb28181_rs::Gb28181Server;
use tokio::net::UdpSocket;

const SESSION_ID: &str = "0123456789abcdef0123456789abcdef";
const UPLOAD_URL: &str =
    "http://192.168.63.30:9090/api/gb28181/snapshot/upload?session=0123456789abcdef0123456789abcdef";

/// Accepts any REGISTER silently (no Note verification).
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
}

/// Records the parsed command and returns a fixed ID list.
struct OkExec {
    cmd: Mutex<Option<SnapshotCommand>>,
    ids: Vec<String>,
}

impl SnapshotExecutor for OkExec {
    fn execute(&self, cmd: SnapshotCommand) -> SnapshotExchange {
        *self.cmd.lock().unwrap() = Some(cmd);
        let ids = self.ids.clone();
        Box::pin(async move { Ok(ids) })
    }
}

/// Always fails the exchange.
struct ErrExec;

impl SnapshotExecutor for ErrExec {
    fn execute(&self, _cmd: SnapshotCommand) -> SnapshotExchange {
        Box::pin(async move { Err(anyhow!("stub: capture failed")) })
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

/// Receives the next datagram that is not the keepalive loop's immediate
/// first tick (`tokio::time::interval` fires at once, so one Keepalive
/// MESSAGE races the REGISTER and the command responses).
async fn recv_skipping_keepalive(platform: &UdpSocket) -> (SipMessage, std::net::SocketAddr) {
    loop {
        let (msg, peer) = recv_sip(platform).await;
        if msg.method.is_some() && msg.body.contains("CmdType=\"Keepalive\"") {
            continue;
        }
        return (msg, peer);
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

fn snapshot_control_message(call_id: &str) -> String {
    let body = format!(
        "<Control><CmdType>DeviceControl</CmdType><SN>17</SN>\
         <DeviceID>34020000001320000001</DeviceID>\
         <SnapShot><SnapNum>3</SnapNum><Interval>2</Interval>\
         <UploadURL>{UPLOAD_URL}</UploadURL>\
         <SessionID>{SESSION_ID}</SessionID></SnapShot></Control>"
    );
    format!(
        "MESSAGE sip:34020000001320000001@3402000000 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:15060;branch=z9hG4bKsn{call_id}\r\n\
         From: <sip:34020000002000000001@3402000000>;tag=plat{call_id}\r\n\
         To: <sip:34020000001320000001@3402000000>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 MESSAGE\r\n\
         Max-Forwards: 70\r\n\
         Content-Type: Application/MANSCDP+xml\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

async fn start_server(
    executor: Option<Arc<dyn SnapshotExecutor>>,
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
            ..Gb28181Config::default()
        },
        Arc::new(MockFrameHub::new()),
        None,
    )
    .with_register_authenticator(Some(Arc::new(StubAuth)))
    .with_snapshot_executor(executor)
    .spawn()
    .await
    .expect("spawn");

    // Quiet the register lifecycle (full 401→challenge→200 dance — the
    // installed authenticator demands it).
    let (reg, peer) = recv_skipping_keepalive(&platform).await;
    assert_eq!(
        reg.method.map(|m| m.to_string()).as_deref(),
        Some("REGISTER")
    );
    platform
        .send_to(
            response(
                "401 Unauthorized",
                &reg,
                &[(
                    "WWW-Authenticate",
                    "Digest realm=\"3402000000\", nonce=\"snap\", algorithm=MD5".to_string(),
                )],
            )
            .as_bytes(),
            peer,
        )
        .await
        .unwrap();
    let (reg2, _) = recv_skipping_keepalive(&platform).await;
    platform
        .send_to(response("200 OK", &reg2, &[]).as_bytes(), peer)
        .await
        .unwrap();

    (platform, peer, handle)
}

#[tokio::test]
async fn snapshot_command_executes_and_notifies_with_file_ids() {
    let exec = Arc::new(OkExec {
        cmd: Mutex::new(None),
        ids: vec![
            "store/2026/09/09/a.jpg".into(),
            "store/2026/09/09/b.jpg".into(),
        ],
    });
    let (platform, peer, mut handle) =
        start_server(Some(Arc::clone(&exec) as Arc<dyn SnapshotExecutor>)).await;

    platform
        .send_to(snapshot_control_message("snap1").as_bytes(), peer)
        .await
        .unwrap();

    // 1) The MESSAGE transaction is answered 200 synchronously.
    let (ok, _) = recv_skipping_keepalive(&platform).await;
    assert_eq!(ok.status_code.map(|c| c.code()), Some(200), "got: {ok:?}");

    // 2) The completion notify arrives asynchronously on the platform
    //    socket, echoing the SessionID and the uploaded-file IDs.
    let (notify, _) = recv_skipping_keepalive(&platform).await;
    assert_eq!(
        notify.method.map(|m| m.to_string()).as_deref(),
        Some("MESSAGE"),
        "got: {notify:?}"
    );
    let body = &notify.body;
    assert!(
        body.contains("<CmdType>UploadSnapShotFinished</CmdType>"),
        "body: {body}"
    );
    assert!(
        body.contains(&format!("<SessionID>{SESSION_ID}</SessionID>")),
        "body: {body}"
    );
    assert_eq!(body.matches("<SnapShotFileID>").count(), 2, "body: {body}");
    assert!(body.contains("store/2026/09/09/a.jpg"), "body: {body}");

    // 3) The executor saw the fully parsed command.
    let cmd = exec.cmd.lock().unwrap().clone().expect("executor ran");
    assert_eq!(cmd.snap_num, 3);
    assert_eq!(cmd.interval, Some(2));
    assert_eq!(cmd.upload_url, UPLOAD_URL);
    assert_eq!(cmd.session_id, SESSION_ID);

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn failed_exchange_notifies_with_empty_list() {
    let (platform, peer, mut handle) = start_server(Some(Arc::new(ErrExec))).await;

    platform
        .send_to(snapshot_control_message("snap2").as_bytes(), peer)
        .await
        .unwrap();

    let (ok, _) = recv_skipping_keepalive(&platform).await;
    assert_eq!(ok.status_code.map(|c| c.code()), Some(200));

    let (notify, _) = recv_skipping_keepalive(&platform).await;
    assert_eq!(
        notify.method.map(|m| m.to_string()).as_deref(),
        Some("MESSAGE")
    );
    let body = &notify.body;
    assert!(
        body.contains("<CmdType>UploadSnapShotFinished</CmdType>"),
        "body: {body}"
    );
    assert!(
        body.contains(&format!("<SessionID>{SESSION_ID}</SessionID>")),
        "body: {body}"
    );
    assert_eq!(body.matches("<SnapShotFileID>").count(), 0, "body: {body}");

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn without_executor_the_control_is_rejected_as_before() {
    let (platform, peer, mut handle) = start_server(None).await;

    platform
        .send_to(snapshot_control_message("snap3").as_bytes(), peer)
        .await
        .unwrap();

    let (ok, _) = recv_skipping_keepalive(&platform).await;
    assert_eq!(ok.status_code.map(|c| c.code()), Some(200));

    // Historical behavior: a control-reject Response MESSAGE follows.
    let (reject, _) = recv_skipping_keepalive(&platform).await;
    assert_eq!(
        reject.method.map(|m| m.to_string()).as_deref(),
        Some("MESSAGE")
    );
    let body = &reject.body;
    assert!(body.contains("CmdType=\"DeviceControl\""), "body: {body}");
    assert!(body.contains("<Result>ERROR</Result>"), "body: {body}");

    let _ = handle.shutdown().await;
}
