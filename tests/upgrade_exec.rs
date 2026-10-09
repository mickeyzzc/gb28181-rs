//! Device-side firmware-upgrade execution (GB/T 28181-2022
//! A.2.3.1.12 + A.2.5.9; gb28181-go #108 twin): a
//! DeviceControl(DeviceUpgrade) MESSAGE is answered 200, handed to the
//! installed DeviceUpgrader, and completes asynchronously with a
//! DeviceUpgradeResult notify echoing the SessionID. Without an
//! upgrader the historical control-reject behavior is preserved.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gb28181_rs::authenticator::RegisterAuthenticator;
use gb28181_rs::config::Gb28181Config;
use gb28181_rs::manscdp::DeviceUpgradeCmd;
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::sip::SipMessage;
use gb28181_rs::upgrade::{DeviceUpgradeExchange, DeviceUpgradeOutcome, DeviceUpgrader};
use gb28181_rs::Gb28181Server;
use tokio::net::UdpSocket;

const SESSION_ID: &str = "0123456789abcdef0123456789abcdef";

/// Accepts any REGISTER silently (no Note verification).
struct StubAuth;

impl RegisterAuthenticator for StubAuth {
    fn initial_authorization(&self) -> String {
        String::new()
    }
    fn authorize_with_challenge(&self, _www_authenticate: &str) -> anyhow::Result<String> {
        Ok(String::new())
    }
    fn verify_ok(&self, _security_info: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Records the command and reports success/failure.
struct StubUpgrader {
    cmd: Mutex<Option<DeviceUpgradeCmd>>,
    ok: bool,
}

impl DeviceUpgrader for StubUpgrader {
    fn upgrade(&self, cmd: DeviceUpgradeCmd) -> DeviceUpgradeExchange {
        *self.cmd.lock().unwrap() = Some(cmd);
        let ok = self.ok;
        Box::pin(async move {
            Ok(DeviceUpgradeOutcome {
                success: ok,
                firmware: if ok { "v9.9.9" } else { "v1.0.0" }.to_string(),
                failed_reason: if ok { String::new() } else { "02".to_string() },
            })
        })
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
/// first tick.
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

fn upgrade_message(call_id: &str) -> String {
    let body = format!(
        "<Control><CmdType>DeviceControl</CmdType><SN>21</SN>\
         <DeviceID>34020000001320000001</DeviceID>\
         <DeviceUpgrade><Firmware>v1.0.0</Firmware>\
         <FileURL>http://192.168.63.30/fw.bin</FileURL>\
         <Manufacturer>MiBee</Manufacturer>\
         <SessionID>{SESSION_ID}</SessionID></DeviceUpgrade></Control>"
    );
    format!(
        "MESSAGE sip:34020000001320000001@3402000000 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:15060;branch=z9hG4bKupg{call_id}\r\n\
         From: <sip:34020000002000000001@3402000000>;tag=platupg{call_id}\r\n\
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
    upgrader: Option<Arc<dyn DeviceUpgrader>>,
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
    .with_device_upgrader(upgrader)
    .spawn()
    .await
    .expect("spawn");

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
                    "Digest realm=\"3402000000\", nonce=\"upg\", algorithm=MD5".to_string(),
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
async fn upgrade_executes_and_notifies_result() {
    let upg = Arc::new(StubUpgrader {
        cmd: Mutex::new(None),
        ok: true,
    });
    let (platform, peer, mut handle) =
        start_server(Some(Arc::clone(&upg) as Arc<dyn DeviceUpgrader>)).await;

    platform
        .send_to(upgrade_message("upg1").as_bytes(), peer)
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
        body.contains("<CmdType>DeviceUpgradeResult</CmdType>")
            && body.contains(&format!("<SessionID>{SESSION_ID}</SessionID>"))
            && body.contains("<UpgradeResult>OK</UpgradeResult>")
            && body.contains("<Firmware>v9.9.9</Firmware>")
            && !body.contains("UpgradeFailedReason"),
        "body: {body}"
    );

    let cmd = upg.cmd.lock().unwrap().clone().expect("upgrader ran");
    assert_eq!(cmd.file_url, "http://192.168.63.30/fw.bin");
    assert_eq!(cmd.session_id, SESSION_ID);

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn upgrade_failure_notifies_with_reason() {
    let (platform, peer, mut handle) = start_server(Some(Arc::new(StubUpgrader {
        cmd: Mutex::new(None),
        ok: false,
    })))
    .await;

    platform
        .send_to(upgrade_message("upg2").as_bytes(), peer)
        .await
        .unwrap();

    let (ok, _) = recv_skipping_keepalive(&platform).await;
    assert_eq!(ok.status_code.map(|c| c.code()), Some(200));
    let (notify, _) = recv_skipping_keepalive(&platform).await;
    assert!(
        notify.body.contains("<UpgradeResult>ERROR</UpgradeResult>")
            && notify
                .body
                .contains("<UpgradeFailedReason>02</UpgradeFailedReason>"),
        "body: {}",
        notify.body
    );

    let _ = handle.shutdown().await;
}

#[tokio::test]
async fn without_upgrader_the_control_is_rejected() {
    let (platform, peer, mut handle) = start_server(None).await;

    platform
        .send_to(upgrade_message("upg3").as_bytes(), peer)
        .await
        .unwrap();

    let (ok, _) = recv_skipping_keepalive(&platform).await;
    assert_eq!(ok.status_code.map(|c| c.code()), Some(200));
    let (reject, _) = recv_skipping_keepalive(&platform).await;
    assert_eq!(
        reject.method.map(|m| m.to_string()).as_deref(),
        Some("MESSAGE")
    );
    assert!(
        reject.body.contains("CmdType=\"DeviceControl\"")
            && reject.body.contains("<Result>ERROR</Result>"),
        "body: {}",
        reject.body
    );

    let _ = handle.shutdown().await;
}
