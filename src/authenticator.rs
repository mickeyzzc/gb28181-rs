//! Opt-in authentication seams for the REGISTER lifecycle. The built-in
//! flow is SIP Digest (RFC 7616); installing a [`RegisterAuthenticator`]
//! via [`Gb28181Server::with_register_authenticator`] replaces it — the
//! reference implementation is the feature-gated
//! [`security35114`](crate::security35114) module (GB 35114 A-level).

use anyhow::Result;

/// Device-side behavior when a platform→device request fails incoming
/// Note verification (GB 35114 §9.4; issue #41).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IncomingNotePolicy {
    /// Answers 403 Forbidden. The default: inside an authenticated
    /// A-level session a Note that does not verify is tampering and must
    /// fail closed. Note-less requests still pass (mixed-mode Digest
    /// platforms).
    #[default]
    Reject,
    /// Logs the failure and serves the request — rollout observation on
    /// deployments with clock skew or signature quirks.
    Warn,
    /// Disables incoming Note verification entirely.
    Off,
}

/// Replaces the built-in Digest REGISTER authentication and (optionally,
/// via the `decorate_outgoing` default-method override) authenticates
/// every subsequent outgoing SIP request. All methods run inside the
/// server lifecycle; errors abort the REGISTER cycle. The `None` default
/// keeps the Digest flow.
pub trait RegisterAuthenticator: Send + Sync {
    /// Returns the Authorization header of the first REGISTER ("" for
    /// none), e.g. a GB35114 Capability announcement.
    fn initial_authorization(&self) -> String;

    /// Consumes the WWW-Authenticate header of the 401 response and
    /// returns the Authorization header of the retried REGISTER.
    fn authorize_with_challenge(&self, www_authenticate: &str) -> Result<String>;

    /// Validates the 200 OK that answers the authenticated REGISTER. The
    /// `security_info` argument is the 200 OK's SecurityInfo extension
    /// header, "" when absent (plain Digest platforms never send one;
    /// implementations decide whether that is acceptable — GB 35114
    /// fails closed).
    fn verify_ok(&self, security_info: &str) -> Result<()>;

    /// Returns the values of the Date and Note headers to stamp on every
    /// outgoing non-REGISTER SIP request (GB 35114 §9.4). The default
    /// leaves messages untouched; the GB35114 implementation overrides it
    /// once the VKEK is negotiated.
    fn decorate_outgoing(
        &self,
        _method: &str,
        _from: &str,
        _to: &str,
        _call_id: &str,
        _body: &str,
    ) -> (String, String) {
        (String::new(), String::new())
    }

    /// Verifies a Note header on a platform→device request — the mirror
    /// of `decorate_outgoing` (GB 35114 §9.4; issue #41). `note` is ""
    /// when the request carries none; implementations decide whether
    /// that is acceptable (GB 35114 tolerates mixed-mode Digest
    /// platforms). `date` is the request's Date header — part of the
    /// digest and the freshness anchor. Verification runs before method
    /// dispatch; the failure behavior is
    /// [`Gb28181Config::incoming_note_policy`](crate::Gb28181Config::incoming_note_policy).
    /// The default accepts everything (plain Digest installations).
    #[allow(clippy::too_many_arguments)] // mirrors the wire-signature fields
    fn verify_incoming_note(
        &self,
        _method: &str,
        _from: &str,
        _to: &str,
        _call_id: &str,
        _date: &str,
        _note: &str,
        _body: &str,
    ) -> Result<()> {
        Ok(())
    }
}
