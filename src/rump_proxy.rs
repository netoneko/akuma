//! Kernel-side rump sysproxy client (RUMP_SYSPROXY.md Step 4).
//!
//! For a `stack=rump` box the kernel forwards the box's socket syscalls to the
//! box's `rump_server` over a kernel **pipe pair** (Akuma has no path AF_UNIX).
//! This module hosts the kernel end: a [`Transport`] over the kernel-held pipe
//! ends, and — for now — a boot demo ([`run_demo`]) that spawns `rump_server`,
//! hands it one end as fd 3, and drives one `rump_sys_socket` over the channel.
//! That is the on-Akuma proof of the kernel-pipe transport; full syscall
//! interception + per-box wiring (a real [`ClientMem`] over user VA, the fd map,
//! the `stack=rump` dispatch hook) come next.

use crate::syscall::pipe;
use akuma_exec::mmu::user_access::{copy_from_user_safe, copy_to_user_safe};
use akuma_exec::{process, threading};
use akuma_rump::sysproxy::{Client, ClientMem, PipeIo, PipeTransport, MAX_TRANSFER};
use akuma_rump::syscall_translation as translation;
use alloc::vec::Vec;

/// EFAULT (NetBSD/Linux share it).
const EFAULT: i32 = 14;
/// Cap a single blocking read so a wedged server fails the request instead of
/// hanging the boot before herd/SSH come up.
const READ_TIMEOUT_US: u64 = 8_000_000;

// ── per-box stack selection + dispatch instrumentation (Phase A) ───────────

use akuma_exec::process::Process;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use spinning_top::Spinlock;

/// Box IDs whose network stack is the NetBSD rump kernel — set via the
/// `SET_BOX_STACK` syscall when herd starts a `stack = rump` service. A box not
/// in this set uses smoltcp (the default), so its socket dispatch is unchanged.
static RUMP_BOXES: Spinlock<BTreeSet<u64>> = Spinlock::new(BTreeSet::new());

/// Fast-path guard: the per-syscall trace/dispatch hook costs a single relaxed
/// load when no `stack=rump` box exists (the common case / pre-rumpnet boot).
static RUMP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark `box_id` as using the rump network stack and EAGERLY bring up its
/// `rump_server --net` (so `rump_init` + DHCP happen at box-setup time, NOT
/// inside the box's first `socket()` — which otherwise pays the whole ~5s
/// bring-up). Idempotent; never un-marks. The first mark spawns a one-shot
/// setup kthread that waits for the tap (NIC1) then brings the proxy up in the
/// background; `ensure_box_proxy` remains a lazy fallback if a socket races in.
pub fn mark_box_rump(box_id: u64) {
    let newly = RUMP_BOXES.lock().insert(box_id);
    RUMP_ACTIVE.store(true, Ordering::Relaxed);
    if !newly {
        return;
    }
    crate::safe_print!(64, "[RUMP-SP] box {} marked stack=rump; pre-bringing-up rump_server\n", box_id);
    let _ = threading::spawn_fn(move || {
        // The tap (NIC1) must be initialized before `rump_server --net` can DHCP.
        let mut waited = 0u64;
        while !akuma_net::rump_tap::is_ready() && waited < 30_000_000 {
            threading::schedule_blocking(crate::timer::uptime_us() + 50_000);
            waited += 50_000;
        }
        let _ = ensure_box_proxy(box_id);
        threading::mark_current_terminated();
        loop {
            threading::yield_now();
        }
    });
}

/// Is this box's network stack the rump kernel?
#[must_use]
pub fn box_is_rump(box_id: u64) -> bool {
    if !RUMP_ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    RUMP_BOXES.lock().contains(&box_id)
}

/// Short name for an [`translation::Op`] (the `safe_print!` formatter is byte-bounded
/// and `{:?}` Debug output is awkward to size, so use a fixed `&str`).
fn op_name(op: translation::Op) -> &'static str {
    use translation::Op;
    match op {
        Op::Socket => "socket",
        Op::Connect => "connect",
        Op::Bind => "bind",
        Op::Listen => "listen",
        Op::Accept => "accept",
        Op::Sendto => "sendto",
        Op::Recvfrom => "recvfrom",
        Op::Setsockopt => "setsockopt",
        Op::Getsockopt => "getsockopt",
        Op::Getsockname => "getsockname",
        Op::Getpeername => "getpeername",
        Op::Sendmsg => "sendmsg",
        Op::Recvmsg => "recvmsg",
        Op::Shutdown => "shutdown",
        Op::Socketpair => "socketpair",
        Op::Read => "read",
        Op::Write => "write",
        Op::Close => "close",
    }
}

// ── per-box proxy state + lazy bring-up (Phase B, approach 1) ──────────────

/// rump_server readiness: `rump_init` + its ~19 kthread spawns take a while, so
/// the handshake read tolerates a long stall before declaring the server dead.
const HANDSHAKE_TIMEOUT_US: u64 = 15_000_000;

type ProxyClient = Client<PipeTransport<KernelPipeIo>>;

/// One box's live sysproxy connection to its `rump_server` (approach 1: the
/// box's own syscall thread drives the round-trip synchronously).
pub struct BoxProxy {
    /// The handshaken client. Driven under cooperative mutual exclusion: a
    /// caller *takes* it out of the slot (yielding while another holds it),
    /// drives one syscall, then puts it back — so the brief guarding spinlock is
    /// never held across the yielding channel read.
    client: Spinlock<Option<ProxyClient>>,
}

impl BoxProxy {
    /// Run `f` with exclusive access to the client (see field doc).
    fn with_client<R>(&self, f: impl FnOnce(&mut ProxyClient) -> R) -> R {
        let mut c = loop {
            if let Some(c) = self.client.lock().take() {
                break c;
            }
            threading::yield_now();
        };
        let r = f(&mut c);
        *self.client.lock() = Some(c);
        r
    }
}

/// Lifecycle of a box's proxy: `Initializing` serializes concurrent first
/// callers; `Failed` is sticky (don't re-spawn a server that didn't come up).
enum ProxyEntry {
    Initializing,
    Ready(Arc<BoxProxy>),
    Failed,
}

static PROXIES: Spinlock<BTreeMap<u64, ProxyEntry>> = Spinlock::new(BTreeMap::new());

/// PIDs of kernel-spawned `rump_server`s. Recorded the instant the server is
/// spawned — BEFORE the handshake — because the sysproxy server (NetBSD
/// `rumpuser_sp.c`) drives its channel fd with socket `sendto`/`recvfrom`, and
/// since the server runs inside the `stack=rump` box those calls would be
/// intercepted and routed back into itself (deadlock during bring-up). Excluded
/// here, they fall through to normal dispatch, which handles the pipe-backed
/// `UnixSocket` channel fd — exactly as the proven box-0 `run_demo` does.
static SERVER_PIDS: Spinlock<BTreeSet<process::Pid>> = Spinlock::new(BTreeSet::new());

/// Is `pid` a kernel-spawned `rump_server`? Its own syscalls must never be
/// proxied (it IS the proxy target). True throughout its life, incl. bring-up.
fn is_server_pid(pid: process::Pid) -> bool {
    SERVER_PIDS.lock().contains(&pid)
}

/// Get (or lazily bring up) the proxy for a `stack=rump` box. The first caller
/// wins the `Initializing` slot and runs [`setup_proxy`]; concurrent callers
/// yield until it publishes `Ready`/`Failed`.
fn ensure_box_proxy(box_id: u64) -> Option<Arc<BoxProxy>> {
    loop {
        let we_init = {
            let mut m = PROXIES.lock();
            match m.get(&box_id) {
                Some(ProxyEntry::Ready(p)) => return Some(p.clone()),
                Some(ProxyEntry::Failed) => return None,
                Some(ProxyEntry::Initializing) => false,
                None => {
                    m.insert(box_id, ProxyEntry::Initializing);
                    true
                }
            }
        };
        if we_init {
            let result = setup_proxy(box_id);
            let mut m = PROXIES.lock();
            m.insert(
                box_id,
                match &result {
                    Some(p) => ProxyEntry::Ready(p.clone()),
                    None => ProxyEntry::Failed,
                },
            );
            return result;
        }
        threading::yield_now(); // another thread is bringing it up
    }
}

/// Bring up a box's `rump_server` + sysproxy channel (kernel-owned). Creates a
/// pipe pair, spawns `/bin/rump_server --fd 3 --log …` into the box, installs
/// the server end at fd 3, and runs the guest handshake.
///
/// B3: `--net` brings the NetBSD stack online (`virt0` + DHCP over
/// `/dev/net/tap0`, needs `RUMP_NIC=1`) so `connect`/`sendto`/`recvfrom` reach
/// the real wire. The sysproxy banner (which the handshake awaits) is printed
/// AFTER DHCP, so the handshake read tolerates the DHCP delay (see
/// `HANDSHAKE_TIMEOUT_US`). The server's stdout goes to the box log since the
/// kernel can't drain its `ProcessChannel`.
fn setup_proxy(box_id: u64) -> Option<Arc<BoxProxy>> {
    crate::safe_print!(96, "[RUMP-SP] box={} bringing up rump_server (--fd 3 --net)...\n", box_id);
    // px: kernel→server (server reads via its rx); py: server→kernel.
    let px = pipe::pipe_create();
    let py = pipe::pipe_create();

    let logpath = format!("/var/log/box/{box_id}/rump_server.log");
    let pid = match process::spawn_process_with_channel_ext(
        "/bin/rump_server",
        Some(&["--fd", "3", "--net", "--log", &logpath]),
        None,
        None,
        Some("/"),
        box_id,
    ) {
        Ok((_tid, _chan, pid)) => pid,
        Err(e) => {
            crate::safe_print!(128, "[RUMP-SP] box={} rump_server spawn failed: {}\n", box_id, e);
            return None;
        }
    };

    // Exclude the server from interception NOW, before it can run (its handshake
    // I/O uses socket sendto/recvfrom on the channel fd — see SERVER_PIDS).
    SERVER_PIDS.lock().insert(pid);

    // Install the server end at fd 3 BEFORE it runs (single-core: the child is
    // not scheduled until our first blocking handshake read yields).
    let Some(server) = process::lookup_process(pid) else {
        crate::safe_print!(96, "[RUMP-SP] box={} lookup_process failed\n", box_id);
        SERVER_PIDS.lock().remove(&pid);
        let _ = process::kill_process(pid);
        return None;
    };
    server.set_fd(3, process::FileDescriptor::UnixSocket { rx: px, tx: py });

    let chan = PipeTransport { io: KernelPipeIo, wr: px, rd: py, timeout_us: HANDSHAKE_TIMEOUT_US };
    match Client::connect(chan, b"akuma-kernel") {
        Ok(client) => {
            crate::safe_print!(96, "[RUMP-SP] box={} rump_server ready (pid={})\n", box_id, pid);
            Some(Arc::new(BoxProxy { client: Spinlock::new(Some(client)) }))
        }
        Err(e) => {
            crate::safe_print!(96, "[RUMP-SP] box={} handshake failed errno={}\n", box_id, e);
            SERVER_PIDS.lock().remove(&pid);
            let _ = process::kill_process(pid);
            None
        }
    }
}

// ── dispatch interception ──────────────────────────────────────────────────

/// Linux errnos returned to the box (negated, as the syscall ABI expects).
fn neg_linux_errno(e: i32) -> u64 {
    (-i64::from(e)) as u64
}
const LINUX_EBADF: i32 = 9;
const LINUX_EFAULT: i32 = 14;
const LINUX_EINVAL: i32 = 22;
const LINUX_ENOMEM: i32 = 12;
const LINUX_EOPNOTSUPP: i32 = 95;
const LINUX_EAFNOSUPPORT: i32 = 97;

/// Intercept a `stack=rump` box's socket-family syscall and forward it to the
/// box's `rump_server`. Returns `Some(result)` to short-circuit normal smoltcp
/// dispatch, or `None` to fall through (non-rump box, non-socket syscall, or a
/// non-rump fd for read/write/close). Also emits the `[RUMP-SP]` trace.
pub fn intercept_box_syscall(syscall_num: u64, args: &[u64; 6]) -> Option<u64> {
    if !RUMP_ACTIVE.load(Ordering::Relaxed) {
        return None; // no rump box exists → single relaxed load, no lock
    }
    let op = translation::op_from_linux_sysno(syscall_num)?;
    let pid = process::read_current_pid().unwrap_or(0);
    let proc: &Process = process::lookup_process(pid)?;
    let box_id = proc.box_id;
    if !box_is_rump(box_id) {
        return None;
    }

    // read/write/close also hit files/pipes — only a rump socket fd is ours.
    let fd_is_rump = matches!(
        proc.get_fd(args[0] as u32),
        Some(process::FileDescriptor::RumpSocket { .. })
    );
    if matches!(op, translation::Op::Read | translation::Op::Write | translation::Op::Close) && !fd_is_rump {
        return None;
    }
    // Never proxy the box's own rump_server back into itself.
    if is_server_pid(pid) {
        return None;
    }

    crate::safe_print!(
        160,
        "[RUMP-SP] route box={} pid={} {} fd={} a1=0x{:x} a2=0x{:x}\n",
        box_id,
        pid,
        op_name(op),
        args[0],
        args[1],
        args[2]
    );

    Some(match op {
        translation::Op::Socket => proxy_socket(args, proc, box_id),
        translation::Op::Close => proxy_close(args, proc, box_id),
        translation::Op::Connect => proxy_connect(args, proc, box_id),
        translation::Op::Getsockname => proxy_getname(args, proc, box_id, translation::Op::Getsockname),
        translation::Op::Getpeername => proxy_getname(args, proc, box_id, translation::Op::Getpeername),
        translation::Op::Getsockopt => proxy_getsockopt(args, proc),
        translation::Op::Setsockopt => proxy_setsockopt(args, proc),
        translation::Op::Sendto => proxy_transfer(args, proc, box_id, translation::Op::Sendto),
        translation::Op::Recvfrom => proxy_transfer(args, proc, box_id, translation::Op::Recvfrom),
        translation::Op::Read => proxy_transfer(args, proc, box_id, translation::Op::Read),
        translation::Op::Write => proxy_transfer(args, proc, box_id, translation::Op::Write),
        // Not on the curl HTTP-to-IP path yet (bind/listen/accept/shutdown/
        // sendmsg/recvmsg — the last two incl. DNS's UDP recvmsg). Clean error so
        // the box never reaches smoltcp with a rump fd.
        _ => neg_linux_errno(LINUX_EOPNOTSUPP),
    })
}

/// `socket(domain, type, proto)` → a rump socket fd. Only `AF_INET` is proxied;
/// `AF_INET6` (and other families) return `EAFNOSUPPORT` so the box falls back
/// to IPv4 (curl's first call is an `AF_INET6` probe — see the Phase-A trace).
fn proxy_socket(args: &[u64; 6], proc: &Process, box_id: u64) -> u64 {
    let domain = args[0] as i32;
    if domain != 2 {
        return neg_linux_errno(LINUX_EAFNOSUPPORT);
    }
    let (base_type, nonblock, _cloexec) = translation::strip_sock_type(args[1]);
    let proto = args[2];
    let Some(proxy) = ensure_box_proxy(box_id) else {
        return neg_linux_errno(LINUX_ENOMEM); // server didn't come up
    };
    let mut mem = NoMem;
    let res = proxy.with_client(|c| {
        let a = translation::pack_args(&[2, base_type, proto]);
        c.syscall(translation::netbsd_sysno(translation::Op::Socket), &a, &mut mem)
    });
    match res {
        Ok([fd, _]) if fd >= 0 => {
            let box_fd = proc.alloc_fd(process::FileDescriptor::RumpSocket {
                rump_fd: fd as i32,
                nonblock,
            });
            u64::from(box_fd)
        }
        Ok(_) => neg_linux_errno(LINUX_EOPNOTSUPP),
        Err(e) => neg_linux_errno(translation::errno_netbsd_to_linux(e)),
    }
}

/// `close(fd)` on a rump socket: drop the box fd, then close the rump fd on the
/// server. The local drop happens first so the fd is freed even if the server
/// is gone.
fn proxy_close(args: &[u64; 6], proc: &Process, box_id: u64) -> u64 {
    let box_fd = args[0] as u32;
    let Some(process::FileDescriptor::RumpSocket { rump_fd, .. }) = proc.get_fd(box_fd) else {
        return neg_linux_errno(LINUX_EBADF);
    };
    proc.remove_fd(box_fd);
    let Some(proxy) = ensure_box_proxy(box_id) else {
        return 0; // server gone; fd already dropped locally
    };
    let mut mem = NoMem;
    let res = proxy.with_client(|c| {
        let a = translation::pack_args(&[rump_fd as u64]);
        c.syscall(translation::netbsd_sysno(translation::Op::Close), &a, &mut mem)
    });
    match res {
        Ok(_) => 0,
        Err(e) => neg_linux_errno(translation::errno_netbsd_to_linux(e)),
    }
}

// ── B2: TCP-path marshaling + user-VA ClientMem ────────────────────────────

/// NetBSD EFAULT (== Linux EFAULT) for [`ClientMem`] copy failures.
const NETBSD_EFAULT: i32 = 14;

/// [`ClientMem`] over the calling box process's user VA (`current` TTBR0 — valid
/// because approach 1 drives on the box's own syscall thread, so its page tables
/// are active when the server's copyin/copyout callbacks run).
///
/// `cin_override` serves pre-translated bytes for a pointer arg (e.g. the NetBSD
/// `sockaddr_in` built from the box's Linux one for `connect`); `cout_sockaddr`
/// marks result addresses whose NetBSD `sockaddr_in` is translated back to Linux
/// before the user write (`getsockname`/`getpeername`). Plain buffers (send/recv
/// data, the socklen word) pass through verbatim. Sizes are capped by
/// [`MAX_TRANSFER`]; bad addresses fault safely via `copy_*_user_safe`.
struct ProcMem {
    cin_override: BTreeMap<u64, Vec<u8>>,
    cout_sockaddr: BTreeSet<u64>,
}

impl ProcMem {
    fn new() -> Self {
        Self { cin_override: BTreeMap::new(), cout_sockaddr: BTreeSet::new() }
    }
}

impl ClientMem for ProcMem {
    fn copyin(&mut self, addr: u64, len: usize, out: &mut Vec<u8>) -> Result<(), i32> {
        if len > MAX_TRANSFER {
            return Err(NETBSD_EFAULT);
        }
        out.clear();
        if let Some(b) = self.cin_override.get(&addr) {
            let n = len.min(b.len());
            out.extend_from_slice(&b[..n]);
            return Ok(());
        }
        out.resize(len, 0);
        if unsafe { copy_from_user_safe(out.as_mut_ptr(), addr as *const u8, len).is_err() } {
            return Err(NETBSD_EFAULT);
        }
        Ok(())
    }

    fn copyinstr(&mut self, addr: u64, max: usize, out: &mut Vec<u8>) -> Result<(), i32> {
        out.clear();
        for i in 0..max.min(MAX_TRANSFER) {
            let mut b = [0u8; 1];
            if unsafe {
                copy_from_user_safe(b.as_mut_ptr(), (addr + i as u64) as *const u8, 1).is_err()
            } {
                return Err(NETBSD_EFAULT);
            }
            out.push(b[0]);
            if b[0] == 0 {
                break;
            }
        }
        Ok(())
    }

    fn copyout(&mut self, addr: u64, data: &[u8]) -> Result<(), i32> {
        // A result sockaddr: translate NetBSD → Linux before writing the box VA.
        if self.cout_sockaddr.contains(&addr)
            && let Some(li) = translation::sockaddr_in_netbsd_to_linux(data)
        {
            return if unsafe { copy_to_user_safe(addr as *mut u8, li.as_ptr(), li.len()).is_err() } {
                Err(NETBSD_EFAULT)
            } else {
                Ok(())
            };
        }
        if unsafe { copy_to_user_safe(addr as *mut u8, data.as_ptr(), data.len()).is_err() } {
            return Err(NETBSD_EFAULT);
        }
        Ok(())
    }

    fn anonmmap(&mut self, _len: usize) -> u64 {
        0 // unused on the TCP path (sendto/recvfrom); recvmsg/DNS comes later
    }
}

/// Resolve a box socket fd → (its proxy, the server's rump fd), or a negated
/// Linux errno to return to the box.
fn proxy_and_fd(
    args: &[u64; 6],
    proc: &Process,
    box_id: u64,
) -> Result<(Arc<BoxProxy>, i32), u64> {
    let rump_fd = match proc.get_fd(args[0] as u32) {
        Some(process::FileDescriptor::RumpSocket { rump_fd, .. }) => rump_fd,
        _ => return Err(neg_linux_errno(LINUX_EBADF)),
    };
    match ensure_box_proxy(box_id) {
        Some(p) => Ok((p, rump_fd)),
        None => Err(neg_linux_errno(LINUX_ENOMEM)),
    }
}

/// `connect(fd, addr, len)` → translate the box's Linux `sockaddr_in` to NetBSD
/// (served via `cin_override`) and forward. The rump socket is kept blocking, so
/// this completes synchronously (no EINPROGRESS dance). Reaches the wire only
/// once the server runs with `--net` (else `ENETUNREACH`).
fn proxy_connect(args: &[u64; 6], proc: &Process, box_id: u64) -> u64 {
    let (proxy, rump_fd) = match proxy_and_fd(args, proc, box_id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let addr_ptr = args[1];
    if (args[2] as usize) < 16 {
        return neg_linux_errno(LINUX_EINVAL);
    }
    let mut lin = [0u8; 16];
    if unsafe { copy_from_user_safe(lin.as_mut_ptr(), addr_ptr as *const u8, 16).is_err() } {
        return neg_linux_errno(LINUX_EFAULT);
    }
    let Some(nb) = translation::sockaddr_in_linux_to_netbsd(&lin) else {
        return neg_linux_errno(LINUX_EAFNOSUPPORT);
    };
    let mut mem = ProcMem::new();
    mem.cin_override.insert(addr_ptr, nb.to_vec());
    // DEBUG: dest from the translated NetBSD sockaddr (len,fam,port-hi,port-lo,ip…).
    crate::safe_print!(
        128,
        "[RUMP-SP] connect dest len={} fam={} port={} ip={}.{}.{}.{}\n",
        nb[0], nb[1],
        (u16::from(nb[2]) << 8) | u16::from(nb[3]),
        nb[4], nb[5], nb[6], nb[7]
    );
    let t0 = crate::timer::uptime_us();
    let res = proxy.with_client(|c| {
        let a = translation::pack_args(&[rump_fd as u64, addr_ptr, 16]);
        c.syscall(translation::netbsd_sysno(translation::Op::Connect), &a, &mut mem)
    });
    let dt = crate::timer::uptime_us().saturating_sub(t0);
    match res {
        Ok(r) => {
            crate::safe_print!(96, "[RUMP-SP] connect -> OK r0={} ({}us)\n", r[0], dt);
            0
        }
        Err(e) => {
            crate::safe_print!(96, "[RUMP-SP] connect -> errno {} after {}us (timeout={})\n", e, dt, READ_TIMEOUT_US);
            neg_linux_errno(translation::errno_netbsd_to_linux(e))
        }
    }
}

/// `getsockname`/`getpeername(fd, addr, len)` → forward; the result NetBSD
/// `sockaddr_in` is translated back to Linux via `cout_sockaddr`.
fn proxy_getname(args: &[u64; 6], proc: &Process, box_id: u64, op: translation::Op) -> u64 {
    let (proxy, rump_fd) = match proxy_and_fd(args, proc, box_id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let addr_ptr = args[1];
    let len_ptr = args[2];
    if addr_ptr == 0 || len_ptr == 0 {
        return neg_linux_errno(LINUX_EINVAL);
    }
    let mut mem = ProcMem::new();
    mem.cout_sockaddr.insert(addr_ptr);
    let res = proxy.with_client(|c| {
        let a = translation::pack_args(&[rump_fd as u64, addr_ptr, len_ptr]);
        c.syscall(translation::netbsd_sysno(op), &a, &mut mem)
    });
    match res {
        Ok(_) => 0,
        Err(e) => neg_linux_errno(translation::errno_netbsd_to_linux(e)),
    }
}

/// `getsockopt`: special-case `SO_ERROR` (the only one curl needs). Since the
/// rump socket is blocking, `connect` already finished synchronously → no
/// pending error, so report 0. Other options return `EOPNOTSUPP` (curl tolerates
/// it — it ignored the `EOPNOTSUPP` on `setsockopt`). Level/optname values differ
/// Linux↔NetBSD, so forwarding the rest would need a translation table (later).
fn proxy_getsockopt(args: &[u64; 6], proc: &Process) -> u64 {
    if !matches!(proc.get_fd(args[0] as u32), Some(process::FileDescriptor::RumpSocket { .. })) {
        return neg_linux_errno(LINUX_EBADF);
    }
    let (level, optname, optval_ptr, optlen_ptr) = (args[1], args[2], args[3], args[4]);
    // SO_ERROR: Linux SOL_SOCKET=1, SO_ERROR=4.
    if level == 1 && optname == 4 && optval_ptr != 0 {
        let zero: i32 = 0;
        let four: u32 = 4;
        let ok = unsafe {
            copy_to_user_safe(optval_ptr as *mut u8, (&raw const zero).cast::<u8>(), 4).is_ok()
                && (optlen_ptr == 0
                    || copy_to_user_safe(optlen_ptr as *mut u8, (&raw const four).cast::<u8>(), 4)
                        .is_ok())
        };
        return if ok { 0 } else { neg_linux_errno(LINUX_EFAULT) };
    }
    neg_linux_errno(LINUX_EOPNOTSUPP)
}

/// `setsockopt`: best-effort no-op. curl tolerates failure (TCP_NODELAY/keepalive
/// are optimizations), and level/optname differ Linux↔NetBSD; returning success
/// avoids both a translation table and a spurious curl abort.
fn proxy_setsockopt(args: &[u64; 6], proc: &Process) -> u64 {
    if !matches!(proc.get_fd(args[0] as u32), Some(process::FileDescriptor::RumpSocket { .. })) {
        return neg_linux_errno(LINUX_EBADF);
    }
    0
}

/// Data transfer on a connected rump socket: `sendto`/`recvfrom` (curl's TCP I/O)
/// and `read`/`write` (other programs). `buf`=args[1], `len`=args[2] for all
/// four; flags + dest addr are ignored (connected TCP, rump socket blocking — no
/// MSG_NOSIGNAL needed). Returns the byte count.
fn proxy_transfer(args: &[u64; 6], proc: &Process, box_id: u64, op: translation::Op) -> u64 {
    let (proxy, rump_fd) = match proxy_and_fd(args, proc, box_id) {
        Ok(x) => x,
        Err(e) => return e,
    };
    let (buf_ptr, len) = (args[1], args[2]);
    let nb_args = match op {
        // sendto/recvfrom(s, buf, len, flags=0, addr=NULL, addrlen=0)
        translation::Op::Sendto | translation::Op::Recvfrom => {
            translation::pack_args(&[rump_fd as u64, buf_ptr, len, 0, 0, 0])
        }
        // read/write(fd, buf, len)
        _ => translation::pack_args(&[rump_fd as u64, buf_ptr, len]),
    };
    let mut mem = ProcMem::new();
    let t0 = crate::timer::uptime_us();
    let res = proxy.with_client(|c| c.syscall(translation::netbsd_sysno(op), &nb_args, &mut mem));
    let dt = crate::timer::uptime_us().saturating_sub(t0);
    match res {
        Ok([n, _]) => {
            crate::safe_print!(96, "[RUMP-SP] {} len={} -> {} ({}us)\n", op_name(op), len, n, dt);
            n as u64
        }
        Err(e) => {
            crate::safe_print!(96, "[RUMP-SP] {} -> errno {} ({}us)\n", op_name(op), e, dt);
            neg_linux_errno(translation::errno_netbsd_to_linux(e))
        }
    }
}

/// Kernel [`PipeIo`]: wraps the kernel pipe API + scheduler yield + clock. The
/// blocking read loop (poll + yield + timeout) lives in `akuma_rump` (host-tested
/// via a mock `PipeIo`); this just supplies the real primitives.
struct KernelPipeIo;
impl PipeIo for KernelPipeIo {
    fn read(&mut self, id: u32, buf: &mut [u8]) -> (usize, bool) {
        pipe::pipe_read(id, buf)
    }
    fn write(&mut self, id: u32, buf: &[u8]) -> bool {
        pipe::pipe_write(id, buf).is_ok()
    }
    fn yield_now(&mut self) {
        // Don't busy-spin: the rump_server's poll-loop thread (and its tap-RX
        // thread) share this single core, and a tight `yield_now` loop here
        // starves them — every channel round-trip stretched to ~0.5-6s because
        // the server couldn't get scheduled to read our request / write its
        // reply. Sleep a short interval instead so the core goes to the server;
        // we re-check the channel each timer tick. (Proper fix later: wake on
        // pipe-write so this is event-driven, not a ~tick poll.)
        threading::schedule_blocking(crate::timer::uptime_us() + 1_000);
    }
    fn now_us(&mut self) -> u64 {
        crate::timer::uptime_us()
    }
}

/// A no-op [`ClientMem`] for syscalls with no pointer args (e.g. `socket()`).
/// The real per-box accessor over the calling process's user VA arrives with the
/// interception wiring.
struct NoMem;
impl ClientMem for NoMem {
    fn copyin(&mut self, _a: u64, _l: usize, _o: &mut Vec<u8>) -> Result<(), i32> {
        Err(EFAULT)
    }
    fn copyinstr(&mut self, _a: u64, _m: usize, _o: &mut Vec<u8>) -> Result<(), i32> {
        Err(EFAULT)
    }
    fn copyout(&mut self, _a: u64, _d: &[u8]) -> Result<(), i32> {
        Err(EFAULT)
    }
    fn anonmmap(&mut self, _l: usize) -> u64 {
        0
    }
}

/// Boot demo — only when NIC1 is present (`RUMP_NIC=1`). Spawns `/bin/rump_server`,
/// hands it one end of a kernel pipe pair as fd 3, and drives a `rump_sys_socket`
/// over the other end. Prints `[Test] rump_sysproxy PASSED/FAILED`.
pub fn run_demo() {
    if !akuma_net::rump_tap::is_ready() {
        return; // no NIC1 → skip cleanly
    }
    crate::console::print("[Test] rump_sysproxy: spawning /bin/rump_server (--fd 3)...\n");

    // px: kernel → server (server reads via rx); py: server → kernel (server tx).
    // pipe_create starts each at 1 reader + 1 writer: the server fd takes one
    // slot per pipe, the kernel holds the other (no fd, via pipe_read/pipe_write).
    let px = pipe::pipe_create();
    let py = pipe::pipe_create();

    // The demo runs the server in box 0; its output goes to the box log
    // (/var/log/box/0/rump_server.log) since the kernel does not drain its
    // ProcessChannel this early in boot. `cat` it over SSH to see rump_init.
    let pid = match process::spawn_process_with_channel(
        "/bin/rump_server",
        Some(&["--fd", "3", "--log", "/var/log/box/0/rump_server.log"]),
        None,
    ) {
        Ok((_tid, _chan, pid)) => pid,
        Err(e) => {
            crate::safe_print!(96, "[Test] rump_sysproxy FAILED: spawn: {}\n", e);
            return;
        }
    };

    // Install the server end at fd 3 BEFORE it runs. Single-core: we have not
    // yielded since spawn, so the child is not scheduled until our first blocking
    // read yields — and the server only touches fd 3 after rump_init() anyway.
    let Some(p) = process::lookup_process(pid) else {
        crate::console::print("[Test] rump_sysproxy FAILED: lookup_process\n");
        let _ = process::kill_process(pid);
        return;
    };
    p.set_fd(3, process::FileDescriptor::UnixSocket { rx: px, tx: py });

    // Drive the handshake + one socket syscall, then ALWAYS tear the server down
    // (kill from outside — cascades to its ~19 rump kthreads) so it does not leak.
    let outcome = drive_socket(px, py);
    let _ = process::kill_process(pid);

    match outcome {
        Ok(fd) => crate::safe_print!(
            96,
            "[Test] rump_sysproxy PASSED — rump_sys_socket -> fd {} over kernel pipe\n",
            fd
        ),
        Err(msg) => crate::safe_print!(96, "[Test] rump_sysproxy FAILED — {}\n", msg),
    }
}

/// Connect over the kernel pipe pair and issue one `rump_sys_socket`. Returns the
/// rump fd, or a short failure reason (errno baked in).
fn drive_socket(px: u32, py: u32) -> Result<i64, alloc::string::String> {
    use alloc::format;
    let chan = PipeTransport { io: KernelPipeIo, wr: px, rd: py, timeout_us: READ_TIMEOUT_US };
    let mut client = Client::connect(chan, b"akuma-kernel")
        .map_err(|e| format!("handshake errno {e}"))?;
    crate::console::print("[Test] rump_sysproxy: handshake OK; rump_sys_socket...\n");

    // rump_sys_socket(AF_INET=2, SOCK_STREAM=1, 0) — no pointer args, no copyin.
    let args = translation::pack_args(&[2, 1, 0]);
    let mut mem = NoMem;
    match client.syscall(translation::netbsd_sysno(translation::Op::Socket), &args, &mut mem) {
        Ok([fd, _]) if fd >= 0 => Ok(fd),
        Ok([fd, _]) => Err(format!("socket returned {fd}")),
        Err(e) => Err(format!("socket errno {e}")),
    }
}
