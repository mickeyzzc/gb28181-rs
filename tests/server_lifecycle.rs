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
