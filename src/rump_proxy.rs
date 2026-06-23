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

use alloc::collections::BTreeSet;
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
    use xlate::Op::*;
    match op {
        Socket => "socket",
        Connect => "connect",
        Bind => "bind",
        Listen => "listen",
        Accept => "accept",
        Sendto => "sendto",
        Recvfrom => "recvfrom",
        Setsockopt => "setsockopt",
        Getsockopt => "getsockopt",
        Getsockname => "getsockname",
        Getpeername => "getpeername",
        Sendmsg => "sendmsg",
        Recvmsg => "recvmsg",
        Shutdown => "shutdown",
        Socketpair => "socketpair",
        Read => "read",
        Write => "write",
        Close => "close",
    }
}

/// Diagnostic (Phase A): for a `stack=rump` box, log every socket-family
/// syscall — plus read/write/close on a socket fd — then RETURN so normal
/// dispatch (smoltcp) still runs. Non-breaking by design: it changes no control
/// flow, it only reveals the exact syscall/fd sequence the box's networked
/// programs (e.g. curl) issue, so the Phase-B proxy can be built to cover it.
/// Directly serves "we are probably not dispatching the syscalls right."
pub fn trace_box_syscall(syscall_num: u64, args: &[u64; 6]) {
    if !RUMP_ACTIVE.load(Ordering::Relaxed) {
        return; // no rump box exists → single relaxed load, no lock
    }
    let Some(op) = xlate::op_from_linux_sysno(syscall_num) else {
        return; // not a proxied syscall family
    };
    let pid = process::read_current_pid().unwrap_or(0);
    let Some(proc) = process::lookup_process(pid) else {
        return;
    };
    if !box_is_rump(proc.box_id) {
        return;
    }
    // read/write/close also hit files/pipes — only trace them on a socket fd
    // (the Phase-B proxy must likewise leave non-socket fds to normal dispatch).
    if matches!(op, xlate::Op::Read | xlate::Op::Write | xlate::Op::Close)
        && !matches!(proc.get_fd(args[0] as u32), Some(process::FileDescriptor::Socket(_)))
    {
        return;
    }
    crate::safe_print!(
        192,
        "[RUMP-SP] box={} pid={} {} nr={} a0={} a1=0x{:x} a2=0x{:x} a3=0x{:x}\n",
        proc.box_id,
        pid,
        op_name(op),
        syscall_num,
        args[0],
        args[1],
        args[2],
        args[3]
    );
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
