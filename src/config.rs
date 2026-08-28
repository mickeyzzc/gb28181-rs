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
