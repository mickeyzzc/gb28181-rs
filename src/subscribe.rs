//! SUBSCRIBE/NOTIFY framework, device side (GB/T 28181-2016 §9.5 /
//! 2022, issue #57's subscription half).
//!
//! The platform SUBSCRIBEs to `Catalog`, `Alarm` or `MobilePosition`
//! events; the device answers 200 OK (echoing `Expires`) and keeps a
//! per-event subscription book. When something changes the device sends
//! a SIP **NOTIFY** carrying the matching `Notify` XML body on the
//! subscription dialog. Expiry and renewal are handled by re-UPSERT on
//! a fresh SUBSCRIBE; expired entries stop matching.
//!
//! Host seams: hold the [`DeviceNotifier`] (from
//! [`crate::server::Gb28181Server::device_notifier`]) and call
//! [`DeviceNotifier::send_alarm`] /
//! [`DeviceNotifier::send_catalog_change`] whenever the business side
//! has something to report; install a [`MobilePositionSource`] for the
//! periodic position task. Wire shapes (headers and XML field order)
//! mirror the Go twin's platform side (`platform/sip` subscribe.go /
//! handleNotify, landed with #341) so both twins interop byte-for-byte.

use crate::sip::{random_branch, SipMessage, SipMethod};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A subscription subject (SIP `Event` header / MANSCDP `CmdType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SubscribeEvent {
    Catalog,
    Alarm,
    MobilePosition,
}

impl SubscribeEvent {
    /// Parses the `Event` header value (or the SUBSCRIBE body CmdType).
    /// Unknown subjects yield `None` — the caller keeps answering 200 OK
    /// without bookkeeping (legacy-platform safe).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "Catalog" => Some(Self::Catalog),
            "Alarm" => Some(Self::Alarm),
            "MobilePosition" => Some(Self::MobilePosition),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Catalog => "Catalog",
            Self::Alarm => "Alarm",
            Self::MobilePosition => "MobilePosition",
        }
    }
}

/// One mobile-position report; all fields are wire-verbatim strings
/// (the standard carries them as text, and fixed cameras report
/// configured constants).
#[derive(Debug, Clone, PartialEq)]
pub struct PositionReport {
    pub time: String,
    pub longitude: String,
    pub latitude: String,
    pub speed: String,
    pub direction: String,
    pub altitude: String,
}

/// Host-provided position source for the periodic MobilePosition task.
/// Pulled on the report cadence; `None` skips that report.
pub trait MobilePositionSource: Send + Sync {
    fn current_position(&self) -> Option<PositionReport>;
}

struct ActiveSubscription {
    expires_at: Instant,
    peer: SocketAddr,
    /// The SUBSCRIBE's From (platform) — becomes the NOTIFY's To.
    from: String,
    /// The SUBSCRIBE's To (this device) — becomes the NOTIFY's From.
    to: String,
    call_id: String,
    next_cseq: u32,
}

/// Per-event subscription book. One subscription per event (the standard
/// model for a single-platform camera); a renewed SUBSCRIBE refreshes the
/// deadline and dialog snapshot.
#[derive(Default)]
pub struct SubscriptionRegistry {
    inner: Mutex<HashMap<SubscribeEvent, ActiveSubscription>>,
    sn: AtomicU32,
}

impl SubscriptionRegistry {
    // Dialog snapshot params mirror the SUBSCRIBE request fields.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn upsert(
        &self,
        event: SubscribeEvent,
        expires_secs: u64,
        peer: SocketAddr,
        from: String,
        to: String,
        call_id: String,
        cseq_base: u32,
    ) {
        let mut g = self.inner.lock().expect("subscription lock");
        g.insert(
            event,
            ActiveSubscription {
                expires_at: Instant::now() + Duration::from_secs(expires_secs.max(1)),
                peer,
                from,
                to,
                call_id,
                next_cseq: cseq_base.wrapping_add(1),
            },
        );
    }

    /// The live subscription for `event`, or `None` when absent/expired
    /// (expired entries are dropped on read).
    fn active(&self, event: SubscribeEvent) -> Option<(SocketAddr, String, String, String, u32)> {
        let mut g = self.inner.lock().expect("subscription lock");
        let sub = g.get_mut(&event)?;
        if sub.expires_at <= Instant::now() {
            g.remove(&event);
            return None;
        }
        let cseq = sub.next_cseq;
        sub.next_cseq = sub.next_cseq.wrapping_add(1);
        Some((
            sub.peer,
            sub.from.clone(),
            sub.to.clone(),
            sub.call_id.clone(),
            cseq,
        ))
    }

    fn is_active(&self, event: SubscribeEvent) -> bool {
        let mut g = self.inner.lock().expect("subscription lock");
        match g.get(&event) {
            Some(sub) if sub.expires_at > Instant::now() => true,
            Some(_) => {
                g.remove(&event);
                false
            }
            None => false,
        }
    }

    fn next_sn(&self) -> u32 {
        self.sn.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Extracts the MobilePosition report cadence (`<Interval>` seconds)
/// from a SUBSCRIBE body; `None` when absent/unparseable (callers
/// default to 5s, matching the Go platform's request cadence).
#[must_use]
pub fn parse_subscribe_interval(body: &str) -> Option<u64> {
    let start = body.find("<Interval>")? + "<Interval>".len();
    let end = body[start..].find("</Interval>")? + start;
    body[start..end].trim().parse().ok()
}

/// Host-facing notifier: sends NOTIFYs for subscribed events. No-op
/// (returns `false`) when the event has no live subscription — hosts
/// never need to check first.
pub struct DeviceNotifier {
    registry: SubscriptionRegistry,
    sip: OnceLock<ArcUdp>,
    device_id: OnceLock<String>,
    domain: OnceLock<String>,
    local_ip: OnceLock<String>,
    local_port: OnceLock<u16>,
}

/// `Arc<UdpSocket>` alias kept short for the OnceLock fields. A std
/// (blocking) socket: NOTIFY sends fire from host threads that may not
/// run in a tokio context, where tokio's `try_send_to` fails with
/// WouldBlock. Local UDP `send_to` returns immediately.
type ArcUdp = std::sync::Arc<UdpSocket>;

impl Default for DeviceNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceNotifier {
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: SubscriptionRegistry::default(),
            sip: OnceLock::new(),
            device_id: OnceLock::new(),
            domain: OnceLock::new(),
            local_ip: OnceLock::new(),
            local_port: OnceLock::new(),
        }
    }

    /// Binds the sending context; called once from the server's UDP run
    /// loop before it starts answering SUBSCRIBEs.
    pub(crate) fn bind(
        &self,
        sip: ArcUdp,
        device_id: String,
        domain: String,
        local_ip: String,
        local_port: u16,
    ) {
        let _ = self.sip.set(sip);
        let _ = self.device_id.set(device_id);
        let _ = self.domain.set(domain);
        let _ = self.local_ip.set(local_ip);
        let _ = self.local_port.set(local_port);
    }

    pub(crate) fn registry(&self) -> &SubscriptionRegistry {
        &self.registry
    }

    /// Whether the platform currently holds a live subscription to
    /// `event` (handy for UI/metrics; the senders already no-op safely).
    #[must_use]
    pub fn subscribed(&self, event: SubscribeEvent) -> bool {
        self.registry.is_active(event)
    }

    /// Sends an alarm NOTIFY (§9.5.2). `priority` "1"-"4" (severity),
    /// `method` per A.2.6.1 (e.g. "5" motion), `time` GB28181 timestamp,
    /// `alarm_type` the 2022 classification code (optional, may be "").
    pub fn send_alarm(
        &self,
        priority: &str,
        method: &str,
        time: &str,
        alarm_type: &str,
        description: &str,
    ) -> bool {
        let sn = self.registry.next_sn();
        let device_id = self.device_id.get().cloned().unwrap_or_default();
        let mut body = format!(
            "<Notify><CmdType>Alarm</CmdType><SN>{sn}</SN><DeviceID>{device_id}</DeviceID>\
             <AlarmPriority>{priority}</AlarmPriority><AlarmMethod>{method}</AlarmMethod>\
             <AlarmTime>{time}</AlarmTime><AlarmDescription>{desc}</AlarmDescription>",
            priority = crate::client::xml_escape(priority),
            method = crate::client::xml_escape(method),
            time = crate::client::xml_escape(time),
            desc = crate::client::xml_escape(description),
        );
        if !alarm_type.is_empty() {
            body.push_str(&format!(
                "<AlarmType>{}</AlarmType>",
                crate::client::xml_escape(alarm_type)
            ));
        }
        body.push_str("</Notify>");
        self.send_notify(SubscribeEvent::Alarm, body, "active")
    }

    /// Sends a mobile-position NOTIFY (§9.5.3).
    pub fn send_mobile_position(&self, p: &PositionReport) -> bool {
        let sn = self.registry.next_sn();
        let device_id = self.device_id.get().cloned().unwrap_or_default();
        let body = format!(
            "<Notify><CmdType>MobilePosition</CmdType><SN>{sn}</SN><DeviceID>{device_id}</DeviceID>\
             <Time>{time}</Time><Longitude>{lon}</Longitude><Latitude>{lat}</Latitude>\
             <Speed>{speed}</Speed><Direction>{dir}</Direction><Altitude>{alt}</Altitude></Notify>",
            time = crate::client::xml_escape(&p.time),
            lon = crate::client::xml_escape(&p.longitude),
            lat = crate::client::xml_escape(&p.latitude),
            speed = crate::client::xml_escape(&p.speed),
            dir = crate::client::xml_escape(&p.direction),
            alt = crate::client::xml_escape(&p.altitude),
        );
        self.send_notify(SubscribeEvent::MobilePosition, body, "active")
    }

    /// Sends a catalog-change NOTIFY (§9.5.1) with the changed channels.
    pub fn send_catalog_change(&self, items: &[crate::manscdp::ChannelItem]) -> bool {
        let sn = self.registry.next_sn();
        let device_id = self.device_id.get().cloned().unwrap_or_default();
        let items_xml = items
            .iter()
            .map(|it| {
                format!(
                    "<Item><DeviceID>{}</DeviceID><Name>{}</Name><Manufacturer>{}</Manufacturer>\
                     <Model>{}</Model><Owner>{}</Owner><CivilCode>{}</CivilCode><Address>{}</Address>\
                     <Parental>{}</Parental><ParentID>{}</ParentID><SafetyWay>{}</SafetyWay>\
                     <RegisterWay>{}</RegisterWay><Secrecy>{}</Secrecy><Status>{}</Status>\
                     <IPAddress>{}</IPAddress><Port>{}</Port><Longitude>{}</Longitude><Latitude>{}</Latitude></Item>",
                    crate::client::xml_escape(&it.device_id),
                    crate::client::xml_escape(&it.name),
                    crate::client::xml_escape(&it.manufacturer),
                    crate::client::xml_escape(&it.model),
                    crate::client::xml_escape(&it.owner),
                    crate::client::xml_escape(&it.civil_code),
                    crate::client::xml_escape(&it.address),
                    it.parental,
                    crate::client::xml_escape(&it.parent_id),
                    it.safety_way,
                    it.register_way,
                    it.secrecy,
                    crate::client::xml_escape(&it.status),
                    crate::client::xml_escape(&it.ip_address),
                    it.port,
                    it.longitude,
                    it.latitude,
                )
            })
            .collect::<String>();
        let body = format!(
            "<Notify><CmdType>Catalog</CmdType><SN>{sn}</SN><DeviceID>{device_id}</DeviceID>\
             <SumNum>{n}</SumNum><DeviceList Num=\"{n}\">{items_xml}</DeviceList></Notify>",
            n = items.len(),
        );
        self.send_notify(SubscribeEvent::Catalog, body, "active")
    }

    /// Sends one NOTIFY on the subscription dialog. Direction mirrors the
    /// SUBSCRIBE: our From is the device, To is the platform subscriber.
    fn send_notify(&self, event: SubscribeEvent, body: String, state: &str) -> bool {
        let Some((peer, sub_from, sub_to, call_id, cseq)) = self.registry.active(event) else {
            return false;
        };
        let Some(socket) = self.sip.get() else {
            return false;
        };
        let (Some(device_id), Some(domain), Some(local_ip), Some(local_port)) = (
            self.device_id.get(),
            self.domain.get(),
            self.local_ip.get(),
            self.local_port.get(),
        ) else {
            return false;
        };

        let mut headers = Vec::new();
        headers.push((
            "Via".to_string(),
            format!(
                "SIP/2.0/UDP {local_ip}:{local_port};rport;branch={}",
                random_branch()
            ),
        ));
        // NOTIFY direction: From = the device (the SUBSCRIBE's To), To =
        // the platform subscriber (the SUBSCRIBE's From).
        headers.push(("From".to_string(), sub_to));
        headers.push(("To".to_string(), sub_from));
        headers.push(("Call-ID".to_string(), call_id));
        headers.push(("CSeq".to_string(), format!("{cseq} NOTIFY")));
        headers.push(("Max-Forwards".to_string(), "70".to_string()));
        headers.push(("Event".to_string(), event.as_str().to_string()));
        headers.push((
            "Subscription-State".to_string(),
            format!("{state};expires=3600"),
        ));
        headers.push((
            "Content-Type".to_string(),
            "Application/MANSCDP+xml".to_string(),
        ));
        headers.push(("Content-Length".to_string(), body.len().to_string()));

        let msg = SipMessage {
            start_line: format!("NOTIFY sip:{device_id}@{domain} SIP/2.0"),
            method: Some(SipMethod::Notify),
            status_code: None,
            uri: Some(format!("sip:{device_id}@{domain}")),
            version: "SIP/2.0".to_string(),
            headers,
            body,
        };
        let data = crate::server::serialize_wire(&msg);
        match socket.send_to(&data, peer) {
            Ok(_) => true,
            Err(e) => {
                log::warn!("gb28181: {event:?} NOTIFY send failed: {e}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound_notifier() -> (
        DeviceNotifier,
        std::sync::Arc<UdpSocket>,
        UdpSocket,
        SocketAddr,
    ) {
        let sip = std::sync::Arc::new(UdpSocket::bind("127.0.0.1:0").expect("sip bind"));
        let platform = UdpSocket::bind("127.0.0.1:0").expect("platform bind");
        let peer = platform.local_addr().expect("platform addr");
        let n = DeviceNotifier::new();
        n.bind(
            sip.clone(),
            "34020000001320000001".to_string(),
            "3402000000".to_string(),
            "127.0.0.1".to_string(),
            5060,
        );
        (n, sip, platform, peer)
    }

    fn book(n: &DeviceNotifier, event: SubscribeEvent, expires: u64, peer: SocketAddr) {
        n.registry().upsert(
            event,
            expires,
            peer,
            "<sip:34020000002000000001@3402000000>;tag=plat".to_string(),
            "<sip:34020000001320000001@3402000000>".to_string(),
            "sub-1@127.0.0.1".to_string(),
            1,
        );
    }

    fn recv_text(platform: &UdpSocket) -> String {
        platform
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("timeout");
        let mut buf = [0u8; 4096];
        let (n, _) = platform.recv_from(&mut buf).expect("recv NOTIFY");
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    #[test]
    fn alarm_notify_golden() {
        let (n, _sip, platform, peer) = bound_notifier();
        assert!(!n.send_alarm("2", "5", "2026-09-15T12:00:00", "", "motion"));
        book(&n, SubscribeEvent::Alarm, 3600, peer);
        assert!(n.subscribed(SubscribeEvent::Alarm));
        assert!(n.send_alarm("2", "5", "2026-09-15T12:00:00", "1", "motion"));
        let text = recv_text(&platform);
        assert!(
            text.starts_with("NOTIFY sip:34020000001320000001@3402000000 SIP/2.0\r\n"),
            "{text}"
        );
        assert!(text.contains("Event: Alarm\r\n"), "{text}");
        assert!(text.contains("Subscription-State: active"), "{text}");
        assert!(text.contains("CSeq: 2 NOTIFY\r\n"), "{text}");
        // §9.5.2 body, field order mirrors the Go twin's manscdp.Alarm.
        assert!(
            text.ends_with(
                "<Notify><CmdType>Alarm</CmdType><SN>2</SN>\
<DeviceID>34020000001320000001</DeviceID>\
<AlarmPriority>2</AlarmPriority><AlarmMethod>5</AlarmMethod>\
<AlarmTime>2026-09-15T12:00:00</AlarmTime>\
<AlarmDescription>motion</AlarmDescription>\
<AlarmType>1</AlarmType></Notify>"
            ),
            "{text}"
        );
    }

    #[test]
    fn mobile_position_notify_golden() {
        let (n, _sip, platform, peer) = bound_notifier();
        book(&n, SubscribeEvent::MobilePosition, 3600, peer);
        assert!(n.send_mobile_position(&PositionReport {
            time: "2026-09-15T12:00:01".to_string(),
            longitude: "116.40".to_string(),
            latitude: "39.90".to_string(),
            speed: "0.0".to_string(),
            direction: "0.0".to_string(),
            altitude: "50.0".to_string(),
        }));
        let text = recv_text(&platform);
        assert!(text.contains("Event: MobilePosition\r\n"), "{text}");
        assert!(
            text.ends_with(
                "<Notify><CmdType>MobilePosition</CmdType><SN>1</SN>\
<DeviceID>34020000001320000001</DeviceID>\
<Time>2026-09-15T12:00:01</Time>\
<Longitude>116.40</Longitude><Latitude>39.90</Latitude>\
<Speed>0.0</Speed><Direction>0.0</Direction>\
<Altitude>50.0</Altitude></Notify>"
            ),
            "{text}"
        );
    }

    #[test]
    fn catalog_change_notify_golden() {
        let (n, _sip, platform, peer) = bound_notifier();
        book(&n, SubscribeEvent::Catalog, 3600, peer);
        let item = crate::manscdp::ChannelItem {
            device_id: "34020000001320000001".to_string(),
            name: "cam".to_string(),
            manufacturer: "MiBee".to_string(),
            model: "X".to_string(),
            owner: "3402000000".to_string(),
            civil_code: "3402000000".to_string(),
            address: "127.0.0.1".to_string(),
            parental: 0,
            parent_id: "34020000002000000001".to_string(),
            safety_way: 0,
            register_way: 1,
            secrecy: 0,
            status: "ON".to_string(),
            ip_address: "127.0.0.1".to_string(),
            port: 5060,
            longitude: 0.0,
            latitude: 0.0,
        };
        assert!(n.send_catalog_change(&[item]));
        let text = recv_text(&platform);
        assert!(text.contains("Event: Catalog\r\n"), "{text}");
        assert!(
            text.contains("<SumNum>1</SumNum><DeviceList Num=\"1\">"),
            "{text}"
        );
        assert!(
            text.contains("<DeviceID>34020000001320000001</DeviceID><Name>cam</Name>"),
            "{text}"
        );
    }

    #[test]
    fn subscription_expires_and_renews() {
        let (n, _sip, _platform, peer) = bound_notifier();
        book(&n, SubscribeEvent::Alarm, 1, peer);
        assert!(n.subscribed(SubscribeEvent::Alarm));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(!n.subscribed(SubscribeEvent::Alarm));
        // Renewal re-books with a fresh deadline.
        book(&n, SubscribeEvent::Alarm, 3600, peer);
        assert!(n.subscribed(SubscribeEvent::Alarm));
    }

    #[test]
    fn subscribe_interval_parsing() {
        assert_eq!(
            parse_subscribe_interval(
                "<SUBSCRIBE><CmdType>MobilePosition</CmdType><SN>1</SN>\
                 <DeviceID>d</DeviceID><Interval>3</Interval></SUBSCRIBE>"
            ),
            Some(3)
        );
        assert_eq!(parse_subscribe_interval(""), None);
        assert_eq!(parse_subscribe_interval("<Interval>x</Interval>"), None);
    }

    #[test]
    fn cseq_advances_per_notify() {
        let (n, _sip, platform, peer) = bound_notifier();
        book(&n, SubscribeEvent::Alarm, 3600, peer);
        assert!(n.send_alarm("1", "5", "t", "", "a"));
        let t1 = recv_text(&platform);
        assert!(t1.contains("CSeq: 2 NOTIFY"), "{t1}");
        assert!(n.send_alarm("1", "5", "t", "", "b"));
        let t2 = recv_text(&platform);
        assert!(t2.contains("CSeq: 3 NOTIFY"), "{t2}");
    }
}
