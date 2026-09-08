//! The platform-side (UAS) GB35114 A-level REGISTER state machine — the
//! counterpart of the device-side [`Authenticator`]:
//!
//! ```text
//! REGISTER  (Authorization: Capability …)          device → platform
//! 401       (WWW-Authenticate: … random1="…")      platform → device   challenge
//! REGISTER  (Authorization: Unidirection/Bidirection … sign1="…")
//! 200 OK    (SecurityInfo: cryptkey [, sign2])     platform → device   verify_register
//! ```
//!
//! After the handshake the negotiated VKEK keys the Note-header integrity
//! of every subsequent device request ([`Platform::verify_note`]).
//! Sessions are keyed by device ID; the type is safe for concurrent use.
//! There is no UAS SIP server in this crate (device/UAC role only) — wire
//! this state machine into any platform stack; see the Go twin's
//! `platform/sip` seam and its `tmp/fakeplatform2` reference platform.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rand::RngCore;

use super::headers::{
    build_challenge, build_security_info, parse_auth_authorization, parse_capability_authorization,
    Mode, SecurityInfo, CAPABILITY_ALGORITHM,
};
use super::integrity::verify_note_header;
use super::{
    encrypt_vkek, load_certificate, sign2_payload, sign_auth_payload, sign_message, verify_message,
    Certificate, Identity, RandomEncoding, Sign2Order, VkekEncoding,
};

/// Platform-side failure modes, mirroring the Go twin's sentinel errors so
/// SIP servers can map them onto 4xx responses. Carried as the root error
/// of `anyhow` chains (`err.downcast_ref::<PlatformError>()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PlatformError {
    /// `verify_register` arrived with no pending challenge for the device.
    NoChallenge,
    /// The Authorization echoes a random1 that is not the pending one — a
    /// replayed or forged handshake.
    ChallengeMismatch,
    /// No trusted certificate for the device — neither pre-provisioned nor
    /// announced via the Capability cnonce.
    DeviceCert,
    /// The device signed for a different SIP server ID.
    ServerIdMismatch,
    /// No completed handshake under this device ID.
    NoSession,
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            PlatformError::NoChallenge => "security35114: no pending challenge for device",
            PlatformError::ChallengeMismatch => {
                "security35114: Authorization random1 does not match the pending challenge"
            }
            PlatformError::DeviceCert => "security35114: no trusted certificate for device",
            PlatformError::ServerIdMismatch => {
                "security35114: Authorization signed for a different server ID"
            }
            PlatformError::NoSession => "security35114: no negotiated VKEK for device",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for PlatformError {}

/// Configures a [`Platform`]. `server_id` and at least one device
/// certificate source (`device_certs` or the Capability cnonce) are
/// required.
#[derive(Debug, Clone)]
pub struct PlatformConfig {
    /// The platform's 20-digit SIP server ID, echoed in challenges and
    /// SecurityInfo.
    pub server_id: String,
    /// The platform SM2 signing identity, required for Bidirection
    /// challenges (it produces sign2).
    pub identity: Option<Identity>,
    /// Pre-provisioned FDWSF signing certificates by device ID. A
    /// certificate announced via the Capability cnonce is used when no
    /// pre-provisioned entry exists.
    pub device_certs: HashMap<String, Certificate>,
    /// Selects the challenge scheme; defaults to Bidirection when
    /// `identity` is set and Unidirection otherwise.
    pub mode: Option<Mode>,
    /// Signed-payload representation (default matches captures).
    pub random_encoding: RandomEncoding,
    /// Platform sign2 operand order (default is the standard text order).
    pub sign2_order: Sign2Order,
    /// How the VKEK enters the Note digest (default raw bytes).
    pub vkek_encoding: VkekEncoding,
}

impl PlatformConfig {
    /// Convenience builder with the mandatory field; everything else
    /// defaults.
    pub fn new(server_id: impl Into<String>) -> Self {
        Self {
            server_id: server_id.into(),
            identity: None,
            device_certs: HashMap::new(),
            mode: None,
            random_encoding: RandomEncoding::default(),
            sign2_order: Sign2Order::default(),
            vkek_encoding: VkekEncoding::default(),
        }
    }
}

#[derive(Debug, Default)]
struct PlatformSession {
    mode: Option<Mode>,
    random1: String,
    announced_cert: Option<Certificate>,
    vkek: Option<Vec<u8>>,
    last_auth: String,
    last_security_info: String,
}

/// The platform (UAS) side of GB35114 A-level. Mirrors
/// `gb28181-go`'s `security35114.Platform`.
#[derive(Debug)]
pub struct Platform {
    cfg: PlatformConfig,
    sessions: Mutex<HashMap<String, PlatformSession>>,
}

/// Locks the session map tolerating poisoning: the sessions are plain data
/// with no invariants, so a panicked peer thread's guard is still
/// consistent to read/write. Keeps the library unwrap-free (hygiene rule).
fn lock_sessions(
    sessions: &Mutex<HashMap<String, PlatformSession>>,
) -> std::sync::MutexGuard<'_, HashMap<String, PlatformSession>> {
    match sessions.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Platform {
    /// Validates the configuration and returns a ready [`Platform`].
    pub fn new(cfg: PlatformConfig) -> Result<Self> {
        if cfg.server_id.is_empty() {
            bail!("security35114: PlatformConfig server_id is required");
        }
        let mode = match cfg.mode {
            None => {
                if cfg.identity.is_some() {
                    Mode::Bidirection
                } else {
                    Mode::Unidirection
                }
            }
            Some(Mode::Bidirection) => {
                if cfg.identity.is_none() {
                    bail!("security35114: Bidirection requires PlatformConfig identity");
                }
                Mode::Bidirection
            }
            Some(Mode::Unidirection) => Mode::Unidirection,
        };
        let mut cfg = cfg;
        cfg.mode = Some(mode);
        Ok(Self {
            cfg,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// Records the Capability announcement (parsing an optional cnonce
    /// device certificate) and issues a fresh random1 challenge for the
    /// device. Returns the complete WWW-Authenticate header value for the
    /// 401. An already-negotiated VKEK stays active while the device
    /// re-registers, so keepalives between the challenge and the
    /// completing REGISTER still verify.
    pub fn challenge(&self, device_id: &str, capability_authorization: &str) -> Result<String> {
        let mut random1_bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut random1_bytes);
        let random1 = base64::engine::general_purpose::STANDARD.encode(random1_bytes);

        let mode = self
            .cfg
            .mode
            .ok_or_else(|| anyhow!("security35114: platform mode unset (constructor bug)"))?;
        let announced_cert = if capability_authorization.is_empty() {
            None
        } else {
            parse_capability_authorization(capability_authorization)
                .ok()
                .and_then(|ann| {
                    if ann.device_cert_pem.is_empty() {
                        None
                    } else {
                        load_certificate(&ann.device_cert_pem).ok()
                    }
                })
        };

        let mut sessions = lock_sessions(&self.sessions);
        let session = sessions.entry(device_id.to_string()).or_default();
        session.mode = Some(mode);
        session.random1 = random1.clone();
        session.last_auth.clear();
        session.last_security_info.clear();
        if announced_cert.is_some() {
            session.announced_cert = announced_cert;
        }
        Ok(build_challenge(mode, &random1))
    }

    /// Validates the Authorization header of the retried REGISTER against
    /// the pending challenge and the device certificate, negotiates the
    /// VKEK, seals it to the device public key (cryptkey), and — for
    /// Bidirection — signs the response (sign2). Returns the complete
    /// SecurityInfo header value for the 200 OK. A byte-identical
    /// retransmission of the completed REGISTER returns the same
    /// SecurityInfo (SIP-over-UDP retransmission); any other
    /// Authorization with a stale random1 is rejected as a replay.
    pub fn verify_register(&self, device_id: &str, authorization: &str) -> Result<String> {
        let aa = parse_auth_authorization(authorization)?;

        let (random1, mode, last_auth, last_security_info, announced_cert) = {
            let sessions = lock_sessions(&self.sessions);
            match sessions.get(device_id) {
                None => return Err(anyhow!(PlatformError::NoChallenge)),
                Some(s) => (
                    s.random1.clone(),
                    s.mode,
                    s.last_auth.clone(),
                    s.last_security_info.clone(),
                    s.announced_cert.clone(),
                ),
            }
        };
        if authorization == last_auth {
            return Ok(last_security_info); // idempotent retransmission
        }
        if aa.random1 != random1 {
            return Err(anyhow!(PlatformError::ChallengeMismatch));
        }
        let mode = mode.ok_or_else(|| anyhow!(PlatformError::NoChallenge))?;
        if aa.mode != mode {
            bail!(
                "security35114: Authorization scheme {:?} does not answer the {:?} challenge",
                aa.mode,
                mode
            );
        }
        if aa.server_id != self.cfg.server_id {
            return Err(anyhow!(PlatformError::ServerIdMismatch));
        }
        if !aa.device_id.is_empty() && aa.device_id != device_id {
            bail!(
                "security35114: Authorization deviceid {:?} does not match the registering device {:?}",
                aa.device_id,
                device_id
            );
        }

        let cert = self
            .cfg
            .device_certs
            .get(device_id)
            .or(announced_cert.as_ref());
        let Some(cert) = cert else {
            return Err(anyhow!(PlatformError::DeviceCert));
        };

        let payload = sign_auth_payload(
            &aa.random1,
            &aa.random2,
            &aa.server_id,
            self.cfg.random_encoding,
        );
        verify_message(cert, &payload, &aa.sign1).context("security35114: verifying sign1")?;

        let mut vkek = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut vkek);
        let crypt_key =
            encrypt_vkek(&cert.public_key, &vkek).context("security35114: sealing VKEK")?;

        let mut si = SecurityInfo {
            mode: aa.mode,
            algorithm: CAPABILITY_ALGORITHM.to_string(),
            random1: String::new(),
            random2: String::new(),
            device_id: String::new(),
            server_id: String::new(),
            crypt_key: crypt_key.clone(),
            sign2: String::new(),
        };
        if aa.mode == Mode::Bidirection {
            let identity = self
                .cfg
                .identity
                .as_ref()
                .ok_or_else(|| anyhow!("security35114: Bidirection requires an identity"))?;
            si.random1 = aa.random1.clone();
            si.random2 = aa.random2.clone();
            si.device_id = device_id.to_string();
            si.server_id = self.cfg.server_id.clone();
            si.sign2 = sign_message(
                &identity.secret_key,
                &sign2_payload(
                    &aa.random1,
                    &aa.random2,
                    device_id,
                    &crypt_key,
                    self.cfg.sign2_order,
                    self.cfg.random_encoding,
                ),
            )
            .context("security35114: signing sign2")?;
        }
        let security_info = build_security_info(&si);

        let mut sessions = lock_sessions(&self.sessions);
        let session = sessions.entry(device_id.to_string()).or_default();
        session.vkek = Some(vkek.to_vec());
        session.last_auth = authorization.to_string();
        session.last_security_info = security_info.clone();
        Ok(security_info)
    }

    /// Validates the Note header of a subsequent request from the device.
    /// An empty note passes (the request carries no integrity header —
    /// e.g. a device that has not completed the handshake); a present but
    /// invalid one fails.
    #[allow(clippy::too_many_arguments)] // mirrors the wire-signature fields
    pub fn verify_note(
        &self,
        device_id: &str,
        note: &str,
        method: &str,
        from: &str,
        to: &str,
        call_id: &str,
        date: &str,
        body: &str,
    ) -> Result<()> {
        if note.is_empty() {
            return Ok(());
        }
        let vkek = self
            .vkek(device_id)
            .ok_or_else(|| anyhow!(PlatformError::NoSession))?;
        verify_note_header(
            note,
            method,
            from,
            to,
            call_id,
            date,
            &vkek,
            body,
            self.cfg.vkek_encoding,
        )
    }

    /// Returns the device's negotiated VKEK, or `None` before the
    /// handshake completed.
    pub fn vkek(&self, device_id: &str) -> Option<Vec<u8>> {
        lock_sessions(&self.sessions)
            .get(device_id)
            .and_then(|s| s.vkek.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authenticator::RegisterAuthenticator;
    use crate::security35114::authenticator::{Authenticator, Options};
    use crate::security35114::headers::{
        build_auth_authorization, build_capability_authorization, Challenge,
    };
    use crate::security35114::load_identity;

    // Same goldens/fixtures as the Go twin (gb28181-go/security35114).
    const GOLDEN_RANDOM1: &str = "PRAIIbutDbd5x/NKsbwwYw==";
    const GOLDEN_SERVER: &str = "34020000002000000001";
    const GOLDEN_DEVICE: &str = "34020000001320000001";
    const GOLDEN_SIGN1: &str = "MEUCIQD/9gP8olHM0TeLj0MxBRw3C8tQKFMMRgUupnyD4xXTTwIhAJvXxvTEDXj8Yk5qjHwujzUjpYpxxCGq7Zz0tKzhhJUU";

    const DEVICE_CERT: &str = include_str!("testdata/device_cert.pem");
    const DEVICE_KEY: &str = include_str!("testdata/device_key.pem");
    const PLATFORM_CERT: &str = include_str!("testdata/platform_cert.pem");
    const PLATFORM_KEY: &str = include_str!("testdata/platform_key.pem");

    fn device_identity() -> Identity {
        load_identity(DEVICE_CERT, DEVICE_KEY).unwrap()
    }

    fn platform_identity() -> Identity {
        load_identity(PLATFORM_CERT, PLATFORM_KEY).unwrap()
    }

    fn is_platform_err(err: &anyhow::Error, want: PlatformError) -> bool {
        err.downcast_ref::<PlatformError>() == Some(&want)
    }

    fn bidir_platform() -> Platform {
        let mut cfg = PlatformConfig::new(GOLDEN_SERVER);
        cfg.identity = Some(platform_identity());
        cfg.device_certs.insert(
            GOLDEN_DEVICE.to_string(),
            load_certificate(DEVICE_CERT).unwrap(),
        );
        Platform::new(cfg).unwrap()
    }

    fn bidir_device() -> Authenticator {
        let mut opts = Options::new(device_identity(), GOLDEN_DEVICE, GOLDEN_SERVER);
        opts.platform_cert = Some(platform_identity().certificate);
        opts.key_version = Some("2026-01-01T00:00:00.000".to_string());
        Authenticator::new(opts).unwrap()
    }

    #[test]
    fn loopback_bidirection_with_note() {
        let a = bidir_device();
        let p = bidir_platform();

        let www_auth = p
            .challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap();
        assert!(www_auth.starts_with("Bidirection algorithm=\"A:SM2;H:SM3\", random1=\""));
        let auth = a.authorize_with_challenge(&www_auth).unwrap();
        let si = p.verify_register(GOLDEN_DEVICE, &auth).unwrap();
        a.verify_ok(&si).unwrap();
        assert_eq!(a.vkek(), p.vkek(GOLDEN_DEVICE));

        // Note roundtrip: the device signs a keepalive, the platform verifies.
        let (from, to, call_id, body) = (
            "<sip:d@3402000000>",
            "<sip:s@3402000000>",
            "1@dev",
            "keepalive",
        );
        let (date, note) = a.decorate_outgoing("MESSAGE", from, to, call_id, body);
        assert!(!note.is_empty());
        p.verify_note(
            GOLDEN_DEVICE,
            &note,
            "MESSAGE",
            from,
            to,
            call_id,
            &date,
            body,
        )
        .unwrap();
        assert!(p
            .verify_note(
                GOLDEN_DEVICE,
                &note,
                "MESSAGE",
                from,
                to,
                call_id,
                &date,
                "tampered"
            )
            .is_err());
        // Empty note is a pass-through.
        p.verify_note(GOLDEN_DEVICE, "", "MESSAGE", from, to, call_id, &date, body)
            .unwrap();
    }

    #[test]
    fn loopback_unidirection() {
        let mut opts = Options::new(device_identity(), GOLDEN_DEVICE, GOLDEN_SERVER);
        opts.key_version = Some("2026-01-01T00:00:00.000".to_string());
        let a = Authenticator::new(opts).unwrap();

        // No identity: Unidirection challenge, no sign2.
        let mut cfg = PlatformConfig::new(GOLDEN_SERVER);
        cfg.device_certs.insert(
            GOLDEN_DEVICE.to_string(),
            load_certificate(DEVICE_CERT).unwrap(),
        );
        let p = Platform::new(cfg).unwrap();

        let www_auth = p
            .challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap();
        assert!(www_auth.starts_with("Unidirection "));
        let auth = a.authorize_with_challenge(&www_auth).unwrap();
        let si = p.verify_register(GOLDEN_DEVICE, &auth).unwrap();
        a.verify_ok(&si).unwrap();
        assert_eq!(a.vkek(), p.vkek(GOLDEN_DEVICE));
    }

    #[test]
    fn loopback_cnonce_cert() {
        // Device announces its certificate; the platform has no
        // pre-provisioned certs at all.
        let mut opts = Options::new(device_identity(), GOLDEN_DEVICE, GOLDEN_SERVER);
        opts.platform_cert = Some(platform_identity().certificate);
        opts.include_device_cert = true;
        let a = Authenticator::new(opts).unwrap();

        let p = Platform::new(PlatformConfig {
            identity: Some(platform_identity()),
            ..PlatformConfig::new(GOLDEN_SERVER)
        })
        .unwrap();

        let www_auth = p
            .challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap();
        let auth = a.authorize_with_challenge(&www_auth).unwrap();
        p.verify_register(GOLDEN_DEVICE, &auth).unwrap();
    }

    #[test]
    fn idempotent_retransmission() {
        let a = bidir_device();
        let p = bidir_platform();
        let www_auth = p
            .challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap();
        let auth = a.authorize_with_challenge(&www_auth).unwrap();
        let si1 = p.verify_register(GOLDEN_DEVICE, &auth).unwrap();
        let si2 = p.verify_register(GOLDEN_DEVICE, &auth).unwrap();
        assert_eq!(si1, si2);
    }

    #[test]
    fn rejects_stale_random1() {
        let a = bidir_device();
        let p = bidir_platform();
        let www_auth = p
            .challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap();
        let auth = a.authorize_with_challenge(&www_auth).unwrap();

        // A fresh challenge draws a different random1 (128-bit real rand);
        // the old handshake's Authorization must now be refused as a replay.
        p.challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap();
        let err = p.verify_register(GOLDEN_DEVICE, &auth).unwrap_err();
        assert!(
            is_platform_err(&err, PlatformError::ChallengeMismatch),
            "err = {err:#}"
        );
    }

    #[test]
    fn rejects_unknown_cert() {
        let a = bidir_device();
        let p = Platform::new(PlatformConfig::new(GOLDEN_SERVER)).unwrap();
        let www_auth = p
            .challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap(); // no cnonce
        let auth = a.authorize_with_challenge(&www_auth).unwrap();
        let err = p.verify_register(GOLDEN_DEVICE, &auth).unwrap_err();
        assert!(
            is_platform_err(&err, PlatformError::DeviceCert),
            "err = {err:#}"
        );
    }

    #[test]
    fn rejects_bad_sign1() {
        // The platform trusts the platform's own cert for the device ID —
        // the device's sign1 must not verify against it.
        let a = bidir_device();
        let mut cfg = PlatformConfig::new(GOLDEN_SERVER);
        cfg.identity = Some(platform_identity());
        cfg.device_certs.insert(
            GOLDEN_DEVICE.to_string(),
            load_certificate(PLATFORM_CERT).unwrap(),
        );
        let p = Platform::new(cfg).unwrap();
        let www_auth = p
            .challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap();
        let auth = a.authorize_with_challenge(&www_auth).unwrap();
        let err = p.verify_register(GOLDEN_DEVICE, &auth).unwrap_err();
        assert!(err.to_string().contains("sign1"), "err = {err:#}");
    }

    #[test]
    fn rejects_server_id_mismatch() {
        // The device believes it talks to a different platform: sign1
        // covers that other server ID, so this platform must refuse.
        let mut opts = Options::new(device_identity(), GOLDEN_DEVICE, "34020000002000000099");
        opts.platform_cert = Some(platform_identity().certificate);
        let a = Authenticator::new(opts).unwrap();
        let p = bidir_platform();
        let www_auth = p
            .challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap();
        let auth = a.authorize_with_challenge(&www_auth).unwrap();
        let err = p.verify_register(GOLDEN_DEVICE, &auth).unwrap_err();
        assert!(
            is_platform_err(&err, PlatformError::ServerIdMismatch),
            "err = {err:#}"
        );
    }

    #[test]
    fn rejects_scheme_confusion() {
        // A Unidirection platform (no identity) receiving a Bidirection
        // Authorization that echoes ITS pending random1 must refuse on the
        // scheme mismatch — not dereference a missing identity (the crash
        // class the Go twin's test matrix caught there).
        let p = Platform::new(PlatformConfig::new(GOLDEN_SERVER)).unwrap(); // Unidirection
        let www_auth = p
            .challenge(GOLDEN_DEVICE, &build_capability_authorization("k", ""))
            .unwrap();
        let ch = crate::security35114::parse_challenge(&www_auth).unwrap();
        let auth = build_auth_authorization(
            &Challenge {
                mode: Mode::Bidirection,
                algorithm: String::new(),
                random1: ch.random1,
            },
            "F4InuQewuMMqYPy1ItBdhQ==",
            GOLDEN_SERVER,
            GOLDEN_DEVICE,
            GOLDEN_SIGN1,
        );
        let err = p.verify_register(GOLDEN_DEVICE, &auth).unwrap_err();
        assert!(err.to_string().contains("does not answer"), "err = {err:#}");
    }

    #[test]
    fn verify_register_without_challenge() {
        let p = bidir_platform();
        let auth = build_auth_authorization(
            &Challenge {
                mode: Mode::Bidirection,
                algorithm: "A:SM2;H:SM3".to_string(),
                random1: GOLDEN_RANDOM1.to_string(),
            },
            "F4InuQewuMMqYPy1ItBdhQ==",
            GOLDEN_SERVER,
            GOLDEN_DEVICE,
            GOLDEN_SIGN1,
        );
        let err = p.verify_register(GOLDEN_DEVICE, &auth).unwrap_err();
        assert!(
            is_platform_err(&err, PlatformError::NoChallenge),
            "err = {err:#}"
        );
    }

    #[test]
    fn note_before_handshake_fails() {
        let p = bidir_platform();
        let err = p
            .verify_note(
                GOLDEN_DEVICE,
                "Digest nonce=\"x\",algorithm=SM3",
                "MESSAGE",
                "f",
                "t",
                "c",
                "2024-01-31T14:40:49.583",
                "",
            )
            .unwrap_err();
        assert!(
            is_platform_err(&err, PlatformError::NoSession),
            "err = {err:#}"
        );
    }

    #[test]
    fn challenge_keeps_active_vkek() {
        // A re-REGISTER starts a new challenge; until it completes,
        // keepalives keyed with the previous VKEK must keep verifying.
        let a = bidir_device();
        let p = bidir_platform();
        let www_auth = p
            .challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap();
        let auth = a.authorize_with_challenge(&www_auth).unwrap();
        let si = p.verify_register(GOLDEN_DEVICE, &auth).unwrap();
        a.verify_ok(&si).unwrap();
        let (from, to, call_id, body) = (
            "<sip:d@3402000000>",
            "<sip:s@3402000000>",
            "1@dev",
            "keepalive",
        );
        let (date, note) = a.decorate_outgoing("MESSAGE", from, to, call_id, body);

        p.challenge(GOLDEN_DEVICE, &a.initial_authorization())
            .unwrap();
        p.verify_note(
            GOLDEN_DEVICE,
            &note,
            "MESSAGE",
            from,
            to,
            call_id,
            &date,
            body,
        )
        .unwrap();
    }

    #[test]
    fn bidirection_requires_identity() {
        let err = Platform::new(PlatformConfig {
            mode: Some(Mode::Bidirection),
            ..PlatformConfig::new(GOLDEN_SERVER)
        });
        assert!(err.is_err());
    }

    #[test]
    fn server_id_required() {
        assert!(Platform::new(PlatformConfig::new("")).is_err());
    }
}
