//! Git pkt-line format
//!
//! Git uses pkt-line framing for protocol communication:
//! - 4 hex digits specify the length (including the 4 bytes)
//! - "0000" is a flush packet (section separator)
//! - "0001" is a delimiter packet (v2 protocol)
//! - "0002" is a response-end packet (v2 protocol)

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{Error, Result};

/// Special packet types
pub const FLUSH_PKT: &[u8] = b"0000";
pub const DELIM_PKT: &[u8] = b"0001";
pub const RESPONSE_END_PKT: &[u8] = b"0002";

/// Read a single pkt-line from data
///
/// Returns (line_content, bytes_consumed)
/// line_content is None for flush/delim/response-end packets
pub fn read_pkt_line(data: &[u8]) -> Result<(Option<&[u8]>, usize)> {
    if data.len() < 4 {
        return Err(Error::protocol("pkt-line too short"));
    }

    // Parse length (4 hex digits)
    let len_hex = &data[..4];
    
    // Check for special packets
    if len_hex == FLUSH_PKT {
        return Ok((None, 4));
    }
    if len_hex == DELIM_PKT {
        return Ok((None, 4));
    }
    if len_hex == RESPONSE_END_PKT {
        return Ok((None, 4));
    }

    let len = parse_hex_u16(len_hex)
        .ok_or_else(|| Error::protocol("invalid pkt-line length"))? as usize;

    if len < 4 {
        return Err(Error::protocol("pkt-line length too small"));
    }

    if data.len() < len {
        return Err(Error::protocol("pkt-line truncated"));
    }

    // Return content (excluding length prefix, may include trailing newline)
    let content = &data[4..len];
    Ok((Some(content), len))
}

/// Read all pkt-lines until a flush packet
///
/// Returns a vector of line contents (excluding length prefixes and flush)
pub fn read_until_flush(data: &[u8]) -> Result<(Vec<&[u8]>, usize)> {
    let mut lines = Vec::new();
    let mut pos = 0;

    loop {
        if pos >= data.len() {
            break;
        }

        let (content, consumed) = read_pkt_line(&data[pos..])?;
        pos += consumed;

        match content {
            None => break, // Flush packet
            Some(line) => lines.push(line),
        }
    }

    Ok((lines, pos))
}

/// Write a pkt-line
pub fn write_pkt_line(content: &[u8]) -> Vec<u8> {
    let len = content.len() + 4;
    let mut pkt = Vec::with_capacity(len);
    
    // Write length as 4 hex digits
    pkt.push(HEX_CHARS[(len >> 12) & 0xf]);
    pkt.push(HEX_CHARS[(len >> 8) & 0xf]);
    pkt.push(HEX_CHARS[(len >> 4) & 0xf]);
    pkt.push(HEX_CHARS[len & 0xf]);
    
    pkt.extend_from_slice(content);
    pkt
}

/// Write a flush packet
pub fn write_flush() -> Vec<u8> {
    FLUSH_PKT.to_vec()
}

/// Parse pkt-line content as a string (trimming trailing newline)
pub fn line_to_str(line: &[u8]) -> Option<&str> {
    let line = if line.ends_with(b"\n") {
        &line[..line.len() - 1]
    } else {
        line
    };
    core::str::from_utf8(line).ok()
}

/// Parse the capability advertisement line
///
/// Format: "<sha> <ref>\0<capabilities>" or "<sha> <ref>"
pub fn parse_ref_line(line: &[u8]) -> Option<(String, String, Option<String>)> {
    let line_str = core::str::from_utf8(line).ok()?;
    let line_str = line_str.trim_end_matches('\n');

    // Split at null byte for capabilities
    let (ref_part, caps) = if let Some(null_pos) = line_str.find('\0') {
        (&line_str[..null_pos], Some(String::from(&line_str[null_pos + 1..])))
    } else {
        (line_str, None)
    };

    // Split sha and ref name
    let mut parts = ref_part.splitn(2, ' ');
    let sha = String::from(parts.next()?);
    let ref_name = String::from(parts.next()?);

    Some((sha, ref_name, caps))
}

const HEX_CHARS: [u8; 16] = *b"0123456789abcdef";

fn parse_hex_u16(hex: &[u8]) -> Option<u16> {
    if hex.len() != 4 {
        return None;
    }
    
    let mut value = 0u16;
    for &byte in hex {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | (digit as u16);
    }
    
    Some(value)
}

/// Demultiplex side-band data
///
/// Side-band protocol:
/// - Channel 1: Pack data
/// - Channel 2: Progress messages
/// - Channel 3: Error messages
pub fn demux_sideband(data: &[u8]) -> Result<(Vec<u8>, Vec<String>)> {
    let mut pack_data = Vec::new();
    let mut messages = Vec::new();
    let mut pos = 0;

    while pos < data.len() {
        let (content, consumed) = read_pkt_line(&data[pos..])?;
        pos += consumed;

        let line = match content {
            None => continue, // Flush packet
            Some(l) if l.is_empty() => continue,
            Some(l) => l,
        };

        // First byte is the channel
        let channel = line[0];
        let payload = &line[1..];

        match channel {
            1 => pack_data.extend_from_slice(payload),
            2 => {
                if let Ok(msg) = core::str::from_utf8(payload) {
                    messages.push(String::from(msg.trim()));
                }
            }
            3 => {
                // Error channel
                if let Ok(msg) = core::str::from_utf8(payload) {
                    return Err(Error::protocol(msg));
                }
            }
            _ => {
                // Unknown channel, might be raw pack data
                // Some servers send pack data without side-band framing
                pack_data.extend_from_slice(line);
            }
        }
    }

    Ok((pack_data, messages))
}
