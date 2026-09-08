//! Library-neutral observability hooks (#30): the host bridges events to
//! Prometheus (or any backend) without this crate taking a metrics
//! dependency. All methods default to no-ops — implement only what you
//! need:
//!
//! ```rust,ignore
//! struct PrometheusBridge { /* your registry */ }
//! impl gb28181_rs::metrics::MetricsHooks for PrometheusBridge {
//!     fn register_ok(&self) { /* counter.inc() */ }
//!     // …
//! }
//! let server = Gb28181Server::new(cfg)
//!     .with_metrics(std::sync::Arc::new(PrometheusBridge))
//!     .spawn().await?;
//! ```
//!
//! Hooks must be cheap (they fire on hot paths such as every RTP packet);
//! never block inside them.

/// Observation hooks fired by the server lifecycle and media paths.
/// Default implementations are no-ops.
pub trait MetricsHooks: Send + Sync + 'static {
    /// A REGISTER attempt started (each retry counts).
    fn register_attempt(&self) {}
    /// The platform accepted the REGISTER.
    fn register_ok(&self) {}
    /// A REGISTER attempt failed (before retries are exhausted).
    fn register_fail(&self) {}
    /// A keepalive MESSAGE failed (timeout/error).
    fn keepalive_fail(&self) {}
    /// An INVITE media session started streaming.
    fn invite_session_started(&self) {}
    /// An INVITE media session stopped (BYE, error, shutdown).
    fn invite_session_stopped(&self) {}
    /// MPEG-PS bytes handed to the RTP path.
    fn ps_bytes_out(&self, _bytes: u64) {}
    /// RTP packets sent (media + playback).
    fn rtp_packets_out(&self, _packets: u64) {}
}

/// The default no-op hooks used when none are configured.
pub struct NoopMetrics;

impl MetricsHooks for NoopMetrics {}
