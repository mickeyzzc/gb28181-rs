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
    build_program_stream_map_for(STREAM_TYPE_H264)
}

/// Build a Program Stream Map declaring the given elementary-stream type.
/// `STREAM_TYPE_H264` (0x1B) keeps the historical wire bytes; H.265 devices
/// declare `STREAM_TYPE_H265` (0x24, GB/T 28181-2022).
pub fn build_program_stream_map_for(stream_type: u8) -> Vec<u8> {
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
    psm.push(stream_type); // stream_type: H.264 (0x1B) / H.265 (0x24)
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

    // Packet length = 3 header bytes (flags + hdrlen + gap) + optional fields
    // + payload, computed wide so a >64KB payload can never wrap mod 65536:
    // fall back to unbounded (0) only past the 16-bit field's cap.
    // mux_h264_to_ps splits large access units, so its PES stay bounded.
    let packet_len: u16 = if optional_header_len > 0 || !payload.is_empty() {
        let declared = 3u32 + optional_header_len as u32 + payload.len() as u32;
        if declared <= 65535 {
            declared as u16
        } else {
            0
        }
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

/// Bounds the elementary-stream bytes carried by one PES packet.
/// PES_packet_length is a 16-bit field counting 3 header bytes + optional
/// fields + payload (≤ 65535), so an access unit larger than ~64KB MUST be
/// split across continuation PES packets — receivers accumulate the ES of one
/// access unit across its PES packets. 65000 leaves headroom below the cap.
pub const MAX_PES_CHUNK_BYTES: usize = 65000;

/// PSM stream_type for H.264 (MPEG-4 AVC) elementary video.
pub const STREAM_TYPE_H264: u8 = 0x1B;
/// PSM stream_type for H.265 (HEVC) elementary video (GB/T 28181-2022).
pub const STREAM_TYPE_H265: u8 = 0x24;

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
/// - PES packets with the concatenated NAL units (Annex-B start code
///   0x00 0x00 0x00 0x01), split so every PES stays bounded: the first
///   carries PTS/DTS, continuation PES packets (access units larger than
///   MAX_PES_CHUNK_BYTES) carry none — the ES is continuous across the
///   PES packets of one access unit.
pub fn mux_h264_to_ps(nalus: &[&[u8]], is_key_frame: bool, pts: u64, dts: u64) -> Vec<u8> {
    mux_nalus_to_ps(nalus, is_key_frame, pts, dts, STREAM_TYPE_H264)
}

/// Multiplex H.265 NAL units into an MPEG-PS packet (issue #7).
///
/// Identical framing to [`mux_h264_to_ps`] — Annex-B start-code
/// concatenation, bounded PES splitting, PTS/DTS on the first PES only —
/// with the PSM declaring stream_type 0x24 (GB/T 28181-2022). RTP packaging
/// (PS-over-RTP, PT=96) is codec-agnostic and reused as-is.
pub fn mux_h265_to_ps(nalus: &[&[u8]], is_key_frame: bool, pts: u64, dts: u64) -> Vec<u8> {
    mux_nalus_to_ps(nalus, is_key_frame, pts, dts, STREAM_TYPE_H265)
}

fn mux_nalus_to_ps(
    nalus: &[&[u8]],
    is_key_frame: bool,
    pts: u64,
    dts: u64,
    stream_type: u8,
) -> Vec<u8> {
    let mut ps = Vec::new();

    // Add pack header
    // SCR from PTS (approximate), mux_rate at typical value
    let scr = pts * 300; // Convert 90kHz to 27MHz
    let mux_rate = 10000; // 50 bytes/sec units (adjust based on actual bitrate)
    ps.extend_from_slice(&build_ps_pack_header(scr, mux_rate));

    // Add PSM on keyframes only
    if is_key_frame {
        ps.extend_from_slice(&build_program_stream_map_for(stream_type));
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

    // Add PES packets: split the ES into bounded chunks. Only the first PES
    // carries PTS/DTS; a 16-bit PES_packet_length cannot describe an access
    // unit larger than ~64KB in one packet, and letting the field wrap
    // truncates every large IDR (issue #11).
    let mut start = 0usize;
    loop {
        let end = (start + MAX_PES_CHUNK_BYTES).min(payload.len());
        let chunk = &payload[start..end];
        if start == 0 {
            ps.extend_from_slice(&build_pes_packet(0xE0, chunk, Some(pts), Some(dts)));
        } else {
            ps.extend_from_slice(&build_pes_packet(0xE0, chunk, None, None));
        }
        if end == payload.len() {
            break;
        }
        start = end;
    }

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

    // Deterministic filler with no zero bytes: the elementary stream can
    // never fake an Annex-B or PS start code inside the payload.
    fn lcg_fill(n: usize) -> Vec<u8> {
        let mut b = Vec::with_capacity(n);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..n {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            b.push(1 + ((x >> 16) % 255) as u8);
        }
        b
    }

    fn nal(head: u8, n: usize) -> Vec<u8> {
        let mut v = vec![head];
        v.extend_from_slice(&lcg_fill(n - 1));
        v
    }

    /// One walked PES packet of a muxed PS burst.
    struct WalkedPes {
        offset: usize,
        declared: usize,
        actual: usize,
        has_ts: bool,
        header_len: usize,
    }

    /// Walk a PS burst the way a strict receiver does: advance by each PES's
    /// declared length instead of scanning to the end of data.
    fn walk_pes_packets(ps: &[u8]) -> Vec<WalkedPes> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos + 9 <= ps.len() {
            if ps[pos] != 0 || ps[pos + 1] != 0 || ps[pos + 2] != 1 || ps[pos + 3] != 0xE0 {
                pos += 1;
                continue;
            }
            let declared = ((ps[pos + 4] as usize) << 8) | ps[pos + 5] as usize;
            let pkt = WalkedPes {
                offset: pos,
                declared,
                actual: ps.len() - pos - 6,
                has_ts: ps[pos + 6] & 0xC0 != 0,
                header_len: ps[pos + 7] as usize,
            };
            if declared == 0 {
                panic!(
                    "PES at {} is unbounded (length=0) — mux must never emit these",
                    pos
                );
            }
            let done = declared > pkt.actual;
            let actual = declared.min(pkt.actual);
            out.push(WalkedPes { actual, ..pkt });
            if done {
                // Strict receiver: pending PES at end of burst (issue #11
                // failure signature when the wrapped length over-declares).
                break;
            }
            pos += 6 + declared;
        }
        out
    }

    /// PES_packet_length must equal the bytes actually written after it — the
    /// gap byte makes the timestamped layout balance (contract pin; the Go
    /// twin's pre-fix off-by-one is the cautionary tale, mibee-eye-raspi #15).
    #[test]
    fn test_pes_packet_length_balanced() {
        let payload = [0x00u8, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84];
        for (pts, dts) in [
            (Some(90000u64), Some(90000)),
            (Some(90000), None),
            (None, None),
        ] {
            let pes = build_pes_packet(0xE0, &payload, pts, dts);
            let declared = ((pes[4] as usize) << 8) | pes[5] as usize;
            assert!(declared > 0, "expected bounded PES");
            assert_eq!(
                pes.len(),
                6 + declared,
                "PES_packet_length={declared} must match written bytes (pts={pts:?})"
            );
        }
    }

    /// Access units larger than the 16-bit PES_packet_length cap MUST be split
    /// across bounded continuation PES packets: first carries PTS/DTS,
    /// continuations carry none, ES is continuous across the chunks. The
    /// pre-fix code wrapped the length computation mod 65536, truncating every
    /// large IDR and smearing ~64KB of residual ES into the next AU (issue #11).
    #[test]
    fn test_mux_large_au_splits_into_bounded_pes() {
        let sps = nal(0x67, 12);
        let idr = nal(0x65, 199_999); // 200000-byte ES → 4 PES chunks
        let nalus: Vec<&[u8]> = vec![&sps, &idr];

        let ps_data = mux_h264_to_ps(&nalus, true, 90_000, 90_000);
        let pkts = walk_pes_packets(&ps_data);

        assert_eq!(pkts.len(), 4, "expected 4 PES packets for a 200KB ES");

        let mut es: Vec<u8> = Vec::new();
        for (i, pkt) in pkts.iter().enumerate() {
            assert_eq!(pkt.declared, pkt.actual, "PES {i} over-declares its bytes");
            if i == 0 {
                assert!(
                    pkt.has_ts && pkt.header_len == 10,
                    "first PES must carry PTS+DTS"
                );
            } else {
                assert!(
                    !pkt.has_ts && pkt.header_len == 0,
                    "continuation PES {i} must not carry timestamps"
                );
            }
            let start = pkt.offset + 9 + pkt.header_len; // prefix(6)+flags(1)+hdrlen(1)+gap(1)+optional
            let end = pkt.offset + 6 + pkt.declared;
            es.extend_from_slice(&ps_data[start..end]);
        }

        let mut want_es = vec![0x00, 0x00, 0x00, 0x01];
        want_es.extend_from_slice(&sps);
        want_es.extend_from_slice(&[0x00, 0x00, 0x01]);
        want_es.extend_from_slice(&idr);
        assert_eq!(
            es.len(),
            want_es.len(),
            "split ES must reassemble to the full ES"
        );
        assert_eq!(es, want_es, "split ES content must match byte-for-byte");
    }

    /// A continuation PES has no optional fields: flags 0x00, header_data_length
    /// 0x00, then the gap byte — so a standard-layout receiver reads
    /// PES_header_data_length 0 at byte 8 and locates the payload at 9.
    #[test]
    fn test_mux_continuation_pes_shape() {
        let idr = nal(0x65, 70_000); // 70000-byte ES → 65000 + 5000
        let nalus: Vec<&[u8]> = vec![&idr];
        let ps_data = mux_h264_to_ps(&nalus, false, 90_000, 90_000);
        let pkts = walk_pes_packets(&ps_data);

        assert_eq!(pkts.len(), 2);
        let second = &pkts[1];
        // ES = 4-byte start code + 70000 = 70004; chunk 2 = 5004 payload bytes
        // → PES_packet_length = 3 + 0 + 5004 = 0x138F.
        // 00 00 01 E0 | 13 8F | 00 00 00 | payload...
        let head = &ps_data[second.offset..second.offset + 9];
        let want = [0x00, 0x00, 0x01, 0xE0, 0x13, 0x8F, 0x00, 0x00, 0x00];
        assert_eq!(head, want, "continuation PES header shape");
        // ...and its payload continues the ES mid-NAL, exactly where chunk 1
        // ended: ES = start code(4) + idr, chunk 2 starts at ES byte 65000 →
        // idr byte 64996.
        assert_eq!(
            &ps_data[second.offset + 9..second.offset + 9 + 4],
            &idr[64_996..65_000]
        );
    }

    /// H.265 PSM must declare stream_type 0x24 (GB/T 28181-2022); the H.264
    /// builder keeps 0x1B (issue #7).
    #[test]
    fn test_h265_psm_declares_stream_type_0x24() {
        let psm = build_program_stream_map_for(STREAM_TYPE_H265);
        let stream_type_pos = 4 + 2 + 1 + 2 + 2;
        assert_eq!(psm[stream_type_pos], 0x24);
        assert_eq!(psm[3], 0xBB, "PSM start code unchanged");

        let h264 = build_program_stream_map();
        assert_eq!(h264[stream_type_pos], 0x1B, "H.264 PSM contract unchanged");
        assert_eq!(h264.len(), psm.len());
    }

    /// mux_h265_to_ps carries VPS/SPS/PPS/IDR NALs with the same PES framing
    /// as H.264: balanced bounded lengths, PSM on keyframes only, and the
    /// >64KB split (issue #7).
    #[test]
    fn test_mux_h265_to_ps_framing() {
        // H.265 NAL headers are 2 bytes; types VPS=32, SPS=33, PPS=34, IDR=19.
        let vps: Vec<u8> = vec![0x40, 0x01, 0x01, 0x01, 0xFF];
        let sps: Vec<u8> = vec![0x42, 0x01, 0x01, 0x01, 0x01, 0x90];
        let pps: Vec<u8> = vec![0x44, 0x01, 0xC0, 0xF5];
        let idr: Vec<u8> = vec![0x26, 0x01, 0xAF, 0x0E];

        let ps = mux_h265_to_ps(&[&vps, &sps, &pps, &idr], true, 90_000, 90_000);
        // PSM present on keyframe with 0x24
        let psm_at = ps
            .windows(4)
            .position(|w| w == [0x00, 0x00, 0x01, 0xBB])
            .expect("PSM on keyframe");
        assert_eq!(ps[psm_at + 11], 0x24);

        // P-frame: no PSM
        let ps_p = mux_h265_to_ps(&[&idr], false, 180_000, 180_000);
        assert!(!ps_p.windows(4).any(|w| w == [0x00, 0x00, 0x01, 0xBB]));

        // PES lengths balanced (strict walk over both muxes)
        for data in [&ps, &ps_p] {
            for p in walk_pes_packets(data) {
                assert!(p.declared <= p.actual, "H.265 PES over-declares");
            }
        }
    }

    #[test]
    fn test_mux_h265_large_au_splits() {
        let mut idr = vec![0x26u8, 0x01];
        idr.extend(lcg_fill(199_998)); // 200000-byte ES → 4 PES chunks
        let nalus: Vec<&[u8]> = vec![&idr];
        let ps = mux_h265_to_ps(&nalus, true, 90_000, 90_000);
        let pkts = walk_pes_packets(&ps);
        assert_eq!(
            pkts.len(),
            4,
            "H.265 AUs split across bounded PES like H.264"
        );
        for (i, p) in pkts.iter().enumerate() {
            assert_eq!(p.declared, p.actual, "PES {i} balanced");
            if i == 0 {
                assert!(p.has_ts);
            } else {
                assert!(!p.has_ts);
            }
        }
    }

    /// The issue #11 wire contract end to end: walking the muxed burst by
    /// declared lengths must consume the whole access unit with no pending
    /// PES — for normal frames and for >64KB split frames alike.
    #[test]
    fn test_mux_strict_receiver_walks_full_au() {
        for size in [16_000usize, 70_000, 200_000] {
            let n = nal(0x65, size);
            let nalus: Vec<&[u8]> = vec![&n];
            let ps_data = mux_h264_to_ps(&nalus, true, 90_000, 90_000);
            let pkts = walk_pes_packets(&ps_data);

            for (i, p) in pkts.iter().enumerate() {
                assert!(
                    p.declared <= p.actual,
                    "size {size}: PES {i} declares {} bytes but {} arrived",
                    p.declared,
                    p.actual
                );
            }
            let last = pkts.last().unwrap();
            assert_eq!(
                last.offset + 6 + last.declared,
                ps_data.len(),
                "size {size}: strict walk must consume the full AU"
            );
        }
    }
}
