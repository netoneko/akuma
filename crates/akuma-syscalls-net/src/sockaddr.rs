//! `struct sockaddr_in` as it crosses the user boundary, and the errno convention of the
//! socket arms - the rules, not the copy.
//!
//! Both kernels' `sock.rs` (`amd64/src/sock.rs`, `taoxichuan/src/sock.rs`) carried the same three
//! functions: decode a user `sockaddr_in`, encode one, and turn `akuma-net`'s positive errno
//! into the kernel's negative return. The first two did it with `unsafe`
//! (`read_unaligned` of a `repr(C)` struct, `from_raw_parts` over one) for sixteen bytes of
//! plain data. Here they are byte arithmetic, host-tested against `akuma-net`'s `SockAddrIn`
//! (the type the old code reinterpreted), and the kernels keep only the *copy* across the
//! privilege boundary, which is per-architecture.
//!
//! # Layout (16 bytes)
//! | offset | field | encoding |
//! |---|---|---|
//! | 0 | `sin_family` | native-endian `u16`, `AF_INET` = 2 |
//! | 2 | `sin_port` | big-endian `u16` |
//! | 4 | `sin_addr` | the four IP octets, already network order |
//! | 8 | `sin_zero` | eight zero bytes |

/// `AF_INET`.
pub const AF_INET: u16 = 2;
/// `sizeof(struct sockaddr_in)`.
pub const SOCKADDR_IN_LEN: usize = 16;

/// Why a user `sockaddr_in` was refused. Both kernels answer `EINVAL` for either; the split
/// is for the tests and for a caller that wants `EAFNOSUPPORT` later.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SockaddrError {
    /// `addrlen` is smaller than a `sockaddr_in`.
    TooShort,
    /// `sin_family` is not `AF_INET`.
    NotInet,
}

/// Decode the 16 bytes read from user memory into `(ip, port)`.
///
/// `addrlen` is what the caller passed (a short length is refused before any byte is looked
/// at, matching the old code, which refused before it copied); `raw` is the 16 bytes read.
///
/// # Errors
/// [`SockaddrError::TooShort`] if `addrlen < 16`; [`SockaddrError::NotInet`] if the family is
/// not `AF_INET`.
pub fn decode_sockaddr_in(addrlen: u64, raw: &[u8; SOCKADDR_IN_LEN]) -> Result<([u8; 4], u16), SockaddrError> {
    if addrlen < SOCKADDR_IN_LEN as u64 {
        return Err(SockaddrError::TooShort);
    }
    if u16::from_ne_bytes([raw[0], raw[1]]) != AF_INET {
        return Err(SockaddrError::NotInet);
    }
    Ok(([raw[4], raw[5], raw[6], raw[7]], u16::from_be_bytes([raw[2], raw[3]])))
}

/// Encode `(ip, port)` as the 16 bytes to write to user memory.
#[must_use]
pub fn encode_sockaddr_in(ip: [u8; 4], port: u16) -> [u8; SOCKADDR_IN_LEN] {
    let mut raw = [0u8; SOCKADDR_IN_LEN];
    raw[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    raw[2..4].copy_from_slice(&port.to_be_bytes());
    raw[4..8].copy_from_slice(&ip);
    raw
}

/// `akuma-net`'s errno (a positive `i32`) as the kernel's negative syscall return.
#[must_use]
pub const fn net_err(e: i32) -> u64 {
    (-(e as i64)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use akuma_net::socket::{SockAddrIn, SocketAddrV4};

    /// The bytes the OLD code produced: `SockAddrIn::from_addr` viewed as its own memory.
    fn old_encode(addr: &SocketAddrV4) -> [u8; 16] {
        let sa = SockAddrIn::from_addr(addr);
        let mut raw = [0u8; 16];
        raw[0..2].copy_from_slice(&sa.sin_family.to_ne_bytes());
        raw[2..4].copy_from_slice(&sa.sin_port.to_ne_bytes());
        raw[4..8].copy_from_slice(&sa.sin_addr.to_ne_bytes());
        raw[8..16].copy_from_slice(&sa.sin_zero);
        raw
    }

    /// What the OLD code decoded: reinterpret as `SockAddrIn`, check the family, `to_addr`.
    fn old_decode(raw: &[u8; 16]) -> Option<SocketAddrV4> {
        let sa = SockAddrIn {
            sin_family: u16::from_ne_bytes([raw[0], raw[1]]),
            sin_port: u16::from_ne_bytes([raw[2], raw[3]]),
            sin_addr: u32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]),
            sin_zero: [raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15]],
        };
        (sa.sin_family == 2).then(|| sa.to_addr())
    }

    #[test]
    fn encode_matches_the_old_struct_bytes_for_many_addresses() {
        for ip in [[0, 0, 0, 0], [127, 0, 0, 1], [10, 0, 2, 15], [255, 255, 255, 255], [192, 168, 1, 126]] {
            for port in [0u16, 1, 22, 80, 443, 0x1F90, 0x901F, 8080, 65535] {
                let addr = SocketAddrV4::new(ip, port);
                assert_eq!(encode_sockaddr_in(ip, port), old_encode(&addr), "{ip:?}:{port}");
            }
        }
    }

    #[test]
    fn decode_matches_the_old_path_including_refusals() {
        // Exhaustive over family x a spread of ports/addresses (the other bytes are data).
        for fam in 0u16..=40 {
            for port in [0u16, 80, 0x1F90, 0x901F, 65535] {
                for ip in [[0, 0, 0, 0], [10, 0, 2, 2], [255, 0, 255, 1]] {
                    let mut raw = encode_sockaddr_in(ip, port);
                    raw[0..2].copy_from_slice(&fam.to_ne_bytes());
                    let new = decode_sockaddr_in(16, &raw).ok();
                    let old = old_decode(&raw).map(|a| (a.ip, a.port));
                    assert_eq!(new, old, "family {fam} {ip:?}:{port}");
                }
            }
        }
    }

    #[test]
    fn round_trip() {
        let raw = encode_sockaddr_in([1, 2, 3, 4], 0x1234);
        assert_eq!(decode_sockaddr_in(16, &raw), Ok(([1, 2, 3, 4], 0x1234)));
    }

    #[test]
    fn port_is_big_endian_on_the_wire() {
        // 80 must be 0x00 0x50, not 0x50 0x00: the bind-lands-on-the-wrong-port bug.
        assert_eq!(&encode_sockaddr_in([0; 4], 80)[2..4], &[0x00, 0x50]);
        assert_eq!(&encode_sockaddr_in([0; 4], 0x1F90)[2..4], &[0x1F, 0x90]);
    }

    #[test]
    fn short_length_is_refused_before_the_family_is_read() {
        let mut raw = encode_sockaddr_in([1, 2, 3, 4], 5);
        raw[0..2].copy_from_slice(&99u16.to_ne_bytes()); // would be NotInet if looked at
        assert_eq!(decode_sockaddr_in(15, &raw), Err(SockaddrError::TooShort));
        assert_eq!(decode_sockaddr_in(0, &raw), Err(SockaddrError::TooShort));
        assert_eq!(decode_sockaddr_in(16, &raw), Err(SockaddrError::NotInet));
        // A longer addrlen than 16 is fine: only the first 16 bytes are read.
        assert!(decode_sockaddr_in(28, &encode_sockaddr_in([1, 2, 3, 4], 5)).is_ok());
    }

    #[test]
    fn padding_is_zero_and_the_buffer_is_sixteen_bytes() {
        let raw = encode_sockaddr_in([9, 9, 9, 9], 9);
        assert_eq!(raw.len(), SOCKADDR_IN_LEN);
        assert!(raw[8..].iter().all(|&b| b == 0));
    }

    #[test]
    fn net_err_is_the_negative_errno() {
        assert_eq!(net_err(0), 0);
        assert_eq!(net_err(11) as i64, -11);
        assert_eq!(net_err(98) as i64, -98);
        assert_eq!(net_err(111) as i64, -111);
    }
}
