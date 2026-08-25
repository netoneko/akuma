use alloc::boxed::Box;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;
use alloc::sync::Arc;
use alloc::format;

use crate::runtime::{runtime, config};
use crate::process::types::{Pid, DEFAULT_ENV};
use crate::process::channel::{ProcessChannel, register_channel, remove_channel};
use crate::process::table::{register_process};
use crate::process::children::{lookup_process_shared, current_terminal_state};
use crate::process::lifecycle::LifecycleGuard;

use super::{Process, enter_user_mode, read_current_pid, get_box_name};

/// Longest `#!` line honoured, matching Linux's `BINPRM_BUF_SIZE`.
///
/// Public because `do_execve`'s `exec_shebang` (bin crate) truncates its
/// already-read file bytes to the same window before parsing — the two paths
/// must agree on where a shebang line ends.
pub const SHEBANG_MAX: usize = 256;

/// How many `#!` hops to follow before giving up — an interpreter may itself be
/// a script. Same limit Linux uses (`BINPRM_MAX_RECURSION`), and it is what
/// stops a script whose interpreter is itself from spinning forever.
const SHEBANG_MAX_DEPTH: usize = 4;

/// Parse a file's leading bytes as a `#!` line: `(interpreter, optional arg)`.
///
/// `None` for a normal binary or a malformed line — in every one of those cases
/// the caller carries on with the ELF loader, which produces the real error.
///
/// Linux takes at most ONE argument after the interpreter and does NOT split it
/// on whitespace (`#!/usr/bin/env -S foo bar` passes `-S foo bar` as a single
/// argv entry); this matches, as does `do_execve`'s `exec_shebang`. `trim` also
/// disposes of the `\r` a CRLF script leaves on the interpreter path — otherwise
/// `/bin/sh\r` is looked up verbatim and never found.
pub fn parse_shebang(head: &[u8]) -> Option<(&str, Option<&str>)> {
    if head.len() < 2 || head[0] != b'#' || head[1] != b'!' {
        return None;
    }
    let line_end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
    let line = core::str::from_utf8(head.get(2..line_end)?).ok()?.trim();
    if line.is_empty() {
        return None;
    }
    let (interpreter, arg) = match line.split_once(char::is_whitespace) {
        Some((i, a)) => {
            let a = a.trim();
            (i.trim(), if a.is_empty() { None } else { Some(a) })
        }
        None => (line, None),
    };
    if interpreter.is_empty() {
        return None;
    }
    Some((interpreter, arg))
}

/// Build the argv prefix for one `#!` hop.
///
/// `prev` is the prefix built by the hop below this one (empty on the first).
/// Its `argv[0]` is dropped, mirroring `remove_arg_zero` in Linux's
/// `fs/binfmt_script.c`: a script whose interpreter is itself a script must come
/// out as `[sh, interp1, arg1, script]`, not with `interp1` repeated.
pub fn shebang_hop(interpreter: &str, arg: Option<&str>, script_name: &str, prev: &[String]) -> Vec<String> {
    let mut hop = Vec::new();
    hop.push(String::from(interpreter));
    if let Some(a) = arg {
        hop.push(String::from(a));
    }
    hop.push(String::from(script_name));
    if prev.len() > 1 {
        hop.extend_from_slice(&prev[1..]);
    }
    hop
}

/// Read `path`'s `#!` line. Only the first [`SHEBANG_MAX`] bytes are read, so a
/// large non-ELF file costs one short read, not a slurp. An unreadable path
/// yields `None` and the ELF loader reports the real error.
fn shebang_line_of(path: &str) -> Option<(String, Option<String>)> {
    let mut head = [0u8; SHEBANG_MAX];
    let n = (runtime().read_at)(path, 0, &mut head).ok()?;
    let (interpreter, arg) = parse_shebang(head.get(..n)?)?;
    Some((String::from(interpreter), arg.map(String::from)))
}

/// Follow a `#!` chain from `resolved` (the symlink-resolved on-disk path) and
/// return the ELF that should actually be loaded plus the argv prefix that
/// replaces `argv[0]`.
///
/// Empty prefix means "not a script, load `resolved` as-is". Otherwise the
/// prefix is `[interpreter, shebang_arg?, script_path, ...]`, exactly the shape
/// `execve` produces: the caller appends the user's arguments after it.
///
/// `display` is the path as the *caller* wrote it, which is what lands in the
/// interpreter's argv (a shell must see the name it was asked to run, not the
/// symlink target).
///
/// Nesting drops the previous `argv[0]` on each hop, mirroring Linux's
/// `remove_arg_zero` in `fs/binfmt_script.c`: a script whose interpreter is
/// itself a script yields `[sh, interp1, arg1, script]`, not a duplicated
/// `interp1`.
///
/// Public so the boot suite can drive it against the real VFS
/// (`spawn_resolves_a_shebang_script` in `src/process_tests.rs`) without
/// spawning a process during boot; the pure halves it is built from are
/// host-tested in `shebang_tests` below.
pub fn resolve_shebang_chain(resolved: &str, display: &str) -> (String, Vec<String>) {
    let mut elf_path = String::from(resolved);
    let mut prefix: Vec<String> = Vec::new();
    let mut probe = String::from(resolved);
    let mut script_name = String::from(display);

    for _ in 0..SHEBANG_MAX_DEPTH {
        let Some((interpreter, arg)) = shebang_line_of(&probe) else {
            break;
        };
        prefix = shebang_hop(&interpreter, arg.as_deref(), &script_name, &prefix);

        elf_path = (runtime().resolve_symlinks)(&interpreter);
        probe.clone_from(&elf_path);
        script_name = interpreter;
    }

    (elf_path, prefix)
}

/// Spawn a process on a user thread for concurrent execution
///
/// This function creates a new process from the ELF file and spawns it on a
/// dedicated user thread (slots 8-31). The process runs concurrently with
/// other threads and processes.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Thread ID of the spawned thread, or error message
pub fn spawn_process(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Result<usize, String> {
    let (thread_id, _channel, _pid) = spawn_process_with_channel(path, args, stdin)?;
    Ok(thread_id)
}

/// Spawn a process on a user thread with a channel for I/O
///
/// Like spawn_process, but returns a ProcessChannel that can be used to
/// read the process's output and check its exit status.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
/// * `cwd` - Optional current working directory (defaults to "/")
///
/// # Returns
/// Tuple of (thread_id, channel, pid) or error message
pub fn spawn_process_with_channel(
    path: &str,
    args: Option<&[&str]>,
    stdin: Option<&[u8]>,
) -> Result<(usize, Arc<ProcessChannel>, Pid), String> {
    spawn_process_with_channel_cwd(path, args, None, stdin, None)
}

/// Spawn a process on a user thread with a channel for I/O and specified cwd
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
/// * `cwd` - Optional current working directory (defaults to "/")
///
/// # Returns
/// Tuple of (thread_id, channel, pid) or error message
pub fn spawn_process_with_channel_cwd(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
) -> Result<(usize, Arc<ProcessChannel>, Pid), String> {
    spawn_process_with_channel_ext(path, args, env, stdin, cwd, 0, false)
}

/// Whether a spawn should inherit the caller's `TerminalState` rather than
/// get a fresh one.
///
/// `false` on either condition alone is enough — a `pty` spawn always gets a
/// fresh terminal (a multiplexing daemon's sessions must not alias
/// `input_waker`), and so does any spawn that crosses into a different box
/// (`box_id != 0`, `SPAWN_EXT`'s "enter this box" signal): sharing the
/// object across the box boundary lets a boxed process's ioctl or
/// foreground-pgid change reach back into the caller's own terminal. Pulled
/// out as a pure function so the decision is host-testable without a running
/// kernel — see `spawn_process_with_channel_ext`'s doc comment for the full
/// story and `docs/reference/subsystems/ssh.md` "Terminal handling".
#[must_use]
const fn spawn_inherits_terminal(pty: bool, box_id: u64) -> bool {
    !pty && box_id == 0
}

/// Extended version of spawn_process_with_channel.
///
/// `pty`: when `true`, the child's channel is marked as a real terminal
/// (`isatty()` reports true) so the kernel's canonical line discipline (ICRNL,
/// echo, line editing) runs on its stdin — for interactive sessions that
/// allocate a pty (e.g. sshd handling a client's `pty-req` for a login shell).
/// When `false` (the default for piped spawns) the child's stdin is a raw pipe.
///
/// `box_id`: `0` stays in the caller's own box (an ordinary same-session
/// spawn); nonzero crosses into that box. Independent of `pty`, a nonzero
/// `box_id` also forces a **fresh** `TerminalState` instead of inheriting the
/// caller's — see the box-crossing branch below for why sharing one across
/// the box boundary is a real leak, not just an isatty() mismatch.
pub fn spawn_process_with_channel_ext(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
    box_id: u64,
    pty: bool,
) -> Result<(usize, Arc<ProcessChannel>, Pid), String> {
    // NOTE: the LifecycleGuard is acquired at the PUBLISH point (just before
    // `register_process`, below), NOT here. The whole load phase above it — path/
    // namespace resolution, stat, the ELF read from ext2 — does block I/O and
    // cooperative waits; holding the preemption-disable guard across those wedged
    // SMP=4 at bringup (`[WATCHDOG] disabled at spawn.rs:96` for 100+ ms while every
    // peer core spun on the BKL). Nothing built before registration is globally
    // visible, so the load needs no guard. See `process/lifecycle.rs`.
    if crate::threading::user_threads_available() == 0 {
        return Err("No available user threads for process execution".into());
    }

    // Reject new processes under memory pressure to prevent OOM cascade
    if (runtime().is_memory_low)() {
        return Err("Kernel memory low, cannot spawn new process".into());
    }

    // If the box has a namespace with mounts (SubdirFs at /), activate a
    // per-thread namespace override so that runtime().read_file and
    // resolve_symlinks go through the container's mount table.
    let container_ns = if box_id != 0 {
        (runtime().get_box_namespace)(box_id)
    } else {
        None
    };
    let use_ns_override = container_ns.as_ref().is_some_and(|ns| !ns.mount.lock().is_empty());

    if use_ns_override {
        (runtime().set_spawn_namespace)(container_ns.as_ref().unwrap().clone());
    }

    let resolved = (runtime().resolve_symlinks)(path);

    // `#!` scripts. `do_execve` has always handled these; spawn did not, so every
    // caller that goes through the SPAWN abi instead of exec — herd's services and
    // `box run` — could only start real ELFs. That is what made `box run
    // redis:alpine` fail with "failed to spawn": every official OCI image's
    // Entrypoint is `docker-entrypoint.sh`, a `#!/bin/sh` script.
    //
    // It has to happen HERE rather than in the syscall layer: the namespace
    // override is already active, and a container's interpreter (`/bin/sh` inside
    // the image's layers) exists ONLY in the box's mount table — reading the
    // shebang from box 0's view would resolve the wrong `/bin/sh`, or none.
    let (elf_path_owned, shebang_prefix) = resolve_shebang_chain(&resolved, path);
    let elf_path = &elf_path_owned;

    let mut full_args = Vec::new();
    if shebang_prefix.is_empty() {
        full_args.push(path.to_string());
    } else {
        full_args.extend_from_slice(&shebang_prefix);
    }
    if let Some(arg_slice) = args {
        for arg in arg_slice {
            full_args.push(arg.to_string());
        }
    }

    let mut full_env = match env {
        Some(e) if !e.is_empty() => e.to_vec(),
        _ => DEFAULT_ENV.iter().map(|s| String::from(*s)).collect(),
    };

    if box_id != 0 && !full_env.iter().any(|e| e.starts_with("HOSTNAME=")) {
        if let Some(name) = get_box_name(box_id) {
            let hostname: String = core::iter::once("box-")
                .flat_map(|s| s.chars())
                .chain(name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }))
                .collect();
            full_env.push(format!("HOSTNAME={hostname}"));
        }
    }

    // Cap how large an ELF we slurp whole into the kernel heap.  `read_file`
    // returns the entire binary as a Vec<u8>; for a multi-MB executable (apk is
    // ~5 MB) that alone exhausted the 8 MB heap at MEMORY=64 and crashed the
    // kernel with a garbage-PC EL1 fault (EC=0x22) — reproducible only at low
    // RAM because the heap scales with RAM.  Above this threshold use the
    // demand-paged path loader (`from_elf_path`), which maps segments lazily
    // from the file and keeps heap use flat regardless of binary size.  Small
    // binaries keep the well-trodden whole-file path.
    //
    // On the size profile every binary uses the demand-paged path so the kernel
    // heap never needs a scratch buffer sized to the binary (tcc is 723 KB —
    // the whole reason 8 MB couldn't load it despite having >700 KB PMM free).
    #[cfg(kernel_profile_extreme)]
    const HEAP_SLURP_MAX: usize = 0;
    #[cfg(not(kernel_profile_extreme))]
    const HEAP_SLURP_MAX: usize = 1024 * 1024; // 1 MiB
    let stat_size = (runtime().file_size)(elf_path).ok().map(|s| s as usize);

    // Pick the loader. Prefer the demand-paged path loader whenever slurping is
    // disabled (size profile, HEAP_SLURP_MAX == 0) OR the binary is large.
    //
    // CRITICAL: this must NOT fall back to a whole-file read_file() merely
    // because file_size() returned None. That hole meant a transient stat
    // failure under memory pressure routed a 723 KB binary (tcc) into a single
    // ~706 KB kernel-heap slurp; with the heap watermark already high the alloc
    // failed in a kernel thread with no current process, so alloc_error_handler
    // had nothing to kill and panicked the whole kernel (EC=0x3c BRK). On the
    // size profile we now always use the path loader and re-stat if needed,
    // never slurp.
    let want_demand_paged =
        HEAP_SLURP_MAX == 0 || matches!(stat_size, Some(sz) if sz > HEAP_SLURP_MAX);

    let mut process = if want_demand_paged {
        // The path loader needs a size; re-stat if the first stat failed rather
        // than silently slurping the whole file.
        let file_size = stat_size
            .or_else(|| (runtime().file_size)(elf_path).ok().map(|s| s as usize))
            .ok_or_else(|| {
                if use_ns_override { (runtime().clear_spawn_namespace)(); }
                format!("Failed to stat {}", elf_path)
            })?;
        let result = Process::from_elf_path(elf_path, elf_path, file_size, &full_args, &full_env, None);
        if use_ns_override { (runtime().clear_spawn_namespace)(); }
        result.map_err(|e| format!("Failed to load ELF: {}", e))?
    } else {
        // Small binary on a profile that permits slurping: whole-file path, with
        // a demand-paged fallback if the read itself fails.
        match (runtime().read_file)(elf_path) {
            Ok(elf_data) => {
                let result = Process::from_elf(elf_path, &full_args, &full_env, &elf_data, None);
                if use_ns_override { (runtime().clear_spawn_namespace)(); }
                result.map_err(|e| format!("Failed to load ELF: {}", e))?
            }
            Err(_) => {
                let file_size = stat_size
                    .or_else(|| (runtime().file_size)(elf_path).ok().map(|s| s as usize))
                    .ok_or_else(|| {
                        if use_ns_override { (runtime().clear_spawn_namespace)(); }
                        format!("Failed to stat {}", elf_path)
                    })?;
                let result = Process::from_elf_path(elf_path, elf_path, file_size, &full_args, &full_env, None);
                if use_ns_override { (runtime().clear_spawn_namespace)(); }
                result.map_err(|e| format!("Failed to load ELF: {}", e))?
            }
        }
    };
    crate::mmu::as_trace(format_args!("[AS-NEW] pid={} l0=0x{:x} asid=0x{:x} via=spawn\n",
        process.pid, process.address_space.l0_phys(), process.address_space.asid()));

    // Always create a fresh channel per spawned process.
    // Reusing the parent's channel would cause the child's set_exited() call
    // to contaminate the parent's channel, leaking exit codes.
    let channel = Arc::new(ProcessChannel::new());

    // A spawned child's stdin/stdout is a pipe (this channel), not a real
    // terminal — unless the spawner explicitly requested a pty. When `pty` is
    // false, isatty() reports false: shells like busybox then batch-read piped
    // input instead of starting an interactive line editor that queries the
    // (absent) terminal for its cursor position (ESC[6n) — the right default for
    // piped spawns. When `pty` is true (sshd handling a client `pty-req` for a
    // login shell), the channel is a terminal so the kernel line discipline
    // (ICRNL CR->NL, canonical editing, echo) runs on the child's stdin.
    channel.set_terminal(pty);

    // Seed the channel with initial stdin data if provided.
    // Empty stdin (Some(b"")) keeps stdin open so sys_write enables ONLCR
    // translation — use this for subprocesses that need terminal-style output.
    if let Some(data) = stdin {
        if !data.is_empty() {
            // Short write is possible in principle (the buffer is bounded at 1 MiB
            // and `write_stdin` no longer drops to make room), but not in practice
            // here: the channel was created two statements ago, so the whole
            // buffer is free and any seed up to 1 MiB lands whole. A larger seed
            // has nowhere to go — nothing has started draining yet — so there is
            // no retry to make; the tail is dropped, as it was before.
            channel.write_stdin(data);
            channel.close_stdin();
        }
    }

    // Set the channel in the process struct (UNIFIED I/O)
    process.channel = Some(channel.clone());

    // Inherit terminal state from caller if available — but NOT for a `pty`
    // spawn, and NOT for a spawn that crosses into a different box. `pty`
    // means "give the child a brand-new controlling terminal" (real Unix
    // semantics: allocating a new pty starts a new session, it does not
    // share the allocator's terminal). A multiplexing daemon — one OS
    // process/thread serving many independent interactive sessions, e.g.
    // userspace sshd handling several concurrent SSH connections — has
    // exactly one `terminal_state` on itself; inheriting it for every
    // spawned pty session would make them all share one `input_waker` slot
    // (`crates/akuma-terminal`): session B's blocked stdin read stores its
    // waker there, but a stdin write for session A wakes whatever waker is
    // *currently* in that shared slot instead of targeting the right pid —
    // so B can permanently miss its wakeup while A keeps using its terminal,
    // and only resumes once A exits and stops re-registering.
    //
    // `box_id != 0` is the same shape of bug at the box boundary rather than
    // the session boundary: `SPAWN_EXT` (what `box run`/herd's per-service
    // launch use) always passes `pty=false` — a boxed process's stdin is a
    // plain pipe by default, not a pty, so line discipline stays correctly
    // off — but before this check that meant the box's init process kept
    // *sharing* the calling shell's `TerminalState` object rather than
    // getting its own. That's not just a naming mismatch: `foreground_pgid`
    // gets overwritten to the box's pid a few lines below on the SAME shared
    // object, silently repointing the caller's own terminal's `Ctrl+C`
    // target at a process in a different box; and any ioctl the boxed
    // process makes against its "own" termios (raw mode, ECHO) mutates the
    // caller's terminal too. `box_id == 0` (SPAWN_EXT's "stay in the
    // caller's box" case, and every non-EXT `sys_spawn` call) is unaffected
    // and keeps inheriting, matching a real shell subprocess sharing its
    // parent's controlling terminal. See `docs/reference/subsystems/ssh.md`
    // "Terminal handling".
    //
    // `Process::new` already gives every process its own fresh
    // `TerminalState` (`process/mod.rs`), so either condition just keeps
    // that instead of overwriting it.
    if spawn_inherits_terminal(pty, box_id) && let Some(shared_state) = current_terminal_state() {
        if config().syscall_debug_info_enabled {
            log::debug!("[Process] Inheriting shared terminal state at {:p} for PID {}", Arc::as_ptr(&shared_state), process.pid);
        }
        process.terminal_state = shared_state;
    } else if config().syscall_debug_info_enabled {
        if pty {
            log::debug!("[Process] Fresh (non-inherited) terminal state for pty spawn PID {}", process.pid);
        } else if box_id != 0 {
            log::debug!("[Process] Fresh (non-inherited) terminal state for box-crossing spawn (box={}) PID {}", box_id, process.pid);
        } else {
            log::debug!("[Process] NO shared terminal state found for caller thread {}, using default for PID {}", crate::threading::current_thread_id(), process.pid);
        }
    }

    // Auto-delegate foreground to the new process (on whichever
    // terminal_state it ended up with, shared or fresh). For interactive
    // spawns, the child should start in the foreground.
    process.terminal_state.lock().foreground_pgid = process.pid;

    // Save arguments in process struct for ProcessInfo page
    process.args = if let Some(arg_slice) = args {
        arg_slice.iter().map(|s| String::from(*s)).collect()
    } else {
        Vec::new()
    };

    // Set up stdin if provided
    if let Some(data) = stdin {
        process.set_stdin(data);
    }
    
    // Set up cwd if provided
    if let Some(dir) = cwd {
        process.set_cwd(dir);
    }

    // Set up isolation context (Inherit from caller by default)
    let (caller_box_id, caller_namespace) = match read_current_pid() {
        Some(pid) => {
            if let Some(proc) = lookup_process_shared(pid) {
                (proc.box_id, proc.namespace.clone())
            } else {
                (0, akuma_isolation::global_namespace())
            }
        }
        None => (0, akuma_isolation::global_namespace()),
    };

    if box_id != 0 {
        process.box_id = box_id;
        if let Some(ns) = (runtime().get_box_namespace)(box_id) {
            process.namespace = ns;
        } else {
            process.namespace = caller_namespace;
        }
    } else {
        process.box_id = caller_box_id;
        process.namespace = caller_namespace;
    }

    if config().syscall_debug_info_enabled {
        log::debug!("[Process] Spawning {} (box_id={}, ns_id={})", path, process.box_id, process.namespace.id);
    }

    // Set spawner PID (the process that called spawn, if any)
    // This is used by procfs to control who can write to stdin
    process.spawner_pid = read_current_pid();
    
    // Get the PID before boxing
    let pid = process.pid;

    // Box the process for heap allocation (fallible to avoid kernel panic on OOM)
    let boxed_process = Box::try_new(process)
        .map_err(|_| format!("Failed to allocate Process struct for {path}"))?;

    // Serialize the PUBLISH window (register → thread spawn → return) against
    // preemption under shared-kernel SMP — from here on the half-built Process is
    // globally visible and a peer core's EL1 code must not observe it mid-flight.
    // See `process/lifecycle.rs` and the load-phase note at the top of this fn.
    let _lifecycle = LifecycleGuard::acquire();

    // CRITICAL: Register the process in the table immediately.
    // This ensures that lookup_process_shared(pid) works as soon as this function returns,
    // allowing reattach() to succeed without races.
    register_process(pid, boxed_process);

    // Register the channel for the thread ID placeholder (0 for now, will be updated)
    // Actually, current_channel() now uses the field in Process struct, so this is mostly for legacy.
    register_channel(0, channel.clone());

    // The channel THIS spawn created, captured now rather than re-read from the
    // process below. `p.channel` is the process's *I/O* channel and can be
    // borrowed: `reattach` points it at the caller's channel so a container's
    // output appears on the caller's terminal. The per-tid registration is a
    // different thing — it is the process's identity for exit notification
    // (`sys_exit` stamps `get_channel(tid)`).
    //
    // Re-reading `p.channel` here conflated the two, and lost a race: `box run`
    // calls `reattach` immediately after `spawn_ext` returns, usually before
    // this thread first runs, so the container registered the *shell's* channel
    // as its own. Its exit then stamped that channel, and sshd — which ends the
    // session when `waitpid_status(shell_pid)` reports the shell's channel
    // exited — closed the connection every time a container finished. The fork
    // path has always kept these separate on purpose; see the exit_channel
    // comment in `process/mod.rs`.
    let spawn_channel = channel.clone();

    // Spawn on a user thread
    let thread_id = crate::threading::spawn_user_thread_fn_for_process(move || {
        let tid = crate::threading::current_thread_id();

        // Update thread_id in the registered process (Arc clone in the closure
        // is refcount-only — no allocation).
        if let Some(ch) = crate::process::table::with_process(pid, |p| {
            p.thread_id = Some(tid);
            spawn_channel.clone()
        }) {
            // Register in THREAD_PID_MAP so on_thread_cleanup can reap this
            // process when the thread slot is recycled.  Without this, the
            // process becomes a permanent zombie.
            crate::runtime::with_irqs_disabled(|| {
                crate::process::table::THREAD_PID_MAP.lock().insert(tid, pid);
            });

            // Move the channel registration to the correct TID
            remove_channel(0);
            register_channel(tid, ch);

            // Execute the process (already in the table)
            run_registered_process(pid);
        } else {
            log::debug!("[Process] FATAL: PID {} disappeared during spawn", pid);
            // Mark terminated BEFORE parking: the scheduler then switches away permanently
            // (reconciling the BKL to the next thread) instead of this thread busy-spinning
            // in `yield_now` holding the Big Kernel Lock forever, which freezes every peer
            // core under shared-kernel SMP.
            crate::threading::mark_current_terminated();
            loop { crate::threading::yield_now(); }
        }
    })
    .map_err(|e| format!("Failed to spawn thread: {}", e))?;

    // Set the thread ID in the process table entry for the parent to see immediately
    let _ = crate::process::table::with_process(pid, |p| p.thread_id = Some(thread_id));

    Ok((thread_id, channel, pid))
}


/// Execute a process that is already registered in the PROCESS_TABLE
pub(crate) fn run_registered_process(pid: Pid) -> ! {
    // SAFETY: called only from the process's own just-spawned thread (or the
    // spawner handing off to it) on a BKL-held path, before first entry to
    // user mode — no other reference to this Process is live. First-run
    // lifecycle window, same class as execve's `replace_image` (Phase 7f).
    unsafe {
        let _ = crate::process::table::with_process_exclusive::<(), _>(pid, |proc| {
            // Prepare the process (set state, write process info page)
            proc.prepare_for_execution();

            // Activate the user address space (sets TTBR0)
            proc.address_space.activate();

            // Now safe to enable IRQs - TTBR0 is set to user tables
            (runtime().enable_irqs)();

            // Enter user mode via ERET - this never returns
            enter_user_mode(&proc.context);
        });
    }
    // Reached only if the process vanished between spawn and first run.
    panic!("Process not found in run_registered_process");
}

#[cfg(test)]
mod terminal_inheritance_tests {
    use super::spawn_inherits_terminal;

    /// Plain same-box spawn (`sys_spawn`, or `SPAWN_EXT` with `box_id == 0`)
    /// — a shell subprocess sharing its parent's controlling terminal.
    #[test]
    fn same_box_non_pty_spawn_inherits() {
        assert!(spawn_inherits_terminal(false, 0));
    }

    /// `SPAWN_FLAG_PTY` (sshd handling a client `pty-req`) always gets a
    /// fresh terminal, box-crossing or not.
    #[test]
    fn pty_spawn_never_inherits_even_in_the_same_box() {
        assert!(!spawn_inherits_terminal(true, 0));
    }

    /// The bug this predicate fixes: `box run`/herd's per-service launch go
    /// through `SPAWN_EXT` with `pty=false` and a nonzero target `box_id` —
    /// before this existed, that combination inherited the caller's
    /// `TerminalState`, leaking `foreground_pgid`/termios changes across the
    /// box boundary (docs/reference/subsystems/ssh.md "Terminal handling").
    #[test]
    fn box_crossing_spawn_never_inherits_even_without_pty() {
        assert!(!spawn_inherits_terminal(false, 7));
    }

    #[test]
    fn box_crossing_pty_spawn_never_inherits() {
        assert!(!spawn_inherits_terminal(true, 7));
    }
}

#[cfg(test)]
mod shebang_tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| String::from(*s)).collect()
    }

    #[test]
    fn an_elf_is_not_a_script() {
        assert!(parse_shebang(b"\x7fELF\x02\x01\x01\x00").is_none());
        assert!(parse_shebang(b"").is_none());
        assert!(parse_shebang(b"#").is_none());
        // A `#` comment is not a shebang: only `#!` at offset 0 is.
        assert!(parse_shebang(b"# not a shebang\n").is_none());
    }

    #[test]
    fn plain_interpreter_has_no_arg() {
        assert_eq!(parse_shebang(b"#!/bin/sh\nexit 0\n"), Some(("/bin/sh", None)));
    }

    #[test]
    fn a_missing_newline_still_parses() {
        assert_eq!(parse_shebang(b"#!/bin/sh"), Some(("/bin/sh", None)));
    }

    /// Linux passes everything after the interpreter as ONE argv entry rather
    /// than splitting it — `env -S` depends on that.
    #[test]
    fn the_argument_is_not_split_on_whitespace() {
        assert_eq!(
            parse_shebang(b"#!/usr/bin/env -S foo bar\n"),
            Some(("/usr/bin/env", Some("-S foo bar")))
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(parse_shebang(b"#!  /bin/sh   \n"), Some(("/bin/sh", None)));
        assert_eq!(parse_shebang(b"#!/bin/sh   -e  \n"), Some(("/bin/sh", Some("-e"))));
    }

    /// A CRLF script would otherwise look up the interpreter as `/bin/sh\r`,
    /// which no filesystem has.
    #[test]
    fn a_crlf_script_does_not_keep_the_carriage_return() {
        assert_eq!(parse_shebang(b"#!/bin/sh\r\nexit 0\r\n"), Some(("/bin/sh", None)));
    }

    #[test]
    fn an_empty_shebang_line_is_rejected() {
        assert!(parse_shebang(b"#!\n").is_none());
        assert!(parse_shebang(b"#!   \n").is_none());
    }

    /// The shape `execve` produces: interpreter, its optional arg, then the
    /// script named the way the caller asked for it.
    #[test]
    fn first_hop_is_interpreter_arg_script() {
        assert_eq!(
            shebang_hop("/bin/sh", None, "/usr/local/bin/docker-entrypoint.sh", &[]),
            strs(&["/bin/sh", "/usr/local/bin/docker-entrypoint.sh"])
        );
        assert_eq!(
            shebang_hop("/bin/sh", Some("-e"), "/run.sh", &[]),
            strs(&["/bin/sh", "-e", "/run.sh"])
        );
    }

    /// Nesting drops the previous argv[0] (Linux's `remove_arg_zero`), so the
    /// inner interpreter appears exactly once.
    #[test]
    fn a_nested_hop_drops_the_previous_argv0() {
        let first = shebang_hop("/bin/interp1", Some("-x"), "/script", &[]);
        assert_eq!(first, strs(&["/bin/interp1", "-x", "/script"]));
        assert_eq!(
            shebang_hop("/bin/sh", None, "/bin/interp1", &first),
            strs(&["/bin/sh", "/bin/interp1", "-x", "/script"])
        );
    }
}
