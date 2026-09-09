//! SIP parse/serialize round-trips (issue #34): the REGISTER the device
//! sends every refresh and the catalog MESSAGE it answers most often.
//! Run with `cargo bench --bench sip`.

use criterion::{criterion_group, criterion_main, Criterion};
use gb28181_rs::sip::SipMessage;

const REGISTER: &str = "REGISTER sip:3402000000@3402000000 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.168.62.10:5060;rport;branch=z9hG4bK1234567890abcdef\r\n\
From: <sip:34020000001320000001@3402000000>;tag=abc123\r\n\
To: <sip:34020000001320000001@3402000000>\r\n\
Call-ID: 1757000000@192.168.62.10\r\n\
CSeq: 1 REGISTER\r\n\
Contact: <sip:34020000001320000001@192.168.62.10:5060>\r\n\
Max-Forwards: 70\r\n\
Expires: 3600\r\n\
User-Agent: gb28181-rs/0.9.0\r\n\
Content-Length: 0\r\n\
\r\n";

const CATALOG_MESSAGE: &str = "MESSAGE sip:34020000002000000001@3402000000 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.168.62.10:5060;rport;branch=z9hG4bKfedcba0987654321\r\n\
From: <sip:34020000001320000001@3402000000>;tag=def456\r\n\
To: <sip:34020000002000000001@3402000000>\r\n\
Call-ID: 1757000001@192.168.62.10\r\n\
CSeq: 2 MESSAGE\r\n\
Max-Forwards: 70\r\n\
Content-Type: Application/MANSCDP+xml\r\n\
Content-Length: 190\r\n\
\r\n<?xml version=\"1.0\"?><Response><CmdType>Catalog</CmdType><SN>1</SN><DeviceID>34020000001320000001</DeviceID><SumNum>1</SumNum><DeviceList Num=\"1\"><Item><DeviceID>34020000001320000001</DeviceID><Name>Camera</Name><Manufacturer>Unknown</Manufacturer><Model>Unknown</Model><Status>ON</Status></Item></DeviceList></Response>";

fn bench_sip(c: &mut Criterion) {
    c.bench_function("sip/parse_register", |b| {
        b.iter(|| SipMessage::parse(REGISTER).expect("parse"))
    });

    c.bench_function("sip/parse_catalog_message", |b| {
        b.iter(|| SipMessage::parse(CATALOG_MESSAGE).expect("parse"))
    });

    let msg = SipMessage::parse(CATALOG_MESSAGE).expect("parse");
    c.bench_function("sip/serialize_catalog_message", |b| {
        b.iter(|| msg.serialize())
    });

    c.bench_function("sip/roundtrip_catalog_message", |b| {
        b.iter(|| {
            let m = SipMessage::parse(CATALOG_MESSAGE).expect("parse");
            m.serialize()
        })
    });
}

criterion_group!(benches, bench_sip);
criterion_main!(benches);
