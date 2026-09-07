//! Opt-in authentication seams for the REGISTER lifecycle. The built-in
//! flow is SIP Digest (RFC 7616); installing a [`RegisterAuthenticator`]
//! via [`Gb28181Server::with_register_authenticator`] replaces it — the
//! reference implementation is the feature-gated
//! [`security35114`](crate::security35114) module (GB 35114 A-level).

use anyhow::Result;

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
}
