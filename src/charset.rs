//! Wire-charset handling for SIP/MANSCDP bodies.
//!
//! GB28181 platforms in the field send MANSCDP bodies in UTF-8 **or** in a
//! legacy Chinese charset (GB2312/GBK/GB18030 — the XML declaration in the
//! wild says `encoding="GB2312"` even when the bytes are GBK/GB18030).
//! Conversely, this library historically emitted bodies that *declared*
//! GB2312 while sending UTF-8 bytes, which only worked because the content
//! was pure ASCII.
//!
//! The rules implemented here:
//!
//! - **Inbound** ([`decode_wire_body`]): strict UTF-8 first; if the bytes are
//!   not valid UTF-8, decode as GB18030 (a superset of GBK and GB2312), with
//!   U+FFFD replacement for undecodable sequences. Undecodable-as-both input
//!   still yields a lossy string instead of dropping the datagram.
//! - **Outbound** ([`encode_wire_body`]): pure-ASCII bodies are byte-identical
//!   in every charset (the historical wire bytes, goldens preserved). A body
//!   whose XML declaration says GB2312 and which contains non-ASCII is
//!   actually encoded as GB18030 so the declaration matches the bytes; a body
//!   declaring UTF-8 (or no declaration) is sent as UTF-8.
//!
//! Both directions are lossless for content representable in the target
//! charset; characters outside GB18030's 2-byte range are emitted as 4-byte
//! GB18030 sequences by the encoder.

/// Decode an inbound SIP/MANSCDP body from wire bytes to a Rust string.
///
/// Valid UTF-8 passes through unchanged. Invalid UTF-8 is treated as
/// GB18030 (superset of GBK/GB2312) and decoded with replacement — a
/// datagram is never dropped just because a platform used a legacy charset.
#[must_use]
pub fn decode_wire_body(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (cow, _encoding_used, _had_errors) = encoding_rs::GB18030.decode(bytes);
    cow.into_owned()
}

/// Encode an outbound body for the wire.
///
/// - Pure-ASCII body → identical bytes (charset-independent; preserves the
///   historical/golden wire format).
/// - Non-ASCII body declaring `encoding="GB2312"` → GB18030 bytes (the
///   declaration is honored for the common Chinese-content case).
/// - Anything else → UTF-8 bytes.
#[must_use]
pub fn encode_wire_body(body: &str) -> Vec<u8> {
    if body.is_ascii() {
        return body.as_bytes().to_vec();
    }
    if declares_gb2312(body) {
        let (cow, _encoding_used, _had_errors) = encoding_rs::GB18030.encode(body);
        return cow.into_owned();
    }
    body.as_bytes().to_vec()
}

/// Whether the body's XML declaration (within the first line) says GB2312.
fn declares_gb2312(body: &str) -> bool {
    let head = body.lines().next().unwrap_or("");
    head.contains("encoding=\"GB2312\"") || head.contains("encoding='GB2312'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passthrough() {
        let s = "MESSAGE body with 中文";
        assert_eq!(decode_wire_body(s.as_bytes()), s);
    }

    #[test]
    fn ascii_passthrough() {
        let s = "<?xml version=\"1.0\" encoding=\"GB2312\"?><Notify/>";
        assert_eq!(decode_wire_body(s.as_bytes()), s);
    }

    #[test]
    fn gb2312_body_decodes() {
        // "前端摄像机" encoded in GB18030/GBK (hand-computed bytes).
        let gbk: &[u8] = &[0xC7, 0xB0, 0xB6, 0xCB, 0xC9, 0xE3, 0xCF, 0xF1, 0xBB, 0xFA];
        let decoded = decode_wire_body(gbk);
        assert_eq!(decoded, "前端摄像机");
    }

    #[test]
    fn invalid_in_both_charsets_is_lossy_not_panicking() {
        // 0xFF is invalid in UTF-8 and invalid as a GB18030 lead byte.
        let out = decode_wire_body(&[0xFF, 0xFE, 0x00]);
        assert!(
            !out.is_empty(),
            "must produce a lossy string, not drop input"
        );
        assert!(out.contains('\u{FFFD}'));
    }

    #[test]
    fn ascii_body_encodes_identically() {
        let body = "<?xml version=\"1.0\" encoding=\"GB2312\"?><Response CmdType=\"RecordInfo\" SN=\"10\"/>";
        assert_eq!(encode_wire_body(body), body.as_bytes());
    }

    #[test]
    fn gb2312_declared_body_with_chinese_encodes_gb18030() {
        let body = "<?xml version=\"1.0\" encoding=\"GB2312\"?><Name>前端摄像机</Name>";
        let bytes = encode_wire_body(body);
        // Decode back through the GB18030 decoder must round-trip the text.
        assert_eq!(decode_wire_body(&bytes), body);
        // And it must NOT be the UTF-8 encoding (proves charset selection).
        assert_ne!(bytes, body.as_bytes());
        // The ASCII XML scaffolding stays single-byte ASCII.
        assert!(bytes.starts_with(b"<?xml version=\"1.0\" encoding=\"GB2312\"?>"));
    }

    #[test]
    fn utf8_declared_body_stays_utf8() {
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Name>摄像机</Name>";
        assert_eq!(encode_wire_body(body), body.as_bytes());
    }

    #[test]
    fn no_declaration_stays_utf8() {
        let body = "<Name>摄像机</Name>";
        assert_eq!(encode_wire_body(body), body.as_bytes());
    }
}
