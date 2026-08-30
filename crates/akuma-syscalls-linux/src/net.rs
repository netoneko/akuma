//! Socket-family wire types that are pure Linux ABI: `struct msghdr`,
//! `struct ucred`, and the `ifreq`/`ifconf` shapes.
//!
//! **`sockaddr_in` and `sockaddr_un` are deliberately not here.** They already
//! live in `akuma-net` (`socket::SockAddrIn`) and `akuma-net-unix`
//! (`SockAddrUn`) — library crates every caller can reach, which is the
//! condition this crate exists to create. Moving them would be motion, not
//! de-duplication, and it would put a dependency on those crates into what is
//! otherwise a leaf.
//!
//! The 2026-08-30 split of `akuma-net-unix` out of `akuma-net`
//! (`docs/archive/AKUMA_NET_SPLIT.md` §5.1) **strengthens** this, it does not
//! weaken it: `SockAddrUn` now lives in a dependency-light leaf that is cheaper
//! to reach than the TCP/IP crate was, so the "already reachable" premise holds
//! more firmly than before.
//!
//! For the same reason the `SIOCGIFCONF` record type stays in
//! `src/syscall/net.rs`: it embeds a `SockAddrIn`, so it cannot be defined here
//! without dragging `akuma-net` in behind it.

/// Linux `struct msghdr` as `sendmsg`/`recvmsg` take it on a 64-bit ABI.
///
/// The two explicit `_pad` words are the ABI: `msg_namelen` is a `socklen_t`
/// (32-bit) followed by a pointer, and `msg_iovlen` is a `size_t` in the C
/// header but is treated as a 32-bit count plus padding by every 64-bit caller
/// this kernel serves. Drop either pad and `msg_control` is read from the wrong
/// place — a `recvmsg` that silently returns no control messages.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct MsgHdr {
    pub msg_name: u64,
    pub msg_namelen: u32,
    pub _pad1: u32,
    pub msg_iov: u64,
    pub msg_iovlen: u32,
    pub _pad2: u32,
    pub msg_control: u64,
    pub msg_controllen: u64,
    pub msg_flags: i32,
}

/// Linux `struct ucred` — what `SO_PEERCRED` returns.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Ucred {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

/// The `sockaddr` shape `SIOCGIFHWADDR` writes: `sa_family = ARPHRD_ETHER` (1)
/// plus the 6-byte MAC, zero-padded to the same 16 bytes as a `sockaddr_in`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct SockAddrHw {
    pub sa_family: u16,
    pub mac: [u8; 6],
    pub pad: [u8; 8],
}

/// `struct ifconf { int ifc_len; char *ifc_buf; }` — 16 bytes on a 64-bit ABI:
/// a 4-byte length, four bytes of padding, then the 8-byte pointer.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct IfConfHdr {
    pub len: i32,
    pub _pad: i32,
    pub buf: u64,
}

/// `ARPHRD_ETHER`, the `sa_family` of a `SIOCGIFHWADDR` reply.
pub const ARPHRD_ETHER: u16 = 1;

/// `sizeof(struct ifreq)` — 40 bytes.
///
/// The 16-byte `ifr_name` plus the union sized to its largest member (`struct
/// ifmap`), **not** the 16-byte `sockaddr` a given record actually fills.
/// Callers stride `ifc_buf` by this.
pub const SIZEOF_IFREQ: usize = 40;

/// `offsetof(struct ifreq, ifr_ifru)` — where every `SIOCGIF*` union member
/// starts. The ioctl handlers used to spell this as a bare `arg + 16`.
pub const IFREQ_UNION_OFFSET: usize = 16;

/// `IFNAMSIZ`.
pub const IFNAMSIZ: usize = 16;

/// `socket(2)` / `accept4(2)` type-and-flag bits. These share the `type`
/// argument with `SOCK_STREAM`/`SOCK_DGRAM`, which is why they are so high.
pub mod sock_flags {
    pub const SOCK_NONBLOCK: u32 = 0x800;
    pub const SOCK_CLOEXEC: u32 = 0x8_0000;
}

const _: () = assert!(core::mem::size_of::<MsgHdr>() == 56);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_iov) == 16);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_control) == 32);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_controllen) == 40);
const _: () = assert!(core::mem::offset_of!(MsgHdr, msg_flags) == 48);
const _: () = assert!(core::mem::size_of::<Ucred>() == 12);
const _: () = assert!(core::mem::size_of::<SockAddrHw>() == 16);
const _: () = assert!(core::mem::size_of::<IfConfHdr>() == 16);
const _: () = assert!(core::mem::offset_of!(IfConfHdr, buf) == 8);

#[cfg(test)]
mod tests {
    use super::*;

    /// The two padding words, demonstrated: `msg_iov` is the third 8-byte word
    /// and `msg_control` the fifth. Without `_pad1`/`_pad2` they would be at 12
    /// and 28, and `recvmsg` would read a pointer straddling two fields.
    #[test]
    fn msghdr_pointers_land_on_word_boundaries() {
        let m = MsgHdr {
            msg_name: 0x1111_1111_1111_1111,
            msg_namelen: 16,
            _pad1: 0,
            msg_iov: 0x2222_2222_2222_2222,
            msg_iovlen: 3,
            _pad2: 0,
            msg_control: 0x3333_3333_3333_3333,
            msg_controllen: 64,
            msg_flags: -1,
        };
        let raw: [u8; 56] = unsafe { core::mem::transmute(m) };
        let word = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
        assert_eq!(word(0), 0x1111_1111_1111_1111, "msg_name");
        assert_eq!(word(1), 16, "msg_namelen + zero pad");
        assert_eq!(word(2), 0x2222_2222_2222_2222, "msg_iov");
        assert_eq!(word(3), 3, "msg_iovlen + zero pad");
        assert_eq!(word(4), 0x3333_3333_3333_3333, "msg_control");
        assert_eq!(word(5), 64, "msg_controllen");
    }

    /// `SOCK_CLOEXEC` and `SOCK_NONBLOCK` must not collide with the socket
    /// *types* they share an argument with (`SOCK_STREAM` 1, `SOCK_DGRAM` 2,
    /// `SOCK_RAW` 3, `SOCK_SEQPACKET` 5).
    #[test]
    fn sock_flags_clear_the_type_field() {
        const TYPE_MASK: u32 = 0xF;
        assert_eq!(sock_flags::SOCK_NONBLOCK & TYPE_MASK, 0);
        assert_eq!(sock_flags::SOCK_CLOEXEC & TYPE_MASK, 0);
        assert_ne!(sock_flags::SOCK_NONBLOCK, sock_flags::SOCK_CLOEXEC);
    }

    /// A `SIOCGIFCONF` record is 32 bytes of payload inside a 40-byte stride.
    /// The two numbers are different and mixing them up truncates the list.
    #[test]
    fn ifreq_stride_exceeds_the_record_it_carries() {
        assert_eq!(IFREQ_UNION_OFFSET, IFNAMSIZ);
        assert!(SIZEOF_IFREQ > IFREQ_UNION_OFFSET + core::mem::size_of::<SockAddrHw>());
    }
}
