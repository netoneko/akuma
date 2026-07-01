//! Linux ⇄ NetBSD translation for the kernel-as-client sysproxy path (Step 4).
//!
//! The [`sysproxy`](crate::sysproxy) client is ABI-agnostic; this module is the
//! one place ABI knowledge lives — the hijack.c logic (sockaddr `sin_len`,
//! `SOCK_*` type-bit stripping, errno divergence) ported to Rust, plus the
//! Linux→NetBSD syscall-number map and the per-box fd map. The kernel dispatch
//! glue uses these to marshal a `stack=rump` box process's socket syscalls into
//! rumpsp `(sysnum, register_t args)` and to un-translate results.
//!
//! Numbers: NetBSD sysnums from `src-netbsd/sys/sys/syscall.h`; Linux aarch64
//! sysnums from the generic syscall ABI (`asm-generic/unistd.h`); errno tables
//! from the two `errno.h`s. socket is NetBSD `__socket30` (394). hijack.c is the
//! reference for the socket/sockaddr quirks.

extern crate alloc;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

// ── socket-family syscalls ────────────────────────────────────────────────

/// The socket-family operations the proxy forwards. (Plus read/write/close,
/// which a socket fd also takes once it is rump-owned.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Socket,
    Connect,
    Bind,
    Listen,
    Accept,
    Sendto,
    Recvfrom,
    Setsockopt,
    Getsockopt,
    Getsockname,
    Getpeername,
    Sendmsg,
    Recvmsg,
    Shutdown,
    Socketpair,
    Read,
    Write,
    Readv,
    Writev,
    Close,
}

/// Linux aarch64 (generic ABI) syscall number → [`Op`]. `None` = not proxied.
#[must_use]
pub fn op_from_linux_sysno(n: u64) -> Option<Op> {
    Some(match n {
        198 => Op::Socket,
        199 => Op::Socketpair,
        200 => Op::Bind,
        201 => Op::Listen,
        202 => Op::Accept,
        203 => Op::Connect,
        204 => Op::Getsockname,
        205 => Op::Getpeername,
        206 => Op::Sendto,
        207 => Op::Recvfrom,
        208 => Op::Setsockopt,
        209 => Op::Getsockopt,
        210 => Op::Shutdown,
        211 => Op::Sendmsg,
        212 => Op::Recvmsg,
        63 => Op::Read,
        64 => Op::Write,
        65 => Op::Readv,
        66 => Op::Writev,
        57 => Op::Close,
        _ => return None,
    })
}

/// Is `n` a socket-*family* Linux aarch64 syscall?
///
/// I.e. one that operates on a socket — and therefore, for a `stack=rump` box, must
/// be owned by the rump proxy and NEVER fall through to native smoltcp. This is a
/// SUPERSET of the ops [`op_from_linux_sysno`] can actually marshal: it also lists
/// the socket syscalls the proxy does not implement yet (`accept4`, `recvmmsg`,
/// `sendmmsg`) so the dispatch can return a clean error for them on a rump box
/// instead of leaking them to the native stack. Numbers: generic ABI
/// `asm-generic/unistd.h`.
#[must_use]
pub fn is_socket_family_sysno(n: u64) -> bool {
    matches!(
        n,
        198..=212 // socket, socketpair, bind, listen, accept, connect, getsockname,
                  // getpeername, sendto, recvfrom, setsockopt, getsockopt, shutdown,
                  // sendmsg, recvmsg
            | 242 // accept4
            | 243 // recvmmsg
            | 269 // sendmmsg
    )
}

/// NetBSD rump syscall number for an [`Op`] (what rides in rumpsp `rsp_sysnum`).
#[must_use]
pub fn netbsd_sysno(op: Op) -> u32 {
    match op {
        Op::Socket => 394, // __socket30
        Op::Connect => 98,
        Op::Bind => 104,
        Op::Listen => 106,
        Op::Accept => 30,
        Op::Sendto => 133,
        Op::Recvfrom => 29,
        Op::Setsockopt => 105,
        Op::Getsockopt => 118,
        Op::Getsockname => 32,
        Op::Getpeername => 31,
        Op::Sendmsg => 28,
        Op::Recvmsg => 27,
        Op::Shutdown => 134,
        Op::Socketpair => 135,
        Op::Read => 3,
        Op::Write => 4,
        Op::Readv => 120,
        Op::Writev => 121,
        Op::Close => 6,
    }
}

// ── argument marshaling ───────────────────────────────────────────────────

/// Pack syscall args into NetBSD's `register_t` block (the rumpsp SYSCALL data).
///
/// Each arg widened to 8 bytes, little-endian, in order. (Confirmed against
/// `rump_syscalls.c`, whose `sys_*_args` lay each `syscallarg(T)` in a
/// register_t-sized slot.)
#[must_use]
pub fn pack_args(args: &[u64]) -> Vec<u8> {
    let mut v = Vec::with_capacity(args.len() * 8);
    for a in args {
        v.extend_from_slice(&a.to_le_bytes());
    }
    v
}

// ── socket() type bits ────────────────────────────────────────────────────

const LINUX_SOCK_NONBLOCK: u64 = 0x800;
const LINUX_SOCK_CLOEXEC: u64 = 0x8_0000;

/// Strip Linux-only `SOCK_NONBLOCK`/`SOCK_CLOEXEC` from `socket()`'s `type` arg.
///
/// NetBSD rejects them there. Reports whether nonblock was requested. Like
/// hijack.c, the proxy keeps the rump socket blocking and handles nonblock
/// semantics itself, so the bit is informational.
#[must_use]
pub fn strip_sock_type(ty: u64) -> (u64, bool, bool) {
    let nonblock = ty & LINUX_SOCK_NONBLOCK != 0;
    let cloexec = ty & LINUX_SOCK_CLOEXEC != 0;
    (ty & !(LINUX_SOCK_NONBLOCK | LINUX_SOCK_CLOEXEC), nonblock, cloexec)
}

// ── sockaddr_in: Linux (16B) → NetBSD (16B, leading sin_len) ───────────────

/// AF_INET on both Linux and NetBSD.
pub const AF_INET: u8 = 2;
const LINUX_AF_INET: u16 = 2;

/// Translate a Linux `sockaddr_in` to NetBSD layout.
///
/// Linux:  `{ u16 sin_family; u16 sin_port; u32 sin_addr; u8 pad[8] }`
/// NetBSD: `{ u8 sin_len=16; u8 sin_family; u16 sin_port; u32 sin_addr; u8 zero[8] }`
///
/// `sin_port`/`sin_addr` are network-order and copied verbatim. Returns `None`
/// if the input is not a 16-byte AF_INET sockaddr (only AF_INET is proxied).
#[must_use]
pub fn sockaddr_in_linux_to_netbsd(linux: &[u8]) -> Option<[u8; 16]> {
    if linux.len() < 16 {
        return None;
    }
    let fam = u16::from_le_bytes([linux[0], linux[1]]);
    if fam != LINUX_AF_INET {
        return None;
    }
    let mut out = [0u8; 16];
    out[0] = 16; // sin_len
    out[1] = AF_INET; // sin_family (1 byte on NetBSD)
    out[2] = linux[2]; // sin_port hi (network order, verbatim)
    out[3] = linux[3]; // sin_port lo
    out[4..8].copy_from_slice(&linux[4..8]); // sin_addr
    // remaining 8 bytes stay zero
    Some(out)
}

/// Translate a NetBSD `sockaddr_in` back to Linux layout (for `accept`/
/// `getsockname`/`recvfrom` results copied out to the box process).
#[must_use]
pub fn sockaddr_in_netbsd_to_linux(nb: &[u8]) -> Option<[u8; 16]> {
    if nb.len() < 16 {
        return None;
    }
    // nb[0] = sin_len, nb[1] = sin_family(1B)
    if nb[1] != AF_INET {
        return None;
    }
    let mut out = [0u8; 16];
    out[0..2].copy_from_slice(&LINUX_AF_INET.to_le_bytes());
    out[2] = nb[2]; // port hi
    out[3] = nb[3]; // port lo
    out[4..8].copy_from_slice(&nb[4..8]); // addr
    Some(out)
}

// ── errno: NetBSD → Linux ─────────────────────────────────────────────────
//
// Errnos 1..=10 match; they diverge after. EAGAIN/EDEADLK swap, and the whole
// socket range (35..) differs. We map the socket-path errnos explicitly and
// fall back to identity for the shared low range.

/// Map a NetBSD errno (as returned by a rump syscall) to the Linux errno the
/// box process expects. Identity for the shared 1..=10 range; explicit for the
/// socket-relevant divergences.
#[must_use]
pub fn errno_netbsd_to_linux(nb: i32) -> i32 {
    match nb {
        11 => 35,  // EDEADLK
        35 => 11,  // EAGAIN/EWOULDBLOCK
        36 => 115, // EINPROGRESS
        37 => 114, // EALREADY
        38 => 88,  // ENOTSOCK
        39 => 89,  // EDESTADDRREQ
        40 => 90,  // EMSGSIZE
        41 => 91,  // EPROTOTYPE
        42 => 92,  // ENOPROTOOPT
        43 => 93,  // EPROTONOSUPPORT
        44 => 94,  // ESOCKTNOSUPPORT
        45 => 95,  // EOPNOTSUPP
        46 => 96,  // EPFNOSUPPORT
        47 => 97,  // EAFNOSUPPORT
        48 => 98,  // EADDRINUSE
        49 => 99,  // EADDRNOTAVAIL
        50 => 100, // ENETDOWN
        51 => 101, // ENETUNREACH
        52 => 102, // ENETRESET
        53 => 103, // ECONNABORTED
        54 => 104, // ECONNRESET
        55 => 105, // ENOBUFS
        56 => 106, // EISCONN
        57 => 107, // ENOTCONN
        58 => 108, // ESHUTDOWN
        59 => 109, // ETOOMANYREFS
        60 => 110, // ETIMEDOUT
        61 => 111, // ECONNREFUSED
        62 => 40,  // ELOOP   (Linux ELOOP=40)
        63 => 36,  // ENAMETOOLONG (Linux=36)
        64 => 112, // EHOSTDOWN
        65 => 113, // EHOSTUNREACH
        other => other, // shared low range / not-yet-mapped: pass through
    }
}

// ── per-box fd map (box fd ⇄ rump-server fd) ───────────────────────────────

/// Maps a box process's socket fd numbers to the rump_server's fd numbers (the
/// box never sees rump fds directly). One per `stack=rump` box.
#[derive(Default)]
pub struct FdMap {
    box_to_rump: BTreeMap<i32, i32>,
    next_box_fd: i32,
}

impl FdMap {
    /// Fresh map. Box-side fds for rump sockets start high to avoid colliding
    /// with the box's real (host) fds.
    #[must_use]
    pub fn new(first_box_fd: i32) -> Self {
        Self { box_to_rump: BTreeMap::new(), next_box_fd: first_box_fd }
    }

    /// Register a freshly-created rump fd, returning the box-visible fd.
    #[must_use]
    pub fn insert(&mut self, rump_fd: i32) -> i32 {
        let bf = self.next_box_fd;
        self.next_box_fd += 1;
        self.box_to_rump.insert(bf, rump_fd);
        bf
    }

    /// Resolve a box fd to its rump fd, if this fd is rump-owned.
    #[must_use]
    pub fn to_rump(&self, box_fd: i32) -> Option<i32> {
        self.box_to_rump.get(&box_fd).copied()
    }

    /// Is this box fd one of ours (rump-owned)?
    #[must_use]
    pub fn is_rump(&self, box_fd: i32) -> bool {
        self.box_to_rump.contains_key(&box_fd)
    }

    /// Drop a box fd on close; returns the rump fd to close on the server.
    pub fn remove(&mut self, box_fd: i32) -> Option<i32> {
        self.box_to_rump.remove(&box_fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_sysno_maps_to_ops() {
        assert_eq!(op_from_linux_sysno(198), Some(Op::Socket));
        assert_eq!(op_from_linux_sysno(203), Some(Op::Connect));
        assert_eq!(op_from_linux_sysno(63), Some(Op::Read));
        assert_eq!(op_from_linux_sysno(57), Some(Op::Close));
        assert_eq!(op_from_linux_sysno(999), None); // not proxied
    }

    #[test]
    fn socket_family_covers_unmarshaled_ops_for_isolation() {
        // Every op we can marshal must classify as socket-family (else a rump box
        // could leak it to native smoltcp).
        for n in 198..=212u64 {
            assert!(is_socket_family_sysno(n), "nr {n} should be socket-family");
        }
        // Socket syscalls we do NOT marshal yet must still be owned (return an error,
        // not fall through). This is the leak the isolation guarantee closes.
        assert!(is_socket_family_sysno(242)); // accept4
        assert!(is_socket_family_sysno(243)); // recvmmsg
        assert!(is_socket_family_sysno(269)); // sendmmsg
        assert_eq!(op_from_linux_sysno(242), None); // ...and indeed unmarshaled
        // Non-socket syscalls (and read/write/close, which are fd-generic) are NOT
        // socket-family — they only get owned when they target a rump fd.
        assert!(!is_socket_family_sysno(63)); // read
        assert!(!is_socket_family_sysno(57)); // close
        assert!(!is_socket_family_sysno(73)); // ppoll
        assert!(!is_socket_family_sysno(214)); // brk
    }

    #[test]
    fn netbsd_sysnos_match_syscall_h() {
        assert_eq!(netbsd_sysno(Op::Socket), 394); // __socket30
        assert_eq!(netbsd_sysno(Op::Connect), 98);
        assert_eq!(netbsd_sysno(Op::Bind), 104);
        assert_eq!(netbsd_sysno(Op::Listen), 106);
        assert_eq!(netbsd_sysno(Op::Sendto), 133);
        assert_eq!(netbsd_sysno(Op::Socketpair), 135);
        assert_eq!(netbsd_sysno(Op::Read), 3);
    }

    #[test]
    fn args_packed_as_le_register_block() {
        let p = pack_args(&[3, 0x4000, 16]);
        assert_eq!(p.len(), 24);
        assert_eq!(&p[0..8], &3u64.to_le_bytes());
        assert_eq!(&p[8..16], &0x4000u64.to_le_bytes());
        assert_eq!(&p[16..24], &16u64.to_le_bytes());
    }

    #[test]
    fn sock_type_bits_stripped() {
        // SOCK_STREAM(1) | SOCK_NONBLOCK | SOCK_CLOEXEC
        let (ty, nb, ce) = strip_sock_type(1 | 0x800 | 0x8_0000);
        assert_eq!(ty, 1);
        assert!(nb);
        assert!(ce);
        let (ty2, nb2, ce2) = strip_sock_type(1);
        assert_eq!(ty2, 1);
        assert!(!nb2 && !ce2);
    }

    #[test]
    fn sockaddr_linux_to_netbsd_inserts_sin_len() {
        // Linux sockaddr_in: family=2 (LE), port=0x0050 (80, net order 00 50),
        // addr=10.0.2.2 (0x0a 00 02 02).
        let mut li = [0u8; 16];
        li[0..2].copy_from_slice(&2u16.to_le_bytes());
        li[2] = 0x00;
        li[3] = 0x50; // port 80 network order
        li[4..8].copy_from_slice(&[10, 0, 2, 2]);
        let nb = sockaddr_in_linux_to_netbsd(&li).expect("af_inet");
        assert_eq!(nb[0], 16); // sin_len
        assert_eq!(nb[1], AF_INET); // 1-byte family
        assert_eq!(nb[2], 0x00);
        assert_eq!(nb[3], 0x50); // port preserved (network order)
        assert_eq!(&nb[4..8], &[10, 0, 2, 2]); // addr preserved
        assert_eq!(&nb[8..16], &[0u8; 8]);
    }

    #[test]
    fn sockaddr_roundtrip_netbsd_to_linux() {
        let mut li = [0u8; 16];
        li[0..2].copy_from_slice(&2u16.to_le_bytes());
        li[3] = 0x50;
        li[4..8].copy_from_slice(&[1, 2, 3, 4]);
        let nb = sockaddr_in_linux_to_netbsd(&li).unwrap();
        let back = sockaddr_in_netbsd_to_linux(&nb).unwrap();
        assert_eq!(back, li);
    }

    #[test]
    fn sockaddr_rejects_non_inet() {
        let mut li = [0u8; 16];
        li[0..2].copy_from_slice(&10u16.to_le_bytes()); // AF_INET6 on Linux
        assert_eq!(sockaddr_in_linux_to_netbsd(&li), None);
        let short = [0u8; 4];
        assert_eq!(sockaddr_in_linux_to_netbsd(&short), None);
    }

    #[test]
    fn errno_socket_divergences() {
        assert_eq!(errno_netbsd_to_linux(35), 11); // EAGAIN
        assert_eq!(errno_netbsd_to_linux(36), 115); // EINPROGRESS
        assert_eq!(errno_netbsd_to_linux(61), 111); // ECONNREFUSED
        assert_eq!(errno_netbsd_to_linux(48), 98); // EADDRINUSE
        assert_eq!(errno_netbsd_to_linux(60), 110); // ETIMEDOUT
        assert_eq!(errno_netbsd_to_linux(65), 113); // EHOSTUNREACH
        // shared low range passes through
        assert_eq!(errno_netbsd_to_linux(1), 1); // EPERM
        assert_eq!(errno_netbsd_to_linux(2), 2); // ENOENT
        assert_eq!(errno_netbsd_to_linux(12), 12); // ENOMEM
    }

    #[test]
    fn fd_map_basics() {
        let mut m = FdMap::new(0x4000_0000);
        let b0 = m.insert(3);
        let b1 = m.insert(4);
        assert_eq!(b0, 0x4000_0000);
        assert_eq!(b1, 0x4000_0001);
        assert_eq!(m.to_rump(b0), Some(3));
        assert_eq!(m.to_rump(b1), Some(4));
        assert!(m.is_rump(b0));
        assert!(!m.is_rump(5)); // a host fd
        assert_eq!(m.remove(b0), Some(3));
        assert!(!m.is_rump(b0));
        assert_eq!(m.to_rump(b0), None);
    }
}
