use alloc::string::String;
use alloc::vec::Vec;
use alloc::sync::Arc;

use crate::runtime::config;
use crate::process::types::{Pid, YieldOnce};
use crate::process::channel::{ProcessChannel, get_channel};
use crate::process::children::{lookup_process, read_current_pid};
use crate::process::spawn::spawn_process_with_channel_cwd;
use super::get_box_info;

/// Execute an ELF binary from the filesystem with per-process I/O (blocking)
///
/// This spawns the process on a user thread and polls for completion.
/// Use exec_async() for non-blocking execution.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments (first arg is conventionally the program name)
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Tuple of (exit_code, stdout_data), or error message
pub fn exec_with_io(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Result<(i32, Vec<u8>), String> {
    exec_with_io_cwd(path, args, None, stdin, None)
}

/// exec_with_io with explicit cwd
pub fn exec_with_io_cwd(path: &str, args: Option<&[&str]>, env: Option<&[String]>, stdin: Option<&[u8]>, cwd: Option<&str>) -> Result<(i32, Vec<u8>), String> {
    // Spawn process with channel and cwd
    let (thread_id, channel, _pid) = spawn_process_with_channel_cwd(path, args, env, stdin, cwd)?;
    
    // For non-interactive execution, if no stdin was provided, mark it as closed
    // so the process doesn't block forever if it tries to read from it.
    if stdin.is_none() {
        channel.close_stdin();
    }

    // Poll until process exits (blocking)
    loop {
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            break;
        }
        // Yield to let process run
        crate::threading::yield_now();
    }
    
    // Collect output
    let mut stdout_data = Vec::new();
    while let Some(data) = channel.try_read() {
        stdout_data.extend_from_slice(&data);
    }
    
    // Cleanup terminated thread
    crate::threading::cleanup_terminated();
    
    Ok((channel.exit_code(), stdout_data))
}

/// Execute an ELF binary from the filesystem (legacy API for backwards compatibility)
///
/// # Arguments
/// * `path` - Path to the ELF binary
///
/// # Returns
/// Exit code of the process, or error message
#[allow(dead_code)]
pub fn exec(path: &str) -> Result<i32, String> {
    let (exit_code, _stdout) = exec_with_io(path, None, None)?;
    Ok(exit_code)
}

/// Execute a binary asynchronously and return its output when complete
///
/// Spawns the process on a user thread and polls for completion,
/// yielding to other async tasks while waiting. Returns the buffered
/// output when the process exits.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
///
/// # Returns
/// Tuple of (exit_code, stdout_data) or error message
pub async fn exec_async(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>) -> Result<(i32, Vec<u8>), String> {
    exec_async_cwd(path, args, None, stdin, None).await
}

/// exec_async with explicit cwd and env
pub async fn exec_async_cwd(path: &str, args: Option<&[&str]>, env: Option<&[String]>, stdin: Option<&[u8]>, cwd: Option<&str>) -> Result<(i32, Vec<u8>), String> {

    // Spawn process with channel and cwd
    let (thread_id, channel, _pid) = spawn_process_with_channel_cwd(path, args, env, stdin, cwd)?;

    // For non-interactive execution, if no stdin was provided, mark it as closed
    if stdin.is_none() {
        channel.close_stdin();
    }

    // Wait for process to complete
    // Each iteration yields once (returns Pending) so block_on can yield to scheduler
    loop {
        // Check if process has exited or was interrupted
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            break;
        }

        if channel.is_interrupted() {
            break;
        }

        // Yield once - this returns Pending, block_on yields, then we get polled again
        YieldOnce::new().await;
    }

    // Collect all output
    let output = channel.read_all();
    let exit_code = if channel.is_interrupted() && !channel.has_exited() {
        130 // Interrupted exit code
    } else {
        channel.exit_code()
    };

    // Final cleanup
    crate::threading::cleanup_terminated();

    Ok((exit_code, output))
}

/// Get the process channel for a running process by thread ID
///
/// Used by the SSH shell to get a handle for interrupting a process.
pub fn get_process_channel(thread_id: usize) -> Option<Arc<ProcessChannel>> {
    get_channel(thread_id)
}

/// Execute a binary with streaming output to an async writer
///
/// Spawns the process on a user thread and streams output to the
/// provided writer as it becomes available. This allows real-time
/// output display while keeping SSH responsive.
///
/// # Arguments
/// * `path` - Path to the ELF binary
/// * `args` - Optional command line arguments
/// * `stdin` - Optional stdin data for the process
/// * `output` - Async writer to stream output to
///
/// # Returns
/// Exit code or error message
pub async fn exec_streaming<W>(path: &str, args: Option<&[&str]>, stdin: Option<&[u8]>, output: &mut W) -> Result<i32, String>
where
    W: embedded_io_async::Write,
{
    exec_streaming_cwd(path, args, None, stdin, None, output).await
}

/// exec_streaming with explicit cwd and env
pub async fn exec_streaming_cwd<W>(path: &str, args: Option<&[&str]>, env: Option<&[String]>, stdin: Option<&[u8]>, cwd: Option<&str>, output: &mut W) -> Result<i32, String>
where
    W: embedded_io_async::Write,
{
    // Spawn process with channel and cwd
    let (thread_id, channel, _pid) = spawn_process_with_channel_cwd(path, args, env, stdin, cwd)?;

    // For non-interactive streaming, if no stdin was provided, mark it as closed
    if stdin.is_none() {
        channel.close_stdin();
    }

    // Stream output until process exits
    loop {
        // Read available data
        if let Some(data) = channel.try_read() {
            if let Err(_e) = output.write_all(&data).await {
                // Writer failed, likely connection closed
                break;
            }
        }

        // Check if process has exited
        if channel.has_exited() || crate::threading::is_thread_terminated(thread_id) {
            break;
        }

        if channel.is_interrupted() {
            break;
        }

        // Yield to scheduler
        YieldOnce::new().await;
    }

    // Drain remaining output
    if let Some(data) = channel.try_read() {
        let _ = output.write_all(&data).await;
    }

    let exit_code = if channel.is_interrupted() && !channel.has_exited() {
        130 // Interrupted
    } else {
        channel.exit_code()
    };

    // Final cleanup
    crate::threading::cleanup_terminated();

    Ok(exit_code)
}

/// Reattach I/O from a caller process (or kernel) to a target PID
pub fn reattach_process_ext(caller_pid: Option<Pid>, target_pid: Pid) -> Result<(), &'static str> {
    // 1. Validate hierarchy permissions
    let (caller_box_id, channel) = if let Some(pid) = caller_pid {
        let caller = lookup_process(pid).ok_or("Caller not found")?;
        (caller.box_id, caller.channel.clone())
    } else {
        // Kernel caller (e.g. built-in SSH shell)
        // System threads use thread-ID based channel lookup
        let tid = crate::threading::current_thread_id();
        let ch = get_channel(tid).ok_or("Kernel thread has no channel")?;
        (0, Some(ch)) // Kernel is Box 0
    };

    let target_box_id = {
        let target = lookup_process(target_pid).ok_or("Target not found")?;
        target.box_id
    };

    let mut allowed = false;
    if caller_box_id == 0 {
        allowed = true; // Host/Kernel can reattach anything
    } else if target_box_id == caller_box_id {
        allowed = true; // Same box
    } else if let Some(pid) = caller_pid {
        // Check if caller created the target's box (child box)
        if let Some(info) = get_box_info(target_box_id) {
            if info.creator_pid == pid {
                allowed = true;
            }
        }
    }

    if !allowed {
        return Err("Permission denied: cannot reattach process outside hierarchy");
    }

    // 2. Perform the delegation
    if let Some(pid) = caller_pid {
        let caller = lookup_process(pid).ok_or("Caller not found")?;
        caller.delegate_pid = Some(target_pid);
    } else {
        // For kernel caller, we don't have a 'Process' struct to set delegate_pid,
        // but we still want to link the channel to the target.
    }

    // Target process now uses caller's output channel
    {
        let target = lookup_process(target_pid).ok_or("Target not found")?;
        target.channel = channel;
    }

    if config().syscall_debug_info_enabled {
        log::debug!("[Process] Reattached (caller={:?}) -> PID {}", caller_pid, target_pid);
    }

    Ok(())
}

/// Reattach I/O from the current process to a target PID
pub fn reattach_process(target_pid: Pid) -> Result<(), &'static str> {
    let caller_pid = read_current_pid(); // Can be None for kernel threads
    reattach_process_ext(caller_pid, target_pid)
}
