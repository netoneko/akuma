//! The `akuma-exec` runtime table on aarch64: the amd64 half's `exec_twin`
//! registration, with this machine's clock, console and IRQ mask.
//!
//! Like the amd64 half, this kernel does not call `akuma_exec::init` (that
//! registers the full AArch64 kernel's BKL, MMU gates and prefault hook). The
//! process-crossing arms (`enter_user`, `fork_share_memory`,
//! `bind_child_task`) are `core::proc`'s, through this machine's `Proc`.

use akuma_exec::{ExecConfig, ExecRuntime};

/// The kernel stack each thread runs its syscalls on: the amd64 half's 64 KiB.
pub const KERNEL_STACK_SIZE: usize = 64 * 1024;
/// What `prlimit64(RLIMIT_STACK)` reports: the 8 MiB a main stack may grow to.
pub const USER_STACK_MAX: usize = 8 << 20;

/// Register the table. After the heap and the tick, before the root mount
/// (`/proc` reads the process table through it).
pub fn init() {
    akuma_exec::runtime::register(exec_runtime(), exec_config());
    akuma_exec::process::reserve_pid_floor(2);
}

fn exec_runtime() -> ExecRuntime {
    ExecRuntime {
        uptime_us: crate::fs::uptime_us,
        disable_irqs: akuma_cpu::daif::mask_irq,
        enable_irqs: akuma_cpu::daif::unmask_irq,
        end_of_interrupt: akuma_gic::end_of_interrupt,
        heap_stats: || {
            let s = akuma_alloc::stats();
            (s.heap_size, s.allocated)
        },
        is_memory_low: akuma_alloc::is_memory_low,
        print_str: akuma_primitives::console::print_str,
        // The root, through the shared mount table.
        read_file: |path| akuma_vfs_glue::read_file(path).map_err(|_| -1),
        read_at: |path, off, buf| akuma_vfs_glue::read_at(path, off, buf).map_err(|_| -1),
        read_at_by_inode: |path, ino, off, buf| {
            akuma_vfs_glue::read_at_by_inode(path, ino, off, buf).map_err(|_| -1)
        },
        resolve_symlinks: akuma_vfs_glue::resolve_symlinks,
        resolve_file_id: |path| akuma_vfs_glue::resolve_file_id(path).ok_or(-1),
        file_size: |path| akuma_vfs_glue::file_size(path).map_err(|_| "file_size: not found"),
        // glue's tables, as on amd64.
        pipe_close_write: akuma_syscalls_glue::pipe::pipe_close_write,
        pipe_close_read: akuma_syscalls_glue::pipe::pipe_close_read,
        pipe_clone_ref: akuma_syscalls_glue::pipe::pipe_clone_ref,
        pipe_write: akuma_syscalls_glue::pipe::pipe_write_no_sigpipe,
        remove_socket: akuma_net::socket::remove_socket,
        socket_clone_ref: akuma_net::socket::socket_clone_ref,
        write_user_tid: |va, tid| akuma_user_access::copy_to_user(va as u64, &tid.to_le_bytes()).is_ok(),
        alloc_process_info: |_space| Ok(0),
        futex_wake: akuma_syscalls_glue::futex_wake,
        eventfd_close: akuma_syscalls_glue::eventfd::eventfd_close,
        eventfd_clone_ref: akuma_syscalls_glue::eventfd::eventfd_clone_ref,
        unix_sock_close: akuma_syscalls_glue::unixsock::unix_sock_close,
        unix_sock_clone_ref: akuma_syscalls_glue::unixsock::unix_sock_clone_ref,
        epoll_destroy: akuma_syscalls_glue::poll::epoll_destroy,
        pidfd_close: akuma_runtime_tokio::PIDFD_CLOSE,
        flock_release: akuma_syscalls_glue::flock::flock_release,
        // One core, no SGIs yet (A6 brings the secondaries).
        trigger_sgi: |_| {},
        wake_core: |_| {},
        wake_remote_idle: || false,
        exec_bkl_drop_enabled: || false,
        on_process_exit: |_pid| {},
        check_itimers: || {},
        dsp_close: || {},
        pty_clone_ref: |_, _| {},
        pty_close: |_, _| {},
        get_box_namespace: |_| None,
        set_spawn_namespace: |_| {},
        clear_spawn_namespace: || {},
        rump_socket_clone_ref: |_, _| {},
        // EL0 (A5.6).
        // A fork child's or new thread's first crossing into EL0.
        enter_user: crate::proc::enter_user,
        fork_share_memory,
        bind_child_task: |tid, child, kind| {
            akuma_tokio_kernel_core::proc::bind_child_task::<crate::mm::Aarch64>(
                tid,
                child.pid,
                child.address_space.l0_phys() as u64,
                kind == akuma_exec::process::ChildKind::Thread,
            )
        },
    }
}

/// `ExecRuntime::fork_share_memory`: core's copy-on-write clone of the
/// parent's root into a fresh one, which the child's `Process` then views.
///
/// Not into the space `fork_process` built: on AArch64 `UserAddressSpace::new`
/// identity-maps RAM itself (an L1 and an L2 per GiB), which the clone would
/// overwrite and leak at the child's first exec or exit. That space is dropped
/// here instead (never live, so its frames go straight back), and the child
/// holds a non-owning view of the clone, which core frees at the reap.
fn fork_share_memory(
    parent: &akuma_exec::process::Process,
    child: &mut akuma_exec::process::Process,
) -> Result<(), &'static str> {
    use akuma_tokio_kernel_core::mmu::FreshPage;
    use akuma_tokio_kernel_core::tree;
    let src = parent.address_space.l0_phys() as u64;
    let dst = crate::mm::Frame::alloc_zeroed().ok_or("aarch64: no frame for the child's root")?.publish() as u64;
    if tree::clone_user_half::<crate::mm::Aarch64>(src, dst).is_none() {
        let _ = tree::free_user_root::<crate::mm::Aarch64>(dst);
        return Err("aarch64: user-half clone failed");
    }
    let Some(view) = akuma_mmu::UserAddressSpace::new_shared(dst as usize) else {
        let _ = tree::free_user_root::<crate::mm::Aarch64>(dst);
        return Err("aarch64: no view of the child's root");
    };
    child.address_space = akuma_exec::process::ProcAddressSpace::new(view);
    Ok(())
}

fn exec_config() -> ExecConfig {
    ExecConfig {
        max_threads: 16,
        reserved_threads: 0,
        kernel_stack_size: KERNEL_STACK_SIZE,
        boot_stack_base: 0,
        boot_stack_top: 0,
        default_thread_stack_size: KERNEL_STACK_SIZE,
        system_thread_stack_size: KERNEL_STACK_SIZE,
        user_thread_stack_size: KERNEL_STACK_SIZE,
        user_stack_size: USER_STACK_MAX,
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
        proc_stdin_max_size: 0,
        proc_stdout_max_size: 1024 * 1024,
        cow_fork_enabled: false,
        vfork_fastpath_enabled: false,
        pthread_kill_eintr_enabled: false,
        shared_file_pages_enabled: false,
    }
}
