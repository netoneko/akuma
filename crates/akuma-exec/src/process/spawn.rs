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
use crate::process::children::{lookup_process, current_terminal_state};

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
    spawn_process_with_channel_ext(path, args, env, stdin, cwd, 0)
}

/// Extended version of spawn_process_with_channel
pub fn spawn_process_with_channel_ext(
    path: &str,
    args: Option<&[&str]>,
    env: Option<&[String]>,
    stdin: Option<&[u8]>,
    cwd: Option<&str>,
    box_id: u64,
) -> Result<(usize, Arc<ProcessChannel>, Pid), String> {
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

    let mut process = match (runtime().read_file)(elf_path) {
        Ok(elf_data) => {
            let result = Process::from_elf(elf_path, &full_args, &full_env, &elf_data, None);
            if use_ns_override { (runtime().clear_spawn_namespace)(); }
            result.map_err(|e| format!("Failed to load ELF: {}", e))?
        }
        Err(_) => {
            let file_size = (runtime().file_size)(elf_path)
                .map_err(|e| {
                    if use_ns_override { (runtime().clear_spawn_namespace)(); }
                    format!("Failed to stat {}: {}", elf_path, e)
                })? as usize;
            let result = Process::from_elf_path(elf_path, elf_path, file_size, &full_args, &full_env, None);
            if use_ns_override { (runtime().clear_spawn_namespace)(); }
            result.map_err(|e| format!("Failed to load ELF: {}", e))?
        }
    };

    // Always create a fresh channel per spawned process.
    // Reusing the parent's channel would cause the child's set_exited() call
    // to contaminate the parent's channel, leaking exit codes.
    let channel = Arc::new(ProcessChannel::new());
    
    // Seed the channel with initial stdin data if provided.
    // Empty stdin (Some(b"")) keeps stdin open so sys_write enables ONLCR
    // translation — use this for subprocesses that need terminal-style output.
    if let Some(data) = stdin {
        if !data.is_empty() {
            channel.write_stdin(data);
            channel.close_stdin();
        }
    }

    // Set the channel in the process struct (UNIFIED I/O)
    process.channel = Some(channel.clone());

    // Inherit terminal state from caller if available
    if let Some(shared_state) = current_terminal_state() {
        if config().syscall_debug_info_enabled {
            log::debug!("[Process] Inheriting shared terminal state at {:p} for PID {}", Arc::as_ptr(&shared_state), process.pid);
        }
        process.terminal_state = shared_state;
        
        // Auto-delegate foreground to the new process.
        // For interactive spawns, the child should start in the foreground.
        let pid_to_delegate = process.pid;
        process.terminal_state.lock().foreground_pgid = pid_to_delegate;
    } else {
        if config().syscall_debug_info_enabled {
            log::debug!("[Process] NO shared terminal state found for caller thread {}, using default for PID {}", crate::threading::current_thread_id(), process.pid);
        }
    }

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
            if let Some(proc) = lookup_process(pid) {
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

    // CRITICAL: Register the process in the table immediately.
    // This ensures that lookup_process(pid) works as soon as this function returns,
    // allowing reattach() to succeed without races.
    register_process(pid, boxed_process);

    // Register the channel for the thread ID placeholder (0 for now, will be updated)
    // Actually, current_channel() now uses the field in Process struct, so this is mostly for legacy.
    register_channel(0, channel.clone());

    // Spawn on a user thread
    let thread_id = crate::threading::spawn_user_thread_fn_for_process(move || {
        let tid = crate::threading::current_thread_id();
        
        // Update thread_id in the registered process
        if let Some(p) = lookup_process(pid) {
            p.thread_id = Some(tid);
            
            // Move the channel registration to the correct TID
            remove_channel(0);
            register_channel(tid, p.channel.as_ref().unwrap().clone());
            
            // Execute the process (already in the table)
            run_registered_process(pid);
        } else {
            log::debug!("[Process] FATAL: PID {} disappeared during spawn", pid);
            loop { crate::threading::yield_now(); }
        }
    })
    .map_err(|e| format!("Failed to spawn thread: {}", e))?;

    // Set the thread ID in the process table entry for the parent to see immediately
    if let Some(p) = lookup_process(pid) {
        p.thread_id = Some(thread_id);
    }

    Ok((thread_id, channel, pid))
}

/// Execute a process that is already registered in the PROCESS_TABLE
pub(crate) fn run_registered_process(pid: Pid) -> ! {
    let proc = lookup_process(pid).expect("Process not found in run_registered_process");
    
    // Prepare the process (set state, write process info page)
    proc.prepare_for_execution();
    
    // Activate the user address space (sets TTBR0)
    proc.address_space.activate();

    // Now safe to enable IRQs - TTBR0 is set to user tables
    (runtime().enable_irqs)();

    // Enter user mode via ERET - this never returns
    unsafe {
        enter_user_mode(&proc.context);
    }
}
