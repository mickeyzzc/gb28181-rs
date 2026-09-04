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
    #[serde(default)]
    pub transport: Transport,
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

    /// Log a warning when a running server carries config values that
    /// cannot work or that a mis-loaded host config would otherwise
    /// silently use: an empty SIP password (digest auth cannot succeed),
    /// a password set to the well-known spec-example value, the example
    /// `192.168.1.1` platform address, or the spec-example device ID.
    pub fn warn_on_example_defaults(&self) {
        if self.platform_sip_address == default_gb28181_platform_sip_address() {
            log::warn!(
                "gb28181: platform_sip_address is the example default {} — set it explicitly in the host config",
                default_gb28181_platform_sip_address()
            );
        }
        if self.password.is_empty() {
            log::warn!(
                "gb28181: password is empty — digest auth cannot succeed; set it explicitly in the host config"
            );
        } else if self.password == "12345678" {
            log::warn!(
                "gb28181: password is the well-known spec-example value {:?} — change it in the host config",
                self.password
            );
        }
        if self.device_id == default_gb28181_device_id() {
            log::warn!(
                "gb28181: device_id is the spec-example ID {} — two devices with it collide on the platform",
                default_gb28181_device_id()
            );
        }
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
    String::new()
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
            transport: Transport::default(),
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
        assert!(matches!(d.transport, Transport::Udp));
        assert_eq!(d.user_agent, None);
        assert_eq!(d.device_name, None);
        assert_eq!(d.manufacturer, None);
        assert_eq!(d.model, None);
        assert_eq!(d.firmware, None);
    }

    /// The SIP password must NOT default to a credential: an unset
    /// password stays empty so a mis-loaded host config cannot silently
    /// authenticate with a value that is publicly documented somewhere.
    #[test]
    fn password_default_is_empty() {
        let d = Gb28181Config::default();
        assert!(d.password.is_empty(), "Default password must be empty");
        let s: Gb28181Config = toml::from_str("").expect("empty config deserializes");
        assert!(
            s.password.is_empty(),
            "serde default password must be empty"
        );
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
