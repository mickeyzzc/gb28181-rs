//! Outbound stream transport (`tcp-out`) integration: the device dials
//! the platform, runs the REGISTER digest dance over the connection,
//! keepalives, answers a query on the same stream, and re-dials after
//! the platform drops the connection.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use gb28181_rs::config::Transport;
use gb28181_rs::mock::MockFrameHub;
use gb28181_rs::{Gb28181Config, Gb28181Server};

fn stream_config(port: u16) -> Gb28181Config {
    Gb28181Config {
        enabled: true,
        platform_sip_address: "127.0.0.1".to_string(),
        platform_sip_port: port,
        device_id: "34020000001320000001".to_string(),
        channel_id: "34020000001320000001".to_string(),
        sip_domain: "3402000000".to_string(),
        password: "12345678".to_string(),
        local_sip_port: 5060,
        register_interval_secs: 3600,
        heartbeat_interval_secs: 1,
        heartbeat_timeout_count: 3,
        transport: Transport::TcpOut,
        ..Gb28181Config::default()
    }
}

/// Read one Content-Length framed SIP message from the stream.
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(reader: &mut BufReader<R>) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await.expect("read head");
        assert_eq!(n, 1, "platform connection ended mid-header");
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head_str = String::from_utf8_lossy(&head).to_string();
    let len: usize = head_str
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body).await.expect("read body");
    }
    format!("{head_str}{}", String::from_utf8_lossy(&body))
}

fn header_value<'a>(msg: &'a str, name: &str) -> &'a str {
    let needle = format!("{}:", name.to_ascii_lowercase());
    msg.lines()
        .find(|l| l.to_ascii_lowercase().starts_with(&needle))
        .map(|l| l.split_once(':').unwrap().1.trim())
        .unwrap_or("")
}

/// One fake-platform session: the 401→digest REGISTER dance, a keepalive
/// answer, a Catalog query the device answers on the stream, and (on the
/// first session) the drop that must trigger a re-dial.
async fn platform_session<R, W>(
    read_half: R,
    write_half: W,
    _first: bool,
    keep_open: bool,
) -> (Option<BufReader<R>>, Option<W>)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut write_half = write_half;
    let mut reader = BufReader::new(read_half);

    // Leg 1: unauthenticated REGISTER → 401 with a Digest challenge.
    let reg1 = read_frame(&mut reader).await;
    assert!(reg1.contains("REGISTER"), "leg 1: {reg1}");
    let call_id = header_value(&reg1, "Call-ID").to_string();
    let cseq1 = header_value(&reg1, "CSeq").to_string();
    let via1 = header_value(&reg1, "Via").to_string();
    let from = header_value(&reg1, "From").to_string();
    let to = header_value(&reg1, "To").to_string();
    let resp = format!(
        "SIP/2.0 401 Unauthorized\r\nVia: {via1}\r\nFrom: {from}\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: {cseq1}\r\nWWW-Authenticate: Digest realm=\"3402000000\", nonce=\"stream-nonce\", stale=FALSE\r\nContent-Length: 0\r\n\r\n"
    );
    write_half.write_all(resp.as_bytes()).await.unwrap();

    // Leg 2: authenticated REGISTER → 200 OK with a negotiated expiry.
    let reg2 = read_frame(&mut reader).await;
    assert!(reg2.contains("Authorization: Digest"), "leg 2: {reg2}");
    assert!(
        reg2.contains("stream-nonce"),
        "digest must use our nonce: {reg2}"
    );
    let cseq2 = header_value(&reg2, "CSeq").to_string();
    let via2 = header_value(&reg2, "Via").to_string();
    let resp = format!(
        "SIP/2.0 200 OK\r\nVia: {via2}\r\nFrom: {from}\r\nTo: {to}\r\nCall-ID: {call_id}\r\nCSeq: {cseq2}\r\nExpires: 300\r\nContact: <sip:34020000002000000001@3402000000>\r\nContent-Length: 0\r\n\r\n"
    );
    write_half.write_all(resp.as_bytes()).await.unwrap();

    // Keepalive MESSAGE arrives on the stream; answer 200.
    let keepalive = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader))
        .await
        .expect("keepalive within 5s (1s interval)");
    assert!(keepalive.contains("Keepalive"), "{keepalive}");
    let ka_cseq = header_value(&keepalive, "CSeq").to_string();
    let ka_via = header_value(&keepalive, "Via").to_string();
    let ka_call = header_value(&keepalive, "Call-ID").to_string();
    let resp = format!(
        "SIP/2.0 200 OK\r\nVia: {ka_via}\r\nCall-ID: {ka_call}\r\nCSeq: {ka_cseq}\r\nContent-Length: 0\r\n\r\n"
    );
    write_half.write_all(resp.as_bytes()).await.unwrap();

    // Catalog query over the stream; the device's answer rides the same
    // connection.
    let query = format!(
        "MESSAGE sip:34020000001320000001@3402000000 SIP/2.0\r\nVia: SIP/2.0/TCP 127.0.0.1:9;branch=z9hG4bKtestcat\r\nFrom: <sip:34020000002000000001@3402000000>;tag=plat\r\nTo: <sip:34020000001320000001@3402000000>\r\nCall-ID: cat-1\r\nCSeq: 1 MESSAGE\r\nContent-Type: Application/MANSCDP+xml\r\nContent-Length: {}\r\n\r\n<Query><CmdType>Catalog</CmdType><SN>11</SN><DeviceID>34020000001320000001</DeviceID></Query>",
        "<Query><CmdType>Catalog</CmdType><SN>11</SN><DeviceID>34020000001320000001</DeviceID></Query>".len()
    );
    write_half.write_all(query.as_bytes()).await.unwrap();
    // The device answers the MESSAGE itself with 200 OK first, then the
    // catalog response follows (paced like the UDP path).
    let mut catalog = String::new();
    for _ in 0..3 {
        let frame = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader))
            .await
            .expect("answer within 5s");
        if frame.contains("<Response CmdType=\"Catalog\"") {
            catalog = frame;
            break;
        }
    }
    assert!(
        catalog.contains("<Response CmdType=\"Catalog\" SN=\"11\">"),
        "{catalog}"
    );

    if keep_open {
        // The final session stays open: the caller drives the graceful
        // shutdown (deregister over the stream) after this returns.
        return (Some(reader), Some(write_half));
    }
    drop(write_half);
    (None, None)
}

/// After `shutdown_with_deregister`, the REGISTER Expires: 0 rides the
/// stream (keepalives interleave — read until it arrives), answer 200.
async fn expect_deregister_over_stream<
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
>(
    reader: &mut BufReader<R>,
    write_half: &mut W,
) {
    let mut dereg = String::new();
    for _ in 0..8 {
        let frame = match tokio::time::timeout(Duration::from_secs(2), read_frame(reader)).await {
            Ok(f) => f,
            Err(_) => continue, // quiet moment — keep waiting
        };
        if frame.contains("REGISTER") && frame.contains("Expires: 0") {
            dereg = frame;
            break;
        }
    }
    assert!(
        !dereg.is_empty() && dereg.contains("Expires: 0") && dereg.contains("REGISTER"),
        "deregister REGISTER never arrived"
    );
    let d_cseq = header_value(&dereg, "CSeq").to_string();
    let d_via = header_value(&dereg, "Via").to_string();
    let resp =
        format!("SIP/2.0 200 OK\r\nVia: {d_via}\r\nCSeq: {d_cseq}\r\nContent-Length: 0\r\n\r\n");
    write_half.write_all(resp.as_bytes()).await.unwrap();
}

static PRINT_LOGGER: PrintLogger = PrintLogger;
struct PrintLogger;
impl log::Log for PrintLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        eprintln!("[{}] {}", r.level(), r.args());
    }
    fn flush(&self) {}
}

#[tokio::test]
async fn tcp_out_registers_keepalives_answers_and_redials() {
    let _ = log::set_logger(&PRINT_LOGGER).map(|_| log::set_max_level(log::LevelFilter::Debug));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = Gb28181Server::new(stream_config(port), Arc::new(MockFrameHub::new()));
    let mut handle = server.spawn().await.expect("spawn");

    // First session: full dance, then the platform drops the connection.
    let (conn1, _) = listener.accept().await.expect("accept 1");
    let (r1, w1) = conn1.into_split();
    platform_session(r1, w1, true, false).await;

    // The drop must produce a re-dial: a second connection running the
    // same REGISTER dance.
    let (conn2, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("re-dial within 10s")
        .expect("accept 2");
    let (r2, w2) = conn2.into_split();
    let (mut reader2, mut write2) = platform_session(r2, w2, false, true).await;
    let reader2 = reader2.as_mut().expect("final session keeps halves");
    let write2 = write2.as_mut().expect("final session keeps halves");

    // Graceful shutdown: the deregister REGISTER rides the stream.
    handle.shutdown_with_deregister().await.expect("shutdown");
    expect_deregister_over_stream(reader2, write2).await;
}

#[cfg(feature = "tls")]
mod tls_outbound {
    use super::*;
    use gb28181_rs::Gb28181Server;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::TlsAcceptor;

    /// The same outbound lifecycle over SIPS: REGISTER digest dance and
    /// keepalive ride the TLS stream, with the platform's self-signed
    /// certificate pinned via tls_ca_file.
    #[tokio::test]
    async fn tls_out_registers_and_keepalives() {
        let _ = log::set_logger(&super::PRINT_LOGGER)
            .map(|_| log::set_max_level(log::LevelFilter::Debug));

        // Platform TLS material (self-signed, IP SAN 127.0.0.1).
        let cert_pem = std::fs::read("tests/testdata/platform-cert.pem").unwrap();
        let key_pem = std::fs::read("tests/testdata/platform-key.pem").unwrap();
        let mut cert_reader = std::io::BufReader::new(&cert_pem[..]);
        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
            .collect::<Result<_, _>>()
            .unwrap();
        let mut key_reader = std::io::BufReader::new(&key_pem[..]);
        let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
            .unwrap()
            .unwrap();
        let tls_cfg = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let acceptor = TlsAcceptor::from(std::sync::Arc::new(tls_cfg));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let mut config = super::stream_config(port);
        config.transport = Transport::Tls;
        config.tls_ca_file = "tests/testdata/platform-cert.pem".to_string();

        let server = Gb28181Server::new(config, Arc::new(MockFrameHub::new()));
        let mut handle = server.spawn().await.expect("spawn");

        let (conn, _) = listener.accept().await.expect("accept");
        let tls_conn = acceptor.accept(conn).await.expect("TLS handshake");
        let (r, w) = tokio::io::split(tls_conn);
        platform_session(r, w, false, false).await;

        handle.shutdown().await.expect("shutdown");
    }
}
