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

/// Extended version of spawn_process_with_channel.
///
/// `pty`: when `true`, the child's channel is marked as a real terminal
/// (`isatty()` reports true) so the kernel's canonical line discipline (ICRNL,
/// echo, line editing) runs on its stdin — for interactive sessions that
/// allocate a pty (e.g. sshd handling a client's `pty-req` for a login shell).
/// When `false` (the default for piped spawns) the child's stdin is a raw pipe.
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
    let elf_path = &resolved;

    let mut full_args = Vec::new();
    full_args.push(path.to_string());
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
    // spawn. `pty` means "give the child a brand-new controlling terminal"
    // (real Unix semantics: allocating a new pty starts a new session, it
    // does not share the allocator's terminal). A multiplexing daemon — one
    // OS process/thread serving many independent interactive sessions, e.g.
    // userspace sshd handling several concurrent SSH connections — has
    // exactly one `terminal_state` on itself; inheriting it for every
    // spawned pty session would make them all share one `input_waker` slot
    // (`crates/akuma-terminal`): session B's blocked stdin read stores its
    // waker there, but a stdin write for session A wakes whatever waker is
    // *currently* in that shared slot instead of targeting the right pid —
    // so B can permanently miss its wakeup while A keeps using its terminal,
    // and only resumes once A exits and stops re-registering. `Process::new`
    // already gives every process its own fresh `TerminalState`
    // (`process/mod.rs`), so a `pty` spawn just keeps that instead of
    // overwriting it. Non-pty spawns (plain fork/exec within an existing
    // session) keep inheriting, matching a real shell subprocess sharing its
    // parent's controlling terminal.
    if !pty && let Some(shared_state) = current_terminal_state() {
        if config().syscall_debug_info_enabled {
            log::debug!("[Process] Inheriting shared terminal state at {:p} for PID {}", Arc::as_ptr(&shared_state), process.pid);
        }
        process.terminal_state = shared_state;
    } else if config().syscall_debug_info_enabled {
        if pty {
            log::debug!("[Process] Fresh (non-inherited) terminal state for pty spawn PID {}", process.pid);
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
