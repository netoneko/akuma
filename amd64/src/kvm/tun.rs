//! `/dev/net/tun`: the tap a userspace VMM's network device talks to, with the hypervisor's
//! gateway on the far end.
//!
//! Firecracker's `network-interfaces` open `/dev/net/tun`, name an interface with `TUNSETIFF`
//! (`IFF_TAP | IFF_NO_PI | IFF_VNET_HDR`), set offloads and the virtio-net header size, then
//! `read`/`write` whole frames, each behind that header, and wait for `EPOLLIN` on the fd. This
//! node serves exactly that. The "network" behind it is `akuma_svm_amd64::gateway`, the same
//! slirp-style gateway `kvm0` guests use: it answers ARP, DHCP (10.0.2.2 hands the guest
//! 10.0.2.15), ICMP echo and UDP echo itself; with `tunout` on the kernel command line a guest's
//! TCP and DNS leave through the host's stack (`super::relay`), and `tunfwd=<host>:<guest>` forwards
//! one host port to the guest. Both are off by default for the reason `vm_outbound` is.
//!
//! One tap at a time (`EBUSY`; a tap whose owner process is gone is reclaimed). The gateway and
//! relay are built once, on the first open, and reused. A daemon thread (`pump`) polls the relay
//! (host sockets) between reads; the readiness hook is a relaxed flag it and every read/write
//! refresh, so `epoll` never takes the tap's lock. The tap is never "full": a frame the gateway
//! cannot take is dropped, as on a real wire.
//!
//! No `unsafe`. Allocation: the gateway's storage (about 60 KiB) and the `Tun` box, once, fallibly
//! for the storage; frames pass through a 1.5 KiB stack buffer.

use akuma_svm_amd64::gateway::{self, Gateway};
use akuma_svm_amd64::virtio_net::{NetBackend, MAX_FRAME};
use akuma_vm_spec::Forward;
use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spinning_top::Spinlock;

use super::relay::Relay;
use crate::fd::errno;

const TUNSETIFF: u32 = 0x4004_54ca;
const TUNGETFEATURES: u32 = 0x8004_54cf;
const TUNSETOFFLOAD: u32 = 0x4004_54d0;
const TUNGETIFF: u32 = 0x8004_54d2;
const TUNGETVNETHDRSZ: u32 = 0x8004_54d7;
const TUNSETVNETHDRSZ: u32 = 0x4004_54d8;
const IFF_TAP: u16 = 0x0002;
const IFF_NO_PI: u16 = 0x1000;
const IFF_VNET_HDR: u16 = 0x4000;
/// `struct ifreq`: the name, then the flags short.
const IFREQ_LEN: usize = 40;
const IFNAME: &[u8] = b"tap0";
/// The largest virtio-net header a VMM may ask for (`virtio_net_hdr_v1`).
const MAX_HDR: usize = 12;
const BUF: usize = MAX_HDR + MAX_FRAME;

struct Tun {
    gw: Gateway<'static>,
    relay: Relay,
    /// Bytes of virtio-net header before every frame (`TUNSETVNETHDRSZ`; 10 until told).
    hdr: usize,
    flags: u16,
    named: bool,
}

static TUN: Spinlock<Option<Box<Tun>>> = Spinlock::new(None);
/// The process (tgid) holding the tap, 0 when it is free.
static OWNER: AtomicU32 = AtomicU32::new(0);
/// A frame is waiting for the reader: refreshed after every gateway operation.
static HAS_FRAME: AtomicBool = AtomicBool::new(false);
static PUMP_SPAWNED: AtomicBool = AtomicBool::new(false);
/// Reads that found a frame, reads that found none, writes, readiness polls that said yes.
static READS: AtomicU32 = AtomicU32::new(0);
static EAGAINS: AtomicU32 = AtomicU32::new(0);
static WRITES: AtomicU32 = AtomicU32::new(0);
static POLLS: AtomicU32 = AtomicU32::new(0);
static OUTBOUND: AtomicBool = AtomicBool::new(false);
static FORWARD: AtomicU32 = AtomicU32::new(0);

/// Read `tunout` / `tunfwd=<host>:<guest>` off the kernel command line.
pub fn init(cmdline: &str) {
    OUTBOUND.store(cmdline.split_ascii_whitespace().any(|t| t == "tunout"), Ordering::Relaxed);
    let fwd = cmdline
        .split_ascii_whitespace()
        .find_map(|t| t.strip_prefix("tunfwd="))
        .and_then(|v| v.split_once(':'))
        .and_then(|(h, g)| Some((h.parse::<u16>().ok()?, g.parse::<u16>().ok()?)))
        .map_or(0, |(h, g)| u32::from(h) << 16 | u32::from(g));
    FORWARD.store(fwd, Ordering::Relaxed);
}

/// Is a frame waiting for the tap's reader? (The `epoll` readiness hook.)
#[must_use]
pub fn has_frame() -> bool {
    POLLS.fetch_add(1, Ordering::Relaxed);
    HAS_FRAME.load(Ordering::Relaxed)
}

fn build() -> Option<Box<Tun>> {
    let mut v = alloc::vec::Vec::new();
    v.try_reserve_exact(gateway::STORAGE).ok()?;
    v.resize(gateway::STORAGE, 0u8);
    let mut gw = Gateway::new(v.leak())?;
    let outbound = OUTBOUND.load(Ordering::Relaxed);
    gw.set_outbound(outbound);
    let f = FORWARD.load(Ordering::Relaxed);
    let fwd = [Forward { host: (f >> 16) as u16, guest: f as u16 }];
    let fwds: &[Forward] = if f != 0 { &fwd } else { &[] };
    Some(Box::new(Tun { gw, relay: Relay::new(fwds, outbound), hdr: 10, flags: 0, named: false }))
}

/// `open("/dev/net/tun")`: the tap, when `/dev/kvm` is armed.
pub fn open(flags: u32) -> u64 {
    if !super::api::enabled() {
        return errno::ENOENT;
    }
    let Some(proc) = akuma_exec::process::current_process_shared() else { return errno::ESRCH };
    let owner = OWNER.load(Ordering::Acquire);
    if owner != 0 {
        if akuma_exec::process::lookup_process_shared(owner).is_some() {
            return errno::EBUSY;
        }
        release();
    }
    {
        let mut g = TUN.lock();
        match g.as_mut() {
            Some(t) => {
                t.gw.reset();
                t.hdr = 10;
                t.flags = 0;
                t.named = false;
            }
            None => match build() {
                Some(t) => *g = Some(t),
                None => return errno::ENOMEM,
            },
        }
    }
    HAS_FRAME.store(false, Ordering::Relaxed);
    OWNER.store(proc.tgid, Ordering::Release);
    spawn_pump();
    let fd = proc.alloc_fd(akuma_exec_core::process::FileDescriptor::DevTun);
    if flags & crate::fd::open_flags::O_CLOEXEC != 0 {
        proc.set_cloexec(fd);
    }
    u64::from(fd)
}

/// The tap's descriptor was closed (or its owner is gone): release host sockets and the slot.
pub fn release() {
    if let Some(t) = TUN.lock().as_mut() {
        let t = &mut **t;
        stats_line(t);
        t.relay.close(&mut t.gw);
        t.gw.reset();
    }
    HAS_FRAME.store(false, Ordering::Relaxed);
    OWNER.store(0, Ordering::Release);
}

fn with<R>(f: impl FnOnce(&mut Tun) -> R) -> Option<R> {
    let mut g = TUN.lock();
    let r = f(g.as_mut()?);
    HAS_FRAME.store(g.as_ref().is_some_and(|t| t.gw.has_frame()), Ordering::Relaxed);
    Some(r)
}

/// One frame behind a zeroed header into `b`: its length, or `EAGAIN`.
pub fn read_frame(b: &mut [u8; BUF]) -> Result<usize, u64> {
    let got = with(|t| {
        loop {
            match t.gw.receive(&mut b[t.hdr..]) {
                None => return None,
                Some(0) => {} // dropped as oversize; look at the next
                Some(n) => return Some(t.hdr + n),
            }
        }
    })
    .flatten();
    match got {
        Some(n) => {
            READS.fetch_add(1, Ordering::Relaxed);
            Ok(n)
        }
        None => {
            EAGAINS.fetch_add(1, Ordering::Relaxed);
            Err(errno::EAGAIN)
        }
    }
}

/// One frame behind its header from `b`. A runt is `EINVAL`, an oversize frame is dropped.
pub fn write_frame(b: &[u8]) -> u64 {
    let len = b.len();
    if WRITES.fetch_add(1, Ordering::Relaxed) < 3 {
        akuma_primitives::safe_print!(160, "[tun] write len={} first={:02x?}\n", len, &b[..len.min(30)]);
    }
    with(|t| {
        if len < t.hdr {
            return errno::EINVAL;
        }
        t.gw.transmit(&b[t.hdr..]);
        len as u64
    })
    .unwrap_or(errno::EBADF)
}

/// `read(2)`.
pub fn read(buf: u64, len: usize) -> u64 {
    let mut b = [0u8; BUF];
    match read_frame(&mut b) {
        Ok(n) => {
            let n = n.min(len);
            if crate::uaccess::write_bytes(buf, &b[..n]) { n as u64 } else { errno::EFAULT }
        }
        Err(e) => e,
    }
}

/// `write(2)`.
pub fn write(buf: u64, len: usize) -> u64 {
    let mut b = [0u8; BUF];
    if len > BUF {
        return len as u64;
    }
    if !crate::uaccess::read_bytes(buf, &mut b[..len]) {
        return errno::EFAULT;
    }
    write_frame(&b[..len])
}

/// `writev(2)`: the iovecs are one frame (Firecracker sends the virtio header and the frame as two).
pub fn writev(iov: u64, cnt: usize) -> u64 {
    let mut b = [0u8; BUF];
    let mut n = 0usize;
    for i in 0..cnt.min(64) {
        let Some([base, len]) = crate::uaccess::read_val::<[u64; 2]>(iov + (i as u64) * 16) else { return errno::EFAULT };
        let len = len as usize;
        if len == 0 {
            continue;
        }
        if n + len > BUF {
            return (n + len) as u64; // oversize: dropped, as `write` does
        }
        if !crate::uaccess::read_bytes(base, &mut b[n..n + len]) {
            return errno::EFAULT;
        }
        n += len;
    }
    write_frame(&b[..n])
}

/// `readv(2)`: one frame scattered over the iovecs.
pub fn readv(iov: u64, cnt: usize) -> u64 {
    let mut b = [0u8; BUF];
    let n = match read_frame(&mut b) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let mut done = 0usize;
    for i in 0..cnt.min(64) {
        if done == n {
            break;
        }
        let Some([base, len]) = crate::uaccess::read_val::<[u64; 2]>(iov + (i as u64) * 16) else { return errno::EFAULT };
        let take = (len as usize).min(n - done);
        if take > 0 {
            if !crate::uaccess::write_bytes(base, &b[done..done + take]) {
                return errno::EFAULT;
            }
            done += take;
        }
    }
    done as u64
}

/// The tun ioctls Firecracker issues.
pub fn ioctl(req: u32, arg: u64) -> u64 {
    let r = ioctl_inner(req, arg);
    if IOCTLS.fetch_add(1, Ordering::Relaxed) < 16 {
        akuma_primitives::safe_print!(96, "[tun] ioctl {:#x} arg={:#x} -> {}\n", req, arg, r as i64);
    }
    r
}

static IOCTLS: AtomicU32 = AtomicU32::new(0);

fn ioctl_inner(req: u32, arg: u64) -> u64 {
    match req {
        TUNSETIFF => {
            let mut ifr = [0u8; IFREQ_LEN];
            if !crate::uaccess::read_bytes(arg, &mut ifr) {
                return errno::EFAULT;
            }
            let flags = u16::from_le_bytes([ifr[16], ifr[17]]);
            if flags & IFF_TAP == 0 {
                return errno::EINVAL;
            }
            if ifr[0] == 0 {
                ifr[..IFNAME.len()].copy_from_slice(IFNAME);
            }
            let r = with(|t| {
                t.flags = flags & (IFF_TAP | IFF_NO_PI | IFF_VNET_HDR);
                t.named = true;
            });
            if r.is_none() {
                return errno::EBADF;
            }
            if crate::uaccess::write_bytes(arg, &ifr) { 0 } else { errno::EFAULT }
        }
        TUNGETFEATURES => {
            let f = u32::from(IFF_TAP | IFF_NO_PI | IFF_VNET_HDR);
            if crate::uaccess::write_val(arg, f) { 0 } else { errno::EFAULT }
        }
        TUNGETIFF => {
            let mut ifr = [0u8; IFREQ_LEN];
            ifr[..IFNAME.len()].copy_from_slice(IFNAME);
            let fl = with(|t| t.flags).unwrap_or(0);
            ifr[16..18].copy_from_slice(&fl.to_le_bytes());
            if crate::uaccess::write_bytes(arg, &ifr) { 0 } else { errno::EFAULT }
        }
        // The guest does not negotiate checksum/TSO offload, so the frames are complete either way.
        TUNSETOFFLOAD => 0,
        TUNSETVNETHDRSZ => {
            let Some(sz) = crate::uaccess::read_val::<u32>(arg) else { return errno::EFAULT };
            let sz = sz as usize;
            if !(10..=MAX_HDR).contains(&sz) {
                return errno::EINVAL;
            }
            with(|t| t.hdr = sz).map_or(errno::EBADF, |()| 0)
        }
        TUNGETVNETHDRSZ => {
            let sz = with(|t| t.hdr as u32).unwrap_or(10);
            if crate::uaccess::write_val(arg, sz) { 0 } else { errno::EFAULT }
        }
        _ => errno::ENOTTY,
    }
}

fn stats_line(t: &Tun) {
    akuma_primitives::safe_print!(
            192,
            "[tun] writes={} reads={} eagain={} polls={} gw: frames={} arp={} dhcp={}/{} lost={} rx_delivered={} ignored={}\n",
            WRITES.load(Ordering::Relaxed),
            READS.load(Ordering::Relaxed),
            EAGAINS.load(Ordering::Relaxed),
            POLLS.load(Ordering::Relaxed),
            t.gw.stats.frames,
            t.gw.stats.arp_replies,
            t.gw.stats.dhcp_offers,
            t.gw.stats.dhcp_acks,
            t.gw.stats.lost,
            t.gw.stats.rx_delivered,
            t.gw.stats.ignored
        );
}

extern "C" fn pump_daemon() -> ! {
    let mut last = 0u32;
    let mut next_us = 0u64;
    loop {
        // A counter line every 2 s while the tap is moving frames (the census instrument).
        let now = crate::net::uptime_us();
        let sum = WRITES.load(Ordering::Relaxed) + READS.load(Ordering::Relaxed) + EAGAINS.load(Ordering::Relaxed);
        if now >= next_us && sum != last && OWNER.load(Ordering::Acquire) != 0 {
            next_us = now + 2_000_000;
            last = sum;
            let _ = with(|t| stats_line(t));
        }
        let busy = OWNER.load(Ordering::Acquire) != 0
            && with(|t| {
                let t = &mut *t;
                t.relay.poll(&mut t.gw, crate::net::uptime_us());
                t.gw.has_frame()
            })
            .unwrap_or(false);
        if busy {
            crate::sched::yield_now();
        } else {
            crate::sched::block_until_deadline(crate::net::uptime_us() + crate::lapic::US_PER_TICK_TARGET as u64);
        }
    }
}

fn spawn_pump() {
    if !PUMP_SPAWNED.swap(true, Ordering::AcqRel) && crate::sched::spawn_daemon(pump_daemon).is_none() {
        PUMP_SPAWNED.store(false, Ordering::Release);
    }
}
