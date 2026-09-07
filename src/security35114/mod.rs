//! GB 35114-2017 **A-level** device-side security for GB/T 28181:
//! SM2-certificate mutual authentication during REGISTER and the keyed-SM3
//! integrity header for subsequent SIP signaling.
//!
//! Only level A is implemented. Levels B/C additionally require signed and
//! encrypted media built on GB/T 25724 (SVAC), which is a hardware codec —
//! out of scope by design.
//!
//! Enable with the `gb35114` cargo feature:
//!
//! ```toml
//! gb28181-rs = { version = "0.8", features = ["gb35114"] }
//! ```
//!
//! Wire formats follow the published standard text cross-checked against
//! real device↔platform captures; the golden tests share their fixtures
//! and vectors with the Go twin (`gb28181-go/security35114`), pinning both
//! implementations to the same contract. Two points are ambiguous in the
//! wild and therefore configurable (see [`RandomEncoding`] and
//! [`Sign2Order`]): the representation of the randoms inside the signed
//! payload, and the R1/R2 order of the platform's sign2 input.

pub mod authenticator;
pub mod crypto;
pub mod headers;
pub mod integrity;

pub use authenticator::{Authenticator, Options};
pub use crypto::{
    decrypt_vkek, encrypt_vkek, load_certificate, load_identity, public_key_sec1, sign2_payload,
    sign_auth_payload, sign_message, verify_message, Certificate, Identity,
};
pub use headers::{
    build_auth_authorization, build_capability_authorization, build_security_info,
    parse_auth_authorization, parse_challenge, parse_security_info, AuthAuthorization, Challenge,
    Mode, SecurityInfo, CAPABILITY_ALGORITHM,
};
pub use integrity::{
    build_note_header, digest_payload, format_date, parse_note_header, verify_note_header,
};

/// Selects how the random values enter the signed payload.
///
/// Real-world captures concatenate the base64 strings exactly as they
/// appear in the headers, while a literal reading of the standard
/// concatenates the decoded 16-byte randoms. Both are provided; the
/// default matches captures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RandomEncoding {
    /// Concatenate the base64 header values (default).
    #[default]
    ConcatWireStrings,
    /// Concatenate the decoded random bytes.
    ConcatRawBytes,
}

/// Selects the operand order of the platform's sign2 payload. The standard
/// text reads R1+R2+deviceid+cryptkey; one widely cited capture
/// concatenates R2 first. The default follows the standard text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sign2Order {
    /// Signs random1+random2+deviceid+cryptkey (standard order).
    #[default]
    R1R2,
    /// Signs random2+random1+deviceid+cryptkey (capture order).
    R2R1,
}

/// Selects how the negotiated VKEK enters the Note-header digest input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VkekEncoding {
    /// Uses the raw 16-byte VKEK (default).
    #[default]
    Raw,
    /// Uses its base64 text form.
    Base64String,
}
