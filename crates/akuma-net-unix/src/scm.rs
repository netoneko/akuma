//! `SCM_RIGHTS` ancillary data: the `cmsghdr` wire format, both directions.
//!
//! Pure byte work, so the parts that are easy to get subtly wrong — header
//! alignment, the `CMSG_LEN`/`CMSG_SPACE` distinction, what a too-small
//! receive buffer gets — are host-tested here rather than discovered by a
//! browser losing a shared-memory handle. The kernel side resolves the fds to
//! descriptors and back; see `akuma-syscalls-glue`'s `unixsock`.
//!
//! The layout is the LP64 one both of Akuma's targets use:
//! `struct cmsghdr { size_t cmsg_len; int cmsg_level; int cmsg_type; }`,
//! 16 bytes, data 8-byte aligned.

use alloc::vec::Vec;

use crate::libc_errno;

pub const SOL_SOCKET: i32 = 1;
pub const SCM_RIGHTS: i32 = 1;
/// `MSG_CTRUNC`: some control data did not fit the receiver's buffer.
pub const MSG_CTRUNC: i32 = 0x8;
/// `MSG_CMSG_CLOEXEC`: install received descriptors close-on-exec.
pub const MSG_CMSG_CLOEXEC: i32 = 0x4000_0000;
/// Linux's `SCM_MAX_FD`: the most descriptors one message may carry.
pub const MAX_FDS: usize = 253;

/// `sizeof(struct cmsghdr)`.
pub const HDR: usize = 16;

#[must_use]
pub const fn align(n: usize) -> usize {
    (n + 7) & !7
}

/// `CMSG_LEN(n)`: header plus `n` bytes of data, unpadded.
#[must_use]
pub const fn cmsg_len(n: usize) -> usize {
    HDR + n
}

/// `CMSG_SPACE(n)`: header plus `n` bytes of data, padded to the next header.
#[must_use]
pub const fn cmsg_space(n: usize) -> usize {
    HDR + align(n)
}

/// The descriptors of every `SCM_RIGHTS` message in a `sendmsg` control buffer.
///
/// In order. Other control messages (credentials, anything at another level)
/// are skipped, as Linux's `unix_stream_sendmsg` effectively does for a socket
/// without `SO_PASSCRED`.
///
/// `EINVAL` for a malformed header (`cmsg_len` shorter than a header or
/// running past the buffer) and for more than [`MAX_FDS`] descriptors.
pub fn parse_rights(control: &[u8]) -> Result<Vec<i32>, i32> {
    let mut fds = Vec::new();
    let mut at = 0;
    while control.len().saturating_sub(at) >= HDR {
        let word = |o: usize, n: usize| &control[at + o..at + o + n];
        let len = u64::from_ne_bytes(word(0, 8).try_into().unwrap_or([0; 8])) as usize;
        let level = i32::from_ne_bytes(word(8, 4).try_into().unwrap_or([0; 4]));
        let kind = i32::from_ne_bytes(word(12, 4).try_into().unwrap_or([0; 4]));
        if len < HDR || len > control.len() - at {
            return Err(libc_errno::EINVAL);
        }
        if level == SOL_SOCKET && kind == SCM_RIGHTS {
            let data = &control[at + HDR..at + len];
            if fds.len() + data.len() / 4 > MAX_FDS {
                return Err(libc_errno::EINVAL);
            }
            let (words, _) = data.as_chunks::<4>();
            fds.extend(words.iter().map(|w| i32::from_ne_bytes(*w)));
        }
        at += align(len);
    }
    Ok(fds)
}

/// How many of `n` descriptors fit a receiver's control buffer of `cap`
/// bytes. Linux's `scm_detach_fds`: whatever fits after one header.
#[must_use]
pub const fn rights_fit(cap: usize, n: usize) -> usize {
    if cap < HDR {
        return 0;
    }
    let room = (cap - HDR) / 4;
    if room < n { room } else { n }
}

/// The control bytes a receive writes back for `fds`, and its `msg_controllen`.
///
/// `cap` is the receiver's buffer size; the caller has already trimmed `fds`
/// to [`rights_fit`]. The reported length is
/// `CMSG_SPACE` capped at `cap`, which is what Linux returns: a buffer exactly
/// `CMSG_LEN` long gets `CMSG_LEN`, not a length past its own end.
#[must_use]
pub fn encode_rights(fds: &[i32], cap: usize) -> (Vec<u8>, usize) {
    if fds.is_empty() {
        return (Vec::new(), 0);
    }
    let data = fds.len() * 4;
    let used = cmsg_space(data).min(cap);
    let mut out = Vec::with_capacity(used);
    out.extend_from_slice(&(cmsg_len(data) as u64).to_ne_bytes());
    out.extend_from_slice(&SOL_SOCKET.to_ne_bytes());
    out.extend_from_slice(&SCM_RIGHTS.to_ne_bytes());
    for fd in fds {
        out.extend_from_slice(&fd.to_ne_bytes());
    }
    out.resize(used, 0);
    (out, used)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rights(fds: &[i32]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(cmsg_len(fds.len() * 4) as u64).to_ne_bytes());
        b.extend_from_slice(&SOL_SOCKET.to_ne_bytes());
        b.extend_from_slice(&SCM_RIGHTS.to_ne_bytes());
        for f in fds {
            b.extend_from_slice(&f.to_ne_bytes());
        }
        b.resize(cmsg_space(fds.len() * 4), 0);
        b
    }

    #[test]
    fn macros_match_linux_lp64() {
        // CMSG_LEN(4) = 20, CMSG_SPACE(4) = 24, CMSG_SPACE(12) = 32.
        assert_eq!((cmsg_len(4), cmsg_space(4), cmsg_space(12)), (20, 24, 32));
    }

    #[test]
    fn parses_one_and_two_messages() {
        assert_eq!(parse_rights(&rights(&[7])), Ok(alloc::vec![7]));
        let mut two = rights(&[3, 4, 5]);
        two.extend(rights(&[9]));
        assert_eq!(parse_rights(&two), Ok(alloc::vec![3, 4, 5, 9]));
        assert_eq!(parse_rights(&[]), Ok(Vec::new()));
    }

    #[test]
    fn skips_other_control_messages() {
        // An SCM_CREDENTIALS (type 2) message ahead of the rights.
        let mut b = Vec::new();
        b.extend_from_slice(&(cmsg_len(12) as u64).to_ne_bytes());
        b.extend_from_slice(&SOL_SOCKET.to_ne_bytes());
        b.extend_from_slice(&2i32.to_ne_bytes());
        b.extend_from_slice(&[0; 12]);
        b.resize(cmsg_space(12), 0);
        b.extend(rights(&[11]));
        assert_eq!(parse_rights(&b), Ok(alloc::vec![11]));
    }

    #[test]
    fn rejects_malformed_and_oversized() {
        let mut short = rights(&[1]);
        short[0] = 8; // cmsg_len below a header
        assert_eq!(parse_rights(&short), Err(libc_errno::EINVAL));
        let mut long = rights(&[1]);
        long[0] = 200; // past the buffer
        assert_eq!(parse_rights(&long), Err(libc_errno::EINVAL));
        let many: Vec<i32> = (0..=253).collect(); // MAX_FDS + 1
        assert_eq!(parse_rights(&rights(&many)), Err(libc_errno::EINVAL));
    }

    #[test]
    fn receive_side_fit_and_encoding() {
        assert_eq!(rights_fit(0, 3), 0);
        assert_eq!(rights_fit(HDR, 3), 0);
        assert_eq!(rights_fit(cmsg_space(4), 3), 2); // 24 bytes: header + 2 fds
        assert_eq!(rights_fit(cmsg_space(12), 3), 3);
        let (b, used) = encode_rights(&[5, 6], 64);
        assert_eq!(used, cmsg_space(8));
        assert_eq!(parse_rights(&b), Ok(alloc::vec![5, 6]));
        // A buffer exactly CMSG_LEN(4) long reports CMSG_LEN, not CMSG_SPACE.
        let (b, used) = encode_rights(&[5], cmsg_len(4));
        assert_eq!((used, b.len()), (cmsg_len(4), cmsg_len(4)));
    }
}
