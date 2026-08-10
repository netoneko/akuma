//! Shared host-test scaffolding for this crate.
//!
//! `akuma-exec` is a `no_std` crate whose globals (`runtime()`, `config()`) are
//! registered by the kernel at boot and **panic if read before that**. Any host test
//! that touches a code path reading either one — a `ProcessChannel::write` debug gate, a
//! `ThreadWaker::wake` that rings the scheduler via `trigger_sgi` — has to register them
//! first. This module is that registration, in one place, so tests do not each carry a
//! ~90-line stub.
//!
//! Test-only (`#[cfg(test)]`): nothing here is compiled into the kernel.

use crate::runtime::{ExecConfig, ExecRuntime, register};

/// Register the stub runtime + zeroed config, once.
///
/// `OnceCopy::set` is idempotent — first caller wins, the rest are ignored — so this is
/// safe to call from every test unconditionally and under `cargo test`'s default
/// parallelism.
pub fn ensure_test_runtime() {
    let rt = ExecRuntime {
        uptime_us: || 0,
        disable_irqs: || {},
        enable_irqs: || {},
        end_of_interrupt: |_| {},
        trigger_sgi: |_| {},
        wake_remote_idle: || {},
        wake_core: |_| {},
        alloc_page_zeroed: || None,
        alloc_page: || None,
        free_page: |_| {},
        pmm_stats: || (0, 0, 0),
        track_frame: |_, _| {},
        free_count: || 0,
        total_count: || 0,
        alloc_pages_contiguous_zeroed: |_| None,
        free_pages_contiguous: |_, _| {},
        heap_stats: || (0, 0),
        is_memory_low: || false,
        exec_bkl_drop_enabled: || false,
        read_file: |_| Err(0),
        read_at: |_, _, _| Err(0),
        resolve_inode: |_| Err(0),
        read_at_by_inode: |_, _, _| Err(0),
        on_process_exit: |_| {},
        remove_socket: |_| {},
        socket_clone_ref: |_| {},
        futex_wake: |_, _, _| {},
        pipe_close_write: |_| {},
        pipe_close_read: |_| {},
        pipe_clone_ref: |_, _| {},
        eventfd_close: |_| {},
        eventfd_clone_ref: |_| {},
        epoll_destroy: |_| {},
        pidfd_close: |_| {},
        resolve_symlinks: |_| alloc::string::String::new(),
        file_size: |_| Ok(0),
        get_box_namespace: |_| None,
        set_spawn_namespace: |_| {},
        clear_spawn_namespace: || {},
        print_str: |_| {},
        cow_ref_inc: |_| {},
        cow_ref_dec: |_| false,
        cow_ref_get: |_| 0,
        cow_fault_lock: |_| {},
        cow_fault_unlock: |_| {},
    };
    let cfg = ExecConfig {
        max_threads: 64,
        reserved_threads: 1,
        kernel_stack_size: 0,
        boot_stack_base: 0,
        boot_stack_top: 0,
        default_thread_stack_size: 0,
        system_thread_stack_size: 0,
        user_thread_stack_size: 0,
        user_stack_size: 0,
        enable_stack_canaries: false,
        stack_canary: 0,
        canary_words: 0,
        network_thread_ratio: 0,
        prioritize_never_scheduled: false,
        deferred_thread_cleanup: false,
        thread_cleanup_cooldown_us: 0,
        process_reclaim_cooldown_us: 0,
        syscall_debug_info_enabled: false,
        fork_brk_serial_progress: false,
        enable_sgi_debug_prints: false,
        proc_stdin_max_size: 1 << 20,
        proc_stdout_max_size: 1 << 20,
        cow_fork_enabled: false,
        vfork_fastpath_enabled: false,
        pthread_kill_eintr_enabled: true,
    };
    register(rt, cfg);
}
