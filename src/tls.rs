//! SIPS (TLS) client material for the outbound stream transport
//! (`transport: "tls"`, GB/T 28181-2022 A-level signaling). The GB
//! convention is a self-signed CA whose serial is the entity ID; the
//! host provisions the files — `tls_ca_file` pins the platform, the
//! optional cert/key pair enables mutual auth, and
//! `tls_insecure_skip_verify` is the explicit lab-only opt-out.

use anyhow::{anyhow, Context, Result};

use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

use std::sync::Arc;

/// Build the platform connector from the config's TLS fields. Exactly
/// one trust mode must apply: a CA file (pinned platform) or the
/// explicit insecure opt-out.
pub(crate) fn connector(config: &crate::config::Gb28181Config) -> Result<TlsConnector> {
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow!("gb28181: tls protocol versions: {e}"))?;

    let wants_client_cert = if !config.tls_ca_file.is_empty() {
        let roots = root_store(&config.tls_ca_file)?;
        builder.with_root_certificates(roots)
    } else if config.tls_insecure_skip_verify {
        log::warn!(
            "gb28181: tls_insecure_skip_verify — accepting ANY platform certificate (lab only)"
        );
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier(provider)))
    } else {
        return Err(anyhow!(
            "gb28181: transport \"tls\" requires tls_ca_file (pin the platform) or the explicit tls_insecure_skip_verify opt-out"
        ));
    };

    let client_config = if !config.tls_cert_file.is_empty() || !config.tls_key_file.is_empty() {
        let cert = load_cert(&config.tls_cert_file)?;
        let key = load_key(&config.tls_key_file)?;
        wants_client_cert
            .with_client_auth_cert(cert, key)
            .map_err(|e| anyhow!("gb28181: tls client certificate: {e}"))?
    } else {
        wants_client_cert.with_no_client_auth()
    };
    Ok(TlsConnector::from(Arc::new(client_config)))
}

fn root_store(path: &str) -> Result<RootCertStore> {
    let pem = std::fs::read(path).with_context(|| format!("reading TLS CA file {path}"))?;
    let mut reader = std::io::BufReader::new(&pem[..]);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parsing TLS CA file {path}"))?;
    if certs.is_empty() {
        return Err(anyhow!("TLS CA file {path} contains no certificates"));
    }
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(certs);
    Ok(roots)
}

fn load_cert(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let pem = std::fs::read(path).with_context(|| format!("reading TLS cert file {path}"))?;
    let mut reader = std::io::BufReader::new(&pem[..]);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<std::result::Result<_, _>>()
        .with_context(|| format!("parsing TLS cert file {path}"))?;
    if certs.is_empty() {
        return Err(anyhow!("TLS cert file {path} contains no certificates"));
    }
    Ok(certs)
}

fn load_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    let pem = std::fs::read(path).with_context(|| format!("reading TLS key file {path}"))?;
    let mut reader = std::io::BufReader::new(&pem[..]);
    rustls_pemfile::private_key(&mut reader)
        .with_context(|| format!("parsing TLS key file {path}"))?
        .ok_or_else(|| anyhow!("TLS key file {path} contains no private key"))
}

/// The SIPS ServerName for the platform: its IP literal, or DNS name.
pub(crate) fn server_name(addr: &str) -> Result<ServerName<'static>> {
    if let Ok(ip) = addr.parse::<std::net::IpAddr>() {
        return Ok(ServerName::IpAddress(ip.into()));
    }
    ServerName::try_from(addr.to_string())
        .map_err(|_| anyhow!("gb28181: platform address {addr} is neither an IP nor a DNS name"))
}

/// Accept-any verifier (the explicit `tls_insecure_skip_verify` opt-out).
#[derive(Debug)]
struct NoVerifier(Arc<tokio_rustls::rustls::crypto::CryptoProvider>);

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::ServerCertVerified,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> std::result::Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
