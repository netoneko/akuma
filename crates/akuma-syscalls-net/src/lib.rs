// This crate is unsafe-free by design and stays that way: `forbid` (not
// `deny`) so no module can opt back in with a local `allow`.
#![forbid(unsafe_code)]
#![no_std]
//! The read-only network-introspection surface `ifconfig` needs, as pure
//! marshalling.
//!
//! `busybox ifconfig` with no arguments reads `/proc/net/dev` to enumerate
//! interfaces, then issues `SIOCGIFADDR` / `SIOCGIFFLAGS` / … on each; with an
//! interface name it goes straight to the ioctls; `ifconfig -a` also uses
//! `SIOCGIFCONF`. All of it is **query-only** — there is no `SIOCSIF*` here —
//! and all of it is byte-layout: which field of `struct ifreq` a given command
//! fills, how a `SIOCGIFCONF` record is strided, the exact `/proc/net/dev`
//! column format. That layout is identical on every architecture, and it used
//! to exist only inside the aarch64 kernel's `akuma-syscalls-glue`, which does
//! not build for `x86_64-unknown-none`.
//!
//! This crate is that layout, with nothing else: it does not read or write
//! user memory, does not look up interfaces, and does not know what a socket
//! is. The caller supplies the [`Interface`] list (from
//! `akuma_net::smoltcp_net::interface_snapshot()` on both kernels) and does
//! the user copies; this decides the bytes.
//!
//! Extracted 2026-09-05 so `amd64/src/fd.rs` can answer these without
//! re-deriving the `struct ifreq` offsets — the trap
//! `docs/reference/subsystems/networking.md` records (a tightly-packed 32-byte
//! `SIOCGIFCONF` record vs. the 40-byte stride callers actually use) is the
//! kind of thing that must be written once.

use core::fmt::Write;

use akuma_syscalls_linux::net::{ARPHRD_ETHER, IFNAMSIZ, SIZEOF_IFREQ};

/// `SIOCGIF*` request numbers (`linux/sockios.h`) — arch-generic.
pub mod cmd {
    pub const SIOCGIFCONF: u32 = 0x8912;
    pub const SIOCGIFFLAGS: u32 = 0x8913;
    pub const SIOCGIFADDR: u32 = 0x8915;
    pub const SIOCGIFBRDADDR: u32 = 0x8919;
    pub const SIOCGIFNETMASK: u32 = 0x891b;
    pub const SIOCGIFMTU: u32 = 0x8921;
    pub const SIOCGIFHWADDR: u32 = 0x8927;

    /// Is `cmd` one this crate answers? A caller routes these to
    /// [`super::siocgifreq_reply`] / [`super::siocgifconf_record`] *before* the
    /// `fd > 2 → ENOTTY` gate that every other non-tty ioctl hits.
    #[must_use]
    pub const fn is_interface_query(cmd: u32) -> bool {
        matches!(
            cmd,
            SIOCGIFCONF
                | SIOCGIFFLAGS
                | SIOCGIFADDR
                | SIOCGIFBRDADDR
                | SIOCGIFNETMASK
                | SIOCGIFMTU
                | SIOCGIFHWADDR
        )
    }
}

/// `IFF_*` flag combinations (`linux/if.h`).
pub mod iff {
    /// `IFF_UP | IFF_LOOPBACK | IFF_RUNNING`.
    pub const LOOPBACK: i16 = 0x49;
    /// `IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST`.
    pub const ETHERNET: i16 = 0x1043;
}

/// `AF_INET`.
const AF_INET: u16 = 2;

/// One interface as `ifconfig` sees it. The kernel builds these from its one
/// real smoltcp interface plus a synthetic `lo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interface {
    pub name: &'static [u8],
    pub ip: [u8; 4],
    pub netmask: [u8; 4],
    pub broadcast: [u8; 4],
    pub mac: [u8; 6],
    pub mtu: u32,
    /// `IFF_*` bits.
    pub flags: i16,
}

impl Interface {
    /// The fixed loopback interface. `broadcast` is `0.0.0.0`, matching Linux —
    /// `lo` has no `IFF_BROADCAST`, so `ifconfig` never prints `Bcast:` for it.
    #[must_use]
    pub const fn loopback() -> Self {
        Self {
            name: b"lo",
            ip: [127, 0, 0, 1],
            netmask: [255, 0, 0, 0],
            broadcast: [0, 0, 0, 0],
            mac: [0; 6],
            mtu: 65536,
            flags: iff::LOOPBACK,
        }
    }

    /// The smoltcp interface as `eth0`, from its live address and prefix. The
    /// netmask and broadcast are derived here (and tested here) rather than in
    /// each kernel.
    #[must_use]
    pub fn ethernet(ip: [u8; 4], prefix_len: u8, mac: [u8; 6], mtu: u32) -> Self {
        let prefix = u32::from(prefix_len.min(32));
        let mask_bits: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
        let ip_bits = u32::from_be_bytes(ip);
        Self {
            name: b"eth0",
            ip,
            netmask: mask_bits.to_be_bytes(),
            broadcast: (ip_bits | !mask_bits).to_be_bytes(),
            mac,
            mtu,
            flags: iff::ETHERNET,
        }
    }
}

/// The name out of a 16-byte `ifr_name`, NUL-terminated and `IFNAMSIZ`-bounded.
#[must_use]
pub fn ifname(name_field: &[u8]) -> &[u8] {
    let bounded = &name_field[..name_field.len().min(IFNAMSIZ)];
    let end = bounded.iter().position(|&b| b == 0).unwrap_or(bounded.len());
    &bounded[..end]
}

/// Why a `SIOCGIF*` reply could not be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyError {
    /// No interface by that name — the caller returns `ENODEV`.
    NoDevice,
    /// `cmd` is not one this crate handles — the caller returns `ENOTTY`.
    NotHandled,
}

/// The largest `ifr_ifru` union member is `struct ifmap` at 16 bytes; 24 is
/// headroom. A `SIOCGIF*` reply is written into the first `n` bytes of this.
pub type UnionBytes = [u8; 24];

/// Marshal the `ifr_ifru` union bytes a `SIOCGIF*` command returns for the
/// interface named in `req_name`. The caller copies `&out[..n]` to `arg` plus
/// [`akuma_syscalls_linux::net::IFREQ_UNION_OFFSET`].
pub fn siocgifreq_reply(
    cmd: u32,
    ifaces: &[Interface],
    req_name: &[u8],
    out: &mut UnionBytes,
) -> Result<usize, ReplyError> {
    let want = ifname(req_name);
    let iface = ifaces.iter().find(|f| f.name == want).ok_or(ReplyError::NoDevice)?;
    *out = [0; 24];
    let n = match cmd {
        cmd::SIOCGIFFLAGS => {
            out[..2].copy_from_slice(&iface.flags.to_ne_bytes());
            2
        }
        cmd::SIOCGIFADDR => sockaddr_in(out, iface.ip),
        cmd::SIOCGIFNETMASK => sockaddr_in(out, iface.netmask),
        cmd::SIOCGIFBRDADDR => sockaddr_in(out, iface.broadcast),
        cmd::SIOCGIFMTU => {
            out[..4].copy_from_slice(&i32::try_from(iface.mtu).unwrap_or(i32::MAX).to_ne_bytes());
            4
        }
        cmd::SIOCGIFHWADDR => {
            out[..2].copy_from_slice(&ARPHRD_ETHER.to_ne_bytes());
            out[2..8].copy_from_slice(&iface.mac);
            16
        }
        _ => return Err(ReplyError::NotHandled),
    };
    Ok(n)
}

/// `struct sockaddr_in` (16 bytes): `sin_family` (native-endian `u16`),
/// `sin_port` 0, `sin_addr` (the 4 address bytes, already network order),
/// `sin_zero[8]`.
fn sockaddr_in(out: &mut UnionBytes, addr: [u8; 4]) -> usize {
    out[0..2].copy_from_slice(&AF_INET.to_ne_bytes());
    out[4..8].copy_from_slice(&addr);
    16
}

/// One `SIOCGIFCONF` record — `sizeof(struct ifreq)` bytes: a 16-byte name and
/// a `sockaddr_in` for the address, padded to the full stride.
///
/// **Callers stride `ifc_buf` by [`RECORD_SIZE`]**, not by the 24 bytes this
/// record's name+sockaddr actually occupy: the `ifr_ifru` union is sized to
/// `struct ifmap`. A packed record here reads record 1 mid-`sockaddr` and
/// `ifconfig -a` prints "Device not found".
#[must_use]
pub fn siocgifconf_record(iface: &Interface) -> [u8; RECORD_SIZE] {
    let mut rec = [0u8; RECORD_SIZE];
    let n = iface.name.len().min(IFNAMSIZ - 1);
    rec[..n].copy_from_slice(&iface.name[..n]);
    rec[16..18].copy_from_slice(&AF_INET.to_ne_bytes());
    rec[20..24].copy_from_slice(&iface.ip);
    rec
}

/// The stride of a `SIOCGIFCONF` record — `sizeof(struct ifreq)`, 40 bytes.
pub const RECORD_SIZE: usize = SIZEOF_IFREQ;

/// Bytes a `SIOCGIFCONF` over `ifaces` needs — what a `NULL` `ifc_buf` query
/// (the "ask the size first" pattern) reports.
#[must_use]
pub fn siocgifconf_size(ifaces: &[Interface]) -> usize {
    ifaces.len() * RECORD_SIZE
}

/// How many whole records fit in `cap` bytes.
#[must_use]
pub fn siocgifconf_capacity(ifaces: &[Interface], cap: usize) -> usize {
    (cap / RECORD_SIZE).min(ifaces.len())
}

/// Write `/proc/net/dev`.
///
/// The header plus one all-zero-counter row per interface, in the exact column
/// layout `net/core/net-procfs.c` produces (16 numeric fields; this kernel
/// keeps no per-interface counters, and `0` is what an idle interface shows
/// anyway).
///
/// # Errors
///
/// Propagates a [`core::fmt::Error`] from `w` (a `String` sink never fails).
pub fn write_proc_net_dev<W: Write>(ifaces: &[Interface], w: &mut W) -> core::fmt::Result {
    w.write_str(
        "Inter-|   Receive                                                |  Transmit\n \
         face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n",
    )?;
    for f in ifaces {
        let name = core::str::from_utf8(f.name).unwrap_or("?");
        writeln!(
            w,
            "{name:>6}: {:>7} {:>7} {:>4} {:>4} {:>4} {:>5} {:>10} {:>9} {:>8} {:>7} {:>4} {:>4} {:>4} {:>5} {:>7} {:>10}",
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ethernet_derives_the_mask_and_broadcast_from_the_prefix() {
        let e = Interface::ethernet([10, 0, 2, 15], 24, [2, 0xfc, 0, 0, 0, 1], 1500);
        assert_eq!(e.name, b"eth0");
        assert_eq!(e.netmask, [255, 255, 255, 0]);
        assert_eq!(e.broadcast, [10, 0, 2, 255]);
        // A /0 has no mask and the broadcast is 255.255.255.255.
        let z = Interface::ethernet([1, 2, 3, 4], 0, [0; 6], 1500);
        assert_eq!(z.netmask, [0, 0, 0, 0]);
        assert_eq!(z.broadcast, [255, 255, 255, 255]);
    }

    #[test]
    fn siocgifaddr_fills_a_sockaddr_in() {
        let ifaces = [Interface::loopback()];
        let mut out = [0u8; 24];
        let n = siocgifreq_reply(cmd::SIOCGIFADDR, &ifaces, b"lo", &mut out).unwrap();
        assert_eq!(n, 16);
        assert_eq!(u16::from_ne_bytes([out[0], out[1]]), AF_INET);
        assert_eq!(&out[4..8], &[127, 0, 0, 1]);
    }

    #[test]
    fn siocgifhwaddr_is_arphrd_ether_plus_the_mac() {
        let ifaces = [Interface::ethernet([10, 0, 2, 15], 24, [0xde, 0xad, 0xbe, 0xef, 0, 1], 1500)];
        let mut out = [0u8; 24];
        siocgifreq_reply(cmd::SIOCGIFHWADDR, &ifaces, b"eth0", &mut out).unwrap();
        assert_eq!(u16::from_ne_bytes([out[0], out[1]]), ARPHRD_ETHER);
        assert_eq!(&out[2..8], &[0xde, 0xad, 0xbe, 0xef, 0, 1]);
    }

    #[test]
    fn an_unknown_interface_and_an_unhandled_cmd_are_distinguished() {
        let ifaces = [Interface::loopback()];
        let mut out = [0u8; 24];
        assert_eq!(
            siocgifreq_reply(cmd::SIOCGIFADDR, &ifaces, b"wlan9", &mut out),
            Err(ReplyError::NoDevice)
        );
        assert_eq!(
            siocgifreq_reply(0x1234, &ifaces, b"lo", &mut out),
            Err(ReplyError::NotHandled)
        );
    }

    #[test]
    fn ifname_stops_at_nul_and_the_field_width() {
        assert_eq!(ifname(b"eth0\0\0\0\0\0\0\0\0\0\0\0\0"), b"eth0");
        assert_eq!(ifname(b"0123456789abcdefTRAILING"), b"0123456789abcdef");
    }

    #[test]
    fn siocgifconf_record_is_a_full_stride_with_the_address_at_offset_20() {
        let rec = siocgifconf_record(&Interface::loopback());
        assert_eq!(rec.len(), 40);
        assert_eq!(&rec[..2], b"lo");
        assert_eq!(u16::from_ne_bytes([rec[16], rec[17]]), AF_INET);
        assert_eq!(&rec[20..24], &[127, 0, 0, 1]);
    }

    #[test]
    fn siocgifconf_capacity_counts_whole_records_only() {
        let ifaces = [Interface::loopback(), Interface::loopback()];
        assert_eq!(siocgifconf_size(&ifaces), 80);
        assert_eq!(siocgifconf_capacity(&ifaces, 0), 0);
        assert_eq!(siocgifconf_capacity(&ifaces, 39), 0);
        assert_eq!(siocgifconf_capacity(&ifaces, 40), 1);
        assert_eq!(siocgifconf_capacity(&ifaces, 79), 1);
        assert_eq!(siocgifconf_capacity(&ifaces, 400), 2, "capped at the interface count");
    }

    #[test]
    fn proc_net_dev_has_a_header_and_one_row_per_interface() {
        extern crate std;
        let ifaces = [Interface::loopback(), Interface::ethernet([10, 0, 2, 15], 24, [0; 6], 1500)];
        let mut s = std::string::String::new();
        write_proc_net_dev(&ifaces, &mut s).unwrap();
        let lines: std::vec::Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 4, "2 header lines + 2 interfaces");
        assert!(lines[2].trim_start().starts_with("lo:"));
        assert!(lines[3].trim_start().starts_with("eth0:"));
    }
}
