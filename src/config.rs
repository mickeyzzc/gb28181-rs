//! GB28181 device configuration (serde-friendly).
//!
//! Mirrors the configuration surface the device server consumes; TOML/JSON
//! shapes are identical to the `mibee-eye-raspi-rs` `[gb28181]` section, so
//! hosts can re-export [`Gb28181Config`] directly into their own config
//! structs without changing config files.

use serde::{Deserialize, Serialize};

/// GB28181 SIP transport protocol.
///
/// Fieldless enum: serde rejects unknown variants natively, so an invalid
/// config value like `transport = "sctp"` is a parse error.
///
/// Note: [`crate::sip::Transport`] is a separate, protocol-facing enum with
/// a `Display` impl; this one exists for serde configuration parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Udp,
    Tcp,
}

/// GB28181 SIP device settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gb28181Config {
    /// Host-side enable switch. The library itself NEVER reads this field —
    /// the host gates `Gb28181Server::start()` on it. Keeping it here lets
    /// hosts re-export [`Gb28181Config`] straight into their config files.
    #[serde(default = "default_gb28181_enabled")]
    pub enabled: bool,
    #[serde(default = "default_gb28181_platform_sip_address")]
    pub platform_sip_address: String,
    #[serde(default = "default_gb28181_platform_sip_port")]
    pub platform_sip_port: u16,
    #[serde(default = "default_gb28181_device_id")]
    pub device_id: String,
    #[serde(default = "default_gb28181_channel_id")]
    pub channel_id: String,
    #[serde(default = "default_gb28181_sip_domain")]
    pub sip_domain: String,
    #[serde(default = "default_gb28181_password")]
    pub password: String,
    #[serde(default = "default_gb28181_local_sip_port")]
    pub local_sip_port: u16,
    #[serde(default = "default_gb28181_register_interval_secs")]
    pub register_interval_secs: u64,
    #[serde(default = "default_gb28181_heartbeat_interval_secs")]
    pub heartbeat_interval_secs: u64,
    #[serde(default = "default_gb28181_heartbeat_timeout_count")]
    pub heartbeat_timeout_count: u32,
    /// Strict mode (issue #32): when true, spec-example defaults that
    /// would otherwise only log a warning refuse to start instead.
    /// Host products opt in; labs may keep false (the default).
    #[serde(default)]
    pub strict_example_defaults: bool,
    #[serde(default)]
    pub transport: Transport,
    /// Device-side failure behavior for incoming Note verification on
    /// platform→device requests (GB 35114 §9.4; issue #41). Only active
    /// when a `RegisterAuthenticator` overrides `verify_incoming_note`
    /// (the security35114 A-level reference implementation does).
    /// Default: reject with 403.
    #[serde(default)]
    pub incoming_note_policy: crate::authenticator::IncomingNotePolicy,
    /// SIP `User-Agent` header value. `None` → neutral
    /// `gb28181-rs/<version>` (never a product name).
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Catalog/DeviceInfo `Name`. `None` → `Camera <device_id>`.
    #[serde(default)]
    pub device_name: Option<String>,
    /// Catalog/DeviceInfo `Manufacturer`. `None` → `Unknown`.
    #[serde(default)]
    pub manufacturer: Option<String>,
    /// Catalog/DeviceInfo `Model`. `None` → `Unknown`.
    #[serde(default)]
    pub model: Option<String>,
    /// DeviceInfo `Firmware` version string. `None` → the crate version.
    #[serde(default)]
    pub firmware: Option<String>,
}

impl Gb28181Config {
    /// Effective SIP User-Agent (config override or the neutral default).
    #[must_use]
    pub fn effective_user_agent(&self) -> String {
        self.user_agent
            .clone()
            .unwrap_or_else(|| format!("gb28181-rs/{}", env!("CARGO_PKG_VERSION")))
    }

    /// Effective catalog/device display name.
    #[must_use]
    pub fn effective_device_name(&self) -> String {
        self.device_name
            .clone()
            .unwrap_or_else(|| format!("Camera {}", self.device_id))
    }

    /// Effective manufacturer string.
    #[must_use]
    pub fn effective_manufacturer(&self) -> String {
        self.manufacturer
            .clone()
            .unwrap_or_else(|| "Unknown".to_string())
    }

    /// Effective model string.
    #[must_use]
    pub fn effective_model(&self) -> String {
        self.model.clone().unwrap_or_else(|| "Unknown".to_string())
    }

    /// Effective firmware string.
    #[must_use]
    pub fn effective_firmware(&self) -> String {
        self.firmware
            .clone()
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
    }

    /// Check the spec-example defaults that a mis-loaded host config
    /// would otherwise silently carry into production: platform
    /// `192.168.1.1`, password `12345678`, and the spec-example device
    /// ID (two devices sharing it collide on the platform).
    ///
    /// With `strict_example_defaults = false` (the default) each finding
    /// logs a warning — the historical behavior. With strict mode on,
    /// every finding is refused with an error naming the fields, so a
    /// misconfigured product build fails fast at startup (issue #32).
    ///
    /// # Errors
    /// Strict mode + at least one example default still in effect.
    pub fn check_example_defaults(&self) -> Result<(), String> {
        let mut offending: Vec<&str> = Vec::new();
        if self.platform_sip_address == default_gb28181_platform_sip_address() {
            offending.push("platform_sip_address");
        }
        if self.password == default_gb28181_password() {
            offending.push("password");
        }
        if self.device_id == default_gb28181_device_id() {
            offending.push("device_id");
        }
        if offending.is_empty() {
            return Ok(());
        }
        if self.strict_example_defaults {
            return Err(format!(
                "gb28181: strict_example_defaults: spec-example values still in effect for {} — set real values in the host config",
                offending.join(", ")
            ));
        }
        if offending.contains(&"platform_sip_address") {
            log::warn!(
                "gb28181: platform_sip_address is the example default {} — set it explicitly in the host config",
                default_gb28181_platform_sip_address()
            );
        }
        if offending.contains(&"password") {
            log::warn!(
                "gb28181: password is the example default {:?} — set it explicitly in the host config",
                default_gb28181_password()
            );
        }
        if offending.contains(&"device_id") {
            log::warn!(
                "gb28181: device_id is the spec-example ID {} — two devices with it collide on the platform",
                default_gb28181_device_id()
            );
        }
        Ok(())
    }
}

fn default_gb28181_enabled() -> bool {
    false
}
fn default_gb28181_platform_sip_address() -> String {
    "192.168.1.1".to_string()
}
fn default_gb28181_platform_sip_port() -> u16 {
    5060
}
fn default_gb28181_device_id() -> String {
    "34020000001320000001".to_string()
}
fn default_gb28181_channel_id() -> String {
    "34020000001320000001".to_string()
}
fn default_gb28181_sip_domain() -> String {
    "3402000000".to_string()
}
fn default_gb28181_password() -> String {
    "12345678".to_string()
}
fn default_gb28181_local_sip_port() -> u16 {
    5060
}
fn default_gb28181_register_interval_secs() -> u64 {
    60
}
fn default_gb28181_heartbeat_interval_secs() -> u64 {
    60
}
fn default_gb28181_heartbeat_timeout_count() -> u32 {
    3
}

impl Default for Gb28181Config {
    fn default() -> Self {
        Self {
            enabled: default_gb28181_enabled(),
            platform_sip_address: default_gb28181_platform_sip_address(),
            platform_sip_port: default_gb28181_platform_sip_port(),
            device_id: default_gb28181_device_id(),
            channel_id: default_gb28181_channel_id(),
            sip_domain: default_gb28181_sip_domain(),
            password: default_gb28181_password(),
            local_sip_port: default_gb28181_local_sip_port(),
            register_interval_secs: default_gb28181_register_interval_secs(),
            heartbeat_interval_secs: default_gb28181_heartbeat_interval_secs(),
            heartbeat_timeout_count: default_gb28181_heartbeat_timeout_count(),
            strict_example_defaults: false,
            transport: Transport::default(),
            incoming_note_policy: crate::authenticator::IncomingNotePolicy::default(),
            user_agent: None,
            device_name: None,
            manufacturer: None,
            model: None,
            firmware: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #32: strict mode turns the example-default warnings into a
    /// startup refusal; non-strict keeps the historical warn-only path.
    #[test]
    fn strict_mode_check_example_defaults() {
        // Defaults + strict → Err naming every offending field.
        let cfg = Gb28181Config {
            strict_example_defaults: true,
            ..Gb28181Config::default()
        };
        let err = cfg.check_example_defaults().unwrap_err();
        for field in ["platform_sip_address", "password", "device_id"] {
            assert!(err.contains(field), "must name {field}: {err}");
        }

        // Defaults + non-strict → Ok (warn-only, historical behavior).
        assert!(Gb28181Config::default().check_example_defaults().is_ok());

        // Real values + strict → Ok.
        let cfg = Gb28181Config {
            strict_example_defaults: true,
            platform_sip_address: "10.0.0.5".to_string(),
            password: "real".to_string(),
            device_id: "34020000001320000042".to_string(),
            ..Gb28181Config::default()
        };
        assert!(cfg.check_example_defaults().is_ok());
    }

    /// `Default` must stay in lockstep with the serde defaults so hosts that
    /// construct `Gb28181Config::default()` and hosts that deserialize an
    /// empty `[gb28181]` section see the same values.
    #[test]
    fn default_matches_serde_defaults() {
        let d = Gb28181Config::default();
        let s: Gb28181Config = toml::from_str("").expect("empty config deserializes");
        assert_eq!(d.enabled, s.enabled);
        assert_eq!(d.platform_sip_address, s.platform_sip_address);
        assert_eq!(d.platform_sip_port, s.platform_sip_port);
        assert_eq!(d.device_id, s.device_id);
        assert_eq!(d.channel_id, s.channel_id);
        assert_eq!(d.sip_domain, s.sip_domain);
        assert_eq!(d.password, s.password);
        assert_eq!(d.local_sip_port, s.local_sip_port);
        assert_eq!(d.register_interval_secs, s.register_interval_secs);
        assert_eq!(d.heartbeat_interval_secs, s.heartbeat_interval_secs);
        assert_eq!(d.heartbeat_timeout_count, s.heartbeat_timeout_count);
        assert_eq!(d.strict_example_defaults, s.strict_example_defaults);
        assert_eq!(d.incoming_note_policy, s.incoming_note_policy);
        assert!(
            !d.strict_example_defaults,
            "strict mode must default to warn-only"
        );
        assert!(matches!(d.transport, Transport::Udp));
        assert_eq!(d.user_agent, None);
        assert_eq!(d.device_name, None);
        assert_eq!(d.manufacturer, None);
        assert_eq!(d.model, None);
        assert_eq!(d.firmware, None);
    }

    /// Identity defaults are neutral (no product/vendor branding) and
    /// overridable — the core library-neutrality contract.
    #[test]
    fn identity_defaults_are_neutral_and_overridable() {
        let cfg = Gb28181Config::default();
        assert!(cfg.effective_user_agent().starts_with("gb28181-rs/"));
        assert!(!cfg.effective_user_agent().to_lowercase().contains("mibee"));
        assert_eq!(
            cfg.effective_device_name(),
            format!("Camera {}", cfg.device_id)
        );
        assert!(!cfg.effective_device_name().contains("MiBee"));
        assert_eq!(cfg.effective_manufacturer(), "Unknown");
        assert_eq!(cfg.effective_model(), "Unknown");
        assert_eq!(cfg.effective_firmware(), env!("CARGO_PKG_VERSION"));

        let cfg = Gb28181Config {
            user_agent: Some("host/1.0".to_string()),
            device_name: Some("前门摄像头".to_string()),
            manufacturer: Some("Acme".to_string()),
            model: Some("Cam-X".to_string()),
            firmware: Some("9.9.9".to_string()),
            ..Gb28181Config::default()
        };
        assert_eq!(cfg.effective_user_agent(), "host/1.0");
        assert_eq!(cfg.effective_device_name(), "前门摄像头");
        assert_eq!(cfg.effective_manufacturer(), "Acme");
        assert_eq!(cfg.effective_model(), "Cam-X");
        assert_eq!(cfg.effective_firmware(), "9.9.9");
    }

    /// serde round-trip of the optional identity overrides.
    #[test]
    fn identity_overrides_survive_serde() {
        let toml_src = r#"
platform_sip_address = "10.0.0.5"
device_id = "34020000001320000099"
user_agent = "host/2.0"
device_name = "Gate Camera"
manufacturer = "Acme"
model = "Cam-Y"
firmware = "1.2.3"
"#;
        let cfg: Gb28181Config = toml::from_str(toml_src).expect("parse");
        assert_eq!(cfg.effective_user_agent(), "host/2.0");
        assert_eq!(cfg.effective_device_name(), "Gate Camera");
        assert_eq!(cfg.effective_manufacturer(), "Acme");
        assert_eq!(cfg.effective_model(), "Cam-Y");
        assert_eq!(cfg.effective_firmware(), "1.2.3");
    }
}
