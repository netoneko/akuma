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

/// Bring up a real `akuma_pmm` allocator over a leaked host-heap arena, once.
///
/// PMM calls no longer go through `ExecRuntime` at all — `docs/archive/PMM_EXTRACT.md`
/// §7 Step 5 deleted the dozen PMM fields `ensure_test_runtime` used to fake
/// (`alloc_page_zeroed: || None` and friends), so this crate's call sites now
/// reach `akuma_pmm::*` directly and need a REAL PMM behind them, not a fake.
/// This is the plan's §6 payoff: `akuma_primitives::phys_to_virt` is the
/// identity (`paddr as *mut u8`), so a real host allocation's address works as
/// a "physical" page directly — run the actual allocator over a host arena
/// instead of faking it, which is strictly better coverage than the old fake.
///
/// `pmm_uaf_quarantine`/`pmm_premature_free_check` start **off**: turning them
/// on changes `free_page`'s behaviour (frames get poisoned and parked instead
/// of freed immediately) in a way some existing test could be sensitive to.
/// Exercising them from a host test is Step 7's job ("host tests: allocator,
/// refcounts, quarantine"), not this step's — Step 5 must stay
/// behaviour-preserving. (`ExecConfig` briefly carried a same-named
/// `pmm_uaf_quarantine` flag of its own, gating the pre-Step-6
/// `memmath::poison_word_frame`; Step 6 deleted it once that function moved to
/// `src/pmm.rs` and switched to reading this crate's copy instead — see
/// `ExecConfig`'s doc comment.)
///
/// A real `std::sync::Once`, not `OnceCopy::set`'s idempotent-ignore:
/// `akuma_pmm::init` mutates the bitmap unconditionally on every call (it is
/// not itself registration-idempotent the way `register_config`/`register_hooks`
/// are), so two test threads racing their first call could reset frames the
/// other's test is already relying on.
pub fn ensure_test_pmm() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // 64 MiB: headroom for the whole `cargo test` binary's cumulative
        // allocations across every test that reaches this path, not just one.
        const ARENA_WORDS: usize = 64 * 1024 * 1024 / 8;
        let arena: alloc::vec::Vec<u64> = alloc::vec![0u64; ARENA_WORDS];
        // Leaked: this arena outlives every test in the binary; nothing ever
        // frees it back to the host allocator.
        let arena: &'static mut [u64] = alloc::boxed::Box::leak(arena.into_boxed_slice());
        let base = arena.as_ptr() as usize;
        let size = core::mem::size_of_val(arena);

        akuma_pmm::register_config(akuma_pmm::PmmConfig {
            cow_ref_ledger: true,
            pmm_uaf_quarantine: false,
            pmm_premature_free_check: false,
        });
        akuma_pmm::register_hooks(akuma_pmm::PmmHooks {
            heap_reclaim: || 0,
            drain_retired: || 0,
            evict_clean_file_pages: |_| 0,
            shrink_page_cache: |_| 0,
        });
        // No `register_surviving_mapper_hook`: it degrades via `.get()` rather
        // than panicking (the module doc's one non-mandatory hook), and every
        // caller of it here is a diagnostic print gated behind the two flags
        // just turned off above.
        akuma_pmm::init(base, size, base); // kernel_end == base: nothing pre-reserved
    });
}

/// Register the stub runtime + zeroed config, once.
///
/// `OnceCopy::set` is idempotent — first caller wins, the rest are ignored — so this is
/// safe to call from every test unconditionally and under `cargo test`'s default
/// parallelism.
pub fn ensure_test_runtime() {
    ensure_test_pmm();
    let rt = ExecRuntime {
        uptime_us: || 0,
        disable_irqs: || {},
        enable_irqs: || {},
        end_of_interrupt: |_| {},
        trigger_sgi: |_| {},
        wake_remote_idle: || {},
        wake_core: |_| {},
        heap_stats: || (0, 0),
        is_memory_low: || false,
        exec_bkl_drop_enabled: || false,
        read_file: |_| Err(0),
        read_at: |_, _, _| Err(0),
        resolve_inode: |_| Err(0),
        read_at_by_inode: |_, _, _, _| Err(0),
        on_process_exit: |_| {},
        remove_socket: |_| {},
        socket_clone_ref: |_| {},
        rump_socket_clone_ref: |_, _| {},
        futex_wake: |_, _, _| {},
        check_itimers: || {},
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
        shared_file_pages_enabled: true,
    };
    register(rt, cfg);
}
