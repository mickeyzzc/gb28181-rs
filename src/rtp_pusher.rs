//! RTP pusher — constructs and sends RTP packets to a destination.

use std::net::SocketAddr;

use super::sip::Transport;

/// Common dynamic payload type for H.264 (RFC 6184).
pub const H264_PAYLOAD_TYPE: u8 = 96;

/// Information about an RTP stream established via SIP INVITE.
#[derive(Debug, Clone)]
pub struct RtpStreamInfo {
    /// Device ID that is sending/receiving the stream
    pub device_id: String,
    /// Channel ID
    pub channel_id: String,
    /// SSRC of the RTP stream
    pub ssrc: u32,
    /// Transport protocol
    pub transport: Transport,
    /// Remote address for RTP data
    pub remote_addr: String,
    /// Remote port for RTP data
    pub remote_port: u16,
}

/// Constructs RTP packets for pushing media to a destination.
///
/// Builds an RFC 3550 compliant RTP header and wraps each H.264 NAL unit
/// in a Single NAL Unit packet (RFC 6184), incrementing the sequence number
/// after every packet.
#[derive(Debug, Clone)]
pub struct RtpPusher {
    /// Destination socket address
    pub destination: SocketAddr,
    /// Synchronization source identifier
    pub ssrc: u32,
    /// Sequence number (incremented per packet)
    pub sequence_number: u16,
    /// Timestamp (90kHz clock, typical for H.264)
    pub timestamp: u32,
    /// RTP payload type (typically 96 for H.264/PS)
    pub payload_type: u8,
}

impl RtpPusher {
    /// Create a new RTP pusher.
    pub fn new(destination: SocketAddr, ssrc: u32, payload_type: u8) -> Self {
        Self {
            destination,
            ssrc,
            sequence_number: 0,
            timestamp: 0,
            payload_type,
        }
    }

    /// Build an RTP packet containing a H.264 NAL unit.
    ///
    /// Uses Single NAL Unit packet format (RFC 6184 section 5.6) with a
    /// 12-byte RFC 3550 header (V=2, no padding/extension/CSRC, no marker).
    /// The sequence number is auto-incremented after each packet.
    /// Returns the serialized RTP packet bytes.
    pub fn build_rtp_packet(&mut self, nal: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12 + nal.len());
        // First byte: version 2, padding 0, extension 0, csrc_count 0
        buf.push(0x80);
        // Second byte: marker 0, payload type
        buf.push(self.payload_type & 0x7F);
        buf.extend_from_slice(&self.sequence_number.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.ssrc.to_be_bytes());
        buf.extend_from_slice(nal);

        // Increment sequence number for next packet
        self.sequence_number = self.sequence_number.wrapping_add(1);

        buf
    }

    /// Increment the timestamp by the given amount.
    ///
    /// Typical increment for 30fps H.264 at 90kHz clock is 3000 (90000/30).
    pub fn increment_timestamp(&mut self, increment: u32) {
        self.timestamp = self.timestamp.wrapping_add(increment);
    }
}
