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
use akuma_exec::{process, threading};
use akuma_rump::sysproxy::{Client, ClientMem, PipeIo, PipeTransport};
use akuma_rump::syscall_translation as xlate;
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

/// Mark `box_id` as using the rump network stack. Idempotent; never un-marks
/// (a later smoltcp-default spawn into the same box must not clear it).
pub fn mark_box_rump(box_id: u64) {
    RUMP_BOXES.lock().insert(box_id);
    RUMP_ACTIVE.store(true, Ordering::Relaxed);
    crate::safe_print!(64, "[RUMP-SP] box {} marked stack=rump\n", box_id);
}

/// Is this box's network stack the rump kernel?
#[must_use]
pub fn box_is_rump(box_id: u64) -> bool {
    if !RUMP_ACTIVE.load(Ordering::Relaxed) {
        return false;
    }
    RUMP_BOXES.lock().contains(&box_id)
}

/// Short name for an [`xlate::Op`] (the `safe_print!` formatter is byte-bounded
/// and `{:?}` Debug output is awkward to size, so use a fixed `&str`).
fn op_name(op: xlate::Op) -> &'static str {
    use xlate::Op;
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
/// NOTE (B1): no `--net` yet. This proves the proxy round-trip (`socket`/
/// `close`) without DHCP, which is a separate CPU-spin problem (see
/// RUMP_SYSPROXY.md "Open items"). The server's stdout goes to the box log
/// since the kernel can't drain its `ProcessChannel`.
fn setup_proxy(box_id: u64) -> Option<Arc<BoxProxy>> {
    crate::safe_print!(96, "[RUMP-SP] box={} bringing up rump_server (--fd 3)...\n", box_id);
    // px: kernel→server (server reads via its rx); py: server→kernel.
    let px = pipe::pipe_create();
    let py = pipe::pipe_create();

    let logpath = format!("/var/log/box/{box_id}/rump_server.log");
    let pid = match process::spawn_process_with_channel_ext(
        "/bin/rump_server",
        Some(&["--fd", "3", "--log", &logpath]),
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
    let op = xlate::op_from_linux_sysno(syscall_num)?;
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
    if matches!(op, xlate::Op::Read | xlate::Op::Write | xlate::Op::Close) && !fd_is_rump {
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
        xlate::Op::Socket => proxy_socket(args, proc, box_id),
        xlate::Op::Close => proxy_close(args, proc, box_id),
        // B1: connection round-trip only. The pointer-arg ops (connect/bind/
        // sendto/recvfrom/recvmsg/getsockname/{get,set}sockopt) need the real
        // user-VA `ClientMem` + Linux↔NetBSD sockaddr translation — B2. Return
        // a clean error so the box never hits smoltcp with a rump fd.
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
    let (base_type, nonblock, _cloexec) = xlate::strip_sock_type(args[1]);
    let proto = args[2];
    let Some(proxy) = ensure_box_proxy(box_id) else {
        return neg_linux_errno(LINUX_ENOMEM); // server didn't come up
    };
    let mut mem = NoMem;
    let res = proxy.with_client(|c| {
        let a = xlate::pack_args(&[2, base_type, proto]);
        c.syscall(xlate::netbsd_sysno(xlate::Op::Socket), &a, &mut mem)
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
        Err(e) => neg_linux_errno(xlate::errno_netbsd_to_linux(e)),
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
        let a = xlate::pack_args(&[rump_fd as u64]);
        c.syscall(xlate::netbsd_sysno(xlate::Op::Close), &a, &mut mem)
    });
    match res {
        Ok(_) => 0,
        Err(e) => neg_linux_errno(xlate::errno_netbsd_to_linux(e)),
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
        threading::yield_now();
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
    let args = xlate::pack_args(&[2, 1, 0]);
    let mut mem = NoMem;
    match client.syscall(xlate::netbsd_sysno(xlate::Op::Socket), &args, &mut mem) {
        Ok([fd, _]) if fd >= 0 => Ok(fd),
        Ok([fd, _]) => Err(format!("socket returned {fd}")),
        Err(e) => Err(format!("socket errno {e}")),
    }
}
