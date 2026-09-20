//! Property tests for the parsers facing untrusted network input (#29):
//! no panic on any byte sequence, and parse↔serialize round-trips for
//! well-formed messages.

use proptest::prelude::*;

use gb28181_rs::charset::{decode_wire_body, encode_wire_body};
use gb28181_rs::manscdp::{
    parse_control_snapshot, parse_device_config, parse_device_control, parse_ptz_command,
    PtzCommand,
};
use gb28181_rs::sip::{build_register_request, parse_digest_auth, parse_sip_date, SipMessage};

proptest! {
    /// Arbitrary bytes (including invalid UTF-8) never panic the SIP
    /// parser — malformed input surfaces as an error, nothing else.
    #[test]
    fn sip_parse_never_panics_on_arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..8192)) {
        let lossy = String::from_utf8_lossy(&data).into_owned();
        let _ = SipMessage::parse(&lossy);
    }

    /// Header-shaped garbage: arbitrary lines with SIP-ish punctuation.
    #[test]
    fn sip_parse_never_panics_on_ascii_lines(lines in proptest::collection::vec("[A-Za-z:@;=<>0-9.\"]{0,64}", 0..32)) {
        let msg = lines.join("\r\n");
        let _ = SipMessage::parse(&msg);
    }

    /// A REGISTER built by this crate parses back and re-serializes to the
    /// identical wire form.
    #[test]
    fn register_roundtrip_stable(call_id in "[a-zA-Z0-9@.-]{1,32}", cseq in 1u32..u32::MAX, tag in "[a-zA-Z0-9]{1,16}") {
        let msg = build_register_request(
            "34020000001320000001", "192.168.1.30", 5060,
            "34020000002000000001", "3402000000", 3600,
            None, &call_id, cseq, &tag, "prop-test",
        );
        let wire = msg.serialize();
        let back = SipMessage::parse(&wire).expect("own serialization must parse");
        prop_assert_eq!(back.serialize(), wire);
    }

    /// GB18030/UTF-8 decoding never panics on arbitrary bytes, and ASCII
    /// always round-trips byte-identically through encode/decode.
    #[test]
    fn charset_decode_never_panics_and_ascii_roundtrips(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let _ = decode_wire_body(&data);
    }
    #[test]
    fn charset_ascii_roundtrip(text in proptest::collection::vec(any::<char>().prop_map(|c| if c.is_ascii() { c } else { 'x' }), 0..256)) {
        let s: String = text.into_iter().collect();
        let encoded = encode_wire_body(&s);
        prop_assert_eq!(decode_wire_body(&encoded), s);
    }

    /// The MANSCDP XML parsers (device control / config / snapshot, the
    /// DeviceControl family surfaces platforms push at the device) never
    /// panic on arbitrary bytes — garbage surfaces as None, nothing else.
    #[test]
    fn manscdp_parse_never_panics(data in proptest::collection::vec(any::<u8>(), 0..8192)) {
        let lossy = String::from_utf8_lossy(&data).into_owned();
        let _ = parse_device_control(&lossy);
        let _ = parse_device_config(&lossy);
        let _ = parse_control_snapshot(&lossy);
    }

    /// PTZ decode is byte-faithful in both directions: arbitrary input
    /// yields `Invalid` with the trimmed input preserved, and every
    /// 8-byte A5 command with a matching checksum must decode to a
    /// non-`Invalid` variant.
    #[test]
    fn ptz_command_invalid_preserves_raw(data in proptest::collection::vec(any::<u8>(), 0..64)) {
        let lossy = String::from_utf8_lossy(&data).into_owned();
        if let PtzCommand::Invalid { raw } = parse_ptz_command(&lossy) {
            prop_assert_eq!(raw, lossy.trim());
        }
    }
    #[test]
    fn ptz_command_valid_always_decodes(bytes in any::<[u8; 8]>()) {
        let mut raw = bytes;
        raw[0] = 0xA5;
        raw[7] = raw[..7].iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        prop_assert!( !matches!(parse_ptz_command(&hex), PtzCommand::Invalid { .. }),
            "valid A5 command with matching checksum must not decode as Invalid: {hex}" );
    }

    /// Digest-auth challenge parsing and SIP Date parsing never panic on
    /// arbitrary strings — failure is an Err/None, nothing else.
    #[test]
    fn digest_auth_and_sip_date_never_panics(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        let lossy = String::from_utf8_lossy(&data).into_owned();
        let _ = parse_digest_auth(&lossy);
        let _ = parse_sip_date(&lossy);
    }
}
