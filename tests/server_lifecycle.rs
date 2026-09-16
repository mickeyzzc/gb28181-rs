//! Server lifecycle integration tests: construction must be side-effect
//! free, and shutdown must actually stop the server (v0.6.0 regressions —
//! the constructors used to panic 100% of the time, and spawned tasks had
//! no shutdown path).

use std::sync::Arc;
use std::time::Duration;

use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::{Gb28181Config, Gb28181Server};

fn test_config(local_sip_port: u16, transport: gb28181_rs::config::Transport) -> Gb28181Config {
    Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: 5060,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001320000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "12345678".to_string(),
        local_sip_port,
        register_interval_secs: 3600,
        heartbeat_interval_secs: 3600,
        heartbeat_timeout_count: 3,
        transport,
        ..Gb28181Config::default()
    }
}

/// Regression: `Gb28181Server::new` / `with_recording_index` must never
/// panic (they used to call a panicking placeholder socket stub).
#[test]
fn constructors_do_not_panic_or_perform_io() {
    let server = Gb28181Server::new(
        test_config(5060, gb28181_rs::config::Transport::Udp),
        Arc::new(MockFrameHub::new()),
    );
    drop(server);
    let server = Gb28181Server::with_recording_index(
        test_config(5060, gb28181_rs::config::Transport::Tcp),
        Arc::new(MockFrameHub::new()),
        None,
    );
    drop(server);
}

/// Regression: shutdown must stop a UDP server's run loop. The handle's
/// task must finish promptly after `shutdown()` instead of running forever.
#[tokio::test]
async fn udp_server_shutdown_stops_run_loop() -> anyhow::Result<()> {
    // The platform address points at loopback with no listener —
    // registration fails after retries; the recv loop keeps running (this
    // is the pre-existing listen-only behavior) until shutdown.
    let mut handle = Gb28181Server::start(
        // Bind the SIP socket on an ephemeral port to avoid clashing with
        // anything real on 5060.
        Gb28181Config {
            local_sip_port: 0,
            register_interval_secs: 3600,
            ..test_config(0, gb28181_rs::config::Transport::Udp)
        },
        Arc::new(MockFrameHub::new()),
        None,
    )
    .await?;

    tokio::time::timeout(Duration::from_secs(3), handle.shutdown())
        .await
        .expect("shutdown must complete within 3s (run loop exited)")?;
    Ok(())
}

/// Regression: shutdown must stop a TCP server's accept loop.
#[tokio::test]
async fn tcp_server_shutdown_stops_accept_loop() -> anyhow::Result<()> {
    // Reserve an ephemeral port, release it, and let the server bind it.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = probe.local_addr()?.port();
    drop(probe);

    let mut handle = Gb28181Server::start(
        test_config(port, gb28181_rs::config::Transport::Tcp),
        Arc::new(MockFrameHub::new()),
        None,
    )
    .await?;

    // Give the accept loop a moment to bind, then verify a client can no
    // longer connect after shutdown (listener closed).
    tokio::time::sleep(Duration::from_millis(100)).await;
    tokio::time::timeout(Duration::from_secs(3), handle.shutdown())
        .await
        .expect("shutdown must complete within 3s (accept loop exited)")?;

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "listener must be closed after shutdown"
    );
    Ok(())
}

/// Graceful deregistration end-to-end (issue #62): register against a
/// fake platform, then `shutdown_with_deregister` — the platform must see
/// the REGISTER `Expires: 0` legs (401 challenge dance, same Call-ID)
/// before the server task exits.
#[tokio::test]
async fn shutdown_with_deregister_sends_expires_zero() -> anyhow::Result<()> {
    use tokio::net::UdpSocket;

    let platform = UdpSocket::bind("127.0.0.1:0").await?;
    let platform_port = platform.local_addr()?.port();
    let (registered_tx, registered_rx) = tokio::sync::oneshot::channel::<()>();

    // Fake platform: answers the registration (401 → 200) and the
    // de-registration (401 → 200) by CSeq; keepalive MESSAGEs are
    // ignored. Collects every REGISTER wire form for assertions. The
    // oneshot fires on the server's FIRST datagram after the authed
    // REGISTER was answered (keepalive or de-registration leg) — both
    // prove the registration completed; signalling on the 200's send
    // alone races the server's register-phase shutdown arm (a Deregister
    // arriving mid-registration is a documented fast-stop no-op, not a
    // de-registration).
    let platform_task = tokio::spawn(async move {
        let mut registered_tx = Some(registered_tx);
        let mut registers: Vec<String> = Vec::new();
        let mut buf = vec![0u8; 4096];
        let mut signal_on_next = false;
        loop {
            let (n, peer) = match platform.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(_) => break,
            };
            let msg = String::from_utf8_lossy(&buf[..n]).to_string();
            if signal_on_next {
                signal_on_next = false;
                if let Some(tx) = registered_tx.take() {
                    let _ = tx.send(());
                }
            }
            if !msg.contains("REGISTER sip:") {
                continue; // keepalive MESSAGE or media noise
            }
            let cseq: u32 = msg
                .lines()
                .find_map(|l| l.strip_prefix("CSeq: "))
                .and_then(|v| v.split(' ').next().map(str::parse))
                .and_then(Result::ok)
                .unwrap_or(0);
            registers.push(msg);
            // Replies from the platform's own socket; the server learns
            // nothing from Contact here (it answers to the source addr).
            let reply = match cseq {
                1 | 3 => "SIP/2.0 401 Unauthorized\r\nCSeq: {cseq} REGISTER\r\nWWW-Authenticate: Digest realm=\"3402000000\", nonce=\"n{cseq}\", algorithm=MD5\r\nContent-Length: 0\r\n\r\n".replace("{cseq}", &cseq.to_string()),
                2 | 4 => format!("SIP/2.0 200 OK\r\nCSeq: {cseq} REGISTER\r\nContent-Length: 0\r\n\r\n"),
                _ => continue,
            };
            let _ = platform.send_to(reply.as_bytes(), peer).await;
            if registers.len() == 2 {
                signal_on_next = true;
            }
            if registers.len() == 4 {
                break;
            }
        }
        registers
    });

    let mut handle = Gb28181Server::start(
        Gb28181Config {
            local_sip_port: 0,
            platform_sip_port: platform_port,
            ..test_config(0, gb28181_rs::config::Transport::Udp)
        },
        Arc::new(MockFrameHub::new()),
        None,
    )
    .await?;

    // The de-register request must only go out once the registration
    // exists (requesting it earlier is a documented no-op fast-stop).
    tokio::time::timeout(Duration::from_secs(10), registered_rx)
        .await
        .expect("registration must complete against the fake platform")?;
    tokio::time::timeout(Duration::from_secs(10), handle.shutdown_with_deregister())
        .await
        .expect("shutdown_with_deregister must complete (registration + deregistration)")?;

    let registers = platform_task
        .await
        .expect("platform task must finish after 4 REGISTERs");
    assert_eq!(registers.len(), 4, "exactly the 4 REGISTER legs");
    assert!(
        !registers[0].contains("Expires: 0"),
        "registration leg 1 carries the configured expiry: {}",
        registers[0]
    );
    assert!(
        registers[2].contains("Expires: 0"),
        "deregistration leg 1 must carry Expires: 0: {}",
        registers[2]
    );
    assert!(
        registers[3].contains("Expires: 0") && registers[3].contains("Authorization"),
        "deregistration leg 2 answers the 401 with Expires: 0: {}",
        registers[3]
    );
    // RFC 3261 §10.2.2: the deregistration rides the registration dialog.
    let call_ids: Vec<&str> = registers
        .iter()
        .map(|m| {
            m.lines()
                .find_map(|l| l.strip_prefix("Call-ID: "))
                .expect("Call-ID header")
        })
        .collect();
    assert!(call_ids.iter().all(|c| *c == call_ids[0]));
    Ok(())
}

/// `shutdown_with_deregister` before a successful registration is a
/// no-op for the wire (nothing to remove) and must not delay shutdown.
#[tokio::test]
async fn shutdown_with_deregister_without_registration_is_noop() -> anyhow::Result<()> {
    // Platform port with no listener: registration attempts fail.
    let probe = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let port = probe.local_addr()?.port();
    drop(probe);

    let mut handle = Gb28181Server::start(
        Gb28181Config {
            local_sip_port: 0,
            platform_sip_port: port,
            ..test_config(0, gb28181_rs::config::Transport::Udp)
        },
        Arc::new(MockFrameHub::new()),
        None,
    )
    .await?;

    tokio::time::timeout(Duration::from_secs(3), handle.shutdown_with_deregister())
        .await
        .expect("shutdown must complete without waiting for registration retries")?;
    Ok(())
}

/// Strict mode (issue #32): spec-example values that would otherwise only
/// warn must refuse to start, naming every offending field — before any
/// socket is bound.
#[tokio::test]
async fn strict_mode_refuses_example_defaults() {
    // test_config deliberately carries the spec-example password/device_id.
    let mut cfg = test_config(0, gb28181_rs::config::Transport::Udp);
    cfg.strict_example_defaults = true;

    let err = Gb28181Server::new(cfg, Arc::new(MockFrameHub::new()))
        .spawn()
        .await
        .expect_err("strict mode must refuse spec-example values");
    let msg = format!("{err:#}");
    for field in ["password", "device_id"] {
        assert!(msg.contains(field), "error must name {field}: {msg}");
    }
    // test_config overrides platform_sip_address to 127.0.0.1, so only
    // password + device_id are expected in the finding list.
    assert!(!msg.contains("platform_sip_address"), "unexpected: {msg}");
}

/// Strict mode with real values starts normally (and shuts down cleanly).
#[tokio::test]
async fn strict_mode_accepts_real_values() -> anyhow::Result<()> {
    let cfg = Gb28181Config {
        strict_example_defaults: true,
        password: "real-password".to_string(),
        device_id: "34020000001320000042".to_string(),
        channel_id: "34020000001320000042".to_string(),
        ..test_config(0, gb28181_rs::config::Transport::Udp)
    };
    let mut handle = Gb28181Server::new(cfg, Arc::new(MockFrameHub::new()))
        .spawn()
        .await
        .expect("strict mode must accept real values");

    tokio::time::timeout(Duration::from_secs(3), handle.shutdown())
        .await
        .expect("shutdown must complete within 3s")?;
    Ok(())
}
