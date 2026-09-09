//! The device-side GB35114 A-level REGISTER state machine, wired into
//! [`Gb28181Server`](crate::server::Gb28181Server) through the
//! [`RegisterAuthenticator`](crate::authenticator::RegisterAuthenticator)
//! seam:
//!
//! ```text
//! REGISTER  (Authorization: Capability …)          device → platform
//! 401       (WWW-Authenticate: … random1="…")      platform → device
//! REGISTER  (Authorization: Unidirection/Bidirection … sign1="…")
//! 200 OK    (SecurityInfo: cryptkey [, sign2])     platform → device
//! ```
//!
//! After the handshake the negotiated VKEK keys the Note-header integrity
//! of every subsequent SIP request
//! ([`OutgoingSigner`](crate::authenticator::OutgoingSigner)).

use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rand::RngCore;

use super::headers::{
    build_auth_authorization, build_capability_authorization, parse_challenge, parse_security_info,
    Mode,
};
use super::integrity::{build_note_header, format_date, parse_note_date, verify_note_header};
use super::{
    decrypt_vkek, sign2_payload, sign_auth_payload, sign_message, verify_message, Certificate,
    Identity, RandomEncoding, Sign2Order, VkekEncoding,
};
use crate::authenticator::RegisterAuthenticator;

/// Bounds how old a signed request's Date header may be (and how far in
/// the future): beyond it the Note is a replay. The digest alone is
/// self-consistent, so the window is the replay guard.
pub const NOTE_FRESHNESS_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Configures an [`Authenticator`]. `device`, `device_id` and `server_id`
/// are required; the rest carry safe defaults.
#[derive(Debug, Clone)]
pub struct Options {
    /// The FDWSF SM2 signing identity (certificate + key).
    pub device: Identity,
    /// The SIP server's signing certificate, required to verify sign2 of
    /// Bidirection handshakes.
    pub platform_cert: Option<Certificate>,
    /// The 20-digit GB28181 device ID.
    pub device_id: String,
    /// The 20-digit SIP server (platform) ID.
    pub server_id: String,
    /// Labels the device key in the Capability announcement; defaults to
    /// the current UTC time in Date-header format.
    pub key_version: Option<String>,
    /// Attaches the certificate PEM as cnonce in the Capability
    /// announcement for platforms that do not pre-provision it.
    pub include_device_cert: bool,
    /// Signed-payload representation (default matches captures).
    pub random_encoding: RandomEncoding,
    /// Platform sign2 operand order (default is the standard text order).
    pub sign2_order: Sign2Order,
}

impl Options {
    /// Convenience builder with the mandatory fields.
    pub fn new(
        device: Identity,
        device_id: impl Into<String>,
        server_id: impl Into<String>,
    ) -> Self {
        Self {
            device,
            platform_cert: None,
            device_id: device_id.into(),
            server_id: server_id.into(),
            key_version: None,
            include_device_cert: false,
            random_encoding: RandomEncoding::default(),
            sign2_order: Sign2Order::default(),
        }
    }
}

#[derive(Debug, Default)]
struct HandshakeState {
    mode: Option<Mode>,
    random1: String,
    random2: String,
    vkek: Option<Vec<u8>>,
}

/// Implements [`RegisterAuthenticator`] and [`OutgoingSigner`] for GB35114
/// A-level.
#[derive(Debug)]
pub struct Authenticator {
    opts: Options,
    state: Mutex<HandshakeState>,
}

/// Locks the handshake state tolerating poisoning: the state is plain
/// data with no invariants, so a panicked peer thread's guard is still
/// consistent to read/write. Keeps the library unwrap-free (hygiene rule).
fn lock_state(state: &Mutex<HandshakeState>) -> std::sync::MutexGuard<'_, HandshakeState> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Authenticator {
    /// Validates the options and returns a ready [`Authenticator`].
    pub fn new(opts: Options) -> Result<Self> {
        if opts.device_id.is_empty() || opts.server_id.is_empty() {
            bail!("security35114: Options device_id and server_id are required");
        }
        Ok(Self {
            opts,
            state: Mutex::new(HandshakeState::default()),
        })
    }

    /// The scheme negotiated by the last challenge (`None` before one
    /// arrives).
    pub fn mode(&self) -> Option<Mode> {
        lock_state(&self.state).mode
    }

    /// The negotiated video-key-encryption-key, or `None` before the
    /// handshake completes.
    pub fn vkek(&self) -> Option<Vec<u8>> {
        lock_state(&self.state).vkek.clone()
    }
}

impl RegisterAuthenticator for Authenticator {
    /// The Capability announcement of the first REGISTER.
    fn initial_authorization(&self) -> String {
        let key_version = self
            .opts
            .key_version
            .clone()
            .unwrap_or_else(|| format_date(SystemTime::now()));
        let cert = if self.opts.include_device_cert {
            self.opts.device.cert_pem.as_str()
        } else {
            ""
        };
        build_capability_authorization(&key_version, cert)
    }

    /// Consumes the 401 challenge, draws random2, signs and returns the
    /// Authorization header for the retried REGISTER.
    fn authorize_with_challenge(&self, www_authenticate: &str) -> Result<String> {
        let ch = parse_challenge(www_authenticate)?;
        let mut random2_bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut random2_bytes);
        let random2 = base64::engine::general_purpose::STANDARD.encode(random2_bytes);

        let payload = sign_auth_payload(
            &ch.random1,
            &random2,
            &self.opts.server_id,
            self.opts.random_encoding,
        );
        let sign1 = sign_message(&self.opts.device.secret_key, &payload)
            .context("security35114: signing challenge")?;

        let mut state = lock_state(&self.state);
        state.mode = Some(ch.mode);
        state.random1 = ch.random1.clone();
        state.random2 = random2.clone();
        drop(state);

        Ok(build_auth_authorization(
            &ch,
            &random2,
            &self.opts.server_id,
            &self.opts.device_id,
            &sign1,
        ))
    }

    /// Opens the cryptkey envelope of the 200 OK, and for Bidirection
    /// additionally verifies the platform's sign2 against the
    /// pre-provisioned platform certificate and the handshake state.
    fn verify_ok(&self, security_info: &str) -> Result<()> {
        let (mode, random1, random2) = {
            let state = lock_state(&self.state);
            (state.mode, state.random1.clone(), state.random2.clone())
        };
        if mode.is_none() {
            bail!("security35114: SecurityInfo received before a challenge was answered");
        }

        let si = parse_security_info(security_info)?;
        let vkek = decrypt_vkek(&self.opts.device.secret_key, &si.crypt_key)
            .context("security35114: opening cryptkey")?;
        if si.mode == Mode::Bidirection {
            let Some(platform_cert) = &self.opts.platform_cert else {
                bail!("security35114: Bidirection requires Options.platform_cert to verify sign2");
            };
            // The echoed handshake fields must match this handshake
            // exactly — anything else is a replay or a foreign session.
            if si.random1 != random1
                || si.random2 != random2
                || si.device_id != self.opts.device_id
                || si.server_id != self.opts.server_id
            {
                bail!("security35114: SecurityInfo handshake fields do not match this handshake");
            }
            let payload = sign2_payload(
                &si.random1,
                &si.random2,
                &si.device_id,
                &si.crypt_key,
                self.opts.sign2_order,
                self.opts.random_encoding,
            );
            verify_message(platform_cert, &payload, &si.sign2)
                .context("security35114: verifying sign2")?;
        }

        lock_state(&self.state).vkek = Some(vkek);
        Ok(())
    }

    /// Stamps the Date and Note headers carrying the keyed-SM3 digest onto
    /// every non-REGISTER request. Messages before the handshake
    /// completes, or REGISTER itself, pass through untouched.
    fn decorate_outgoing(
        &self,
        method: &str,
        from: &str,
        to: &str,
        call_id: &str,
        body: &str,
    ) -> (String, String) {
        let vkek = match self.vkek() {
            Some(v) if method != "REGISTER" => v,
            _ => return (String::new(), String::new()),
        };
        let date = format_date(SystemTime::now());
        let note = build_note_header(
            method,
            from,
            to,
            call_id,
            &date,
            &vkek,
            body,
            VkekEncoding::Raw,
        );
        (date, note)
    }

    /// Verifies a platform→device request's Note against the negotiated
    /// VKEK — the device-side mirror of `decorate_outgoing` (issue #41).
    /// A request without a Note passes (mixed-mode Digest platforms). A
    /// malformed or stale Date fails closed: the digest alone is
    /// self-consistent, so the freshness window is the only replay
    /// guard and a Date it cannot parse disables that guard.
    fn verify_incoming_note(
        &self,
        method: &str,
        from: &str,
        to: &str,
        call_id: &str,
        date: &str,
        note: &str,
        body: &str,
    ) -> Result<()> {
        if note.is_empty() {
            return Ok(());
        }
        let vkek = self.vkek().ok_or_else(|| {
            anyhow!("security35114: incoming Note before the handshake completed")
        })?;
        let stamp = parse_note_date(date).context("security35114: incoming Note Date malformed")?;
        let skew = SystemTime::now()
            .duration_since(stamp)
            .unwrap_or_else(|_| stamp.duration_since(SystemTime::now()).unwrap_or_default());
        if skew > NOTE_FRESHNESS_WINDOW {
            bail!("security35114: incoming Note Date {date} outside the freshness window");
        }
        verify_note_header(
            note,
            method,
            from,
            to,
            call_id,
            date,
            &vkek,
            body,
            VkekEncoding::Raw,
        )
        .context("security35114: incoming Note rejected")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security35114::headers::{
        build_security_info, parse_auth_authorization, Mode, SecurityInfo, CAPABILITY_ALGORITHM,
    };
    use crate::security35114::{encrypt_vkek, load_identity, RandomEncoding, Sign2Order};

    const GOLDEN_RANDOM1: &str = "PRAIIbutDbd5x/NKsbwwYw==";
    const GOLDEN_DEVICE: &str = "34020000001320000001";
    const GOLDEN_SERVER: &str = "34020000002000000001";
    const GOLDEN_VKEK: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    const DEVICE_CERT: &str = include_str!("testdata/device_cert.pem");
    const DEVICE_KEY: &str = include_str!("testdata/device_key.pem");
    const PLATFORM_CERT: &str = include_str!("testdata/platform_cert.pem");
    const PLATFORM_KEY: &str = include_str!("testdata/platform_key.pem");

    fn device_identity() -> Identity {
        load_identity(DEVICE_CERT, DEVICE_KEY).unwrap()
    }

    #[test]
    fn new_validates_mandatory_fields() {
        let id = device_identity();
        assert!(Authenticator::new(Options::new(id.clone(), "", "s")).is_err());
    }

    #[test]
    fn initial_authorization_stable() {
        let a = Authenticator::new(Options::new(
            device_identity(),
            GOLDEN_DEVICE,
            GOLDEN_SERVER,
        ))
        .unwrap();
        let got = a.initial_authorization();
        assert!(got.starts_with(
            "Capability algorithm=\"A:SM2;H:SM3;S:SM4/OFB/PKCS5;SI:SM3-SM2\", keyversion=\""
        ));
        assert_eq!(a.initial_authorization(), got);
    }

    #[test]
    fn handshake_unidirection_end_to_end() {
        let dev = device_identity();
        let a =
            Authenticator::new(Options::new(dev.clone(), GOLDEN_DEVICE, GOLDEN_SERVER)).unwrap();

        let auth = a
            .authorize_with_challenge(
                "Unidirection algorithm=\"A:SM2;H:SM3;S:SM1/OFB/PKCS5;SI:SM3-SM2\", random1=\"PRAIIbutDbd5x/NKsbwwYw==\"",
            )
            .unwrap();
        assert_eq!(a.mode(), Some(Mode::Unidirection));

        // Platform side: verify sign1, seal the VKEK, answer.
        let aa = parse_auth_authorization(&auth).unwrap();
        let payload = sign_auth_payload(
            &aa.random1,
            &aa.random2,
            GOLDEN_SERVER,
            RandomEncoding::ConcatWireStrings,
        );
        verify_message(&dev.certificate, &payload, &aa.sign1).unwrap();
        let crypt = encrypt_vkek(&dev.certificate.public_key, &GOLDEN_VKEK).unwrap();
        let si = build_security_info(&SecurityInfo {
            mode: Mode::Unidirection,
            algorithm: CAPABILITY_ALGORITHM.to_string(),
            random1: String::new(),
            random2: String::new(),
            device_id: String::new(),
            server_id: String::new(),
            crypt_key: crypt,
            sign2: String::new(),
        });
        a.verify_ok(&si).unwrap();
        assert_eq!(a.vkek(), Some(GOLDEN_VKEK.to_vec()));

        // Keepalive signing works with the negotiated VKEK.
        let (date, note) = RegisterAuthenticator::decorate_outgoing(
            &a,
            "MESSAGE",
            "<sip:f@d>",
            "<sip:t@d>",
            "c@h",
            "<Notify/>",
        );
        assert!(!date.is_empty());
        assert!(note.starts_with("Digest nonce=\""));
        // REGISTER never decorated.
        let (d2, n2) = a.decorate_outgoing("REGISTER", "", "", "", "");
        assert!(d2.is_empty() && n2.is_empty());

        // --- device-side downstream Note verification (issue #41) ---
        // Valid signature over a fresh Date passes.
        let date_in = format_date(SystemTime::now());
        let note_in = build_note_header(
            "INVITE",
            "<sip:p@d>",
            "<sip:dev@d>",
            "plat-1",
            &date_in,
            &GOLDEN_VKEK,
            "body",
            VkekEncoding::Raw,
        );
        assert!(
            a.verify_incoming_note(
                "INVITE",
                "<sip:p@d>",
                "<sip:dev@d>",
                "plat-1",
                &date_in,
                &note_in,
                "body"
            )
            .is_ok(),
            "a valid Note must verify"
        );
        // Tampered body fails.
        assert!(
            a.verify_incoming_note(
                "INVITE",
                "<sip:p@d>",
                "<sip:dev@d>",
                "plat-1",
                &date_in,
                &note_in,
                "tampered"
            )
            .is_err(),
            "a tampered body must fail"
        );
        // No Note passes (mixed-mode Digest platforms).
        assert!(
            a.verify_incoming_note("MESSAGE", "f", "t", "c", &date_in, "", "")
                .is_ok(),
            "a Note-less request must pass"
        );
        // Stale Date fails the freshness window even though the digest
        // is self-consistent.
        let stale = format_date(SystemTime::now() - std::time::Duration::from_secs(2 * 3600));
        let stale_note = build_note_header(
            "INVITE",
            "<sip:p@d>",
            "<sip:dev@d>",
            "plat-2",
            &stale,
            &GOLDEN_VKEK,
            "b",
            VkekEncoding::Raw,
        );
        assert!(
            a.verify_incoming_note(
                "INVITE",
                "<sip:p@d>",
                "<sip:dev@d>",
                "plat-2",
                &stale,
                &stale_note,
                "b"
            )
            .is_err(),
            "a stale Date must fail the freshness window"
        );
        // Malformed Date fails closed.
        let bad_note = build_note_header(
            "INVITE",
            "<sip:p@d>",
            "<sip:dev@d>",
            "plat-3",
            "not-a-date",
            &GOLDEN_VKEK,
            "b",
            VkekEncoding::Raw,
        );
        assert!(
            a.verify_incoming_note(
                "INVITE",
                "<sip:p@d>",
                "<sip:dev@d>",
                "plat-3",
                "not-a-date",
                &bad_note,
                "b"
            )
            .is_err(),
            "a malformed Date must fail closed"
        );
        // Before the handshake completes, verification refuses.
        let fresh = Authenticator::new(Options::new(
            device_identity(),
            GOLDEN_DEVICE,
            GOLDEN_SERVER,
        ))
        .unwrap();
        assert!(
            fresh
                .verify_incoming_note("MESSAGE", "f", "t", "c", &date_in, &note_in, "")
                .is_err(),
            "a Note before the handshake must not verify"
        );
    }

    #[test]
    fn handshake_bidirection_and_rejections() {
        let dev = device_identity();
        let plat_id = load_identity(PLATFORM_CERT, PLATFORM_KEY).unwrap();
        let mut opts = Options::new(dev.clone(), GOLDEN_DEVICE, GOLDEN_SERVER);
        opts.platform_cert = Some(plat_id.certificate.clone());
        opts.sign2_order = Sign2Order::R1R2;
        let a = Authenticator::new(opts).unwrap();

        let auth = a
            .authorize_with_challenge(
                "Bidirection algorithm=\"A:SM2;H:SM3\", random1=\"PRAIIbutDbd5x/NKsbwwYw==\"",
            )
            .unwrap();
        assert_eq!(a.mode(), Some(Mode::Bidirection));
        let aa = parse_auth_authorization(&auth).unwrap();
        assert_eq!(aa.device_id, GOLDEN_DEVICE);

        // Full bidirection answer verifies.
        let crypt = encrypt_vkek(&dev.certificate.public_key, &GOLDEN_VKEK).unwrap();
        let sign2 = sign_message(
            &plat_id.secret_key,
            &sign2_payload(
                GOLDEN_RANDOM1,
                &aa.random2,
                GOLDEN_DEVICE,
                &crypt,
                Sign2Order::R1R2,
                RandomEncoding::ConcatWireStrings,
            ),
        )
        .unwrap();
        let ok_si = build_security_info(&SecurityInfo {
            mode: Mode::Bidirection,
            algorithm: CAPABILITY_ALGORITHM.to_string(),
            random1: GOLDEN_RANDOM1.to_string(),
            random2: aa.random2.clone(),
            device_id: GOLDEN_DEVICE.to_string(),
            server_id: GOLDEN_SERVER.to_string(),
            crypt_key: crypt.clone(),
            sign2,
        });
        a.verify_ok(&ok_si).unwrap();
        assert_eq!(a.vkek(), Some(GOLDEN_VKEK.to_vec()));

        // Swapped randoms are rejected: b answered its own challenge, the
        // SecurityInfo must echo b's randoms in order — swapped echoes
        // (even correctly signed over the swapped order) fail.
        let mut opts2 = Options::new(dev.clone(), GOLDEN_DEVICE, GOLDEN_SERVER);
        opts2.platform_cert = Some(plat_id.certificate.clone());
        let b = Authenticator::new(opts2).unwrap();
        let auth_b = b
            .authorize_with_challenge(
                "Bidirection algorithm=\"A:SM2;H:SM3\", random1=\"PRAIIbutDbd5x/NKsbwwYw==\"",
            )
            .unwrap();
        let aa_b = parse_auth_authorization(&auth_b).unwrap();
        let wrong_sign2 = sign_message(
            &plat_id.secret_key,
            &sign2_payload(
                &aa_b.random2,
                &aa_b.random1,
                GOLDEN_DEVICE,
                &crypt,
                Sign2Order::R1R2,
                RandomEncoding::ConcatWireStrings,
            ),
        )
        .unwrap();
        let wrong = build_security_info(&SecurityInfo {
            random1: aa_b.random2.clone(),
            random2: aa_b.random1.clone(),
            sign2: wrong_sign2,
            crypt_key: crypt.clone(),
            mode: Mode::Bidirection,
            algorithm: CAPABILITY_ALGORITHM.to_string(),
            device_id: GOLDEN_DEVICE.to_string(),
            server_id: GOLDEN_SERVER.to_string(),
        });
        assert!(b.verify_ok(&wrong).is_err());

        // Bidirection without a platform certificate is rejected.
        let mut opts3 = Options::new(dev, GOLDEN_DEVICE, GOLDEN_SERVER);
        opts3.platform_cert = None;
        let c = Authenticator::new(opts3).unwrap();
        c.authorize_with_challenge(
            "Bidirection algorithm=\"A:SM2;H:SM3\", random1=\"PRAIIbutDbd5x/NKsbwwYw==\"",
        )
        .unwrap();
        assert!(c.verify_ok(&ok_si).is_err());

        // SecurityInfo before any challenge is rejected.
        let d = Authenticator::new(Options::new(
            load_identity(DEVICE_CERT, DEVICE_KEY).unwrap(),
            GOLDEN_DEVICE,
            GOLDEN_SERVER,
        ))
        .unwrap();
        assert!(d
            .verify_ok("Unidirection cryptkey=\"QUJD\", algorithm=\"A:SM2;H:SM3\"")
            .is_err());
    }
}
