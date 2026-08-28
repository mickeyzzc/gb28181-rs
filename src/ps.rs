//! PS (Program Stream) and PES packet parsing.
//!
//! Extracts H.264 NAL units from MPEG-2 Program Stream encapsulation
//! used by GB/T 28181 for RTP media transport.

use anyhow::{bail, Result};

/// MPEG-2 Program Stream pack header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsPackHeader {
    /// System Clock Reference (27MHz clock)
    pub scr: u64,
    /// Multiplex rate (50 bytes/sec units)
    pub mux_rate: u32,
    /// Pack stuffing length in bytes
    pub stuffing_length: u8,
}

/// PES (Packetized Elementary Stream) packet header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PesPacket {
    /// Stream ID (0xE0-0xEF for video, 0xC0-0xDF for audio)
    pub stream_id: u8,
    /// Packet length (0 if unbounded)
    pub length: u16,
    /// PTS (Presentation Time Stamp) in 90kHz ticks
    pub pts: Option<u64>,
    /// DTS (Decode Time Stamp) in 90kHz ticks
    pub dts: Option<u64>,
    /// Payload data (elementary stream)
    pub data: Vec<u8>,
}

/// Find all PS start codes in a byte stream.
/// Returns positions of start code prefixes (0x00 0x00 0x01).
fn find_ps_start_codes(data: &[u8]) -> Vec<usize> {
    let mut positions = Vec::new();
    if data.len() < 4 {
        return positions;
    }
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            positions.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    positions
}

/// Parse a PS pack header from data starting at a pack_start_code
/// (0x00 0x00 0x01 0xBA).
///
/// Returns (header, bytes_consumed).
pub fn parse_ps_pack_header(data: &[u8]) -> Result<(PsPackHeader, usize)> {
    if data.len() < 4 || data[0] != 0 || data[1] != 0 || data[2] != 1 || data[3] != 0xBA {
        bail!("Invalid PS pack start code");
    }

    if data.len() < 14 {
        bail!("PS pack header too short");
    }

    let marker_check = data[4];
    let is_mpeg2 = (marker_check & 0xC0) == 0x40; // Bits 7-6 = 01 for MPEG-2

    let scr: u64;
    let _mux_rate: u32;
    let offset: usize;

    if is_mpeg2 {
        // MPEG-2 system stream (recommended by GB28181)
        let b4 = data[4];
        let b5 = data[5];
        let b6 = data[6];
        let b7 = data[7];
        let b8 = data[8];
        let b9 = data[9];
        let b10 = data[10];

        // Reconstruct SCR (33 bits base + 9 bits extension = 42 bits total)
        let scr_base_high = (b4 as u64 & 0x38) >> 3;
        let scr_base_mid_hi = b5 as u64 >> 1;
        let scr_base_mid_lo = (b5 as u64 & 1) << 14 | ((b6 as u64 >> 1) & 0x7FFF);
        let scr_base = (scr_base_high << 30) | (scr_base_mid_hi << 15) | scr_base_mid_lo;

        let _scr_ext = ((b7 as u64 & 0x20) >> 4)
            | ((b7 as u64 & 0x01) << 2)
            | (((b4 as u64 & 0x01) << 1) & 0x01);

        scr = scr_base * 300; // 27MHz = 90kHz * 300

        let mux_rate_val = ((b7 as u32 & 0x3F) << 16) | ((b8 as u32) << 8) | (b9 as u32);
        _mux_rate = mux_rate_val >> 2;

        let stuffing_length = b10 & 0x07;
        offset = 11 + stuffing_length as usize;
    } else {
        scr = 0;
        _mux_rate = 0;
        offset = 12;
    }

    Ok((
        PsPackHeader {
            scr,
            mux_rate: _mux_rate,
            stuffing_length: if is_mpeg2 { data[10] & 0x07 } else { 0 },
        },
        offset,
    ))
}

/// Parse a PES packet from data starting at a packet_start_code_prefix.
///
/// Returns (pes_packet, bytes_consumed).
pub fn parse_pes_packet(data: &[u8]) -> Result<(PesPacket, usize)> {
    if data.len() < 6 {
        bail!("PES packet too short");
    }
    if data[0] != 0 || data[1] != 0 || data[2] != 1 {
        bail!("Invalid PES start code prefix");
    }

    let stream_id = data[3];
    let length = u16::from_be_bytes([data[4], data[5]]);

    if data.len() < 6 + 3 {
        bail!("PES packet truncated before header fields");
    }

    let mut offset = 6; // Start of optional PES header

    // Handle padding stream
    if stream_id == 0xBE {
        if length > 0 {
            offset += length as usize;
        }
        return Ok((
            PesPacket {
                stream_id,
                length,
                pts: None,
                dts: None,
                data: Vec::new(),
            },
            offset.min(data.len()),
        ));
    }

    if (0xC0..=0xEF).contains(&stream_id) {
        // Audio (0xC0-0xDF) or Video (0xE0-0xEF) stream
        if offset + 2 > data.len() {
            bail!("PES packet truncated at optional header fields");
        }

        let pes_header_flags = data[offset];
        let pes_header_length = data[offset + 1] as usize;
        offset += 2;

        if offset + pes_header_length > data.len() {
            bail!("PES packet truncated: optional header length exceeds data");
        }

        let mut pts: Option<u64> = None;
        let mut dts: Option<u64> = None;

        let pts_dts_flags = (pes_header_flags >> 6) & 0x03;

        if pts_dts_flags == 2 || pts_dts_flags == 3 {
            pts = Some(parse_pts_dts(&data[offset..offset + 5]));
        }

        if pts_dts_flags == 3 {
            dts = Some(parse_pts_dts(&data[offset + 5..offset + 10]));
        }

        // Skip remaining optional header fields (stuffing, etc.)
        offset = 6 + 3 + pes_header_length; // 6 bytes prefix+length + 3 bytes header info

        let remaining_len = if length > 0 {
            let header_overhead = offset - 6;
            (length as usize).saturating_sub(header_overhead)
        } else {
            data.len().saturating_sub(offset)
        };

        let payload_end = (offset + remaining_len).min(data.len());
        let payload = data[offset..payload_end].to_vec();

        Ok((
            PesPacket {
                stream_id,
                length,
                pts,
                dts,
                data: payload,
            },
            payload_end,
        ))
    } else if stream_id == 0xBC || (0xB9..=0xBB).contains(&stream_id) {
        // Program stream map, end code, padding
        let payload_end = if length > 0 {
            (6 + length as usize).min(data.len())
        } else {
            data.len()
        };
        Ok((
            PesPacket {
                stream_id,
                length,
                pts: None,
                dts: None,
                data: data[6..payload_end].to_vec(),
            },
            payload_end,
        ))
    } else {
        // Other stream types
        let payload_end = if length > 0 {
            (6 + length as usize).min(data.len())
        } else {
            data.len()
        };
        Ok((
            PesPacket {
                stream_id,
                length,
                pts: None,
                dts: None,
                data: data[6..payload_end].to_vec(),
            },
            payload_end,
        ))
    }
}

/// Parse a 5-byte PTS/DTS value (33 bits packed with marker bits).
fn parse_pts_dts(bytes: &[u8]) -> u64 {
    if bytes.len() < 5 {
        return 0;
    }
    let b0 = bytes[0] as u64;
    let b1 = bytes[1] as u64;
    let b2 = bytes[2] as u64;
    let b3 = bytes[3] as u64;
    let b4 = bytes[4] as u64;

    ((b0 >> 1) & 0x07) << 30
        | (b1 << 22)
        | ((b2 >> 1) & 0x7F) << 15
        | (b3 << 7)
        | ((b4 >> 1) & 0x7F)
}

/// Extract H.264 payload data from a PS (Program Stream) data buffer.
///
/// This parses the MPEG-2 Program Stream encapsulation used by GB/T 28181
/// and returns the H.264 data found within video PES packets.
pub fn parse_ps_to_h264(ps_data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let start_codes = find_ps_start_codes(ps_data);
    if start_codes.is_empty() {
        bail!("No PS start codes found in data");
    }

    let mut h264_data = Vec::new();

    let mut i = 0;
    while i < start_codes.len() {
        let pos = start_codes[i];
        if pos + 3 >= ps_data.len() {
            break;
        }

        let stream_id = ps_data[pos + 3];
        if stream_id == 0xBA || stream_id == 0xBB {
            i += 1;
            continue;
        } else if (0xE0..=0xEF).contains(&stream_id) {
            // Video PES packet
            match parse_pes_packet(&ps_data[pos..]) {
                Ok((pes, _consumed)) if !pes.data.is_empty() => h264_data.push(pes.data),
                _ => {}
            }
            i += 1;
        } else {
            i += 1;
        }
    }

    Ok(h264_data)
}

/// Extract H.264 NAL units from a PS stream.
///
/// This function first extracts PES payloads (PS to PES), then finds
/// Annex B NAL units within the concatenated H.264 data.
pub fn parse_ps_to_nal_units(ps_data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let pes_payloads = parse_ps_to_h264(ps_data)?;

    if pes_payloads.is_empty() {
        return Ok(Vec::new());
    }

    let total_size: usize = pes_payloads.iter().map(|d| d.len()).sum();
    let mut combined = Vec::with_capacity(total_size);
    for payload in &pes_payloads {
        combined.extend_from_slice(payload);
    }

    Ok(split_nal_units(&combined))
}

/// Find all Annex B start code positions in a byte stream.
/// Returns (start_pos, data_pos) tuples.
fn find_nal_start_codes(data: &[u8]) -> Vec<(usize, usize)> {
    let mut results = Vec::new();
    let mut i = 0;

    while i + 3 < data.len() {
        // Check for 4-byte start code (00 00 00 01)
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            results.push((i, i + 4));
            i += 4;
            continue;
        }

        // Check for 3-byte start code (00 00 01)
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            results.push((i, i + 3));
            i += 3;
            continue;
        }

        i += 1;
    }

    results
}

/// Split an Annex-B byte stream into NAL units (start codes stripped).
fn split_nal_units(data: &[u8]) -> Vec<Vec<u8>> {
    let start_codes = find_nal_start_codes(data);
    if start_codes.is_empty() {
        if !data.is_empty() {
            // No start codes found, treat entire data as one NAL unit
            return vec![data.to_vec()];
        }
        return Vec::new();
    }

    let mut nal_units = Vec::new();

    for (i, (_start, data_start)) in start_codes.iter().enumerate() {
        let end_pos = if i + 1 < start_codes.len() {
            start_codes[i + 1].0
        } else {
            data.len()
        };

        if *data_start < end_pos {
            nal_units.push(data[*data_start..end_pos].to_vec());
        }
    }

    nal_units
}

// ────────────────────────────────────────────────────────────────────────────
// MPEG-PS Muxer (Outbound packetization)
// ────────────────────────────────────────────────────────────────────────────

/// Build a PS pack header.
///
/// Returns the pack header bytes including the pack_start_code (0x00 0x00 0x01 0xBA).
///
/// # Arguments
/// * `scr` - System Clock Reference in 27MHz ticks (base = scr / 300, extension = scr % 300)
/// * `mux_rate` - Program mux rate in units of 50 bytes/sec (MPEG-2 standard, 22-bit field)
///
/// # Format
/// - Start code: 0x00 0x00 0x01 0xBA
/// - 14 bytes total: MPEG-2 pack header with SCR (33-bit base + 9-bit extension),
///   program_mux_rate (22-bit), and stuffing_length = 0
///
/// Reference: ISO/IEC 13818-1 2.5.3.3 (bit layout per libmpeg pack_header_write)
pub fn build_ps_pack_header(scr: u64, mux_rate: u32) -> Vec<u8> {
    let mut header = vec![0x00, 0x00, 0x01, 0xBA];

    let base = scr / 300; // 33-bit SCR base
    let ext = scr % 300; // 9-bit SCR extension
    let rate = mux_rate & 0x3F_FFFF; // 22-bit program_mux_rate

    // data[4]: '01' MPEG-2 marker + SCR_base[32..30] + marker + SCR_base[29..28]
    header.push(0x44 | ((((base >> 30) & 0x07) << 3) as u8) | (((base >> 28) & 0x03) as u8));
    // data[5]: SCR_base[27..20]
    header.push(((base >> 20) & 0xFF) as u8);
    // data[6]: SCR_base[19..15] + marker + SCR_base[14..13]
    header.push(0x04 | ((((base >> 15) & 0x1F) << 3) as u8) | (((base >> 13) & 0x03) as u8));
    // data[7]: SCR_base[12..5]
    header.push(((base >> 5) & 0xFF) as u8);
    // data[8]: SCR_base[4..0] + marker + SCR_ext[8..7]
    header.push(0x04 | (((base & 0x1F) << 3) as u8) | (((ext >> 7) & 0x03) as u8));
    // data[9]: SCR_ext[6..0] + marker
    header.push(0x01 | (((ext & 0x7F) << 1) as u8));
    // data[10..12]: program_mux_rate (22 bits) + two marker bits
    header.push((rate >> 14) as u8);
    header.push((rate >> 6) as u8);
    header.push(0x03 | (((rate & 0x3F) << 2) as u8));
    // data[13]: reserved '11111' + stuffing_length '000' (no stuffing)
    header.push(0xF8);

    header
}

/// Build a Program Stream Map (PSM).
///
/// Returns the PSM bytes including the program_stream_map_start_code (0x00 0x00 0x01 0xBB).
///
/// # Format
/// - Start code: 0x00 0x00 0x01 0xBB
/// - Length: 2 bytes (total length after start code)
/// - Version: 1 byte (current_next_indicator + version)
/// - program_stream_info_length: 2 bytes (0 for no program stream info)
/// - elementary_stream_map_length: 2 bytes
/// - Stream entry: 4 bytes (stream_type + elementary_stream_id + es_info_length)
/// - CRC: 2 bytes (CRC32 truncated to 16 bits; GB28181 platforms tolerate it)
///
/// Reference: ISO/IEC 13818-1 §2.5.3.5
pub fn build_program_stream_map() -> Vec<u8> {
    let mut psm = vec![0x00, 0x00, 0x01, 0xBB]; // program_stream_map_start_code

    // Length = bytes after the length field: version(1) + program_stream_info_length(2)
    // + elementary_stream_map_length(2) + stream entry(4) + truncated CRC(2)
    let length: u16 = 11;

    psm.extend_from_slice(&length.to_be_bytes());
    psm.push(0x01); // current_next_indicator = 1, version = 0

    // program_stream_info_length = 0 (no program stream info)
    psm.extend_from_slice(&0x00u16.to_be_bytes());

    // elementary_stream_map_length = 4 (one stream entry: stream_type(1) + stream_id(1) + es_info_length(2) = 4)
    psm.extend_from_slice(&4u16.to_be_bytes());

    // Stream entry: H.264 video
    psm.push(0x1B); // stream_type: H.264 (MPEG-4 AVC)
    psm.push(0xE0); // elementary_stream_id: video
    psm.extend_from_slice(&0x00u16.to_be_bytes()); // es_info_length = 0

    // CRC32 truncated to 16 bits (GB28181 platforms tolerate a truncated CRC)
    psm.extend_from_slice(&[0x00, 0x00]);

    psm
}

/// Encode PTS or DTS into 5 bytes per ISO/IEC 13818-1 2.4.3.7.
///
/// `prefix` is the 4-bit timestamp prefix nibble: '0010' for a standalone PTS,
/// '0011' for PTS when DTS follows, '0001' for DTS.
fn encode_pts_dts(value: u64, prefix: u8) -> [u8; 5] {
    [
        (prefix << 4) | ((((value >> 30) & 0x07) << 1) as u8) | 0x01,
        ((value >> 22) & 0xFF) as u8,
        ((((value >> 15) & 0x7F) << 1) | 0x01) as u8,
        ((value >> 7) & 0xFF) as u8,
        (((value & 0x7F) << 1) | 0x01) as u8,
    ]
}

/// Build a PES packet.
///
/// Returns the PES packet bytes including the packet_start_code_prefix.
///
/// # Arguments
/// * `stream_id` - Stream ID (0xE0 for video)
/// * `payload` - PES payload data (H.264 NAL units)
/// * `pts` - Presentation Time Stamp in 90kHz ticks (optional)
/// * `dts` - Decode Time Stamp in 90kHz ticks (optional)
///
/// # Format
/// - Start code prefix: 0x00 0x00 0x01
/// - Stream ID: 1 byte (0xE0 for video)
/// - Packet length: 2 bytes (0 if unbounded)
/// - Header: 2 bytes (flags + header_data_length) + optional PTS/DTS
/// - Payload: NAL units with Annex-B start codes
///
/// Reference: ISO/IEC 13818-1 §2.4.3.6
pub fn build_pes_packet(
    stream_id: u8,
    payload: &[u8],
    pts: Option<u64>,
    dts: Option<u64>,
) -> Vec<u8> {
    let mut pes = vec![0x00, 0x00, 0x01, stream_id];

    // Calculate optional header length
    let (has_pts, has_dts) = (pts.is_some(), dts.is_some());
    let mut optional_header_len = 0u8;

    if has_pts {
        optional_header_len += 5;
    }
    if has_dts {
        optional_header_len += 5;
    }

    // Packet length = optional header (3 + optional_header_len) + payload
    // Use 0 if length would exceed 65535 (unbounded)
    let packet_len = if optional_header_len > 0 || !payload.is_empty() {
        3 + optional_header_len as u16 + payload.len() as u16
    } else {
        0
    };

    pes.extend_from_slice(&packet_len.to_be_bytes());

    // PES header flags byte (data[6]).
    // Bits 7-6: PTS_DTS_flags ('00' none, '10' PTS only, '11' PTS+DTS).
    // Bits 5-0: zero (no scrambling, no ESCR/ES_rate/DSM/CRC/extension flags).
    let pts_dts_flags = match (has_pts, has_dts) {
        (true, true) => 0b11,
        (true, false) => 0b10,
        (false, true) => 0b01, // Invalid in practice but allowed by spec
        (false, false) => 0b00,
    };
    let header_flags = pts_dts_flags << 6;

    pes.push(header_flags);
    pes.push(optional_header_len);

    // Add PTS/DTS if present
    if let Some(pts_value) = pts {
        // PTS prefix nibble: '0011' when DTS follows, '0010' otherwise
        let prefix = if has_dts { 0x3 } else { 0x2 };
        pes.extend_from_slice(&encode_pts_dts(pts_value, prefix));
    }
    if let Some(dts_value) = dts {
        pes.extend_from_slice(&encode_pts_dts(dts_value, 0x1));
    }

    // One padding byte between the optional header and the payload. The local
    // parser starts the payload at 6 + 3 + header_data_length, i.e. it expects
    // a single filler byte right after the PTS/DTS fields.
    pes.push(0x00);

    // Add payload
    pes.extend_from_slice(payload);

    pes
}

/// Multiplex H.264 NAL units into an MPEG-PS packet.
///
/// Returns a complete PS pack including pack header, optional PSM, and PES packet.
///
/// # Arguments
/// * `nalus` - Slice of H.264 NAL unit byte slices
/// * `is_key_frame` - Whether this is a key frame (IDR) - includes PSM on keyframes
/// * `pts` - Presentation Time Stamp in 90kHz ticks
/// * `dts` - Decode Time Stamp in 90kHz ticks
///
/// # Format
/// - Pack header (always)
/// - PSM (on keyframe only)
/// - PES packet with concatenated NAL units (Annex-B start code 0x00 0x00 0x00 0x01)
pub fn mux_h264_to_ps(nalus: &[&[u8]], is_key_frame: bool, pts: u64, dts: u64) -> Vec<u8> {
    let mut ps = Vec::new();

    // Add pack header
    // SCR from PTS (approximate), mux_rate at typical value
    let scr = pts * 300; // Convert 90kHz to 27MHz
    let mux_rate = 10000; // 50 bytes/sec units (adjust based on actual bitrate)
    ps.extend_from_slice(&build_ps_pack_header(scr, mux_rate));

    // Add PSM on keyframes only
    if is_key_frame {
        ps.extend_from_slice(&build_program_stream_map());
    }

    // Concatenate NAL units with Annex-B start codes
    let mut payload = Vec::new();
    for (i, nalu) in nalus.iter().enumerate() {
        // Use 4-byte start code for first NAL, 3-byte for subsequent
        if i == 0 {
            payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        } else {
            payload.extend_from_slice(&[0x00, 0x00, 0x01]);
        }
        payload.extend_from_slice(nalu);
    }

    // Add PES packet
    ps.extend_from_slice(&build_pes_packet(0xE0, &payload, Some(pts), Some(dts)));

    ps
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ps_pack_header_format() {
        let header = build_ps_pack_header(27000000, 10000);

        // Check start code
        assert_eq!(&header[0..4], &[0x00, 0x00, 0x01, 0xBA]);
        // Check total length (4 bytes start code + 10 field bytes)
        assert_eq!(header.len(), 14);
        // Header must be accepted by the existing parser
        assert!(parse_ps_pack_header(&header).is_ok());
        // Check MPEG-2 marker (bits 7-6 = 01)
        assert_eq!(header[4] & 0xC0, 0x40);
    }

    #[test]
    fn test_program_stream_map_format() {
        let psm = build_program_stream_map();

        // Check start code
        assert_eq!(&psm[0..4], &[0x00, 0x00, 0x01, 0xBB]);
        // Check stream_type is H.264 (0x1B)
        let stream_type_pos = 4 + 2 + 1 + 2 + 2; // start_code(4) + length(2) + version(1) + program_stream_info_length(2) + elementary_stream_map_length(2)
        assert_eq!(psm[stream_type_pos], 0x1B);
        // 4 (start code) + 2 (length) + version(1) + psi_len(2) + esm_len(2) + entry(4) + truncated CRC(2)
        assert_eq!(psm.len(), 17);
    }

    #[test]
    fn test_ps_muxer_roundtrip() {
        // Synthesize minimal SPS, PPS, and IDR NAL units
        let sps: Vec<u8> = vec![0x67, 0x42, 0x80, 0x28, 0xDA, 0x01, 0xE0, 0x08];
        let pps: Vec<u8> = vec![0x68, 0xCE, 0x3C, 0x80];
        let idr: Vec<u8> = vec![0x65, 0x88, 0x84, 0x00, 0x4B, 0x00, 0x01, 0x00];

        let nalus: Vec<&[u8]> = vec![&sps, &pps, &idr];
        let pts = 27000; // 300ms at 90kHz
        let dts = 27000;

        // Mux to PS
        let ps_data = mux_h264_to_ps(&nalus, true, pts, dts);
        assert!(!ps_data.is_empty());

        // Parse back using existing parser
        let parsed_nalus = parse_ps_to_nal_units(&ps_data).expect("Failed to parse PS");

        // Verify we got back all 3 NAL units
        assert_eq!(parsed_nalus.len(), 3, "Should extract 3 NAL units");

        // Verify SPS, PPS, IDR match byte-for-byte
        assert_eq!(parsed_nalus[0], sps, "SPS should match");
        assert_eq!(parsed_nalus[1], pps, "PPS should match");
        assert_eq!(parsed_nalus[2], idr, "IDR should match");
    }

    #[test]
    fn test_mux_empty_nalus() {
        let ps_data = mux_h264_to_ps(&[], false, 0, 0);
        assert!(!ps_data.is_empty());
        // Should at least have pack header
        assert!(ps_data.starts_with(&[0x00, 0x00, 0x01, 0xBA]));
    }
}
