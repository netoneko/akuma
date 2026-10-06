//! Lowercase hex, both ways, into caller buffers.

/// Encode `src` into `dst` (which must hold `2 * src.len()` bytes); returns the
/// encoded length, or `None` if `dst` is too short.
pub fn encode(src: &[u8], dst: &mut [u8]) -> Option<usize> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let n = src.len().checked_mul(2)?;
    let out = dst.get_mut(..n)?;
    for (i, &b) in src.iter().enumerate() {
        out[2 * i] = DIGITS[usize::from(b >> 4)];
        out[2 * i + 1] = DIGITS[usize::from(b & 0xf)];
    }
    Some(n)
}

/// Decode hex `src` (either case, even length) into `dst`; returns the decoded
/// length, or `None` on an odd length, a non-hex digit, or a short `dst`.
pub fn decode(src: &[u8], dst: &mut [u8]) -> Option<usize> {
    if !src.len().is_multiple_of(2) {
        return None;
    }
    let n = src.len() / 2;
    let out = dst.get_mut(..n)?;
    for (i, pair) in src.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(n)
}

const fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// A BSSID as `aa:bb:cc:dd:ee:ff` (17 bytes) into `dst`.
pub fn encode_mac(mac: &[u8; 6], dst: &mut [u8]) -> Option<usize> {
    let out = dst.get_mut(..17)?;
    let mut tmp = [0u8; 2];
    for (i, b) in mac.iter().enumerate() {
        encode(core::slice::from_ref(b), &mut tmp)?;
        out[3 * i] = tmp[0];
        out[3 * i + 1] = tmp[1];
        if i < 5 {
            out[3 * i + 2] = b':';
        }
    }
    Some(17)
}

/// `aa:bb:cc:dd:ee:ff` (either case) back to bytes.
#[must_use]
pub fn decode_mac(s: &[u8]) -> Option<[u8; 6]> {
    if s.len() != 17 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, part) in s.split(|&c| c == b':').enumerate() {
        if i >= 6 || part.len() != 2 {
            return None;
        }
        decode(part, &mut mac[i..=i])?;
    }
    Some(mac)
}
