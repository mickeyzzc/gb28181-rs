//! SM2 signing and the VKEK envelope of GB35114 A-level. Certificates
//! follow GM/T 0015-2012 (SM2 X.509); crypto comes from the RustCrypto
//! `sm2`/`sm3` crates (pure Rust, `aarch64-musl` cross-compile friendly).

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use sm2::dsa::signature::{RandomizedSigner, Verifier};
use sm2::elliptic_curve::common::getrandom::SysRng;
use sm2::elliptic_curve::sec1::ToSec1Point;

use super::{RandomEncoding, Sign2Order};

/// Default SM2 distinguished identifier (GM/T 0003-2012 default UID).
const DEFAULT_UID: &str = "1234567812345678";

/// The SM2-relevant parts of one security entity's X.509 certificate.
/// Trust is established by provisioning (the host pins the peer
/// certificate); the self-signature is not re-verified here.
#[derive(Debug, Clone)]
pub struct Certificate {
    pub public_key: sm2::PublicKey,
    pub subject_cn: String,
    pub not_before_unix: i64,
    pub not_after_unix: i64,
}

/// One security entity's SM2 signing credential: the certificate
/// presented to the peer and its private key.
#[derive(Debug, Clone)]
pub struct Identity {
    pub certificate: Certificate,
    pub secret_key: sm2::SecretKey,
    /// Source PEM, so the certificate can be re-announced (Capability
    /// cnonce).
    pub cert_pem: String,
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn pem_first_block(raw: &str, want: &str) -> Result<Vec<u8>> {
    let mut in_block = false;
    let mut body = String::new();
    let mut block_type = String::new();
    for line in raw.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("-----BEGIN ") {
            if let Some(t) = rest.strip_suffix("-----") {
                block_type = t.to_string();
                in_block = block_type.contains(want);
                body.clear();
                continue;
            }
        }
        if in_block && l.starts_with("-----END") {
            return b64()
                .decode(body.as_bytes())
                .with_context(|| format!("decoding PEM block {block_type}"));
        }
        if in_block {
            body.push_str(l);
        }
    }
    Err(anyhow!("no PEM block containing {want} found"))
}

/// Loads a PEM SM2 certificate (GM/T 0015-2012).
pub fn load_certificate(pem: &str) -> Result<Certificate> {
    let der = pem_first_block(pem, "CERTIFICATE")?;
    parse_certificate_der(&der)
}

/// Parses a DER SM2 certificate.
pub fn parse_certificate_der(der: &[u8]) -> Result<Certificate> {
    let cert = x509_parser::parse_x509_certificate(der)
        .map_err(|e| anyhow!("x509 parse: {e}"))?
        .1;
    let spki = cert.public_key();
    let point = spki.subject_public_key.data.as_ref();
    let public_key = sm2::PublicKey::from_sec1_bytes(point)
        .map_err(|e| anyhow!("certificate public key is not an SM2 point: {e}"))?;
    let subject_cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|n| n.as_str().ok())
        .unwrap_or_default()
        .to_string();
    Ok(Certificate {
        public_key,
        subject_cn,
        not_before_unix: cert.validity().not_before.timestamp(),
        not_after_unix: cert.validity().not_after.timestamp(),
    })
}

/// Loads a certificate plus its private key. The key may be an SM2 SEC1
/// PEM (`SM2 PRIVATE KEY`) or PKCS#8 (`PRIVATE KEY`).
pub fn load_identity(cert_pem: &str, key_pem: &str) -> Result<Identity> {
    let certificate = load_certificate(cert_pem)?;
    let secret_key = if let Ok(sec1_der) = pem_first_block(key_pem, "SM2 PRIVATE KEY") {
        parse_sec1_key(&sec1_der)?
    } else {
        let pkcs8_der = pem_first_block(key_pem, "PRIVATE KEY")?;
        <sm2::SecretKey as sm2::elliptic_curve::pkcs8::DecodePrivateKey>::from_pkcs8_der(&pkcs8_der)
            .map_err(|e| anyhow!("pkcs8 key: {e}"))?
    };
    Ok(Identity {
        certificate,
        secret_key,
        cert_pem: cert_pem.to_string(),
    })
}

/// Parses an RFC 5915 ECPrivateKey DER (`SM2 PRIVATE KEY` PEM body):
/// `SEQUENCE { INTEGER 1, OCTET STRING privateKey, [0] OID, [1] BIT STRING }`.
/// Hand-rolled — pulling the `sec1` crate for one fixed shape isn't worth
/// the version-collision surface against elliptic-curve's own `sec1`.
fn parse_sec1_key(der: &[u8]) -> Result<sm2::SecretKey> {
    let err = || anyhow!("sec1 key: malformed ECPrivateKey DER");
    let mut i = 0usize;
    let read_len = |i: &mut usize| -> Result<usize> {
        let b = *der.get(*i).ok_or_else(err)?;
        *i += 1;
        if b & 0x80 == 0 {
            return Ok(b as usize);
        }
        let n = (b & 0x7F) as usize;
        let mut len = 0usize;
        for _ in 0..n {
            len = (len << 8) | *der.get(*i).ok_or_else(err)? as usize;
            *i += 1;
        }
        Ok(len)
    };
    if *der.get(i).ok_or_else(err)? != 0x30 {
        return Err(err());
    }
    i += 1;
    let seq_len = read_len(&mut i)?;
    let end = i + seq_len;
    // INTEGER version (expect 1)
    if *der.get(i).ok_or_else(err)? != 0x02 {
        return Err(err());
    }
    i += 1;
    let vlen = read_len(&mut i)?;
    i += vlen;
    // OCTET STRING privateKey
    if *der.get(i).ok_or_else(err)? != 0x04 {
        return Err(err());
    }
    i += 1;
    let klen = read_len(&mut i)?;
    let key = der.get(i..i + klen).ok_or_else(err)?;
    if i + klen > end {
        return Err(err());
    }
    sm2::SecretKey::from_slice(key).map_err(|e| anyhow!("sm2 scalar: {e}"))
}

/// Returns the octets signed by the device for sign1:
/// random2 ‖ random1 ‖ SIP-server-ID, in the configured representation.
pub fn sign_auth_payload(
    random1: &str,
    random2: &str,
    server_id: &str,
    enc: RandomEncoding,
) -> Vec<u8> {
    match enc {
        RandomEncoding::ConcatRawBytes => {
            let mut out = decode_b64_lossy(random2);
            out.extend_from_slice(&decode_b64_lossy(random1));
            out.extend_from_slice(server_id.as_bytes());
            out
        }
        RandomEncoding::ConcatWireStrings => format!("{random2}{random1}{server_id}").into_bytes(),
    }
}

/// Returns the octets signed by the platform for sign2:
/// randoms ‖ device-ID ‖ cryptkey, operand order per `order`.
pub fn sign2_payload(
    random1: &str,
    random2: &str,
    device_id: &str,
    crypt_key: &str,
    order: Sign2Order,
    enc: RandomEncoding,
) -> Vec<u8> {
    match enc {
        RandomEncoding::ConcatRawBytes => {
            let (first, second) = if order == Sign2Order::R2R1 {
                (decode_b64_lossy(random2), decode_b64_lossy(random1))
            } else {
                (decode_b64_lossy(random1), decode_b64_lossy(random2))
            };
            let mut out = first;
            out.extend_from_slice(&second);
            out.extend_from_slice(device_id.as_bytes());
            out.extend_from_slice(crypt_key.as_bytes());
            out
        }
        RandomEncoding::ConcatWireStrings => {
            if order == Sign2Order::R2R1 {
                format!("{random2}{random1}{device_id}{crypt_key}").into_bytes()
            } else {
                format!("{random1}{random2}{device_id}{crypt_key}").into_bytes()
            }
        }
    }
}

/// Signs `payload` with SM2 (default UID) and returns the base64 DER
/// signature.
pub fn sign_message(secret: &sm2::SecretKey, payload: &[u8]) -> Result<String> {
    let signing = sm2::dsa::SigningKey::new(DEFAULT_UID, secret)
        .map_err(|e| anyhow!("sm2 signing key: {e}"))?;
    let sig = signing
        .try_sign_with_rng(&mut SysRng, payload)
        .map_err(|e| anyhow!("sm2 sign: {e}"))?;
    Ok(b64().encode(sig.to_der().as_bytes()))
}

/// Verifies a base64 DER SM2 signature over `payload` with the
/// certificate's public key (default UID).
pub fn verify_message(cert: &Certificate, payload: &[u8], sig_b64: &str) -> Result<()> {
    let verifying = sm2::dsa::VerifyingKey::new(DEFAULT_UID, cert.public_key)
        .map_err(|e| anyhow!("sm2 verifying key: {e}"))?;
    let sig_der = b64()
        .decode(sig_b64)
        .map_err(|e| anyhow!("signature is not base64: {e}"))?;
    let sig = sm2::dsa::Signature::from_der(&sig_der).map_err(|e| anyhow!("signature DER: {e}"))?;
    verifying
        .verify(payload, &sig)
        .map_err(|_| anyhow!("SM2 signature verification failed"))
}

/// Seals `vkek` to the device public key as the cryptkey value: base64 DER
/// SM2 envelope (C1 ‖ C3 ‖ C2 per GM/T 0009 / GB/T 32918.4). Used by the
/// platform side; provided for conformance tests and UAS implementations.
pub fn encrypt_vkek(public_key: &sm2::PublicKey, vkek: &[u8]) -> Result<String> {
    let encrypting = sm2::pke::EncryptingKey::new_with_mode(*public_key, sm2::pke::Mode::C1C3C2);
    let der = encrypting
        .encrypt_der(&mut SysRng, vkek)
        .map_err(|e| anyhow!("sm2 encrypt: {e}"))?;
    Ok(b64().encode(der))
}

/// Opens the cryptkey envelope from a SecurityInfo header and returns the
/// negotiated VKEK.
pub fn decrypt_vkek(secret: &sm2::SecretKey, crypt_key_b64: &str) -> Result<Vec<u8>> {
    let der = b64()
        .decode(crypt_key_b64)
        .map_err(|e| anyhow!("cryptkey is not base64: {e}"))?;
    let decrypting = sm2::pke::DecryptingKey::new(secret.clone());
    decrypting
        .decrypt_der(&der)
        .map_err(|e| anyhow!("sm2 decrypt: {e}"))
}

/// Decodes base64, falling back to the raw bytes on error (inputs come
/// from peer headers; malformed values surface as signature/decrypt
/// failures rather than panics here).
fn decode_b64_lossy(s: &str) -> Vec<u8> {
    b64().decode(s).unwrap_or_else(|_| s.as_bytes().to_vec())
}

/// Uncompressed SEC1 point encoding of a public key (used by tests and
/// hosts bridging to other crypto stacks).
pub fn public_key_sec1(public_key: &sm2::PublicKey) -> Vec<u8> {
    public_key.to_sec1_point(false).as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security35114::{RandomEncoding::*, Sign2Order::*};

    const GOLDEN_RANDOM1: &str = "PRAIIbutDbd5x/NKsbwwYw==";
    const GOLDEN_RANDOM2: &str = "F4InuQewuMMqYPy1ItBdhQ==";
    const GOLDEN_SERVER: &str = "34020000002000000001";
    const GOLDEN_DEVICE: &str = "34020000001320000001";
    const GOLDEN_CRYPT: &str = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWQ==";
    const GOLDEN_VKEK: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    // Cross-language fixtures produced once by the Go twin (gmsm) —
    // proof that both implementations interoperate on the same wire
    // formats. Regenerate only via gb28181-rs/tmp/gen_fixture (never by
    // hand).
    const INTEROP: &str = include_str!("testdata/interop_fixtures.txt");
    const DEVICE_CERT: &str = include_str!("testdata/device_cert.pem");
    const DEVICE_KEY: &str = include_str!("testdata/device_key.pem");
    const PLATFORM_CERT: &str = include_str!("testdata/platform_cert.pem");
    const PLATFORM_KEY: &str = include_str!("testdata/platform_key.pem");

    fn interop_field(key: &str) -> String {
        INTEROP
            .lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.strip_prefix(key))
            .unwrap()
            .to_string()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sign_payload_goldens() {
        // Same hex as the Go twin's payload goldens.
        assert_eq!(
            hex(&sign_auth_payload(GOLDEN_RANDOM1, GOLDEN_RANDOM2, GOLDEN_SERVER, ConcatWireStrings)),
            "4634496e75516577754d4d71595079314974426468513d3d505241494962757444626435782f4e4b7362777759773d3d3334303230303030303032303030303030303031"
        );
        assert_eq!(
            hex(&sign_auth_payload(GOLDEN_RANDOM1, GOLDEN_RANDOM2, GOLDEN_SERVER, ConcatRawBytes)),
            "178227b907b0b8c32a60fcb522d05d853d100821bbad0db779c7f34ab1bc30633334303230303030303032303030303030303031"
        );
        assert_eq!(
            hex(&sign2_payload(GOLDEN_RANDOM1, GOLDEN_RANDOM2, GOLDEN_DEVICE, GOLDEN_CRYPT, R1R2, ConcatWireStrings)),
            "505241494962757444626435782f4e4b7362777759773d3d4634496e75516577754d4d71595079314974426468513d3d333430323030303030303133323030303030303151554a44524556475230684a536b744d545535505546465355315256566c645957513d3d"
        );
        assert_eq!(
            hex(&sign2_payload(GOLDEN_RANDOM1, GOLDEN_RANDOM2, GOLDEN_DEVICE, GOLDEN_CRYPT, R2R1, ConcatWireStrings)),
            "4634496e75516577754d4d71595079314974426468513d3d505241494962757444626435782f4e4b7362777759773d3d333430323030303030303133323030303030303151554a44524556475230684a536b744d545535505546465355315256566c645957513d3d"
        );
    }

    #[test]
    fn sign_verify_roundtrip() {
        let dev = load_identity(DEVICE_CERT, DEVICE_KEY).unwrap();
        let plat = load_identity(PLATFORM_CERT, PLATFORM_KEY).unwrap();
        let payload = sign_auth_payload(
            GOLDEN_RANDOM1,
            GOLDEN_RANDOM2,
            GOLDEN_SERVER,
            ConcatWireStrings,
        );

        let sig = sign_message(&dev.secret_key, &payload).unwrap();
        verify_message(&dev.certificate, &payload, &sig).unwrap();
        assert!(verify_message(&plat.certificate, &payload, &sig).is_err());

        let mut tampered = payload.clone();
        tampered[0] ^= 0xFF;
        assert!(verify_message(&dev.certificate, &tampered, &sig).is_err());
    }

    #[test]
    fn interop_verify_gmsm_sign1() {
        // The Go twin (gmsm) produced this sign1 over the standard payload
        // with the shared device key; RustCrypto sm2 must verify it.
        let dev = load_identity(DEVICE_CERT, DEVICE_KEY).unwrap();
        let sign1 = interop_field("SIGN1=");
        let payload = sign_auth_payload(
            GOLDEN_RANDOM1,
            GOLDEN_RANDOM2,
            GOLDEN_SERVER,
            ConcatWireStrings,
        );
        verify_message(&dev.certificate, &payload, &sign1)
            .expect("gmsm-produced sign1 must verify under RustCrypto sm2");
    }

    #[test]
    fn vkek_envelope_roundtrip() {
        let dev = load_identity(DEVICE_CERT, DEVICE_KEY).unwrap();
        let crypt = encrypt_vkek(&dev.certificate.public_key, &GOLDEN_VKEK).unwrap();
        let der = b64().decode(crypt.as_bytes()).unwrap();
        // GM/T 0009 envelope: SEQUENCE { INTEGER x, INTEGER y, OCTET STRING
        // hash(32), OCTET STRING cipher }. The INTEGERs grow by a 0x00 pad
        // byte whenever x or y has its high bit set (random ephemeral
        // point), so only the structure is pinned — the trailing ciphertext
        // is exactly the VKEK length.
        assert_eq!(der[0], 0x30, "DER SEQUENCE");
        // The final field's header (04 10 = OCTET STRING, 16 bytes) sits
        // directly before the ciphertext.
        let n = der.len();
        assert_eq!(
            &der[n - 18..n - 16],
            &[0x04, 0x10],
            "trailing 16-byte ciphertext"
        );
        assert_eq!(decrypt_vkek(&dev.secret_key, &crypt).unwrap(), GOLDEN_VKEK);

        let plat = load_identity(PLATFORM_CERT, PLATFORM_KEY).unwrap();
        assert!(decrypt_vkek(&plat.secret_key, &crypt).is_err());
        assert!(decrypt_vkek(&dev.secret_key, "!!!not-base64!!!").is_err());
    }

    #[test]
    fn interop_decrypt_gmsm_cryptkey() {
        // The Go twin (gmsm) sealed the golden VKEK to the shared device
        // key; RustCrypto sm2 must open it.
        let dev = load_identity(DEVICE_CERT, DEVICE_KEY).unwrap();
        let crypt = interop_field("CRYPTKEY=");
        assert_eq!(decrypt_vkek(&dev.secret_key, &crypt).unwrap(), GOLDEN_VKEK);
    }

    #[test]
    fn load_identity_and_certificate() {
        let dev = load_identity(DEVICE_CERT, DEVICE_KEY).unwrap();
        assert_eq!(dev.certificate.subject_cn, GOLDEN_DEVICE);
        assert_eq!(dev.certificate.not_before_unix, 1767225600); // 2026-01-01T00:00:00Z
        let plat = load_certificate(PLATFORM_CERT).unwrap();
        assert_eq!(plat.subject_cn, GOLDEN_SERVER);
        assert_eq!(public_key_sec1(&plat.public_key).len(), 65);
    }
}
