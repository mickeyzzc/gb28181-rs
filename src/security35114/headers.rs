//! GB35114 A-level SIP header construction and parsing. The three
//! authentication headers are:
//!
//! - `Authorization: Capability/Unidirection/Bidirection` (device → platform)
//! - `WWW-Authenticate: Unidirection/Bidirection` challenge (401 response)
//! - `SecurityInfo: Unidirection/Bidirection` result (200 OK response)

use anyhow::{anyhow, bail, Result};

/// Algorithm capability string announced in the Capability Authorization
/// of the first REGISTER. SM4 is advertised for stream ciphers (SM1 is
/// hardware-only).
pub const CAPABILITY_ALGORITHM: &str = "A:SM2;H:SM3;S:SM4/OFB/PKCS5;SI:SM3-SM2";

/// The GB35114 authentication scheme negotiated by the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Authenticates the device to the platform only.
    Unidirection,
    /// Additionally authenticates the platform to the device.
    Bidirection,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Unidirection => "Unidirection",
            Mode::Bidirection => "Bidirection",
        }
    }
}

/// The parsed WWW-Authenticate header of a GB35114 401 response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub mode: Mode,
    pub algorithm: String,
    /// base64-encoded 128-bit server random.
    pub random1: String,
}

/// The parsed Authorization header of a GB35114 authenticated REGISTER
/// (platform side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthAuthorization {
    pub mode: Mode,
    pub random1: String,
    pub random2: String,
    pub server_id: String,
    /// Bidirection only.
    pub device_id: String,
    pub sign1: String,
    pub algorithm: String,
}

/// The parsed SecurityInfo header of the GB35114 200 OK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityInfo {
    pub mode: Mode,
    pub algorithm: String,
    /// base64, echoed (bidirection).
    pub random1: String,
    /// base64, echoed (bidirection).
    pub random2: String,
    /// echoed (bidirection).
    pub device_id: String,
    /// echoed (bidirection).
    pub server_id: String,
    /// base64 DER SM2 envelope carrying the VKEK.
    pub crypt_key: String,
    /// base64 DER SM2 signature (bidirection).
    pub sign2: String,
}

/// Splits `"Scheme rest-of-header"`.
fn split_scheme(header: &str) -> (&str, &str) {
    let header = header.trim();
    match header.find(' ') {
        Some(i) => (&header[..i], header[i + 1..].trim()),
        None => (header, ""),
    }
}

/// Splits a scheme-stripped header value into its parameters. Accepts
/// quoted (`key="value"`) and bare (`key=value`) forms separated by
/// optional spaces after commas — the Authorization builders emit
/// `", "` and captures show SecurityInfo without the space.
fn parse_params(s: &str) -> Vec<(String, String)> {
    let mut params = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // skip separators
        while i < bytes.len() && (bytes[i] == b',' || bytes[i] == b' ') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        // key
        let key_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
            i += 1;
        }
        if i == key_start || i >= bytes.len() || bytes[i] != b'=' {
            break;
        }
        let key = s[key_start..i].to_ascii_lowercase();
        i += 1; // '='
                // value
        let value = if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let v = s[start..i].to_string();
            if i < bytes.len() {
                i += 1; // closing quote
            }
            v
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b',' && bytes[i] != b' ' {
                i += 1;
            }
            s[start..i].to_string()
        };
        params.push((key, value));
    }
    params
}

fn param<'a>(params: &'a [(String, String)], key: &str) -> &'a str {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

fn parse_mode(scheme: &str, header: &str) -> Result<Mode> {
    match scheme {
        "Unidirection" => Ok(Mode::Unidirection),
        "Bidirection" => Ok(Mode::Bidirection),
        _ => Err(anyhow!("not a GB35114 header: {header}")),
    }
}

/// Builds the Authorization header of the first REGISTER, announcing the
/// device's security capability. When `cert_pem` is non-empty the
/// certificate is attached as `cnonce="devicecert:<base64 of PEM>"` —
/// carried by some platforms that do not pre-provision the device
/// certificate; the standard flow omits it.
pub fn build_capability_authorization(key_version: &str, cert_pem: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = write!(
        out,
        "Capability algorithm=\"{CAPABILITY_ALGORITHM}\", keyversion=\"{key_version}\""
    );
    if !cert_pem.is_empty() {
        use base64::Engine;
        let _ = write!(
            out,
            ", cnonce=\"devicecert:{}\"",
            base64::engine::general_purpose::STANDARD.encode(cert_pem.as_bytes())
        );
    }
    out
}

/// Parses the WWW-Authenticate header of a GB35114 401 response. Both
/// Unidirection and Bidirection challenges carry random1.
pub fn parse_challenge(www_authenticate: &str) -> Result<Challenge> {
    let (scheme, rest) = split_scheme(www_authenticate);
    let mode = parse_mode(scheme, www_authenticate)?;
    let params = parse_params(rest);
    let ch = Challenge {
        mode,
        algorithm: param(&params, "algorithm").to_string(),
        random1: param(&params, "random1").to_string(),
    };
    if ch.random1.is_empty() {
        bail!("{scheme} challenge missing random1");
    }
    Ok(ch)
}

/// Builds the WWW-Authenticate header value of a GB35114 401 response for
/// the given mode and random1 (platform side).
pub fn build_challenge(mode: Mode, random1: &str) -> String {
    format!(
        "{} algorithm=\"A:SM2;H:SM3\", random1=\"{random1}\"",
        mode.as_str()
    )
}

/// The parsed Authorization header of the first GB35114 REGISTER
/// (platform side).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityAnnouncement {
    pub algorithm: String,
    pub key_version: String,
    /// From `cnonce="devicecert:<base64>"`, empty when absent.
    pub device_cert_pem: String,
}

/// Parses the Capability announcement of the first GB35114 REGISTER. The
/// cnonce device certificate, when present, is decoded back to its PEM
/// text.
pub fn parse_capability_authorization(authorization: &str) -> Result<CapabilityAnnouncement> {
    let (scheme, rest) = split_scheme(authorization);
    if scheme != "Capability" {
        bail!("not a GB35114 Capability announcement: {authorization}");
    }
    let params = parse_params(rest);
    let algorithm = param(&params, "algorithm").to_string();
    if algorithm.is_empty() {
        bail!("Capability announcement missing algorithm");
    }
    let mut ann = CapabilityAnnouncement {
        algorithm,
        key_version: param(&params, "keyversion").to_string(),
        device_cert_pem: String::new(),
    };
    let cnonce = param(&params, "cnonce");
    if let Some(encoded) = cnonce.strip_prefix("devicecert:") {
        use base64::Engine;
        let der = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| anyhow!("cnonce devicecert is not base64: {e}"))?;
        ann.device_cert_pem = String::from_utf8_lossy(&der).into_owned();
    }
    Ok(ann)
}

/// Builds the Authorization header of the authenticated REGISTER.
/// `server_id` is the SIP server ID; `device_id` is only emitted for
/// Bidirection.
pub fn build_auth_authorization(
    ch: &Challenge,
    random2: &str,
    server_id: &str,
    device_id: &str,
    sign1: &str,
) -> String {
    let mut out = format!(
        "{} random1=\"{}\", random2=\"{}\", serverid=\"{}\"",
        ch.mode.as_str(),
        ch.random1,
        random2,
        server_id
    );
    if ch.mode == Mode::Bidirection {
        out.push_str(&format!(", deviceid=\"{device_id}\""));
    }
    out.push_str(&format!(
        ", sign1=\"{sign1}\", algorithm=\"{CAPABILITY_ALGORITHM}\""
    ));
    out
}

/// Parses the Authorization header produced for the authenticated
/// REGISTER (platform side).
pub fn parse_auth_authorization(authorization: &str) -> Result<AuthAuthorization> {
    let (scheme, rest) = split_scheme(authorization);
    let mode = parse_mode(scheme, authorization)?;
    let params = parse_params(rest);
    let aa = AuthAuthorization {
        mode,
        random1: param(&params, "random1").to_string(),
        random2: param(&params, "random2").to_string(),
        server_id: param(&params, "serverid").to_string(),
        device_id: param(&params, "deviceid").to_string(),
        sign1: param(&params, "sign1").to_string(),
        algorithm: param(&params, "algorithm").to_string(),
    };
    if aa.random1.is_empty()
        || aa.random2.is_empty()
        || aa.server_id.is_empty()
        || aa.sign1.is_empty()
    {
        bail!("{scheme} Authorization missing fields (random1, random2, serverid, sign1)");
    }
    if mode == Mode::Bidirection && aa.device_id.is_empty() {
        bail!("Bidirection Authorization missing deviceid");
    }
    Ok(aa)
}

/// Parses the SecurityInfo header of the GB35114 200 OK. Unidirection
/// requires cryptkey; Bidirection additionally requires random1,
/// random2, deviceid, serverid and sign2.
pub fn parse_security_info(header: &str) -> Result<SecurityInfo> {
    let (scheme, rest) = split_scheme(header);
    let mode = parse_mode(scheme, header)?;
    let params = parse_params(rest);
    let si = SecurityInfo {
        mode,
        algorithm: param(&params, "algorithm").to_string(),
        random1: param(&params, "random1").to_string(),
        random2: param(&params, "random2").to_string(),
        device_id: param(&params, "deviceid").to_string(),
        server_id: param(&params, "serverid").to_string(),
        crypt_key: param(&params, "cryptkey").to_string(),
        sign2: param(&params, "sign2").to_string(),
    };
    if si.crypt_key.is_empty() {
        bail!("{scheme} SecurityInfo missing cryptkey");
    }
    if mode == Mode::Bidirection
        && (si.random1.is_empty()
            || si.random2.is_empty()
            || si.device_id.is_empty()
            || si.server_id.is_empty()
            || si.sign2.is_empty())
    {
        bail!(
            "Bidirection SecurityInfo missing fields (random1, random2, deviceid, serverid, sign2)"
        );
    }
    Ok(si)
}

/// Renders a SecurityInfo header value (platform side of the handshake;
/// used by conformance tests and UAS implementations). Bidirection emits
/// the capture-observed comma-separated form.
pub fn build_security_info(si: &SecurityInfo) -> String {
    if si.mode == Mode::Unidirection {
        return format!(
            "Unidirection cryptkey=\"{}\", algorithm=\"{}\"",
            si.crypt_key, si.algorithm
        );
    }
    format!(
        "Bidirection algorithm=\"{}\",random1=\"{}\",random2=\"{}\",deviceid=\"{}\",serverid=\"{}\",cryptkey=\"{}\",sign2=\"{}\"",
        si.algorithm, si.random1, si.random2, si.device_id, si.server_id, si.crypt_key, si.sign2
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Same goldens as the Go twin (gb28181-go/security35114/headers_test.go).

    const GOLDEN_RANDOM1: &str = "PRAIIbutDbd5x/NKsbwwYw==";
    const GOLDEN_RANDOM2: &str = "F4InuQewuMMqYPy1ItBdhQ==";
    const GOLDEN_SERVER: &str = "34020000002000000001";
    const GOLDEN_DEVICE: &str = "34020000001320000001";
    const GOLDEN_SIGN1: &str = "MEUCIQD/9gP8olHM0TeLj0MxBRw3C8tQKFMMRgUupnyD4xXTTwIhAJvXxvTEDXj8Yk5qjHwujzUjpYpxxCGq7Zz0tKzhhJUU";
    const GOLDEN_SIGN2: &str = "MEUCIQDzvGhJCuxmH/3NNtLNnrXIUOxYkYB7j8/3Th1LvjZHggIgD/nd9RbpEd6neZTuXDsIbNzydyS8WarbN1p6nHD5pHk=";
    const GOLDEN_CRYPT: &str = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWQ==";

    #[test]
    fn capability_header_golden() {
        assert_eq!(
            build_capability_authorization("2019-08-06T05:31:39", ""),
            "Capability algorithm=\"A:SM2;H:SM3;S:SM4/OFB/PKCS5;SI:SM3-SM2\", keyversion=\"2019-08-06T05:31:39\""
        );
    }

    #[test]
    fn capability_header_with_cert() {
        let got = build_capability_authorization(
            "t",
            "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        );
        assert!(got.contains(", cnonce=\"devicecert:LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0t"));
    }

    #[test]
    fn parse_challenge_unidirection() {
        let ch = parse_challenge(
            "Unidirection algorithm=\"A:SM2;H:SM3;S:SM1/OFB/PKCS5;SI:SM3-SM2\", random1=\"PRAIIbutDbd5x/NKsbwwYw==\"",
        )
        .unwrap();
        assert_eq!(ch.mode, Mode::Unidirection);
        assert_eq!(ch.random1, GOLDEN_RANDOM1);
        assert_eq!(ch.algorithm, "A:SM2;H:SM3;S:SM1/OFB/PKCS5;SI:SM3-SM2");
    }

    #[test]
    fn build_challenge_golden() {
        // Same golden as the Go twin (gb28181-go/security35114).
        assert_eq!(
            build_challenge(Mode::Bidirection, GOLDEN_RANDOM1),
            "Bidirection algorithm=\"A:SM2;H:SM3\", random1=\"PRAIIbutDbd5x/NKsbwwYw==\""
        );
        assert_eq!(
            build_challenge(Mode::Unidirection, GOLDEN_RANDOM1),
            "Unidirection algorithm=\"A:SM2;H:SM3\", random1=\"PRAIIbutDbd5x/NKsbwwYw==\""
        );
    }

    #[test]
    fn parse_capability_authorization_roundtrip() {
        let ann = parse_capability_authorization(
            "Capability algorithm=\"A:SM2;H:SM3;S:SM4/OFB/PKCS5;SI:SM3-SM2\", keyversion=\"2026-01-01T00:00:00.000\"",
        )
        .unwrap();
        assert_eq!(ann.algorithm, CAPABILITY_ALGORITHM);
        assert_eq!(ann.key_version, "2026-01-01T00:00:00.000");
        assert_eq!(ann.device_cert_pem, "");

        // With the cnonce certificate the announcement round-trips the
        // exact PEM the device attached.
        let cert_pem = include_str!("testdata/device_cert.pem");
        let built = build_capability_authorization("2026-01-01T00:00:00.000", cert_pem);
        let ann = parse_capability_authorization(&built).unwrap();
        assert_eq!(ann.device_cert_pem, cert_pem);

        assert!(parse_capability_authorization("Digest realm=\"x\"").is_err());
        assert!(parse_capability_authorization("Capability keyversion=\"only\"").is_err());
    }

    #[test]
    fn parse_challenge_rejects_digest() {
        assert!(parse_challenge("Digest realm=\"3402000000\", nonce=\"abc\"").is_err());
        assert!(parse_challenge("Unidirection algorithm=\"A:SM2\"").is_err());
    }

    #[test]
    fn auth_authorization_unidirection_golden() {
        let ch = Challenge {
            mode: Mode::Unidirection,
            algorithm: String::new(),
            random1: GOLDEN_RANDOM1.to_string(),
        };
        assert_eq!(
            build_auth_authorization(&ch, GOLDEN_RANDOM2, GOLDEN_SERVER, "", GOLDEN_SIGN1),
            "Unidirection random1=\"PRAIIbutDbd5x/NKsbwwYw==\", random2=\"F4InuQewuMMqYPy1ItBdhQ==\", serverid=\"34020000002000000001\", sign1=\"MEUCIQD/9gP8olHM0TeLj0MxBRw3C8tQKFMMRgUupnyD4xXTTwIhAJvXxvTEDXj8Yk5qjHwujzUjpYpxxCGq7Zz0tKzhhJUU\", algorithm=\"A:SM2;H:SM3;S:SM4/OFB/PKCS5;SI:SM3-SM2\""
        );
    }

    #[test]
    fn auth_authorization_bidirection_golden_and_roundtrip() {
        let ch = Challenge {
            mode: Mode::Bidirection,
            algorithm: String::new(),
            random1: GOLDEN_RANDOM1.to_string(),
        };
        let built = build_auth_authorization(
            &ch,
            GOLDEN_RANDOM2,
            GOLDEN_SERVER,
            GOLDEN_DEVICE,
            GOLDEN_SIGN1,
        );
        assert!(built.starts_with("Bidirection random1="));
        assert!(built.contains("deviceid=\"34020000001320000001\""));
        let aa = parse_auth_authorization(&built).unwrap();
        assert_eq!(aa.mode, Mode::Bidirection);
        assert_eq!(aa.random1, GOLDEN_RANDOM1);
        assert_eq!(aa.random2, GOLDEN_RANDOM2);
        assert_eq!(aa.server_id, GOLDEN_SERVER);
        assert_eq!(aa.device_id, GOLDEN_DEVICE);
        assert_eq!(aa.sign1, GOLDEN_SIGN1);
    }

    #[test]
    fn security_info_unidirection_golden() {
        let si = parse_security_info(
            "Unidirection cryptkey=\"QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWQ==\", algorithm=\"A:SM2;H:SM3\"",
        )
        .unwrap();
        assert_eq!(si.mode, Mode::Unidirection);
        assert_eq!(si.crypt_key, GOLDEN_CRYPT);
    }

    #[test]
    fn security_info_bidirection_golden() {
        let si = parse_security_info(
            "Bidirection algorithm=\"A:SM2;H:SM3;S:SM4/OFB/PKCS5,SM1/OFB/PKCS5;SI:SM3-SM2\",random1=\"PRAIIbutDbd5x/NKsbwwYw==\",random2=\"F4InuQewuMMqYPy1ItBdhQ==\",deviceid=\"34020000001320000001\",serverid=\"34020000002000000003\",cryptkey=\"MHkCIBHIiuBM7BulVNA9W1lwMzqDWFgmwqmF3lUg2ek0OJ77AiEAhLUtNE+yGqjqOKSUDIMyaSuNTaI5NUkhLq/cDxHKXJwEIHFMxhef2Mm87QjLenmuVKs1rGm7Ls3aMG+1zPp47365BBAnTJVAmqz9pBE2xKOXhpQF\",sign2=\"MEUCIQDzvGhJCuxmH/3NNtLNnrXIUOxYkYB7j8/3Th1LvjZHggIgD/nd9RbpEd6neZTuXDsIbNzydyS8WarbN1p6nHD5pHk=\"",
        )
        .unwrap();
        assert_eq!(si.mode, Mode::Bidirection);
        assert_eq!(si.device_id, GOLDEN_DEVICE);
        assert_eq!(si.server_id, "34020000002000000003");
        assert_eq!(si.sign2, GOLDEN_SIGN2);
    }

    #[test]
    fn security_info_roundtrip() {
        let si = SecurityInfo {
            mode: Mode::Bidirection,
            algorithm: CAPABILITY_ALGORITHM.to_string(),
            random1: GOLDEN_RANDOM1.to_string(),
            random2: GOLDEN_RANDOM2.to_string(),
            device_id: GOLDEN_DEVICE.to_string(),
            server_id: GOLDEN_SERVER.to_string(),
            crypt_key: GOLDEN_CRYPT.to_string(),
            sign2: GOLDEN_SIGN2.to_string(),
        };
        let built = build_security_info(&si);
        assert_eq!(
            built,
            "Bidirection algorithm=\"A:SM2;H:SM3;S:SM4/OFB/PKCS5;SI:SM3-SM2\",random1=\"PRAIIbutDbd5x/NKsbwwYw==\",random2=\"F4InuQewuMMqYPy1ItBdhQ==\",deviceid=\"34020000001320000001\",serverid=\"34020000002000000001\",cryptkey=\"QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWQ==\",sign2=\"MEUCIQDzvGhJCuxmH/3NNtLNnrXIUOxYkYB7j8/3Th1LvjZHggIgD/nd9RbpEd6neZTuXDsIbNzydyS8WarbN1p6nHD5pHk=\""
        );
        assert_eq!(parse_security_info(&built).unwrap(), si);
    }

    #[test]
    fn security_info_errors() {
        assert!(parse_security_info("Unidirection algorithm=\"A:SM2;H:SM3\"").is_err());
        assert!(parse_security_info("Bidirection sign2=\"MEUCIQ==\"").is_err());
    }
}
