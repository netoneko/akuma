//! The amd64 kernel's boot self-tests for ring 3: processes, `spawn`/`fork`/
//! `execve`, the dispatch vocabulary, the ELF loader, threads, signals.
//!
//! Moved out of `usermode.rs` 2026-11-10 (W2 of
//! `docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`), unchanged: that file was 9.3 kLoC
//! and a third of it was tests. Moved again 2026-11-11, out of the kernel into this
//! crate, unchanged but for the paths. The kernel state the tests reach (`Image`'s
//! fields, the spawn table, `reap_exec_process`, ...) is `pub` inside the kernel's
//! private `usermode` module and re-exported only through
//! `akuma_kernel_amd64::testing::usermode`, which is all this file sees.

use akuma_kernel_amd64::testing as k;
use akuma_kernel_amd64::testing::usermode::*;
use akuma_kernel_amd64::testing::{loader, serial};
use akuma_mmap::PhysFrame;
use akuma_mmu::{PteProt, UserAddressSpace};
use akuma_selftest::Suite;
use akuma_syscalls_abi::Syscall;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

/// The two constructors only the suite uses, as a trait: `Image` is the kernel's type, and a
/// crate cannot add inherent methods to a type it does not own. With the trait in scope the
/// call sites still read `Image::new(..)` / `Image::from_elf(..)`.
trait TestImage: Sized {
    fn new(msg: &[u8], rounds: u32, delay: u32, status: u32) -> Option<Self>;
    fn from_elf(image: &[u8]) -> Result<(Self, loader::LoadedImage), &'static str>;
}

/// Where the test's user code and stack live.
///
/// `0x40_0000` is where a static Linux x86_64 binary is linked by default, and
/// since Stage K the kernel no longer occupies the lower half, so a program can
/// simply be mapped where it expects to be. Before that this had to be
/// `0x5000_0000` — chosen to dodge the kernel's identity map — which is exactly
/// the constraint the higher-half move removed.
const USER_CODE_VA: usize = 0x40_0000;

const USER_STACK_VA: usize = 0x41_0000;

/// The guest program, linked at [`USER_CODE_VA`] and embedded in the kernel
/// image.
///
/// **The fallback since Stage N, not the primary.** `elf_test` reads
/// `/bin/hello` off the ext2 root when there is one, which is the interesting
/// case: an image the kernel opened by path, from a filesystem it mounted, on a
/// disk it discovered. This copy is what runs when there is no disk — `DISK=none`,
/// and every stage before Stage M — so the loader is still exercised on a
/// machine with no storage.
///
/// The two are byte-identical: `amd64/build.rs` compiles the program into
/// `OUT_DIR` and `amd64/mkdisk.sh` copies that same file into the image. That is
/// what makes the fallback honest rather than a second, drifting program — and
/// `elf_test` checks it, because "identical" is an assumption about two build
/// steps agreeing.
const HELLO_ELF: &[u8] = include_bytes!(env!("USER_HELLO_ELF"));

/// The `clone`/`futex` probe. See `usermode::thread_test`.
const THREADPROBE_ELF: &[u8] = include_bytes!(env!("USER_THREADPROBE_ELF"));

/// A threaded process to `SIGKILL`. See `usermode::kill_test`.
const KILLPROBE_ELF: &[u8] = include_bytes!(env!("USER_KILLPROBE_ELF"));

/// The signal register-file probe. See `usermode::sigregs_test`.
const SIGREGS_ELF: &[u8] = include_bytes!(env!("USER_SIGREGS_ELF"));

/// Emit the user program into `out`, returning its total length.
///
/// Built rather than written as a byte literal so the message address and the
/// loop displacement are *computed*. A hand-assembled blob with hardcoded
/// operands stays correct until someone edits the message by one character.
///
/// ```text
///   mov r12, <rounds>
/// loop:
///   mov rax, <write>; mov rdi, 1; movabs rsi, <msg>; mov rdx, <len>; syscall
///   mov rax, <sched_yield>; syscall     ; hand the CPU to the other process
///   dec r12
///   jnz loop
///   mov rax, <exit_group>; mov rdi, <status>; syscall
///   jmp $                               ; a guard, not a fallthrough
///   <message bytes>
/// ```
fn build_user_program(
    out: &mut [u8],
    base_va: u64,
    msg: &[u8],
    rounds: u32,
    delay: u32,
    status: u32,
) -> usize {
    let mut n = 0;
    let mut emit = |bytes: &[u8], n: &mut usize| {
        out[*n..*n + bytes.len()].copy_from_slice(bytes);
        *n += bytes.len();
    };

    // mov r64, imm32 (sign-extended). The ModRM byte selects the destination.
    let mov_imm = |modrm: u8, v: u32| {
        let b = v.to_le_bytes();
        [0x48, 0xC7, modrm, b[0], b[1], b[2], b[3]]
    };
    const RAX: u8 = 0xC0;
    const RDI: u8 = 0xC7;
    const RDX: u8 = 0xC2;

    // mov r12, imm32 needs REX.WB (0x49) since r12 is an extended register.
    let r = rounds.to_le_bytes();
    emit(&[0x49, 0xC7, 0xC4, r[0], r[1], r[2], r[3]], &mut n);

    let loop_start = n;
    emit(&mov_imm(RAX, Syscall::Write.to_x86_64() as u32), &mut n);
    emit(&mov_imm(RDI, 1), &mut n);
    let movabs_at = n;
    emit(&[0x48, 0xBE, 0, 0, 0, 0, 0, 0, 0, 0], &mut n); // movabs rsi, msg
    emit(&mov_imm(RDX, msg.len() as u32), &mut n);
    emit(&[0x0F, 0x05], &mut n); // syscall

    if delay == 0 {
        // Cooperative: hand the CPU over explicitly.
        emit(&mov_imm(RAX, Syscall::SchedYield.to_x86_64() as u32), &mut n);
        emit(&[0x0F, 0x05], &mut n); // syscall
    } else {
        // Preemptive: burn time in ring 3 and never yield, so the only way this
        // process can stop running is the timer taking it off the CPU.
        //   mov r13, delay
        // spin:
        //   dec r13
        //   jnz spin
        let d = delay.to_le_bytes();
        emit(&[0x49, 0xC7, 0xC5, d[0], d[1], d[2], d[3]], &mut n);
        let spin = n;
        emit(&[0x49, 0xFF, 0xCD], &mut n); // dec r13
        let back = (n + 2) - spin;
        emit(&[0x75, (back as u8).wrapping_neg()], &mut n);
    }

    emit(&[0x49, 0xFF, 0xCC], &mut n); // dec r12
    // jnz rel8, back to loop_start. The displacement is measured from the *end*
    // of the jump, hence the +2 for the instruction's own bytes. Computed as a
    // positive distance and negated, so no signed cast is needed and the range
    // check is on a value that cannot already have wrapped.
    let back = (n + 2) - loop_start;
    debug_assert!(back <= 127, "loop body outgrew a rel8 jump");
    emit(&[0x75, (back as u8).wrapping_neg()], &mut n);

    emit(&mov_imm(RAX, Syscall::ExitGroup.to_x86_64() as u32), &mut n);
    emit(&mov_imm(RDI, status), &mut n);
    emit(&[0x0F, 0x05], &mut n); // syscall
    emit(&[0xEB, 0xFE], &mut n); // jmp $

    let msg_off = n;
    emit(msg, &mut n);

    let msg_va = base_va + msg_off as u64;
    out[movabs_at + 2..movabs_at + 10].copy_from_slice(&msg_va.to_le_bytes());
    n
}

impl TestImage for Image {
    /// Build an image that prints `msg` `rounds` times then exits with
    /// `status`.
    ///
    /// `delay == 0` yields between rounds (cooperative); anything else spins
    /// that many iterations in ring 3 and never yields, so only preemption can
    /// take it off the CPU.
    ///
    /// The program is assembled byte by byte by [`build_user_program`] rather
    /// than loaded from an image. That is deliberate even now that
    /// [`Self::from_elf`] exists: these two tests are about the scheduler and
    /// the timer, and a blob with no file format between it and the page table
    /// cannot fail for a loader's reasons.
    fn new(msg: &[u8], rounds: u32, delay: u32, status: u32) -> Option<Self> {
        let mut space = UserAddressSpace::new()?;

        // Dropping `space` on either failure arm below releases its tables and
        // whatever the ledger has taken on — the explicit `space.free()` +
        // `free_all_frames` pair this replaces.
        let (Some(mut code), Some(stack)) =
            (k::phys::FreshFrame::alloc_zeroed(), k::phys::FreshFrame::alloc_zeroed())
        else {
            return None;
        };

        // Staged *before* the address space that will hold it is ever activated.
        build_user_program(code.bytes_mut(), USER_CODE_VA as u64, msg, rounds, delay, status);

        // Each frame is published the moment its own map succeeds:
        // `map_and_track_pte` records it first and untracks it again if the map
        // fails, so a refusal leaves the failing frame to the handle's `Drop`, and
        // a frame already tracked is `space`'s to free, not ours.
        if !space.map_and_track_pte(USER_CODE_VA, PhysFrame::new(code.pa().as_usize()), PteProt::USER_RX, false) {
            return None;
        }
        let _ = code.publish();
        if !space.map_and_track_pte(USER_STACK_VA, PhysFrame::new(stack.pa().as_usize()), PteProt::USER_RW, false) {
            return None;
        }
        let _ = stack.publish();
        Some(Self {
            space,
            entry: USER_CODE_VA as u64,
            stack: (USER_STACK_VA + 4096 - 16) as u64,
            regions: Vec::new(),
        })
    }

    /// Build a process from a linked ELF image.
    ///
    /// The address space comes back **from** the loader rather than going into
    /// it: since C1 step 6 the loading is `akuma-elf`'s, and that crate builds
    /// the space itself because the image's own headers decide what it needs.
    /// A failed load therefore drops the space inside the crate, where its
    /// destructor returns every frame the half-finished load had taken — the
    /// property `elf: rejected loads leak nothing` checks.
    /// Returns the process and what the loader found, so a caller can check the
    /// placement as well as the outcome.
    fn from_elf(image: &[u8]) -> Result<(Self, loader::LoadedImage), &'static str> {
        Self::from_elf_argv(image, &[b"hello"])
    }
}

/// Start a **self-test** process: register it, then publish its task.
///
/// The six boot self-tests that run ring-3 programs used to write an
/// `Option<Process>` into `PROCS[slot]` and spawn a task over it. They register
/// with `akuma-exec` now, like every other process — not for tidiness but
/// because they must: `with_current_regions` and `with_current_address_space`
/// resolve through the process table since 5b slice 4, and `fdprobe` and
/// `threadprobe` both `mmap`. An unregistered self-test process would have got
/// `None` from the accessors and failed with no explanation.
///
/// Registered before [`k::sched::publish_task`], the same order every other
/// caller uses, and returning the `(pid, task_slot)` pair the teardown needs.
/// The parent is pid 1: these run before `run_init`, so there is no real
/// parent, and 1 is what `current_pid()` answers for the boot task driving them.
fn start_test_process(
    slot: usize,
    image: Image,
    image_top: u64,
    name: &str,
) -> Option<(u32, usize)> {
    let root = image.space.ttbr0();
    let table_slot = akuma_exec::process::reserve_process_slot()?;
    let task_slot = spawn_process_task(slot, root)?;
    let pid = alloc_pid();
    let mut cmdline = alloc::vec::Vec::with_capacity(name.len() + 1);
    cmdline.extend_from_slice(name.as_bytes());
    cmdline.push(0);
    register_exec_process(pid, 1, task_slot, image, image_top, name, &cmdline, None, None, None, table_slot);
    k::sched::publish_task(task_slot);
    Some((pid, task_slot))
}

/// Drive the scheduler until self-test process `pid` has exited, and return
/// **its own** exit status (`u64::MAX` if it did not exit within `spins`).
///
/// Read from the process's exit channel, which `run_process` marks with the
/// full status after it has drained the process's threads. The tests used to
/// read a global "last exit status" instead, which every process on every core
/// stored as it left ring 3 — a forked child or a straggler from an earlier
/// test could overwrite the probe's status between the reset and the read, and
/// the thread probe intermittently reported `0x0`. A `waitpid` status would not
/// do either: it keeps 8 bits, and the probes report 9- and 12-bit masks.
fn wait_test_exit(pid: u32, spins: u64) -> u64 {
    let Some(ch) = akuma_exec::process::get_child_channel(pid) else {
        return u64::MAX;
    };
    let mut n = 0u64;
    while !ch.has_exited() && n < spins {
        n += 1;
        k::sched::yield_now();
    }
    if ch.has_exited() { i64::from(ch.exit_code()).cast_unsigned() } else { u64::MAX }
}

/// Tear a self-test process down and free its address space **before the next
/// line runs**.
///
/// Every one of those tests ends by asserting that teardown leaked nothing —
/// `akuma_pmm::free_count()` back where it started — and until 5b slice 4 that
/// worked because the test dropped the `Process` itself. The address space
/// belongs to the registered process now, and `unregister_process` only
/// *retires* it; something has to collect. This target sets
/// `process_reclaim_cooldown_us: 0` (`exec_runtime.rs`), so the drain here
/// frees immediately, which is what keeps those assertions meaning what they
/// said. A test that merely retired would report a leak of the whole image.
///
/// `drain_retired_if_requested`, not the `_force` variant: the cooldown is
/// already zero, so forcing would only remove the guard, and the same call is
/// what every production drain site uses. It also drains the TTBR-deferred
/// frame list, which is where an address space's frames go if another core's
/// `CR3` still stands on its L0.
fn finish_test_process(pid: u32, task_slot: usize) {
    reap_exec_process(pid, task_slot);
    akuma_exec::process::reclaim::drain_retired_if_requested();
}

/// Stage R: `sys_spawn` runs a child, its stdout comes back through a pipe, and
/// `waitpid` reports its exit status.
///
/// Spawns `/bin/hello` — the same image `elf_test` runs, but this time its
/// stdout is a pipe rather than the console and its exit status arrives through
/// `waitpid` rather than the process's exit channel. The self-test calls
/// `sys_spawn` with kernel pointers, which is fine: `user_cstr` just does
/// volatile reads and the kernel may read its own memory.
pub fn spawn_test(t: &mut Suite) {
    /// Every check `hello.rs` reports, all passing (bits 0..=6).
    const HELLO_ALL_OK: u64 = 0x7F;

    let free_before = akuma_pmm::free_count();

    // A syscall's "negative" return is an errno in `-1..=-4095`, i.e. a u64 at
    // the very top of the range; anything below that is a real value.
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;

    let path = b"/bin/hello\0";
    let arg0 = b"hello\0";
    let argv: [u64; 2] = [arg0.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0);
    if !t.check("spawn: sys_spawn returned a handle", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    t.check("spawn: pid is a real child pid", pid >= 2);

    // **The child's stdio is one `ProcessChannel`, pinned.** fd 0/1/2 in its own
    // registered table are `Stdin`/`Stdout`/`Stderr`, served by glue's own arms
    // from `Process::channel` — which is what an AArch64 spawn has always had,
    // and what a session needs so the line discipline's echo and the program's
    // own output land in one place.
    //
    // Until 2026-09-11 they were a `PipeRead` and two `PipeWrite`s over two
    // pipes this file allocated (`fd::bind_stdio`), and this check asserted
    // that shape. That was a second implementation of session stdio, and the
    // observable failures if *this* shape stops holding are the same indirect
    // ones: a shell that reads the console instead of its channel, an `EBADF`
    // from a `dup2` target, a lost stderr. Asked of the live child, before it is
    // drained — the one moment the table is guaranteed populated and not yet
    // swept by `close_all`.
    //
    // fd 2 is `Stderr` and not a second name for fd 1: they are distinct
    // descriptors onto one channel, so a `dup2(file, 1)` leaves stderr on the
    // session exactly as the two-names-on-one-pipe arrangement did.
    {
        use akuma_exec::process::FileDescriptor;
        let stdio = akuma_exec::process::with_process(pid, |p| {
            let t = p.fds.table.lock();
            (
                matches!(t.get(&0), Some(FileDescriptor::Stdin)),
                matches!(t.get(&1), Some(FileDescriptor::Stdout)),
                matches!(t.get(&2), Some(FileDescriptor::Stderr)),
                p.channel.is_some(),
            )
        });
        t.check(
            "spawn: the child's registered table holds fd 0 as Stdin",
            stdio.is_some_and(|(r, _, _, _)| r),
        );
        t.check(
            "spawn: fd 1 as Stdout",
            stdio.is_some_and(|(_, w, _, _)| w),
        );
        t.check(
            "spawn: and fd 2 as Stderr",
            stdio.is_some_and(|(_, _, e, _)| e),
        );
        // The descriptors are only meaningful with a channel behind them: glue's
        // `Stdin` arm falls back to `Process::read_stdin` when
        // `current_channel()` is `None`, and that buffer is never filled here —
        // a reader would park rather than report EOF
        // (`AKUMA_AMD64_CONSOLE_PROCESSCHANNEL.md` §3).
        t.check(
            "spawn: and the child has an I/O channel behind them",
            stdio.is_some_and(|(_, _, _, c)| c),
        );
    }

    // Non-blocking reads so the driver keeps polling `waitpid` too.
    k::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut status = u64::MAX;
    let mut buf = [0u8; 64];
    let mut spins = 0u64;
    loop {
        spins += 1;
        if spins > 500_000 {
            break;
        }
        let n = k::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n != 0 && n < ERRNO_FLOOR {
            out.extend_from_slice(&buf[..n as usize]);
        }
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            status = ((st >> 8) & 0xff) as u64;
            break;
        }
        k::sched::yield_now();
    }

    t.check(
        "spawn: the child's stdout came back through the channel",
        out.windows(5).any(|w| w == b"[elf]"),
    );
    t.check_eq("spawn: waitpid reported the child's exit status", status, HELLO_ALL_OK);
    // **`EBADF` past the reap, and that is the shared answer, not a gap.**
    //
    // This asked for `0` while the parent held a `PipeRead`: the pipe outlived
    // the child (the descriptor held a reader reference) and a drained pipe with
    // no writers reads EOF. A `ChildStdout(pid)` names a *registration*, and
    // `reap_child_channel` drops it at the reap — but only once the stdout FIFO
    // is **empty**, which is the check that stops a reap from eating a shell's
    // final buffered write (`children.rs`, and the sshd bridge it was written
    // for: it polls `waitpid` before it drains). The loop above drained to EOF
    // first, so the registration is legitimately gone and there is nothing left
    // to report EOF *about*.
    t.check_eq(
        "spawn: reading the child's stdout after the reap is EBADF",
        k::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64),
        k::fd::errno::EBADF,
    );
    k::fd::sys_close(stdout_fd);
    t.check_eq(
        "spawn: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// **A spawn does not inherit the previous occupant's group death.**
///
/// The bug this pins: `GROUP_EXIT_STATUS` and `GROUP_EXIT` are indexed by
/// process slot, only `fork` cleared them, and `sys_spawn` hands out the lowest
/// free slot — the one a dead process just vacated. After a multithreaded
/// process lost a worker to a default-action `SIGTERM` (kot, stopped by herd or
/// `kill <pid>`), every later spawn into that slot exited at its first syscall
/// return as `SIGTERM`: exit 241, no output, until a power cycle. See
/// `spawn_process_task`.
///
/// Reproduced directly rather than through a threaded victim: poison the exact
/// slot `sys_spawn` will pick next with what `notify_group_of_thread_fatal` and
/// a threaded `exit_group` leave behind, then spawn `/bin/hello`. Without the
/// clear the child is reported `WIFSIGNALED` with `WTERMSIG == 15`.
pub fn spawn_stale_group_state_test(t: &mut Suite) {
    const HELLO_ALL_OK: u64 = 0x7F;
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;
    const SIGTERM: u32 = 15;

    sweep_reaped_spawn_rows();
    // The same search `sys_spawn` runs, so the poison lands where the child will.
    let next = SPAWN.first_free_slot();
    let Some(next) = next else {
        t.check("stale group: a free spawn slot to poison", false);
        return;
    };
    k::thread::set_group_exit_status(next, (-(SIGTERM.cast_signed())).cast_unsigned());
    k::thread::set_group_exiting(next);

    let path = b"/bin/hello\0";
    let arg0 = b"hello\0";
    let argv: [u64; 2] = [arg0.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0);
    if !t.check("stale group: sys_spawn returned a handle", r < ERRNO_FLOOR) {
        k::thread::clear_group_exit_status(next);
        k::thread::clear_group_exiting(next);
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    t.check_eq(
        "stale group: the child took the poisoned slot",
        spawn_row_of(pid).map_or(u64::MAX, |off| (off + SPAWN_SLOT_BASE) as u64),
        next as u64,
    );
    k::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut buf = [0u8; 64];
    let mut st: i32 = -1;
    let mut reaped = false;
    for _ in 0..500_000u32 {
        let _ = k::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            reaped = true;
            break;
        }
        k::sched::yield_now();
    }
    k::fd::sys_close(stdout_fd);
    t.check("stale group: the child was reaped", reaped);
    // Two checks, so a failure says which half broke: a signal death here is
    // the stale-status kill itself; a wrong exit code with no signal is
    // something else.
    t.check_eq("stale group: the child was not killed by a signal", u64::from((st as u32) & 0x7f), 0);
    t.check_eq(
        "stale group: and ran to its own exit status",
        ((st >> 8) & 0xff) as u64,
        HELLO_ALL_OK,
    );
}

/// **An ssh session's window size reaches the child it describes.**
///
/// The bug this pins is `AMD64_SSH_TERM_SIZE_NOT_PASSED.md`, where a full-screen
/// program over ssh always read 80x24 whatever the client's window was. Both
/// halves were silent — `TIOCSWINSZ` returned 0 having written the size into a
/// process that never reads it, and `TIOCGWINSZ` returned a constant — so
/// neither could be caught by anything but a check that follows a size all the
/// way through. That is what this is, in the shape `sshd` uses:
///
/// 1. spawn a child and hold its stdout, which is what `sshd` has after
///    `spawn_pty` (a `PipeRead` on this target; a `ChildStdout` on AArch64);
/// 2. `TIOCSWINSZ` on that descriptor — the parent describing the *client's*
///    terminal, not its own;
/// 3. the size lands on the **child's** `TerminalState` and **not** the
///    caller's, which is the half that was wrong (break 4) and the half a
///    single-process check cannot tell apart;
/// 4. `TIOCGWINSZ` on a console fd reports a tracked size rather than the
///    compiled-in 24x80 (break 5).
///
/// The size is 132x50, deliberately not a default and not square: 80 or 24
/// anywhere in this test would let a hardcoded answer pass it, which is the
/// same reason the doc's own reproduction uses that window.
pub fn winsize_to_child_test(t: &mut Suite) {
    use k::fd::errno;
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;
    const TIOCGWINSZ: u64 = 0x5413;
    const TIOCSWINSZ: u64 = 0x5414;
    const ROWS: u16 = 50;
    const COLS: u16 = 132;

    if k::fs::read_file("/bin/hello").is_err() {
        t.note("winsize: /bin/hello not on the disk; skipped", 0);
        return;
    }

    // The caller's own state, to prove at the end that the child's size did not
    // land here. `boot_row_register` (held by the caller across this block) is
    // what gives the boot task a registered process to own one.
    let caller_pid = current_pid();

    let path = b"/bin/hello\0";
    let arg0 = b"hello\0";
    let argv: [u64; 2] = [arg0.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0);
    if !t.check("winsize: spawned a child to describe a terminal to", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;

    // struct winsize { u16 ws_row, ws_col, ws_xpixel, ws_ypixel }.
    let winsz: [u16; 4] = [ROWS, COLS, 0, 0];
    t.check_eq(
        "winsize: TIOCSWINSZ on the child's stdout is accepted",
        k::fd::sys_ioctl(stdout_fd, TIOCSWINSZ, winsz.as_ptr() as u64),
        0,
    );

    // **Break 4.** Accepting it was never the problem; landing it on the right
    // process was. A `PipeRead` that names no spawn falls through to glue and
    // sets the caller's own state, which is what happened for every fd here.
    let child = akuma_exec::process::with_process(pid, |p| {
        let ts = p.terminal_state.lock();
        (ts.term_height, ts.term_width)
    });
    t.check_eq(
        "winsize: it landed on the CHILD's terminal state (rows)",
        u64::from(child.map_or(0, |(h, _)| h)),
        u64::from(ROWS),
    );
    t.check_eq(
        "winsize: and its columns",
        u64::from(child.map_or(0, |(_, w)| w)),
        u64::from(COLS),
    );
    let caller = akuma_exec::process::with_process(caller_pid, |p| {
        let ts = p.terminal_state.lock();
        (ts.term_height, ts.term_width)
    });
    t.check(
        "winsize: and NOT on the caller's, which is where it used to go",
        caller.is_none_or(|(h, w)| (h, w) != (ROWS, COLS)),
    );

    // **Break 5**, from the reader's side. `console_ioctl` claims fd 0 for the
    // boot task and used to answer 24x80 from literals; it now reports whatever
    // this process's state says. Written here rather than read, because the
    // suite's caller is not the spawned child and cannot run an ioctl as it.
    let restore = akuma_exec::process::with_process(caller_pid, |p| {
        let mut ts = p.terminal_state.lock();
        let was = (ts.term_height, ts.term_width);
        ts.term_height = ROWS;
        ts.term_width = COLS;
        was
    });
    let mut got = [0u16; 4];
    t.check_eq(
        "winsize: TIOCGWINSZ on a console fd succeeds",
        k::fd::sys_ioctl(0, TIOCGWINSZ, got.as_mut_ptr() as u64),
        0,
    );
    t.check_eq(
        "winsize: and reports the tracked size, not a compiled-in 24x80",
        (u64::from(got[0]) << 16) | u64::from(got[1]),
        (u64::from(ROWS) << 16) | u64::from(COLS),
    );
    if let Some((h, w)) = restore {
        akuma_exec::process::with_process(caller_pid, |p| {
            let mut ts = p.terminal_state.lock();
            ts.term_height = h;
            ts.term_width = w;
        });
    }

    // A bad `winsize` pointer is `EFAULT` and not a wild kernel read — the arm
    // copies four bytes in from userspace before it touches any state.
    t.check_eq(
        "winsize: an unreadable TIOCSWINSZ argument is EFAULT",
        k::fd::sys_ioctl(stdout_fd, TIOCSWINSZ, 0x1000),
        errno::EFAULT,
    );

    // The link is carried by the **descriptor**, so pin that: the parent's
    // stdout is a `ChildStdout(pid)` naming this child, which is what glue's
    // `TIOCSWINSZ` arm reads the pid out of. It was a `PipeRead` plus a
    // spawn-row lookup (`child_of_stdout_pipe`) until 2026-09-11, and a stale
    // row there would have aimed a live session's resize at a dead pid — or at
    // a later child that inherited the recycled pipe id. A descriptor cannot go
    // stale that way: it names a pid directly and `close` drops it.
    t.check(
        "winsize: the parent's stdout is a ChildStdout naming this child",
        matches!(
            k::fd::table_get(stdout_fd),
            Some(akuma_exec::process::FileDescriptor::ChildStdout(p)) if p == pid
        ),
    );

    // Drain and reap, so the child leaves no pipe and no row behind — the same
    // shape `spawn_test` ends with, minus the output checks it owns.
    let mut sink = [0u8; 256];
    let mut spins = 0;
    loop {
        spins += 1;
        if spins > 500_000 {
            break;
        }
        let _ = k::fd::sys_read(stdout_fd, sink.as_mut_ptr() as u64, sink.len() as u64);
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            break;
        }
        k::sched::yield_now();
    }
    k::fd::sys_close(stdout_fd);
    t.check(
        "winsize: and names nobody once the descriptor is closed",
        k::fd::table_get(stdout_fd).is_none(),
    );
}

/// A spawned session's line discipline is **its own**, not the console's.
///
/// The defect this pins, live-found 2026-09-18 on the Firecracker self-host
/// guest: `register_exec_process` decided "is this process attached to the
/// serial console?" from its fd table alone, and the day spawned children
/// started carrying `SharedFdTable::with_stdio()` (fd 0 = `Stdin`, the console's
/// own spelling) every `sys_spawn` child began answering yes — so each `ssh`
/// session's shell was handed the **console's shared `TerminalState`** rather
/// than the fresh one `sys_spawn` passes `None` for.
///
/// Nothing failed at spawn time. It failed one session later: `busybox`'s line
/// editor sets raw mode per prompt and restores the flags it read at *startup*,
/// so session 1 left the shared cell raw, session 2 read raw as its "cooked"
/// baseline, and `ISIG` was gone for the rest of the boot — `^C` never
/// interrupted a foreground job again. See
/// `docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §16.
///
/// The check is written the way the bug presents: dirty the *console's* state
/// the way a departing session leaves it, then spawn and read the child's. A
/// child that shares the cell reports the dirt; a child with its own reports
/// the default. Sharing the `Arc` is the thing under test, so this cannot be
/// replaced by comparing pointers — `Arc::ptr_eq` would pass a kernel that
/// copied the console's *values* into a private cell, which is also wrong.
pub fn session_terminal_is_private_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;
    const ISIG: u32 = 0x1;

    if k::fs::read_file("/bin/hello").is_err() {
        t.note("session-term: /bin/hello not on the disk; skipped", 0);
        return;
    }
    let Some(console_ts) = k::console::terminal_state() else {
        t.note("session-term: no console terminal state; skipped", 0);
        return;
    };

    // What a session leaves behind when its shell exits from a prompt.
    let saved = {
        let mut ts = console_ts.lock();
        let was = ts.lflag;
        ts.lflag &= !ISIG;
        was
    };

    let path = b"/bin/hello\0";
    let arg0 = b"hello\0";
    let argv: [u64; 2] = [arg0.as_ptr() as u64, 0];
    // `SPAWN_FLAG_PTY`: the session shape, the one whose `^C` must be a signal.
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, SPAWN_FLAG_PTY);
    if !t.check("session-term: spawned a pty child", r < ERRNO_FLOOR) {
        console_ts.lock().lflag = saved;
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;

    let child_lflag = akuma_exec::process::with_process(pid, |p| p.terminal_state.lock().lflag);
    t.check(
        "session-term: the child's ISIG is set though the console's is not",
        child_lflag.is_some_and(|l| l & ISIG != 0),
    );

    // And the reverse direction: what the child does to its terminal must not
    // reach the console's. (A shell going raw for its prompt is exactly this
    // write.)
    // Zero, not "clear ISIG": the console's cell already has `ISIG` clear
    // above, so clearing it again is a no-op a *shared* cell would also pass.
    // A value neither cell can be holding is what makes this discriminate.
    akuma_exec::process::with_process(pid, |p| p.terminal_state.lock().lflag = 0);
    t.check(
        "session-term: the child going raw leaves the console's state alone",
        console_ts.lock().lflag == (saved & !ISIG),
    );

    console_ts.lock().lflag = saved;

    // Drain and reap, the shape `winsize_to_child_test` ends with.
    let mut sink = [0u8; 256];
    let mut spins = 0;
    loop {
        spins += 1;
        if spins > 500_000 {
            break;
        }
        let _ = k::fd::sys_read(stdout_fd, sink.as_mut_ptr() as u64, sink.len() as u64);
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            break;
        }
        k::sched::yield_now();
    }
    k::fd::sys_close(stdout_fd);
}

/// The `console_notify` syscall (Akuma-private 322), the kernel half of
/// `/bin/wall`. Only the argument-validation edges are checkable from here — a
/// success needs a mapped user page this context does not have — but those edges
/// are what pin the ABI `libakuma::console_notify` is written against: that the
/// length is `a2`, that a zero length is `EINVAL` and a bad pointer is `EFAULT`,
/// and that the number resolves at all rather than falling through to `ENOSYS`.
pub fn console_notify_test(t: &mut Suite) {
    use k::fd::errno;
    const NR: u64 = 0x1000 + 322;

    t.check_eq(
        "console_notify: a zero-length message is EINVAL",
        syscall_dispatch(NR, 0, 0, 0, 0, 0, 0),
        errno::EINVAL,
    );
    // An unmapped low user address: the `rep movsb` user copy faults and
    // `idt.rs`'s `user_copy_fixup` turns that into a returned error, exactly as
    // it does for `resolve_host` and `spawn`'s argument reads.
    t.check_eq(
        "console_notify: an unreadable pointer is EFAULT",
        syscall_dispatch(NR, 0x1000, 16, 0, 0, 0, 0),
        errno::EFAULT,
    );
}

/// Stage S: a real static musl **busybox** — a program the tree did not compile
/// — runs an applet and its output comes back.
///
/// `busybox uname -m` exercises the whole "run a foreign binary" surface: the
/// ELF loader on a ~1 MB image, `arch_prctl` for the TLS base, SSE made legal in
/// `boot.s` (busybox's startup `movups` #UD'd without it), `uname(2)`, and
/// `writev` (busybox prints through it, not `write`). Skipped when busybox is
/// not on the disk.
pub fn busybox_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;

    if k::fs::read_file("/bin/busybox").is_err() {
        t.note("busybox: not on the disk; skipped", 0);
        return;
    }

    let free_before = akuma_pmm::free_count();
    let path = b"/bin/busybox\0";
    let (a0, a1a, a2a) = (b"busybox\0", b"uname\0", b"-m\0");
    let argv: [u64; 4] = [
        a0.as_ptr() as u64,
        a1a.as_ptr() as u64,
        a2a.as_ptr() as u64,
        0,
    ];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0);
    if !t.check("busybox: spawned", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    k::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut status = u64::MAX;
    let mut buf = [0u8; 64];
    let mut spins = 0u64;
    loop {
        spins += 1;
        if spins > 2_000_000 {
            break;
        }
        let n = k::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n != 0 && n < ERRNO_FLOOR {
            out.extend_from_slice(&buf[..n as usize]);
        }
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            status = ((st >> 8) & 0xff) as u64;
            break;
        }
        k::sched::yield_now();
    }
    k::fd::sys_close(stdout_fd);

    t.check_eq("busybox: exited 0", status, 0);
    t.check(
        "busybox: `uname -m` printed x86_64",
        out.windows(6).any(|w| w == b"x86_64"),
    );
    t.check_eq(
        "busybox: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Stage T: `execve` with no `fork`.
///
/// `busybox sh -c "uname"` is spawned; ash resolves `uname` on `PATH`,
/// `stat`s `/bin/uname` (the path-`stat` half of this stage) and `execve`s it
/// **in place** — no fork. The spawned task keeps its slot, pid and stdout
/// pipe, so `uname`'s output ("Akuma") comes back the same way the shell's would
/// and `waitpid` reaps one child, not two.
pub fn execve_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;

    if k::fs::read_file("/bin/busybox").is_err() || k::fs::read_file("/bin/sh").is_err() {
        t.note("execve: busybox /bin/sh not on the disk; skipped", 0);
        return;
    }

    let free_before = akuma_pmm::free_count();
    let path = b"/bin/sh\0";
    // The exact shape `sshd`'s exec sessions use: `sh -c "<cmd with args>"`.
    // ash runs a single simple command (even with arguments) by `execve` in
    // place, no fork — a `;`/`|` sequence is what forces the fork it does not
    // have yet.
    let (a0, a1a, a2a) = (b"sh\0", b"-c\0", b"uname -a\0");
    let argv: [u64; 4] = [a0.as_ptr() as u64, a1a.as_ptr() as u64, a2a.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0);
    if !t.check("execve: sh spawned", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    k::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut status = u64::MAX;
    let mut buf = [0u8; 64];
    let mut spins = 0u64;
    loop {
        spins += 1;
        if spins > 3_000_000 {
            break;
        }
        let n = k::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n != 0 && n < ERRNO_FLOOR {
            out.extend_from_slice(&buf[..n as usize]);
        }
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            status = ((st >> 8) & 0xff) as u64;
            break;
        }
        k::sched::yield_now();
    }
    k::fd::sys_close(stdout_fd);

    t.check_eq("execve: `sh -c \"uname -a\"` exited 0", status, 0);
    t.check(
        "execve: the exec'd program's output came back",
        out.windows(5).any(|w| w == b"Akuma") && out.windows(6).any(|w| w == b"x86_64"),
    );
    t.check_eq(
        "execve: teardown leaks nothing (one child reaped, not two)",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Run `busybox sh -c <cmd>` and collect its stdout and exit status.
///
/// The drive loop `fork_test` and `execve_test` each spell out longhand. Split
/// out for [`redirect_test`], which needs it twice; the two older tests are
/// deliberately left alone so a failure there still bisects to their own code.
fn run_sh_capture(cmd: &[u8]) -> Option<(u64, alloc::vec::Vec<u8>)> {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;
    let path = b"/bin/sh\0";
    let (a0, adash) = (b"sh\0", b"-c\0");
    let argv: [u64; 4] = [a0.as_ptr() as u64, adash.as_ptr() as u64, cmd.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0);
    if r >= ERRNO_FLOOR {
        return None;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    k::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut status = u64::MAX;
    let mut buf = [0u8; 64];
    let mut spins = 0u64;
    loop {
        spins += 1;
        if spins > 4_000_000 {
            break;
        }
        let n = k::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n != 0 && n < ERRNO_FLOOR {
            out.extend_from_slice(&buf[..n as usize]);
        }
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            status = ((st >> 8) & 0xff) as u64;
            break;
        }
        k::sched::yield_now();
    }
    k::fd::sys_close(stdout_fd);
    Some((status, out))
}

/// `rt_sigsuspend` from ring 3, through busybox ash's `wait`, which parks in
/// it with every signal blocked around the call.
///
/// The trapped `SIGUSR1` must end the `wait` (status 128 + 10), its handler
/// must run, and the mask from before the `sigsuspend` must be back after it:
/// the second `kill` is delivered only if `SIGUSR1` is not left blocked by the
/// call's temporary mask. `ENOSYS` (this kernel's answer until 2026-11-02)
/// made ash's loop spin with every signal blocked.
pub fn sigsuspend_test(t: &mut Suite) {
    if k::fs::read_file("/bin/busybox").is_err() || k::fs::read_file("/bin/sh").is_err() {
        t.note("sigsuspend: busybox /bin/sh not on the disk; skipped", 0);
        return;
    }
    let cmd = b"trap 'echo got' USR1; (sleep 1; kill -USR1 $$) & wait; echo rc=$?; kill -USR1 $$; echo end\0";
    let Some((status, out)) = run_sh_capture(cmd) else {
        t.check("sigsuspend: sh spawned", false);
        return;
    };
    t.check_eq("sigsuspend: the shell exited 0", status, 0);
    const WANT: &[u8] = b"got\nrc=138\ngot\nend\n";
    t.check(
        "sigsuspend: trap ran, wait said 138, and the mask was restored (trap ran again)",
        out.windows(WANT.len()).any(|w| w == WANT),
    );
}

/// Shell redirection and pipelines — `dup2` and `pipe(2)`, end to end.
///
/// These are the two things a build system cannot do without, and until
/// 2026-09-06 neither worked: descriptors 0/1/2 were routed by number below
/// `fd`'s table, so `dup2` had nowhere to land, and `pipe`/`pipe2` were not
/// dispatched at all. The symptoms were `echo x > file` leaving a
/// **zero-length file** and `cmd | cmd` reporting *can't create pipe* — both
/// of which read as filesystem or resource problems and are neither.
///
/// Driven through the real `busybox ash` rather than by calling the syscalls
/// directly, because what has to work is the shell's *own* sequence:
/// `open(file)` → `dup2(fd,1)` → `close(fd)` for a redirect, and `pipe()` plus
/// two `dup2`s across a `fork` for a pipeline. A unit test of `dup2` in
/// isolation passed on kernels where neither of those worked.
///
/// The redirect is checked by reading the file **back through the kernel's own
/// filesystem**, not by trusting the shell's exit status: the pre-fix failure
/// exited 0 and left an empty file, so a status check alone scores it green.
pub fn redirect_test(t: &mut Suite) {
    if k::fs::read_file("/bin/busybox").is_err() || k::fs::read_file("/bin/sh").is_err() {
        t.note("redirect: busybox /bin/sh not on the disk; skipped", 0);
        return;
    }
    let free_before = akuma_pmm::free_count();

    // 0. `/proc/self/exe` — a **magic symlink served by procfs**, not by a
    //    syscall-layer interception.
    //
    //    It was two `if path == "/proc/self/exe"` arms in
    //    `akuma-syscalls-glue` (`sys_openat` and `sys_readlinkat`) patching a
    //    gap in `akuma_vfs_glue::proc`, which is the same shape as this
    //    target's own `/proc` view and the reason both are being deleted. The
    //    gap is closed in procfs now, so the generic paths serve it — and the
    //    answer generalised from *self only* to any visible pid.
    //
    //    Checked from ring 3 because that is the only place it can work: the
    //    boot row is registered nowhere, and procfs renders from the process
    //    table.
    //
    //    Two assertions, because the first one passed while the file was
    //    useless. `readlink` answered `ls`, `ps`-style — `image.name` was
    //    `argv[0]`, a name the caller chose rather than a path — so the
    //    symlink existed and opened nothing. Reading four bytes through it and
    //    finding `\x7fELF` is what says the link names the binary.
    if let Some((status, out)) = run_sh_capture(b"readlink /proc/self/exe\0") {
        t.check_eq("proc: `readlink /proc/self/exe` exited 0", status, 0);
        t.check(
            "proc: and it names an absolute path",
            out.starts_with(b"/"),
        );
    } else {
        t.check("proc: sh spawned for the exe symlink", false);
    }
    if let Some((_, out)) = run_sh_capture(b"head -c 4 /proc/self/exe\0") {
        t.check(
            "proc: opening through /proc/self/exe reads the ELF it names",
            out.windows(4).any(|w| w == b"\x7fELF"),
        );
    }

    // `chdir`/`getcwd`/`chmod` — the 2026-09-12 dispatch fix
    // (`docs/archive/RUST_TOOLCHAIN_AMD64.md`). Three properties, each a
    // different half of the fix: `chdir` moves `Process::cwd` (glue's arm),
    // `getcwd` *reports* the moved cwd (the old local arm hard-coded `/`), and
    // a `fork` child inherits it (`Process::inherit_from`) — which is what
    // `busybox pwd` after a `cd` exercises, because the applet runs as a child
    // of the shell that did the `cd`. `chmod` exercises the `fchmodat` shim
    // plus ext2's on-disk mode bits, checked kernel-side through `metadata`.
    if let Some((status, out)) = run_sh_capture(b"cd /proc && pwd\0") {
        t.check_eq("cwd: `cd /proc && pwd` exited 0", status, 0);
        t.check("cwd: and pwd reports the moved directory", out.starts_with(b"/proc"));
    } else {
        t.check("cwd: sh spawned for `cd /proc`", false);
    }
    let _ = run_sh_capture(b"mkdir -p /tmp/cwdprobe\0");
    if let Some((status, out)) = run_sh_capture(b"cd /tmp/cwdprobe && busybox pwd\0") {
        t.check_eq("cwd: forked child's pwd exited 0", status, 0);
        t.check(
            "cwd: and the fork child inherited the chdir",
            out.starts_with(b"/tmp/cwdprobe"),
        );
    } else {
        t.check("cwd: sh spawned for the fork-inheritance probe", false);
    }
    // **A big environment must survive `execve`.** The loader caps envp entries
    // ([`loader::MAX_ENVP`]) and `execve` refuses rather than truncating, so a
    // cap set too low does not degrade — it kills the child before its first
    // instruction, with `E2BIG` that reaches the parent as a bare `exit status:
    // 1`. At 64 that was every cargo **build script**: measured 2026-09-13, a
    // crate that built went to `exit status: 1` on exporting two more shell
    // variables, and `proc-macro2`'s script — which every derive macro needs —
    // never ran.
    //
    // 80 entries here is past where the old cap sat and far below the new one,
    // so it fails if `MAX_ENVP` is ever lowered back toward a cargo-sized
    // environment. The shell adds its own on top of these.
    {
        let mut cmd = alloc::vec::Vec::new();
        cmd.extend_from_slice(b"env");
        for i in 0u8..80 {
            // Digits by hand: this runs before anything in the suite may
            // allocate for a message, and a two-digit name needs no formatter.
            cmd.extend_from_slice(b" PAD");
            cmd.push(b'0' + i / 10);
            cmd.push(b'0' + i % 10);
            cmd.extend_from_slice(b"=y");
        }
        cmd.extend_from_slice(b" /bin/busybox echo ENVOK\0");
        if let Some((status, out)) = run_sh_capture(&cmd) {
            t.check_eq("exec: 80 environment entries exited 0", status, 0);
            t.check(
                "exec: and the program ran with them (MAX_ENVP is not a cargo-sized cap)",
                out.starts_with(b"ENVOK"),
            );
        } else {
            t.check("exec: sh spawned for the big-environment probe", false);
        }
    }

    // **A long single argument must survive `execve`.** Each argv/envp string
    // was read with a 512-byte cap that answered by *ending the list*, so an
    // argument longer than that took every argument after it with it — the
    // silent-truncation shape that ran every linker without its inputs in
    // session 3, here one layer down. `sh -c "<667 bytes>"` lost the command
    // and busybox reported `sh: -c requires an argument` (measured
    // 2026-09-13); a `cargo`-generated `rustc` line carries several arguments
    // that size.
    {
        let mut cmd = alloc::vec::Vec::new();
        cmd.extend_from_slice(b"/bin/busybox echo LONGARG");
        // Past the old cap by a clear margin, in one argument.
        for _ in 0..700 {
            cmd.push(b'x');
        }
        cmd.push(0);
        // (The *count* limit has its own check below.)
        if let Some((status, out)) = run_sh_capture(&cmd) {
            t.check_eq("exec: an argument past the old 512-byte cap exited 0", status, 0);
            t.check(
                "exec: and the whole command line survived",
                out.starts_with(b"LONGARG"),
            );
        } else {
            t.check("exec: sh spawned for the long-argument probe", false);
        }
    }

    // **And a command line with many arguments.** `MAX_ARGV` was 256, and one
    // `cargo`-generated `rustc` invocation for this kernel's own graph is
    // **300** arguments — so the self-host build stopped at `E2BIG` with cargo
    // reporting `could not compile` and no diagnostic, because rustc never ran
    // to emit one (measured 2026-09-13, `akuma-syscalls-glue`). 400 here is
    // past that workload and past the old cap, so it fails if either the cap or
    // the heap word block regresses.
    {
        let mut cmd = alloc::vec::Vec::new();
        cmd.extend_from_slice(b"/bin/busybox echo");
        for _ in 0..400 {
            cmd.extend_from_slice(b" a");
        }
        cmd.push(0);
        if let Some((status, out)) = run_sh_capture(&cmd) {
            t.check_eq("exec: a 400-argument command line exited 0", status, 0);
            t.check("exec: and the program ran", out.starts_with(b"a a"));
        } else {
            t.check("exec: sh spawned for the many-arguments probe", false);
        }
    }

    // **A relative path means the process's own directory, at `execve` too.**
    // This target's `execve`/`sys_spawn` handed the raw path to `read_image`,
    // which resolves symlinks and not `cwd`, so every relative exec was read
    // from the root. `cd /tmp/x && ./prog` reported "not found" for a binary
    // `ls` in that same shell had just listed — and where a same-named file
    // existed at `/`, it silently ran **that** instead
    // (measured 2026-09-13: `cd /tmp/hi/target/debug && ./hi` ran `/hi`).
    //
    // `/bin/busybox` with no `/busybox` beside it is the whole test: the
    // relative spelling can only work if `cwd` was consulted, and the
    // `exists` guard is what stops the check passing vacuously if some future
    // image ships a `/busybox`.
    t.check(
        "exec: no /busybox, so the relative-exec probe below means something",
        !akuma_vfs_glue::fs::exists("/busybox"),
    );
    if let Some((status, out)) = run_sh_capture(b"cd /bin && ./busybox echo RELEXEC\0") {
        t.check_eq("exec: `cd /bin && ./busybox` exited 0", status, 0);
        t.check(
            "exec: a relative path resolves against cwd, not /",
            out.starts_with(b"RELEXEC"),
        );
    } else {
        t.check("exec: sh spawned for the relative-exec probe", false);
    }

    if let Some((status, _)) = run_sh_capture(b"echo x > /tmp/modeprobe && chmod 755 /tmp/modeprobe\0") {
        t.check_eq("chmod: `chmod 755 file` exited 0", status, 0);
        let mode = akuma_vfs_glue::metadata("/tmp/modeprobe").map(|m| u64::from(m.mode & 0o777));
        t.check_eq("chmod: and the on-disk mode is 755", mode.unwrap_or(0), 0o755);
    } else {
        t.check("chmod: sh spawned for the mode probe", false);
    }

    // 1. `>` — the shell's open/dup2/close sequence onto fd 1.
    let Some((status, _)) = run_sh_capture(b"echo REDIROK > /tmp/redir.txt\0") else {
        t.check("redirect: sh spawned for `>`", false);
        return;
    };
    t.check_eq("redirect: `echo … > file` exited 0", status, 0);
    let written = k::fs::read_file("/tmp/redir.txt");
    t.check("redirect: the redirected file exists", written.is_ok());
    t.check(
        "redirect: the redirected bytes reached the file",
        written.is_ok_and(|d| d.windows(7).any(|w| w == b"REDIROK")),
    );

    // 2. `>>` — the same sequence with `O_APPEND`, which is a *different* open
    // and was the same one until 2026-09-06: `open_flags` read neither
    // `O_APPEND` nor `O_TRUNC`, so every `O_CREAT` began with an empty buffer
    // and `>>` silently behaved as `>`. Checked here rather than beside the
    // `open` unit tests because it is invisible without working redirection —
    // there was no way for a program to reach it before `dup2` landed.
    let Some((astatus, _)) = run_sh_capture(b"echo ONE > /tmp/app.txt\0") else {
        t.check("redirect: sh spawned for `>>`", false);
        return;
    };
    t.check_eq("redirect: `>` for the append case exited 0", astatus, 0);
    let _ = run_sh_capture(b"echo TWO >> /tmp/app.txt\0");
    let appended = k::fs::read_file("/tmp/app.txt");
    t.check(
        "redirect: `>>` kept the first line",
        appended.as_ref().is_ok_and(|d| d.windows(3).any(|w| w == b"ONE")),
    );
    t.check(
        "redirect: `>>` added the second",
        appended.as_ref().is_ok_and(|d| d.windows(3).any(|w| w == b"TWO")),
    );

    // 3. `|` — `pipe(2)` plus a `dup2` on each side of a fork.
    let Some((pstatus, pout)) = run_sh_capture(b"echo PIPEOK | cat\0") else {
        t.check("redirect: sh spawned for `|`", false);
        return;
    };
    t.check_eq("redirect: `cmd | cmd` exited 0", pstatus, 0);
    t.check(
        "redirect: the piped bytes came out the far end",
        pout.windows(6).any(|w| w == b"PIPEOK"),
    );

    // 4. A **deep** pipeline. One `|` proves `pipe`+`dup2`; twelve proves the
    // process ceiling is really gone (the old `proc_entry_for` handed out nine
    // slots) and that nothing leaks a pipe, a process slot or a task per stage
    // — each of which is a fixed-size table that a shell can exhaust.
    let Some((dstatus, dout)) = run_sh_capture(b"echo DEEPOK | cat | cat | cat | cat | cat | cat | cat | cat | cat | cat | cat\0")
    else {
        t.check("redirect: sh spawned for the deep pipeline", false);
        return;
    };
    t.check_eq("redirect: a 12-stage pipeline exited 0", dstatus, 0);
    t.check(
        "redirect: the bytes survived 12 stages",
        dout.windows(6).any(|w| w == b"DEEPOK"),
    );

    // 5. A pipeline whose **reader leaves first**. `head -n 1` prints its line
    // and exits, dropping the last read end while `yes` is still pushing at a
    // buffer that is already full — so `yes` can only learn the pipe broke by
    // its next write reporting it.
    //
    // This is the rule the amd64 pipe table could not express until it became
    // `akuma-pipes` on 2026-09-06: with a single `write_closed` flag and no end
    // reference counts, a dead reader was indistinguishable from a full buffer,
    // and `write_pipe`'s retry loop span here forever. It is the whole reason
    // this case is a boot check and not a unit test — nothing short of the real
    // shell arranges the exit order.
    let Some((ystatus, _)) = run_sh_capture(b"busybox yes | busybox head -n 1\0") else {
        t.check("redirect: sh spawned for the early-exit reader", false);
        return;
    };
    t.check_eq("redirect: `yes | head -n 1` terminates", ystatus, 0);

    // 6. `>` over a file that is **longer than what replaces it**, and two
    // redirects that write **no bytes at all**. Every case above only ever
    // wrote to a path that did not exist yet, which is precisely why they all
    // passed while `O_TRUNC` and `O_CREAT` did nothing (C2 slice 5's
    // `write_at(path, 0, &[])`, short-circuited by `write_at`'s own
    // `data.is_empty()` guard before it resolved anything — see `sys_openat`).
    //
    // The failures that hid behind that: `echo x >` over 31 bytes left
    // `x\nAAAA…`, keeping the tail of the old file behind the new head; and
    // `: > f` — the idiom for "make this empty", and what `2> err` does for a
    // command that prints no errors — created nothing at all. Found from ring
    // 3 over ssh, not by any check in this suite, which is the lesson the
    // plan's § "The lesson C1 step 3 paid for" states: a redirect whose
    // *exit status* is 0 proves nothing about the bytes.
    let _ = run_sh_capture(b"echo AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA > /tmp/tr.txt\0");
    let _ = run_sh_capture(b"echo x > /tmp/tr.txt\0");
    let truncated = k::fs::read_file("/tmp/tr.txt");
    t.check_eq(
        "redirect: `>` truncates a longer file to just the new bytes",
        truncated.as_ref().map_or(u64::MAX, |d| d.len() as u64),
        2,
    );
    t.check(
        "redirect: nothing of the old contents survives the truncate",
        truncated.as_ref().is_ok_and(|d| !d.windows(2).any(|w| w == b"AA")),
    );
    let _ = run_sh_capture(b": > /tmp/empty.txt\0");
    let empty = k::fs::read_file("/tmp/empty.txt");
    t.check("redirect: a zero-byte `>` still creates the file", empty.is_ok());
    t.check_eq(
        "redirect: and creates it empty",
        empty.map_or(u64::MAX, |d| d.len() as u64),
        0,
    );
    let _ = run_sh_capture(b"echo quiet 2> /tmp/noerr.txt\0");
    t.check(
        "redirect: `2>` creates its file even when nothing is written to it",
        k::fs::read_file("/tmp/noerr.txt").is_ok(),
    );

    t.check_eq(
        "redirect: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Stage T: `fork` (with `vfork` semantics) + `execve` + `wait4`.
///
/// `busybox sh -c "uname; echo DONE"` — the `;` makes ash a command list, and
/// it **forks** to run `uname` (an external command, not the last in the list)
/// before running the `echo` builtin. So this exercises the whole path: the
/// shell forks, the child `execve`s `/bin/uname` in the shared address space,
/// the parent blocks in `wait4` until the child is done, then finishes the
/// list. Output must carry both `Akuma` (from the forked `uname`) and `DONE`
/// (from the parent shell), and nothing may leak.
/// `wait4` must answer for **your own** children only, and say `ECHILD` when
/// there are none.
///
/// Pins the 2026-09-08 subshell hang. The spawn table is global, so a scan
/// without a `ppid` filter answers "does any process exist?" — and a forked
/// process, whose own row is in that table, saw itself as an unexited child and
/// parked in `wait4` forever. `( ls; true )` was enough to wedge the machine.
///
/// Both halves are checked here because either alone is still broken: a filter
/// that returns the wrong errno leaves a shell reaping in a loop, and the right
/// errno without the filter never gets asked.
///
/// The suite runs as init, whose `current_pid()` is 1, so a child spawned here
/// is genuinely this caller's — the same relationship a shell has to its own.
pub fn wait4_ownership_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;
    use k::fd::errno;

    // No children yet: the answer must be ECHILD, not "block" and not ESRCH.
    let mut st: i32 = -1;
    let r = sys_waitpid(u64::MAX, core::ptr::addr_of_mut!(st) as u64, 0);
    t.check_eq("wait4: no children -> ECHILD", r, errno::ECHILD);

    if k::fs::read_file("/bin/hello").is_err() {
        t.note("wait4: /bin/hello not on the disk; ownership half skipped", 0);
        return;
    }

    // One child of our own. While it is alive, `wait4(-1)` must report "exists,
    // not exited" (0) rather than reaping something that is not ours.
    let path = b"/bin/hello\0";
    let arg0 = b"hello\0";
    let argv: [u64; 2] = [arg0.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0);
    if !t.check("wait4: child spawned", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;

    let mut spins = 0u64;
    let reaped = loop {
        spins += 1;
        if spins > 4_000_000 {
            break 0;
        }
        let mut st: i32 = -1;
        let r = sys_waitpid(u64::MAX, core::ptr::addr_of_mut!(st) as u64, 0);
        if r != 0 {
            break r;
        }
        k::sched::yield_now();
    };
    k::fd::sys_close(stdout_fd);
    t.check_eq("wait4: -1 reaps our own child by pid", reaped, u64::from(pid));

    // And once it is reaped we are childless again. This is the assertion that
    // fails on the pre-fix kernel: the caller's own row (or any other live
    // process) kept `exists` true, so this returned 0 and the caller parked.
    let mut st: i32 = -1;
    let r = sys_waitpid(u64::MAX, core::ptr::addr_of_mut!(st) as u64, 0);
    t.check_eq("wait4: after the last reap -> ECHILD again", r, errno::ECHILD);
}

pub fn fork_test(t: &mut Suite) {
    const ERRNO_FLOOR: u64 = 0xFFFF_FFFF_FFFF_F000;

    if k::fs::read_file("/bin/busybox").is_err() || k::fs::read_file("/bin/sh").is_err() {
        t.note("fork: busybox /bin/sh not on the disk; skipped", 0);
        return;
    }

    let free_before = akuma_pmm::free_count();
    let path = b"/bin/sh\0";
    let (a0, a1a, a2a) = (b"sh\0", b"-c\0", b"uname; echo DONE\0");
    let argv: [u64; 4] = [a0.as_ptr() as u64, a1a.as_ptr() as u64, a2a.as_ptr() as u64, 0];
    let r = sys_spawn(path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0);
    if !t.check("fork: sh spawned", r < ERRNO_FLOOR) {
        return;
    }
    let pid = (r & 0xFFFF_FFFF) as u32;
    let stdout_fd = (r >> 32) & 0xFFFF_FFFF;
    k::fd::sys_fcntl(stdout_fd, 4, 0x800);

    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut status = u64::MAX;
    let mut buf = [0u8; 64];
    let mut spins = 0u64;
    loop {
        spins += 1;
        if spins > 4_000_000 {
            break;
        }
        let n = k::fd::sys_read(stdout_fd, buf.as_mut_ptr() as u64, buf.len() as u64);
        if n != 0 && n < ERRNO_FLOOR {
            out.extend_from_slice(&buf[..n as usize]);
        }
        let mut st: i32 = -1;
        if sys_waitpid(u64::from(pid), core::ptr::addr_of_mut!(st) as u64, 0) == u64::from(pid) {
            status = ((st >> 8) & 0xff) as u64;
            break;
        }
        k::sched::yield_now();
    }
    k::fd::sys_close(stdout_fd);

    t.check_eq("fork: `sh -c \"uname; echo DONE\"` exited 0", status, 0);
    t.check(
        "fork: the forked child's output came back",
        out.windows(5).any(|w| w == b"Akuma"),
    );
    t.check(
        "fork: the parent shell finished the command list",
        out.windows(4).any(|w| w == b"DONE"),
    );
    t.check_eq(
        "fork: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// The dispatch **vocabulary**: two tables that must stay disjoint, and the
/// number hop that must keep happening.
///
/// Added 2026-09-07 with C1 step 2, which split one 98-arm dispatcher into an
/// x86-only legacy list and the architecture-neutral `Syscall` table. Both
/// failures this guards against are silent:
///
/// - **A number in both tables.** Whichever match runs first wins, and which
///   one that is becomes an accident of ordering rather than a decision.
/// - **The two ABIs agreeing.** `akuma-syscalls-glue` dispatches on
///   asm-generic numbers; this kernel is handed x86_64 ones. If `to_aarch64`
///   ever starts returning the number that came in, the fold is feeding an
///   x86_64 number to an asm-generic `match` — the wrong handler, not none.
///
/// The `symlink` case at the end is the concrete bug this test was written
/// after: x86_64 88 dispatched to `sys_utimensat` for months because a comment
/// called it `futimens`, which is not a syscall at all. `utimensat` on those
/// arguments returns 0 and creates nothing, so only reading the link back tells
/// the two apart — which is exactly why it went unnoticed.
pub fn dispatch_smoke_test(t: &mut Suite, have_fs: bool) {
    // Every number the legacy `match nr` above claims to own. Each must be
    // x86-only; a number that also decodes through `Syscall` is handled twice.
    const X86_ONLY: [u64; 25] = [
        2, 4, 6, 7, 21, 22, 23, 33, 34, 37, 57, 58, 82, 83, 84, 87, 88, 89, 96, 111, 158, 164,
        201, 213, 232,
    ];
    let mut overlap = 0u64;
    for n in X86_ONLY {
        if Syscall::from_x86_64(n).is_some() {
            overlap += 1;
        }
    }
    t.check_eq("dispatch: no number is in both tables", overlap, 0);

    // The hop. `write` arrives as 1 and reaches glue as 64.
    t.check_eq("dispatch: write arrives as x86_64 1", Syscall::Write.to_x86_64(), 1);
    t.check_eq("dispatch: write reaches glue as 64", Syscall::Write.to_aarch64(), 64);
    // The crossing that would be loudest and least explicable: x86_64 63 is
    // `uname`, asm-generic 63 is `read`.
    t.check(
        "dispatch: 63 decodes as uname, not read",
        Syscall::from_x86_64(63) == Some(Syscall::Uname),
    );
    t.check_eq("dispatch: uname reaches glue as 160", Syscall::Uname.to_aarch64(), 160);
    t.check(
        "dispatch: 170/171 decode as sethostname/setdomainname",
        Syscall::from_x86_64(170) == Some(Syscall::Sethostname)
            && Syscall::from_x86_64(171) == Some(Syscall::Setdomainname),
    );
    t.check_eq("dispatch: sethostname reaches glue as 161", Syscall::Sethostname.to_aarch64(), 161);
    t.check(
        "dispatch: x86_64 161 decodes as chroot (asm-generic 161 is sethostname)",
        Syscall::from_x86_64(161) == Some(Syscall::Chroot),
    );
    t.check_eq("dispatch: chroot reaches glue as 51", Syscall::Chroot.to_aarch64(), 51);

    // The arms folded into glue, driven **through the real dispatcher** by the
    // x86_64 number userspace would send.
    //
    // Two properties, and the second is the one that needs a test. The answer
    // is unchanged by the fold — every one of these was `=> 0` here and is `=> 0`
    // in glue — so a regression check on the value alone would pass against a
    // dispatcher that had lost the arms entirely. What it cannot pass against is
    // the number hop being wrong, which is why each is asserted beside the
    // asm-generic constant it must reach. x86_64 102-108 — the whole credential
    // block — lands in asm-generic's timer/module block, where this build
    // already answers one number for real: `nr::SETITIMER` is 103, and
    // `sys_setitimer` reads two `struct itimerval` pointers out of `args[1]` and
    // `args[2]`. A `getuid` arriving there would hand it whatever its unset
    // argument registers held. So a missed hop is a *different arm*, not a
    // missing one — the failure mode the `symlink` bug below already cost this
    // target months.
    //
    // `akuma_syscalls_linux::nr` is the asm-generic table by name; writing 174
    // here would test the transcription rather than the mapping.
    //
    // Unrolled rather than looped so a failure names the syscall it belongs to;
    // six identically-labelled checks would report which *count* broke, not which
    // arm.
    use akuma_syscalls_linux::nr;
    let hop = |call: Syscall, x86: u64, generic: u64| {
        call.to_x86_64() == x86 && call.to_aarch64() == generic
    };
    t.check("dispatch: getuid 102 -> 174", hop(Syscall::Getuid, 102, nr::GETUID));
    t.check("dispatch: getgid 104 -> 176", hop(Syscall::Getgid, 104, nr::GETGID));
    t.check("dispatch: geteuid 107 -> 175", hop(Syscall::Geteuid, 107, nr::GETEUID));
    t.check("dispatch: getegid 108 -> 177", hop(Syscall::Getegid, 108, nr::GETEGID));
    t.check("dispatch: setuid 105 -> 146", hop(Syscall::Setuid, 105, nr::SETUID));
    t.check("dispatch: setgid 106 -> 144", hop(Syscall::Setgid, 106, nr::SETGID));
    // The pair Chromium's zygote child needs before it can ping (2026-10-08).
    t.check("dispatch: capget 125 -> 90", hop(Syscall::Capget, 125, nr::CAPGET));
    t.check("dispatch: capset 126 -> 91", hop(Syscall::Capset, 126, nr::CAPSET));
    t.check("dispatch: fallocate 285 -> 47", hop(Syscall::Fallocate, 285, nr::FALLOCATE));
    t.check("dispatch: faccessat2 439 -> 439", hop(Syscall::Faccessat2, 439, nr::FACCESSAT2));
    t.check("dispatch: close_range 436 -> 436", hop(Syscall::CloseRange, 436, nr::CLOSE_RANGE));

    let d = |nr_x86: u64| syscall_dispatch(nr_x86, 0, 0, 0, 0, 0, 0);
    t.check_eq("dispatch: glue answers getuid 0", d(102), 0);
    t.check_eq("dispatch: glue answers getgid 0", d(104), 0);
    t.check_eq("dispatch: glue answers geteuid 0", d(107), 0);
    t.check_eq("dispatch: glue answers getegid 0", d(108), 0);
    t.check_eq("dispatch: glue accepts setuid", d(105), 0);
    // x86_64 288 is `accept4`, the only accept mio (so tokio) ever calls. It
    // had no row until 2026-09-23 and answered `ENOSYS`. fd 0 is not open in
    // the boot suite, so a row that reaches glue's `net::dispatch_accept`
    // says `EBADF` (`ENOTSOCK` is for an open descriptor that is not a
    // socket; the local copy this replaced said `ENOTSOCK` for both).
    t.check_eq("dispatch: accept4 (288) reaches the socket layer", d(288), k::fd::errno::EBADF);
    t.check_eq("dispatch: glue accepts setgid", d(106), 0);
    t.check_eq("dispatch: glue accepts capset", d(126), 0);
    t.check_eq("dispatch: capget with a NULL header is EFAULT", d(125), k::fd::errno::EFAULT);
    // `fallocate(0, 0, 0, 0)`: a row that reaches glue says `EINVAL` (len 0); no row says `ENOSYS`.
    t.check_eq("dispatch: fallocate (285) reaches glue", d(285), k::fd::errno::EINVAL);
    // `getgroups` is the pair that makes the two-number shape earn itself:
    // asm-generic 158 is `getgroups`, x86_64 158 is `arch_prctl`, and this
    // kernel answers both. `size == 0` is the probe form and must report the
    // count (zero here) without touching the buffer; a negative size is EINVAL.
    t.check("dispatch: getgroups 115 -> 158", hop(Syscall::Getgroups, 115, nr::GETGROUPS));
    t.check_eq("dispatch: glue reports 0 supplementary groups", d(115), 0);
    t.check_eq(
        "dispatch: getgroups(-1) is EINVAL",
        syscall_dispatch(115, (-1i64) as u64, 0, 0, 0, 0, 0),
        (-22i64) as u64,
    );

    // Batch 3. Unlike the credentials above, **both of these change their
    // answer**, so these are value checks with a working negative control: the
    // arm they replaced would fail each one.
    t.check("dispatch: prlimit64 302 -> 261", hop(Syscall::Prlimit64, 302, nr::PRLIMIT64));
    t.check("dispatch: getrandom 318 -> 278", hop(Syscall::Getrandom, 318, nr::GETRANDOM));

    // C3's six. Each was `ENOSYS` on this target — the implementation was in
    // glue all along and there was no number to reach it by — so unlike the
    // credentials above the *value* moves too, and each hop is asserted for
    // the same reason: a missed one lands on a different arm, not on none.
    // `clock_adjtime` is the sharpest: x86_64 305 is `nr::TIME`, an
    // Akuma-private number, so a table that forgot to translate it would
    // answer a `clock_adjtime` with the wall clock in seconds and no error.
    t.check("dispatch: clock_getres 229 -> 114", hop(Syscall::ClockGetres, 229, nr::CLOCK_GETRES));
    t.check(
        "dispatch: clock_nanosleep 230 -> 115",
        hop(Syscall::ClockNanosleep, 230, nr::CLOCK_NANOSLEEP),
    );
    t.check(
        "dispatch: clock_adjtime 305 -> 266, not to nr::TIME",
        hop(Syscall::ClockAdjtime, 305, nr::CLOCK_ADJTIME) && nr::CLOCK_ADJTIME != nr::TIME,
    );
    t.check("dispatch: setitimer 38 -> 103", hop(Syscall::Setitimer, 38, nr::SETITIMER));
    t.check("dispatch: times 100 -> 153", hop(Syscall::Times, 100, nr::TIMES));
    t.check("dispatch: getrusage 98 -> 165", hop(Syscall::Getrusage, 98, nr::GETRUSAGE));

    // `clock_gettime(CLOCK_MONOTONIC)` through the real dispatcher, checked
    // against **this kernel's own clock**.
    //
    // The value check that could not have been written before C3. This arm
    // used to read `net::uptime_us` locally; glue's reads
    // `akuma_primitives::clock`, which `akuma-syscalls-time` never consulted —
    // it asked `akuma_timer::uptime_us`, i.e. CNTVCT, i.e. `0` on x86 forever.
    // So the property is not "a plausible number came back" but "the number is
    // the one this kernel counts", and a fold that lost the hook reports zero
    // against a running uptime rather than an error.
    //
    // Agreement within one tick rather than exact equality: the two reads are
    // separated by a syscall's worth of work, and `net::uptime_us` advances in
    // whole `US_PER_TICK_TARGET` steps.
    //
    // It deliberately does **not** assert that the clock *moves*. The suite
    // runs with the LAPIC timer stopped (`lapic::stop_timer`, so the tick does
    // not interleave with the suite's own output), so `allow_tick` opens
    // windows no interrupt is waiting in and nothing here can advance it —
    // measured, after a first version of this check spun 50 million times and
    // failed against a working clock. Movement is a ring-3 property and
    // `/probes/clockprobe` is where it is proved.
    const CLOCK_MONOTONIC: u64 = 1;
    let mut ts = [0i64; 2];
    let r1 = syscall_dispatch(228, CLOCK_MONOTONIC, ts.as_mut_ptr() as u64, 0, 0, 0, 0);
    let kernel_us = k::net::uptime_us();
    let syscall_us = (ts[0].cast_unsigned()) * 1_000_000 + (ts[1].cast_unsigned()) / 1_000;
    t.check_eq("dispatch: glue answers clock_gettime", r1, 0);
    t.check("dispatch: the monotonic clock is not stuck at zero", syscall_us != 0);
    t.check(
        "dispatch: and it is this kernel's clock, not another one",
        kernel_us.abs_diff(syscall_us) <= u64::from(k::lapic::US_PER_TICK_TARGET),
    );

    // `prlimit64(0, RLIMIT_STACK, NULL, &old)`. The old arm was `=> 0` and
    // wrote nothing, so the sentinel is what catches it — a return-value check
    // alone passes against the bug, exactly as it did for `symlink` below.
    // `RLIM_INFINITY` is the sentinel because it is the one value the arm is
    // *not* supposed to produce for this resource.
    const RLIMIT_STACK: u64 = 3;
    let mut rlim = [u64::MAX; 2];
    let prlimit_rc = syscall_dispatch(302, 0, RLIMIT_STACK, 0, rlim.as_mut_ptr() as u64, 0, 0);
    t.check_eq("dispatch: glue accepts prlimit64", prlimit_rc, 0);
    t.check(
        "dispatch: and writes a real RLIMIT_STACK (the old arm wrote nothing)",
        rlim[0] == (ELF_STACK_MAX_PAGES * 4096) as u64 && rlim[1] == rlim[0],
    );
    // The limit reported must be the stack a program actually gets, and this is
    // the check that says which one that is. `exec_runtime.rs` supplied the
    // *kernel* stack here until the fold made anyone read it — a placeholder
    // that was correct only because it was dead.
    t.check(
        "dispatch: RLIMIT_STACK is the user stack, not the kernel one",
        rlim[0] != k::sched::STACK_SIZE as u64,
    );

    // `getrandom(buf, len, 0)` through glue, which reaches `RDRAND` only via
    // the `akuma_primitives::rng` hook `boot::install_shared_sinks` registers.
    // Without that hook glue asks the virtio-rng device, which no rig of this
    // target has, and every call is `EIO` — so this is the check that the seam
    // is wired, not just that the number hops.
    t.check("dispatch: an entropy source is registered", akuma_primitives::rng::is_registered());
    let mut draw1 = [0u8; 32];
    let mut draw2 = [0u8; 32];
    let got1 = syscall_dispatch(318, draw1.as_mut_ptr() as u64, draw1.len() as u64, 0, 0, 0, 0);
    let got2 = syscall_dispatch(318, draw2.as_mut_ptr() as u64, draw2.len() as u64, 0, 0, 0, 0);
    t.check_eq("dispatch: glue getrandom returns the full length", got1, draw1.len() as u64);
    t.check_eq("dispatch: and again", got2, draw2.len() as u64);
    // Two draws differing is what separates a wired source from a loop that
    // returned the byte count over an untouched buffer — which is what the old
    // arm did whenever `RDRAND` gave up, and it reported success either way.
    t.check("dispatch: two draws differ", draw1 != draw2);
    t.check("dispatch: and neither is all-zero", draw1 != [0u8; 32] && draw2 != [0u8; 32]);
    // Larger than glue's 256-byte chunk, because the arm this replaced capped
    // at one chunk and returned `min(len, 256)`. A caller asking for 300 got
    // 256 and had to loop; glue loops for it.
    let mut big = [0u8; 300];
    t.check_eq(
        "dispatch: getrandom(300) is not truncated to one 256-byte chunk",
        syscall_dispatch(318, big.as_mut_ptr() as u64, big.len() as u64, 0, 0, 0, 0),
        big.len() as u64,
    );
    t.check("dispatch: and the tail past 256 was written", big[256..] != [0u8; 44]);

    // The `sc-epoll`/`sc-eventfd` rows added with `mio`/crossterm's amd64
    // epoll backend. Number-mapping only, not a live round trip: unlike
    // credentials or `getrandom`, `sys_epoll_create1` needs a registered
    // process's `SharedFdTable` to allocate into (`EBADF` without one), and
    // the boot task this suite runs as is not one — a live probe belongs in
    // `userspace/`, not here. What this pins is the fold, the same crossing
    // C3's six timers guarded above: x86_64 233 is `epoll_ctl`, but
    // asm-generic 233 is `nr::MADVISE` — a missed hop would not compile to
    // `ENOSYS`, it would compile to a working `madvise` call fed an fd, an
    // op and an event pointer as if they were an address, a length and an
    // advice value.
    t.check("dispatch: epoll_create1 291 -> 20", hop(Syscall::EpollCreate1, 291, nr::EPOLL_CREATE1));
    t.check("dispatch: epoll_ctl 233 -> 21", hop(Syscall::EpollCtl, 233, nr::EPOLL_CTL));
    t.check("dispatch: epoll_pwait 281 -> 22", hop(Syscall::EpollPwait, 281, nr::EPOLL_PWAIT));
    t.check("dispatch: eventfd2 290 -> 19", hop(Syscall::Eventfd2, 290, nr::EVENTFD2));
    // The two x86-only legacy spellings, `epoll_create`(213) and
    // `epoll_wait`(232), must not also be neutral-table numbers — the
    // overlap check every entry in `X86_ONLY` above gets, spelled out for
    // these two specifically because they are new.
    t.check(
        "dispatch: 213 is not also a neutral-table number",
        Syscall::from_x86_64(213).is_none(),
    );
    t.check(
        "dispatch: 232 is not also a neutral-table number",
        Syscall::from_x86_64(232).is_none(),
    );

    // The `sc-timerfd` rows.
    t.check("dispatch: timerfd_create 283 -> 85", hop(Syscall::TimerfdCreate, 283, nr::TIMERFD_CREATE));
    t.check("dispatch: timerfd_settime 286 -> 86", hop(Syscall::TimerfdSettime, 286, nr::TIMERFD_SETTIME));
    t.check("dispatch: timerfd_gettime 287 -> 87", hop(Syscall::TimerfdGettime, 287, nr::TIMERFD_GETTIME));

    // `pidfd_open`(434) is a **shared-number** dispatch (`amd64/src/
    // usermode.rs`'s third match block), not a `Syscall` row — 434 must NOT
    // decode through the neutral table, or the shared-number arm and a table
    // row would silently race for the same number.
    t.check(
        "dispatch: 434 is not a neutral-table number (shared-number dispatch owns it)",
        Syscall::from_x86_64(434).is_none(),
    );
    // `pidfd_open` on a pid that is not a child of the caller: `ESRCH`
    // (`get_child_channel` returns `None`), through the real dispatcher by the
    // x86_64 number userspace sends. This is the whole live check available
    // without a registered process — see the epoll rows above for why.
    t.check_eq(
        "dispatch: pidfd_open on an unknown pid is ESRCH",
        syscall_dispatch(434, 999_999, 0, 0, 0, 0, 0),
        (-3i64) as u64,
    );
    // `pidfd_send_signal`(424) is the shared-number dispatch's other member.
    t.check(
        "dispatch: 424 is not a neutral-table number (shared-number dispatch owns it)",
        Syscall::from_x86_64(424).is_none(),
    );
    // On a `pidfd` this process never opened, `EBADF` — `get_fd` finds
    // nothing at 999, `_ => return EBADF` in `sys_pidfd_send_signal`.
    t.check_eq(
        "dispatch: pidfd_send_signal on an unopened fd is EBADF",
        syscall_dispatch(424, 999, 15, 0, 0, 0, 0),
        (-9i64) as u64,
    );
    // Linux's one `pidfd_open` flag is `PIDFD_NONBLOCK`; `O_CLOEXEC` is refused
    // (every pidfd is close-on-exec anyway), and before the pid is looked at.
    t.check_eq(
        "dispatch: pidfd_open(O_CLOEXEC) is EINVAL",
        syscall_dispatch(434, 999_999, 0x8_0000, 0, 0, 0, 0),
        (-22i64) as u64,
    );

    // `rt_sigsuspend` and `rt_sigpending` are glue's. A bad `sigsetsize` is
    // `EINVAL` from glue's own check: an answer, where this kernel used to
    // give `ENOSYS` for both.
    t.check("dispatch: rt_sigsuspend 130 -> 133", hop(Syscall::RtSigsuspend, 130, nr::RT_SIGSUSPEND));
    t.check("dispatch: rt_sigpending 127 -> 136", hop(Syscall::RtSigpending, 127, nr::RT_SIGPENDING));
    t.check_eq(
        "dispatch: rt_sigsuspend with sigsetsize 4 is EINVAL",
        syscall_dispatch(130, 0, 4, 0, 0, 0, 0),
        (-22i64) as u64,
    );
    t.check_eq(
        "dispatch: rt_sigpending with sigsetsize 16 is EINVAL",
        syscall_dispatch(127, 0, 16, 0, 0, 0, 0),
        (-22i64) as u64,
    );

    // The `sc-aio` rows. `io_setup` 206 -> asm-generic 0 is the sharpest of
    // these: x86_64 0 is `Read` in this very table, so a missed hop here
    // would not compile to `ENOSYS`, it would compile to a working `read`
    // fed an `nr_events`/`ctx_idp` pair as if they were an fd/buf/len.
    t.check("dispatch: io_setup 206 -> 0", hop(Syscall::IoSetup, 206, nr::IO_SETUP));
    t.check("dispatch: io_destroy 207 -> 1", hop(Syscall::IoDestroy, 207, nr::IO_DESTROY));
    t.check("dispatch: io_getevents 208 -> 4", hop(Syscall::IoGetevents, 208, nr::IO_GETEVENTS));
    t.check("dispatch: io_submit 209 -> 2", hop(Syscall::IoSubmit, 209, nr::IO_SUBMIT));
    t.check("dispatch: io_cancel 210 -> 3", hop(Syscall::IoCancel, 210, nr::IO_CANCEL));
    // No live `io_setup` round trip here, unlike `msgget` below:
    // `sys_io_setup` resolves its own pid through
    // `akuma_exec::process::lookup_process_shared`, which the boot task does
    // not answer to (`EFAULT`, measured) — a different, stricter lookup than
    // `current_process_shared()`'s "am I registered at all", which is why
    // `msgget`'s `current_box_id()` (built on the latter) still works here.
    // A live probe belongs in `userspace/`, same reasoning as the epoll rows
    // above.

    // The `sc-sysv-ipc` rows (message queues only — see the row's own doc
    // comment for why shm/sem are absent).
    t.check("dispatch: msgget 68 -> 186", hop(Syscall::MsgGet, 68, nr::MSGGET));
    t.check("dispatch: msgsnd 69 -> 189", hop(Syscall::MsgSnd, 69, nr::MSGSND));
    t.check("dispatch: msgrcv 70 -> 188", hop(Syscall::MsgRcv, 70, nr::MSGRCV));
    t.check("dispatch: msgctl 71 -> 187", hop(Syscall::MsgCtl, 71, nr::MSGCTL));
    // A live `msgget`/`msgsnd`/`msgrcv` round trip, IPC_PRIVATE so it cannot
    // collide with anything else the suite creates.
    let msqid = syscall_dispatch(68, 0, 0o600, 0, 0, 0, 0);
    t.check("dispatch: glue accepts msgget(IPC_PRIVATE)", !k::fd::errno::is_err(msqid));
    if !k::fd::errno::is_err(msqid) {
        #[repr(C)]
        struct Msgbuf { mtype: i64, data: [u8; 5] }
        let mut out = Msgbuf { mtype: 0, data: [0; 5] };
        let msg = Msgbuf { mtype: 7, data: *b"hello" };
        let send_rc = syscall_dispatch(69, msqid, core::ptr::addr_of!(msg) as u64, 5, 0, 0, 0);
        t.check_eq("dispatch: glue accepts msgsnd", send_rc, 0);
        let recv_rc = syscall_dispatch(70, msqid, core::ptr::addr_of_mut!(out) as u64, 5, 0, 0, 0);
        t.check_eq("dispatch: msgrcv returns the byte count", recv_rc, 5);
        t.check_eq("dispatch: and the right mtype", out.mtype as u64, 7);
        t.check("dispatch: and the right bytes", out.data == *b"hello");
        const IPC_RMID: u64 = 0;
        let rmid_rc = syscall_dispatch(71, msqid, IPC_RMID, 0, 0, 0, 0);
        t.check_eq("dispatch: glue accepts msgctl(IPC_RMID)", rmid_rc, 0);
    }

    // The `sc-containers` rows — `mount`/`umount2`. The box-lifecycle calls
    // (315-318, 324, 325) are private numbers; they are exercised live by
    // `scripts/box_amd64_suite.py`, not by this table.
    t.check("dispatch: mount 165 -> 40", hop(Syscall::Mount, 165, nr::MOUNT));
    t.check("dispatch: umount2 166 -> 39", hop(Syscall::Umount2, 166, nr::UMOUNT2));
    // The global `/` guard fires before any real mount-table lookup, so this
    // needs no disk and runs even when `have_fs` is false.
    let root = b"/\0";
    t.check_eq(
        "dispatch: umount2(\"/\") is EBUSY",
        syscall_dispatch(166, root.as_ptr() as u64, 0, 0, 0, 0, 0),
        (-16i64) as u64,
    );

    if !have_fs {
        t.note("dispatch: no filesystem; symlink round trip skipped", 0);
        return;
    }

    // A live tmpfs mount/write/unmount round trip, through the real
    // dispatcher for `mount`/`umount2` and the same native VFS helpers the
    // redirect suite above uses to observe the effect (`k::fs::*`,
    // `akuma_vfs_glue`'s facade) rather than a hand-rolled openat/read/write
    // sequence. The disk image survives a boot, so clear any leftover before
    // creating it, same discipline as the symlink probe below.
    let _ = k::fs::remove_dir("/mount-probe");
    let dir_ok = k::fs::create_dir("/mount-probe").is_ok();
    t.check("mount: probe directory created", dir_ok);
    if dir_ok {
        let mount_target = b"/mount-probe\0";
        let fstype = b"tmpfs\0";
        let mount_rc = syscall_dispatch(
            165,
            0,
            mount_target.as_ptr() as u64,
            fstype.as_ptr() as u64,
            0,
            0,
            0,
        );
        t.check_eq("dispatch: glue accepts mount(tmpfs)", mount_rc, 0);
        if !k::fd::errno::is_err(mount_rc) {
            t.check(
                "mount: a file written after mounting lands on the new tmpfs",
                k::fs::write_file("/mount-probe/hello.txt", b"MOUNTOK").is_ok(),
            );
            t.check(
                "mount: and reads back the exact bytes",
                k::fs::read_file("/mount-probe/hello.txt").is_ok_and(|d| d == b"MOUNTOK"),
            );
            let umount_rc = syscall_dispatch(166, mount_target.as_ptr() as u64, 0, 0, 0, 0, 0);
            t.check_eq("dispatch: glue accepts umount2", umount_rc, 0);
            // Unmounted: the underlying (empty) ext2 directory is exposed
            // again, so the file that only ever existed on the tmpfs overlay
            // is gone — the property that proves this was a real second
            // filesystem, not a write straight through to `/mount-probe`.
            t.check(
                "mount: the file is gone once the tmpfs is unmounted",
                k::fs::read_file("/mount-probe/hello.txt").is_err(),
            );
        }
        t.check(
            "mount: cleanup removes the probe directory",
            k::fs::remove_dir("/mount-probe").is_ok(),
        );
    }

    let target = b"/probe.txt\0";
    let link = b"/dispatch-symlink-probe\0";
    // The disk image survives a boot, so clear any leftover before creating it.
    let _ = syscall_dispatch(87, link.as_ptr() as u64, 0, 0, 0, 0, 0);

    // Straight through the dispatcher, by number, the way userspace arrives —
    // all four calls of the round trip. Since 4b step 2 batch 1 the `*at`
    // family is glue's, so this now exercises glue's arms end to end from the
    // boot task: `AT_FDCWD` with no registered process resolves against `/`
    // (glue's own default), and the absolute paths never need an fd table.
    let link_rc = syscall_dispatch(88, target.as_ptr() as u64, link.as_ptr() as u64, 0, 0, 0, 0);
    t.check_eq("dispatch: x86_64 88 is symlink(2) and succeeds", link_rc, 0);
    let mut buf = [0u8; 64];
    let n = syscall_dispatch(89, link.as_ptr() as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    t.check_eq("dispatch: the link reads back its target length", n, 10);
    t.check("dispatch: and the target itself", &buf[..10] == b"/probe.txt");
    // Cleanup through the real arm: x86_64 87 is unlink(2), unlinkat's shim.
    let unlink_rc = syscall_dispatch(87, link.as_ptr() as u64, 0, 0, 0, 0, 0);
    t.check_eq("dispatch: unlink removes the link", unlink_rc, 0);
    // And it is gone: a second readlink is ENOENT (glue distinguishes it from
    // the EINVAL a non-symlink gets — the answer the local arm never made).
    let gone_rc = syscall_dispatch(89, link.as_ptr() as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    t.check_eq(
        "dispatch: readlink on a missing path is ENOENT",
        gone_rc,
        (-2i64) as u64,
    );
    // The half that only glue answers right: `/probe.txt` **exists** and is
    // not a symlink, so readlink is `EINVAL` — the local arm collapsed both
    // answers to `ENOENT`, so this check is red against the pre-fold kernel
    // by construction. That is what makes it a differentiator and not a
    // re-statement of the check above.
    let notlink = syscall_dispatch(89, target.as_ptr() as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    t.check_eq(
        "dispatch: readlink on a non-symlink is EINVAL, not ENOENT",
        notlink,
        (-22i64) as u64,
    );
}

/// **The fault path's lookup, priced.**
///
/// 5b slice 4 moved `with_current_regions` / `with_current_address_space` /
/// `cow_swap_frame` off a per-CPU field plus one array index and onto
/// `akuma-exec`'s process table. Those three run on every demand-paging fault
/// and every CoW break, so the honest question about that slice is not whether
/// it is tidier but what it costs, and this is the instrument that answers it —
/// permanently, in every boot, rather than once in a session.
///
/// # Why it is measured here and not with `mem_fault_cost`
///
/// It cannot be measured from ring 3 on this target. `userspace/memprobe/c/`
/// exists and both its instruments build for `x86_64-linux-musl`, but every
/// arm of them is timed with `clock_gettime(CLOCK_MONOTONIC)` and **this
/// kernel's clock has 10 ms granularity**: `net::uptime_us` is
/// `lapic::ticks() * US_PER_TICK`, `US_PER_TICK` is 10 000, and every clock on
/// the target derives from it. Measured 2026-09-08 on QEMU/TCG: the
/// 1000-iteration `mmap_lazy` control reads exactly 10 000 ns per rep (one
/// tick) and every 512-fault bracket reads **0 ns**. A per-fault cost is
/// nanoseconds; the finest thing userspace can see here is ten milliseconds.
/// So the probe is not "not run", it is not *runnable*, and the difference
/// matters — see `scripts/benchmarks/amd64_fault_cost.py`, which will show you.
///
/// TSC has the resolution the tick clock lacks, and the kernel is where the TSC
/// is reachable.
///
/// # What the two numbers are
///
/// * `slot` — [`current_proc_slot`], the per-CPU `UserCtx` field read. This is
///   what the **old** lookup was, plus one bounds-checked array index that no
///   measurement here could separate from noise.
/// * `process` — [`current_process`], the whole new lookup: the same per-CPU
///   read of the task slot, then `THREAD_IDENTITY[tid]`'s cached process-table
///   slot and generation, then `SlotTable::ref_if_current`.
///
/// Both are reported per call, in **TSC ticks**, uncalibrated — the comparison
/// is the point and a frequency would add a second thing to get wrong. Under
/// TCG they are emulation ticks and only the ratio means anything; on real
/// silicon they are cycles.
///
/// # And the check that is not a timing
///
/// A number can drift; what must not is *which path* the lookup takes. The
/// check asserts that `IDENTITY_FALLBACKS` does not move across the whole
/// measured loop — every resolution was a cache hit, not the 256-slot table
/// scan the naive fold would have paid. That is the property the hand-off
/// warned about, and unlike a nanosecond count it cannot pass by luck.
pub fn identity_cost_test(t: &mut Suite) {
    /// Enough iterations that the loop dominates the two `rdtsc`s, and few
    /// enough to cost nothing on a TCG boot.
    const ITERS: u64 = 20_000;

    fn tsc() -> u64 {
        akuma_cpu::tsc::rdtsc()
    }

    let free_before = akuma_pmm::free_count();
    // The boot task is registered nowhere — that is the whole boot-suite window
    // this target has tripped over three times — so measuring the cached path
    // from it needs an identity to cache. Register one, measure, retire it.
    // A real `UserAddressSpace`, because that is what a real process carries and
    // `register_exec_process` takes ownership of one; it is never activated.
    let Some(space) = UserAddressSpace::new() else {
        t.check("identity: probe address space built", false);
        return;
    };
    let image = Image { space, entry: 0, stack: 0, regions: Vec::new() };
    let pid = alloc_pid();
    let task = k::sched::current_task();
    let Some(table_slot) = akuma_exec::process::reserve_process_slot() else {
        t.check("identity: probe process slot reserved", false);
        return;
    };
    register_exec_process(pid, 1, task, image, 0, "cost-probe", b"cost-probe\0", None, None, None, table_slot);

    if !t.check("identity: the running task resolves to its process", current_process().is_some()) {
        finish_test_process(pid, task);
        return;
    }

    let fallbacks_before =
        akuma_exec::process::IDENTITY_FALLBACKS.load(Ordering::Relaxed);

    let t0 = tsc();
    for _ in 0..ITERS {
        core::hint::black_box(current_proc_slot());
    }
    let t1 = tsc();
    for _ in 0..ITERS {
        core::hint::black_box(current_process().is_some());
    }
    let t2 = tsc();

    let fallbacks = akuma_exec::process::IDENTITY_FALLBACKS
        .load(Ordering::Relaxed)
        .saturating_sub(fallbacks_before);

    let slot_ticks = t1.saturating_sub(t0) / ITERS;
    let proc_ticks = t2.saturating_sub(t1) / ITERS;
    t.note("identity: per-call TSC ticks, per-CPU slot read (the old lookup)", slot_ticks);
    t.note("identity: per-call TSC ticks, registered-process lookup", proc_ticks);
    t.note("identity: added ticks per fault-path lookup", proc_ticks.saturating_sub(slot_ticks));
    // The load-bearing assertion. A fallback is the slow path — a `THREAD_PID_MAP`
    // walk and a scan of up to 256 process slots — and one per fault is exactly
    // the regression this slice had to avoid. Zero, not "few": every iteration
    // resolves the same live process from the same live thread, so any fallback
    // at all means the cache is not being consulted.
    t.check_eq("identity: every lookup was a cache hit, not a table scan", fallbacks, 0);

    finish_test_process(pid, task);
    t.check_eq(
        "identity: probe teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Run two isolated processes concurrently and prove they interleave.
pub fn smoke_test(t: &mut Suite) {
    const ROUNDS: u32 = 3;
    const MSG_A: &[u8] = b"    [ring3 A] round\n";
    const MSG_B: &[u8] = b"    [ring3 B] round\n";

    let free_before = akuma_pmm::free_count();

    let (Some(a), Some(b)) = (
        Image::new(MSG_A, ROUNDS, 0, 0x0A),
        Image::new(MSG_B, ROUNDS, 0, 0x0B),
    ) else {
        t.check("ring3: processes built", false);
        return;
    };

    // The isolation property, checked before either runs: the same virtual
    // address resolves to different frames, and to nothing in the kernel's space.
    let pa_a = a.space.translate(USER_CODE_VA);
    let pa_b = b.space.translate(USER_CODE_VA);
    let pa_k = k::paging::translate(USER_CODE_VA);

    let (Some(a), Some(b)) = (
        start_test_process(0, a, 0, "ring3-a"),
        start_test_process(1, b, 0, "ring3-b"),
    ) else {
        t.check("ring3: processes spawned", false);
        return;
    };
    t.check("ring3: processes spawned", true);

    serial::puts("  -- userspace output follows --\n");
    // Drive the round-robin from the boot task until both processes finish.
    let mut spins = 0u64;
    while !k::sched::all_user_tasks_finished() && spins < 10_000 {
        spins += 1;
        k::sched::yield_now();
    }

    t.check("ring3: both spaces map the test VA", pa_a.is_some() && pa_b.is_some());
    t.check("ring3: same VA, different frames", pa_a != pa_b);
    t.check("ring3: kernel space does not map it", pa_k.is_none());
    t.check_eq(
        "ring3: writes served",
        WRITE_SEQ_LEN.load(Ordering::Relaxed),
        u64::from(ROUNDS) * 2,
    );

    // The multitasking claim. Counts alone cannot distinguish "A ran three
    // times then B ran three times" from real interleaving, so this looks for a
    // change of task between consecutive writes.
    let len = WRITE_SEQ_LEN.load(Ordering::Relaxed).min(WRITE_SEQ.len() as u64) as usize;
    let switches = (1..len)
        .filter(|&i| {
            WRITE_SEQ[i].load(Ordering::Relaxed) != WRITE_SEQ[i - 1].load(Ordering::Relaxed)
        })
        .count();
    t.check_eq(
        "ring3: processes interleaved",
        switches as u64,
        u64::from(ROUNDS) * 2 - 1,
    );

    // Both tasks have finished; nothing else names these processes.
    finish_test_process(a.0, a.1);
    finish_test_process(b.0, b.1);
    t.check_eq(
        "ring3: address-space teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Two processes that never yield, interleaved by the timer alone.
///
/// The distinction from [`smoke_test`] is the whole point: those processes call
/// `sched_yield`, so interleaving proves only that the scheduler works. These
/// spin in ring 3 with no syscall between writes, so the *only* way control can
/// leave one is the timer interrupt taking it — which is preemption.
pub fn preempt_test(t: &mut Suite) {
    const ROUNDS: u32 = 3;
    // Long enough to span at least one tick on both machines. Emulation and real
    // silicon differ by ~17x here (§3.9), so this is sized for the slower-to-tick
    // of the two — the Ryzen, at roughly 2.1M spins per tick.
    const DELAY: u32 = 8_000_000;

    let base = WRITE_SEQ_LEN.load(Ordering::Relaxed);
    let free_before = akuma_pmm::free_count();

    let (Some(c), Some(d)) = (
        Image::new(b"    [ring3 C] spinning, never yields\n", ROUNDS, DELAY, 0x0C),
        Image::new(b"    [ring3 D] spinning, never yields\n", ROUNDS, DELAY, 0x0D),
    ) else {
        t.check("preempt: processes built", false);
        return;
    };

    let (Some(c), Some(d)) = (
        start_test_process(2, c, 0, "ring3-c"),
        start_test_process(3, d, 0, "ring3-d"),
    ) else {
        t.check("preempt: processes spawned", false);
        return;
    };
    t.check("preempt: processes spawned", true);

    serial::puts("  -- userspace output follows (no yields) --\n");
    k::lapic::start_timer();
    let mut spins = 0u64;
    while !k::sched::all_user_tasks_finished() && spins < 100_000 {
        spins += 1;
        k::sched::yield_now();
    }
    k::lapic::stop_timer();

    let len = WRITE_SEQ_LEN.load(Ordering::Relaxed).min(WRITE_SEQ.len() as u64);
    let switches = (base + 1..len)
        .filter(|&i| {
            let i = i as usize;
            WRITE_SEQ[i].load(Ordering::Relaxed) != WRITE_SEQ[i - 1].load(Ordering::Relaxed)
        })
        .count();

    t.check_eq("preempt: writes served", len - base, u64::from(ROUNDS) * 2);
    // Only >= 1 is asserted. How *often* the timer lands inside a spin depends on
    // the tick period against the delay loop, which differs by an order of
    // magnitude between QEMU and real silicon; requiring an exact count would be
    // asserting on the host's speed rather than on preemption.
    t.check("preempt: timer interleaved two non-yielding processes", switches >= 1);
    t.note("preempt: task switches observed between writes", switches as u64);

    // Both tasks have finished; nothing else names these processes.
    finish_test_process(c.0, c.1);
    finish_test_process(d.0, d.1);
    t.check_eq(
        "preempt: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Two spinning processes with every core running: do they end up on different
/// cores?
///
/// The ring-3 half of `smp::smoke_test`. Its kernel workers proved two *kernel*
/// tasks can execute at once by dropping the BKL around a spin; this proves the
/// thing the BKL design is for — that user code needs no such favour. The same
/// two non-yielding programs as [`preempt_test`], with each `write` tagged by
/// the core that served it ([`WRITE_CPU`]): with a second core idling next to a
/// BSP that is busy driving the test, the idle core's tick picks one of them
/// up, and the writes arrive from two cores. Skipped, and said so, on one core.
pub fn smp_parallel_test(t: &mut Suite) {
    const ROUNDS: u32 = 3;
    const DELAY: u32 = 8_000_000;

    if k::smp::online_cpus() < 2 {
        t.note("smp: one core online, ring-3 parallelism check skipped", 1);
        return;
    }

    let base = WRITE_SEQ_LEN.load(Ordering::Relaxed);
    let free_before = akuma_pmm::free_count();

    let (Some(c), Some(d)) = (
        Image::new(b"    [ring3 E] spinning on some core\n", ROUNDS, DELAY, 0x0E),
        Image::new(b"    [ring3 F] spinning on some core\n", ROUNDS, DELAY, 0x0F),
    ) else {
        t.check("smp ring3: processes built", false);
        return;
    };

    // Slots 2 and 3 are free again: `preempt_test` reaped its processes.
    let (Some(c), Some(d)) = (
        start_test_process(2, c, 0, "ring3-e"),
        start_test_process(3, d, 0, "ring3-f"),
    ) else {
        t.check("smp ring3: processes spawned", false);
        return;
    };
    t.check("smp ring3: processes spawned", true);

    serial::puts("  -- userspace output follows (two cores) --\n");
    k::lapic::start_timer();
    let mut spins = 0u64;
    while !k::sched::all_user_tasks_finished() && spins < 100_000_000 {
        spins += 1;
        k::sched::yield_now();
    }
    k::lapic::stop_timer();

    let len = WRITE_SEQ_LEN.load(Ordering::Relaxed).min(WRITE_SEQ.len() as u64);
    let cpus = (base..len).fold(0u64, |acc, i| {
        let c = WRITE_CPU[i as usize].load(Ordering::Relaxed);
        if c < 64 { acc | (1 << c) } else { acc }
    });

    t.check_eq("smp ring3: writes served", len - base, u64::from(ROUNDS) * 2);
    t.note("smp ring3: cpu mask the writes came from", cpus);
    t.check("smp ring3: two processes ran on two cores", cpus.count_ones() >= 2);

    // Both tasks have finished; nothing else names these processes.
    finish_test_process(c.0, c.1);
    finish_test_process(d.0, d.1);
    t.check_eq(
        "smp ring3: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Call `f(p_vaddr, p_memsz)` for every non-empty `PT_LOAD` program header in
/// an ELF64 image.
///
/// Reads the fields it needs at their architectural offsets rather than going
/// through the `elf` crate, which is the point: what the loader did is checked
/// against a segment list derived independently of the code that produced it.
/// A parser bug that dropped a segment would otherwise agree with itself — and
/// since C1 step 6 the loader *is* the `elf` crate, so an independent reader
/// here is the only thing left that can disagree with it.
///
/// `p_memsz == 0` segments are skipped, because a segment that occupies no
/// memory is mapped by neither loader and counting one would make the caller's
/// comparison fail for a reason that is not about placement. No image in this
/// tree carries one; the filter is so that the first that does fails loudly
/// somewhere better than here.
fn for_each_pt_load(image: &[u8], mut f: impl FnMut(u64, u64)) {
    const PT_LOAD: u32 = 1;
    let u16_at = |off: usize| u16::from_le_bytes([image[off], image[off + 1]]) as usize;
    let u64_at = |b: &[u8]| {
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    };
    let phoff = u64_at(&image[32..40]) as usize;
    let phentsize = u16_at(54);
    let phnum = u16_at(56);

    for i in 0..phnum {
        let at = phoff + i * phentsize;
        // An ELF64 phdr is 56 bytes: p_type(4) p_flags(4) p_offset(8)
        // p_vaddr(8) p_paddr(8) p_filesz(8) p_memsz(8).
        let Some(ph) = image.get(at..at + 56) else { continue };
        if u32::from_le_bytes([ph[0], ph[1], ph[2], ph[3]]) != PT_LOAD {
            continue;
        }
        let vaddr = u64_at(&ph[16..24]);
        let memsz = u64_at(&ph[40..48]);
        if memsz != 0 {
            f(vaddr, memsz);
        }
    }
}

/// Count non-empty `PT_LOAD` program headers. See [`for_each_pt_load`].
fn count_pt_load(image: &[u8]) -> u64 {
    let mut n = 0;
    for_each_pt_load(image, |_, _| n += 1);
    n
}

/// Load a linked ELF image and run it.
///
/// The distinction from the two tests above is the same one Stage F drew
/// against Stage E: those run a program *this file assembled*, so they can only
/// ever exercise code the kernel already knew how to emit. This one runs an
/// image `rustc` produced and the kernel had to parse — headers it did not
/// write, segment placement it did not choose, and an entry point it read out
/// of the file.
///
/// # What the exit status carries
///
/// `hello.rs` checks six properties of the load and reports them as bits, and
/// the expectation here is spelled out as named constants rather than as `0x3F`
/// so that a partial failure names itself. Reporting through the status rather
/// than through `write` is what makes a bad load fail the boot: a program that
/// printed its verdict would still have "passed" by running at all.
pub fn socketpair_smoke_test(t: &mut Suite) {
    socketpair_test(t);
}

pub fn elf_test(t: &mut Suite) {
    /// `.data` arrived with its linked contents.
    const DATA_OK: u64 = 1 << 0;
    /// `.bss` is zero across all 32 KiB of it.
    const BSS_OK: u64 = 1 << 1;
    /// The `PF_W` segment really is writable.
    const WRITABLE_OK: u64 = 1 << 2;
    /// `argc` is what the stack builder wrote.
    const ARGC_OK: u64 = 1 << 3;
    /// `argv[0]` points at the expected NUL-terminated string.
    const ARGV_OK: u64 = 1 << 4;
    /// `AT_PAGESZ` is present in the auxiliary vector and is 4096.
    const AUXV_OK: u64 = 1 << 5;
    /// A syscall preserved every register the Linux x86_64 ABI promises. This
    /// bit is the one that failed on its first run — see `syscall_entry`.
    const REGS_OK: u64 = 1 << 6;
    const ALL_OK: u64 =
        DATA_OK | BSS_OK | WRITABLE_OK | ARGC_OK | ARGV_OK | AUXV_OK | REGS_OK;

    let free_before = akuma_pmm::free_count();

    // Rejection first, and before anything is allocated: a loader is judged as
    // much by what it refuses as by what it loads, and these cost nothing to
    // check because each fails before a frame is touched.
    reject_test(t);

    // From the filesystem when there is one. This is the whole point of the
    // stage: the bytes came off a disk the kernel discovered, through a
    // filesystem it mounted, found by path — rather than out of its own `.rodata`.
    let from_disk = k::fs::read_file("/bin/hello");
    let image: &[u8] = if let Ok(bytes) = from_disk.as_deref() {
        {
            // The embedded copy and the on-disk copy come from two different
            // build steps (`build.rs` into OUT_DIR, `mkdisk.sh` into the image).
            // They are supposed to be the same file; asserting it is what turns
            // that into a checked fact rather than a convention, and a mismatch
            // would mean the image is stale — which would otherwise show up as
            // the previous run's program silently running again.
            // Reported, not scored. This asserts that two *build steps* agreed
            // — `build.rs` into OUT_DIR and `mkdisk.sh` into the image — which
            // is true when both ran on the same machine from the same tree and
            // false the moment they did not. On the bare-metal box they
            // routinely do not: the kernel is cross-built on a laptop and
            // shipped, while `root.img` stays whatever the last image build
            // made. A scored check there fails for a reason that says nothing
            // about the kernel, and `run_shell = passed && have_fs` then
            // withholds sshd over it. The mismatch is still worth *seeing* —
            // it means the program that ran is not the one you just built.
            t.note("elf: on-disk image size", bytes.len() as u64);
            if bytes != HELLO_ELF {
                serial::puts(
                    "  elf:  NOTE on-disk /bin/hello differs from the embedded copy \
                     — root.img is from a different build than this kernel\n",
                );
            }
            serial::puts("  elf:  loading /bin/hello from ext2\n");
            bytes
        }
    } else {
        serial::puts("  elf:  no filesystem; loading the embedded image\n");
        HELLO_ELF
    };
    t.check("elf: image came from the filesystem", from_disk.is_ok());

    argv_capacity_test(t, image);

    let (proc, img) = match Image::from_elf(image) {
        Ok(p) => p,
        Err(e) => {
            t.check("elf: image loaded", false);
            serial::puts("  elf: load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return;
        }
    };
    t.check("elf: image loaded", true);

    // Every PT_LOAD in the file was placed — asked of the **page tables**, not
    // of a count the loader reported about itself. Before C1 step 6 this was
    // `img.segments == count_pt_load(image)`; `akuma_elf::LoadedElf` reports no
    // segment count, and re-deriving one from the same headers the loader read
    // would have made the check agree with itself. Both ends of each segment
    // are probed, so a loader that mapped a segment's first page and stopped
    // fails here rather than in ring 3.
    //
    // Counted out of the image rather than written as a literal: how many
    // segments lld emits is its decision, not ours — `user.ld` names three
    // output sections and the current link produces four LOADs, because `-z
    // relro` splits .data from .bss. A literal here would turn any future
    // linker flag into a test failure, while this catches the thing that
    // matters: a loader that skipped one would produce a program that runs
    // right up until it touches the segment that is missing.
    //
    // `hello` is `ET_EXEC`, so its `p_vaddr`s are its runtime addresses and
    // there is no load bias to add. A PIE probe here would need the base.
    let mapped_segments = {
        let space = &proc.space;
        let mut n = 0u64;
        for_each_pt_load(image, |vaddr, memsz| {
            let first = (vaddr & !0xfff) as usize;
            let last = ((vaddr + memsz - 1) & !0xfff) as usize;
            if space.is_mapped(first) && space.is_mapped(last) {
                n += 1;
            }
        });
        n
    };
    t.check_eq("elf: every PT_LOAD is mapped", mapped_segments, count_pt_load(image));
    t.check(
        "elf: image ends above its entry point",
        img.end_va > img.entry && img.end_va % 4096 == 0,
    );
    // .bss is 32 KiB, so the writable segment alone is 9 pages; with .text,
    // .rodata and two stack pages the total cannot be a single-page accident.
    t.check(
        "elf: frames owned covers image and stack",
        proc.space.user_frame_count() >= 12,
    );

    // The entry point is what the file said, not what the kernel assumed. Read
    // straight out of the image's `e_entry` field so a loader that ignored it
    // and jumped at the first segment would fail here rather than by crashing.
    let want_entry = u64::from_le_bytes([
        image[24], image[25], image[26], image[27],
        image[28], image[29], image[30], image[31],
    ]);
    t.check_eq("elf: entry point is e_entry", proc.entry, want_entry);

    // Permissions read back out of the page tables — what the hardware will do,
    // not what the loader believes it did. The entry page must be executable and
    // not writable; the stack must be the reverse. Both are W^X, from opposite
    // ends.
    let entry_prot = proc.space.pte_prot(proc.entry as usize & !0xfff).map(|(p, _)| p);
    t.check(
        "elf: entry page is user-executable and not writable",
        entry_prot == Some(PteProt::USER_RX),
    );
    let stack_prot = proc.space.pte_prot((proc.stack as usize) & !0xfff).map(|(p, _)| p);
    t.check(
        "elf: stack page is user-writable and not executable",
        stack_prot == Some(PteProt::USER_RW),
    );

    // The stack is a separate mapping from the image, not an extension of it.
    t.check(
        "elf: stack is above the image and mapped",
        proc.stack < ELF_STACK_TOP && proc.stack >= ELF_STACK_TOP - (ELF_STACK_PAGES as u64 * 4096),
    );

    let Some(started) = start_test_process(4, proc, img.end_va, "hello") else {
        t.check("elf: process spawned", false);
        return;
    };
    t.check("elf: process spawned", true);

    serial::puts("  -- userspace output follows (from an ELF image) --\n");
    let status = wait_test_exit(started.0, 10_000);
    t.check_eq("elf: program ran and reported every check", status, ALL_OK);
    if status != ALL_OK && status != u64::MAX {
        // Name the failures individually. A bare "got 0x2F, want 0x3F" makes the
        // reader decode a bitmask to learn that argv[0] was wrong.
        t.check("elf:   .data holds its linked contents", status & DATA_OK != 0);
        t.check("elf:   .bss is zero-filled", status & BSS_OK != 0);
        t.check("elf:   the PF_W segment is writable", status & WRITABLE_OK != 0);
        t.check("elf:   argc is on the stack", status & ARGC_OK != 0);
        t.check("elf:   argv[0] points at its string", status & ARGV_OK != 0);
        t.check("elf:   auxv carries AT_PAGESZ", status & AUXV_OK != 0);
        t.check("elf:   a syscall preserved the ABI's registers", status & REGS_OK != 0);
    }

    finish_test_process(started.0, started.1);
    t.check_eq(
        "elf: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// `socketpair(2)` — the pair is created, carries bytes both ways, and closes.
///
/// # Why a boot test and not a userspace one
///
/// This syscall is three things stacked, and only the first is a dispatch row:
/// the row in `akuma-syscalls-abi`, glue's AF_UNIX implementation behind it,
/// and the two `ExecRuntime` lifecycle hooks a `FileDescriptor::UnixSocket`
/// needs once one can exist. The middle layer was compiled in and unreachable
/// for the whole life of the port; the hooks were `not_wired!` panics. A test
/// that only asked "does the syscall return 0" would have passed on a kernel
/// that takes itself down the first time a process holding one forks.
///
/// So this exercises the object, not the number: both directions, then the
/// close path, then the frame count. The number gets its own check because it
/// is the part that was missing — `rustc` reached this syscall through
/// `Rust std`'s spawn and got `ENOSYS`, which surfaced as
/// `could not exec the linker` (`docs/archive/RUST_TOOLCHAIN_AMD64.md`).
fn socketpair_test(t: &mut Suite) {
    use k::fd::errno;

    // The row itself. `53` is x86_64's number and the one a program sends.
    t.check(
        "socketpair: x86_64 53 decodes to Socketpair",
        Syscall::from_x86_64(53) == Some(Syscall::Socketpair),
    );

    let free_before = akuma_pmm::free_count();
    let mut sv = [0i32; 2];
    // AF_UNIX (1), SOCK_STREAM (1). The suite runs inside
    // `BypassValidationGuard`, which is what lets a kernel pointer stand in for
    // the `int sv[2]` a ring-3 caller would pass.
    let rc = to_glue(Syscall::Socketpair, [1, 1, 0, sv.as_mut_ptr() as u64, 0, 0]);
    if !t.check_eq("socketpair: AF_UNIX/SOCK_STREAM pair created", rc, 0) {
        return;
    }
    let (a, b) = (sv[0] as u64, sv[1] as u64);
    t.check("socketpair: two distinct descriptors", a != b);

    // Both directions, because the two endpoints are not symmetric in the
    // implementation: each reads one pipe and writes the other, and a crossed
    // pair passes a one-way test.
    let msg = *b"pair";
    let mut got = [0u8; 4];
    t.check_eq(
        "socketpair: a write reaches b",
        sys_write(a, msg.as_ptr() as u64, 4),
        4,
    );
    t.check_eq(
        "socketpair: b reads what a wrote",
        k::fd::sys_read(b, got.as_mut_ptr() as u64, 4),
        4,
    );
    t.check("socketpair: and the bytes match", got == msg);

    got = [0u8; 4];
    let back = *b"kcab";
    t.check_eq(
        "socketpair: b writes back to a",
        sys_write(b, back.as_ptr() as u64, 4),
        4,
    );
    t.check_eq(
        "socketpair: a reads it",
        k::fd::sys_read(a, got.as_mut_ptr() as u64, 4),
        4,
    );
    t.check("socketpair: and those bytes match too", got == back);

    // The close path runs `unix_sock_close` and both pipe closes — the hooks
    // that were `not_wired!` panics until 2026-09-12.
    t.check_eq("socketpair: closing one end", k::fd::sys_close(a), 0);
    t.check_eq("socketpair: closing the other", k::fd::sys_close(b), 0);
    t.check_eq(
        "socketpair: a closed endpoint is EBADF",
        k::fd::sys_read(a, got.as_mut_ptr() as u64, 4),
        errno::EBADF,
    );

    t.check_eq(
        "socketpair: teardown leaks no frames",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );

    // The refusals, which are what a wrong `domain` must get: `EAFNOSUPPORT`,
    // not `ENOSYS`. A caller reads the first as "this kernel has no AF_INET
    // pairs" and the second as "this kernel has no socketpair".
    t.check_eq(
        "socketpair: AF_INET is EAFNOSUPPORT",
        to_glue(Syscall::Socketpair, [2, 1, 0, sv.as_mut_ptr() as u64, 0, 0]),
        errno::EAFNOSUPPORT,
    );
}

/// A **toolchain-sized** argv reaches the program intact.
///
/// `loader::MAX_ARGV` was 16 until 2026-09-12, and `execve` truncated silently
/// at it: `rustc` and `collect2` build linker command lines of forty-odd
/// arguments, so `ld` ran without its `-o` and without its inputs and blamed
/// itself (`docs/archive/RUST_TOOLCHAIN_AMD64.md`, open item 1). This builds a
/// stack with a full argv and reads back what landed on it — `argc`, the first
/// pointer and the **last** pointer, which is the one a cap drops.
///
/// It inspects the address space directly rather than running a program: no
/// probe binary reports its own `argv[255]`, and the property under test is
/// what the stack builder wrote, not what a program made of it.
fn argv_capacity_test(t: &mut Suite, image: &[u8]) {
    let free_before = akuma_pmm::free_count();

    // `arg000` … `arg255`, each distinct, built without `format!`.
    let mut owned: Vec<[u8; 6]> = Vec::with_capacity(loader::MAX_ARGV);
    for i in 0..loader::MAX_ARGV {
        owned.push([
            b'a', b'r', b'g',
            b'0' + (i / 100) as u8,
            b'0' + ((i / 10) % 10) as u8,
            b'0' + (i % 10) as u8,
        ]);
    }
    let argv: Vec<&[u8]> = owned.iter().map(<[u8; 6]>::as_slice).collect();

    let (proc, _img) = match Image::from_elf_argv(image, &argv) {
        Ok(p) => p,
        Err(e) => {
            t.check("elf: a full-size argv builds a stack", false);
            serial::puts("  elf: full argv failed: ");
            serial::puts(e);
            serial::puts("\n");
            return;
        }
    };
    t.check("elf: a full-size argv builds a stack", true);

    let word = |slot: u64| -> u64 {
        let mut w = [0u8; 8];
        if loader::read_user(&proc.space, proc.stack + slot * 8, &mut w) {
            u64::from_le_bytes(w)
        } else {
            0
        }
    };
    let string_at = |va: u64| -> [u8; 6] {
        let mut b = [0u8; 6];
        loader::read_user(&proc.space, va, &mut b);
        b
    };

    t.check_eq("elf:   argc is the whole argv", word(0), loader::MAX_ARGV as u64);
    t.check_eq("elf:   argv[0] reads back", u64::from(string_at(word(1))[3]), u64::from(b'0'));
    // The last entry, and the NULL after it: a cap or an off-by-one in the
    // pointer walk shows up here and nowhere earlier.
    let last = loader::MAX_ARGV as u64;
    t.check_eq(
        "elf:   argv[MAX_ARGV-1] holds its own string",
        u64::from_le_bytes({
            let mut w = [0u8; 8];
            w[..6].copy_from_slice(&string_at(word(last)));
            w
        }),
        u64::from_le_bytes({
            let mut w = [0u8; 8];
            w[..6].copy_from_slice(&owned[loader::MAX_ARGV - 1]);
            w
        }),
    );
    t.check_eq("elf:   argv is NULL-terminated", word(last + 1), 0);

    drop(proc);
    t.check_eq(
        "elf:   the full-argv stack frees cleanly",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Images the loader must refuse, and the reason each one exists.
///
/// Every case is a mutation of the *real* image rather than a hand-written
/// header, so a change to how `hello` is linked cannot leave these testing a
/// shape the loader no longer sees. They check that a rejection happens, not
/// which message comes back — the messages are diagnostics and pinning them
/// would make rewording one a test failure.
fn reject_test(t: &mut Suite) {
    let free_before = akuma_pmm::free_count();

    // A buffer big enough for the header mutations. Only the first 64 bytes are
    // ever changed, and every case below is rejected during header validation,
    // so the truncated image is never actually placed.
    let mut buf = [0u8; 256];
    buf.copy_from_slice(&HELLO_ELF[..256]);

    let cases: [(&str, usize, &[u8]); 4] = [
        // e_ident[EI_MAG0..4]: not an ELF at all.
        ("elf: rejects a non-ELF image", 0, &[0x7f, b'E', b'L', b'G']),
        // e_type at offset 16: ET_DYN (3). Static-PIE (`ET_DYN`, no
        // `PT_INTERP`) is accepted since 2026-09-04 — see the module header's
        // "Static-PIE" section — so this case is no longer refused *for being
        // ET_DYN*; it is refused because `HELLO_ELF[..256]` truncates its
        // first `PT_LOAD` segment's file data (`p_offset=0x1000`,
        // `p_filesz=500`, past this test's 256-byte buffer), which the
        // relocated-at-`PIE_BASE` load path reaches and rejects on exactly
        // the same "segment data past end of image" check an `ET_EXEC` load
        // of the same truncated bytes would hit. Kept under this name because
        // the outcome this suite actually checks (refused) is unchanged, and
        // per this function's own doc it checks that, not why — a dedicated
        // "refuses `PT_INTERP`" case would need a hand-built image rather
        // than a `HELLO_ELF` mutation, which is real, not-yet-written
        // coverage rather than something this case still stands in for.
        ("elf: rejects ET_DYN", 16, &[3, 0]),
        // e_machine at offset 18: EM_AARCH64 (183). The other architecture in
        // this tree, which is the mistake actually available to make.
        ("elf: rejects a non-x86-64 machine", 18, &[183, 0]),
        // e_ident[EI_CLASS] at offset 4: ELFCLASS32.
        ("elf: rejects ELF32", 4, &[1]),
    ];

    for (name, at, bytes) in cases {
        let mut img = buf;
        img[at..at + bytes.len()].copy_from_slice(bytes);
        // The address space is the loader's to build and to drop since C1
        // step 6, so a refused load releases whatever it had already placed
        // without this loop naming it — which is what the `free_count`
        // assertion below is actually testing now.
        let refused = loader::load(&img).is_err();
        t.check(name, refused);
    }

    // A truncated image: the header claims segments the file does not contain.
    t.check("elf: rejects a truncated image", loader::load(&HELLO_ELF[..48]).is_err());

    t.check_eq(
        "elf: rejected loads leak nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// Run the file/memory syscall probe in ring 3.
///
/// `fd::smoke_test` and `mm::smoke_test` call the same functions from ring 0,
/// where a user pointer is just a pointer. This runs them across the privilege
/// boundary through the `syscall` instruction with the real x86_64 numbers,
/// which is the only place a wrong argument register shows up — `r10` versus
/// `rcx` for the fourth argument in particular, which the `syscall` instruction
/// forces and which no ring-0 test can catch.
pub fn fdprobe_test(t: &mut Suite) {
    /// Every bit the probe sets when the whole surface works. See its header for
    /// what each one claims.
    const ALL_OK: u64 = 0xFFF;

    let Ok(image) = k::fs::read_file("/bin/fdprobe") else {
        t.note("fdprobe: not on the disk; skipped", 0);
        return;
    };

    let free_before = akuma_pmm::free_count();
    let (proc, _img) = match Image::from_elf(&image) {
        Ok(p) => p,
        Err(e) => {
            t.check("fdprobe: image loaded", false);
            serial::puts("  fdprobe: load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return;
        }
    };
    let Some(started) = start_test_process(5, proc, _img.end_va, "fdprobe") else {
        t.check("fdprobe: spawned", false);
        return;
    };
    t.check("fdprobe: spawned", true);

    let status = wait_test_exit(started.0, 10_000);
    t.check_eq("fdprobe: every syscall claim held", status, ALL_OK);
    if status != ALL_OK && status != u64::MAX {
        // Name the failures individually rather than making the reader decode a
        // 12-bit mask.
        let claims: [(&str, u64); 12] = [
            ("fdprobe:   openat returns a descriptor", 1 << 0),
            ("fdprobe:   read returns the file's bytes", 1 << 1),
            ("fdprobe:   lseek(SEEK_SET) rewinds", 1 << 2),
            ("fdprobe:   lseek(SEEK_END) reports the size", 1 << 3),
            ("fdprobe:   reading at EOF returns 0", 1 << 4),
            ("fdprobe:   fstat reports the size", 1 << 5),
            ("fdprobe:   close invalidates the descriptor", 1 << 6),
            ("fdprobe:   a missing path is ENOENT", 1 << 7),
            ("fdprobe:   mmap returns zeroed memory", 1 << 8),
            ("fdprobe:   that memory holds a write", 1 << 9),
            ("fdprobe:   munmap succeeds", 1 << 10),
            ("fdprobe:   a file-backed mmap is refused", 1 << 11),
        ];
        for (label, bit) in claims {
            t.check(label, status & bit != 0);
        }
    }

    finish_test_process(started.0, started.1);
    // The probe mmaps and munmaps, so its frames must come back too — a leak
    // here is `mm::sys_munmap` failing to free rather than the loader.
    t.check_eq(
        "fdprobe: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// `clone(CLONE_VM|CLONE_THREAD)` and `futex`, exercised from ring 3.
///
/// Wired 2026-09-06 with the syscalls themselves
/// (`docs/archive/AKUMA_AMD64_RUST_STD.md`). A boot check rather than only the
/// `ruststd` probe, because that one is built by a cross toolchain the kernel's
/// own build does not own and runs only when someone asks for it — a
/// regression in threads has to fail a boot, not wait to be noticed.
///
/// Runs in `PROCS` slot 5, next to `elf_test`'s slot 4 and by the same shape:
/// load, spawn, drive the scheduler until it finishes, read the exit status,
/// tear down, assert the frame count came back.
pub fn thread_test(t: &mut Suite) {
    /// `clone` returned a plausible new tid.
    const TID_OK: u64 = 1 << 0;
    /// `CLONE_PARENT_SETTID` wrote it into the parent's word.
    const PARENT_SETTID_OK: u64 = 1 << 1;
    /// The child ran, and its own `gettid` agrees.
    const CHILD_TID_OK: u64 = 1 << 2;
    /// The child read the parent's memory: `CLONE_VM` shares.
    const SHARE_P2C_OK: u64 = 1 << 3;
    /// The parent read the child's write to the same page.
    const SHARE_C2P_OK: u64 = 1 << 4;
    /// `FUTEX_WAIT` was released by the child's `FUTEX_WAKE`.
    const FUTEX_OK: u64 = 1 << 5;
    /// `CLONE_CHILD_CLEARTID` zeroed the join word on thread exit.
    const CLEARTID_OK: u64 = 1 << 6;
    /// The kernel's own wake on that word released a second wait.
    const JOIN_WAKE_OK: u64 = 1 << 7;
    /// `mmap` still works alongside all of it.
    const MMAP_OK: u64 = 1 << 8;
    const ALL_OK: u64 = TID_OK
        | PARENT_SETTID_OK
        | CHILD_TID_OK
        | SHARE_P2C_OK
        | SHARE_C2P_OK
        | FUTEX_OK
        | CLEARTID_OK
        | JOIN_WAKE_OK
        | MMAP_OK;

    let free_before = akuma_pmm::free_count();

    let (proc, _img) = match Image::from_elf(THREADPROBE_ELF) {
        Ok(p) => p,
        Err(e) => {
            t.check("thread: probe loaded", false);
            serial::puts("  thread: load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return;
        }
    };
    t.check("thread: probe loaded", true);
    let Some(started) = start_test_process(5, proc, _img.end_va, "threadprobe") else {
        t.check("thread: probe spawned", false);
        return;
    };
    t.check("thread: probe spawned", true);

    let status = wait_test_exit(started.0, 200_000);
    t.check_eq("thread: probe reported every check", status, ALL_OK);
    if status != ALL_OK && status != u64::MAX {
        // Named individually: a bitmask in a boot log is a puzzle, and each of
        // these fails for a different reason in a different file.
        t.check("thread:   clone returned a new tid", status & TID_OK != 0);
        t.check("thread:   CLONE_PARENT_SETTID wrote it", status & PARENT_SETTID_OK != 0);
        t.check("thread:   the child ran and gettid agrees", status & CHILD_TID_OK != 0);
        t.check("thread:   the child sees the parent's memory", status & SHARE_P2C_OK != 0);
        t.check("thread:   the parent sees the child's", status & SHARE_C2P_OK != 0);
        t.check("thread:   FUTEX_WAIT was woken by FUTEX_WAKE", status & FUTEX_OK != 0);
        t.check("thread:   CLONE_CHILD_CLEARTID zeroed the join word", status & CLEARTID_OK != 0);
        t.check("thread:   the join word's wake was delivered", status & JOIN_WAKE_OK != 0);
        t.check("thread:   mmap still works", status & MMAP_OK != 0);
    }

    // No thread of the probe may outlive it. `run_process` drains before it
    // finishes, so a non-zero count here means that drain is not working —
    // which is the use-after-free of a page table that `k::thread`'s
    // lifetime rule exists to prevent, seen before it can happen.
    t.check_eq("thread: no thread outlived the process", k::thread::live_count(5) as u64, 0);

    finish_test_process(started.0, started.1);
    t.check_eq(
        "thread: teardown leaks nothing",
        akuma_pmm::free_count() as u64,
        free_before as u64,
    );
}

/// `SIGKILL` to a threaded process takes the group down **on its own
/// threads**, promptly, and leaves nothing behind.
///
/// The program (`userspace/amd64/killprobe`) parks two threads in untimed
/// `FUTEX_WAIT`s, spins a third with no syscall in it, and parks its main
/// thread the same way. This sends it `kill(pid, SIGKILL)` through this
/// target's own `sys_kill` and checks, in order of what each would have cost:
///
/// * **the group is gone within a second.** Through glue's `deliver_signal`
///   (until 2026-10-09) the same kill cost the shared `kill_thread_group`'s
///   full 2 s grace, because no amd64 path consumes the deferred kill it
///   posts;
/// * **no thread was terminated by a peer** — `cross_thread_terminations`
///   did not move. The old path's grace expiry hard-marked every sibling
///   from the killer's core, and a thread so marked never runs
///   `thread::teardown`;
/// * **every `THREADS` row is back** (`live_count == 0`) — the rows the old
///   path leaked, and that made the next process in the slot spin out
///   `DRAIN INCOMPLETE`;
/// * **the status is a SIGKILL death** (`-9`), stamped by the leader's own
///   `run_process` epilogue, not forged by the killer;
/// * **teardown leaks nothing.**
///
/// The spinning thread is the one that proves the tick path: it makes no
/// syscall, so only `deliver_pending_on_tick` can end it.
pub fn kill_test(t: &mut Suite) {
    const SLOT: usize = 6;
    /// `(-9)` as the exit path carries it: `signal::signal_status(9)`.
    const SIGKILL_STATUS: u64 = (-9i64) as u64;
    const WORKERS: usize = 3;

    let free_before = akuma_pmm::free_count();
    let (proc, img) = match Image::from_elf(KILLPROBE_ELF) {
        Ok(p) => p,
        Err(e) => {
            t.check("kill: probe loaded", false);
            serial::puts("  kill: load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return;
        }
    };
    let Some((pid, task_slot)) = start_test_process(SLOT, proc, img.end_va, "killprobe") else {
        t.check("kill: probe spawned", false);
        return;
    };

    // Let it reach the state: three workers registered, then a few hundred
    // more rounds so the parkers are parked and the spinner has been ticked.
    let mut spins = 0u64;
    while k::thread::live_count(SLOT) < WORKERS && spins < 200_000 {
        spins += 1;
        k::sched::yield_now();
    }
    t.check_eq("kill: the probe's threads came up", k::thread::live_count(SLOT) as u64, WORKERS as u64);
    for _ in 0..500 {
        k::sched::yield_now();
    }

    let cross_before = akuma_threading::cross_thread_terminations();
    let t0 = k::net::uptime_us();
    // Through the dispatcher, not `signal::sys_kill` directly: the defect this
    // check exists for was the `Syscall::Kill` *arm* (it forwarded to glue),
    // and a check that called the native function could not tell the two
    // apart — the first negative control (old arm restored) passed every line.
    t.check_eq("kill: kill(pid, SIGKILL) is accepted",
        syscall_dispatch(Syscall::Kill.to_x86_64(), u64::from(pid), 9, 0, 0, 0, 0), 0);

    let status = wait_test_exit(pid, 400_000);
    let took_us = k::net::uptime_us().saturating_sub(t0);
    t.check("kill: the group finished", status != u64::MAX);
    // Well under the 2 s grace the old path always paid; the tick that reaches
    // the spinner is 10 ms.
    t.check("kill: the group was gone within a second", took_us < 1_000_000);
    t.check_eq("kill: no thread was terminated by a peer",
        akuma_threading::cross_thread_terminations() as u64, cross_before as u64);
    t.check_eq("kill: every thread row came back", k::thread::live_count(SLOT) as u64, 0);
    t.check_eq("kill: the status is a SIGKILL death", status, SIGKILL_STATUS);
    // A `kill(2)` at a dead pid: the row is a zombie until reaped, which is
    // still "exists" (0) — by pid and by its process group (Linux keeps a
    // zombie in its pgrp's task list until it is reaped). After the reap the
    // group is empty, which is `ESRCH`.
    let kill62 = |target: u64, sig: u64| syscall_dispatch(Syscall::Kill.to_x86_64(), target, sig, 0, 0, 0, 0);
    t.check_eq("kill: a zombie still answers kill(pid, 0)", kill62(u64::from(pid), 0), 0);
    t.check_eq("kill: a zombie still counts for its process group",
        kill62((-(i64::from(pid))) as u64, 0), 0);

    finish_test_process(pid, task_slot);
    t.check_eq("kill: a reaped process group is ESRCH",
        kill62((-(i64::from(pid))) as u64, 0), k::fd::errno::ESRCH);
    t.check_eq("kill: teardown leaks nothing", akuma_pmm::free_count() as u64, free_before as u64);
}

/// **Does a signal handler's excursion give back every register it found?**
///
/// `userspace/amd64/sigregs/sigregs.rs`: a `fork`ed child `kill`s the parent
/// with `SIGUSR1` every millisecond while the parent spins through windows
/// that hold sentinels in `rcx`, `r11`, `xmm0`–`xmm15` (and `ymm0`–`ymm3`'s
/// upper halves on AVX), every eighth window also faulting on a null store
/// its `SIGSEGV` handler resumes past; both handlers clobber everything. The
/// parent is busy in ring 3 when the signal is pended, so it takes it at a
/// **tick** — the delivery that used to come back through `sysretq` with
/// `RCX = RIP` and no FPU state restored, which is how Go's `memmove` died
/// under `copystack` (`docs/archive/AMD64_FORK_VS_THREADS_SEGV.md`). Exit 0 is
/// the only pass; the probe's header lists the other codes, and a mismatch
/// writes its register mask to the console.
pub fn sigregs_test(t: &mut Suite) {
    const SLOT: usize = 6;
    let free_before = akuma_pmm::free_count();
    let (proc, img) = match Image::from_elf(SIGREGS_ELF) {
        Ok(p) => p,
        Err(e) => {
            t.check("sigregs: probe loaded", false);
            serial::puts("  sigregs: load failed: ");
            serial::puts(e);
            serial::puts("\n");
            return;
        }
    };
    let Some((pid, task_slot)) = start_test_process(SLOT, proc, img.end_va, "sigregs") else {
        t.check("sigregs: probe spawned", false);
        return;
    };
    // 48 deliveries at one per millisecond, plus the TCG tax; the probe caps
    // itself long before this and reports 3/5 rather than hanging.
    let status = wait_test_exit(pid, 8_000_000);
    t.check("sigregs: the probe finished", status != u64::MAX);
    t.check_eq("sigregs: rcx, r11 and the SSE/AVX file survive tick- and fault-time signals (exit 0)",
        status, 0);
    finish_test_process(pid, task_slot);
    t.check_eq("sigregs: teardown leaks nothing", akuma_pmm::free_count() as u64, free_before as u64);
}
