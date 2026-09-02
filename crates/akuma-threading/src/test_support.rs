//! Host-test scaffolding: register stub [`ThreadRuntime`], [`ThreadConfig`] and
//! [`ProcessHooks`] so tests that touch a path reading `runtime()`/`config()`/
//! `process()` do not panic. `#[cfg(test)]` — never compiled into the kernel.
//!
//! `Registered::register` is first-writer-wins and every test writes the same
//! values, so the `Once` guard is just to keep the log quiet under `cargo test`'s
//! parallel threads.

use crate::{ProcessHooks, ThreadConfig, ThreadRuntime};

fn stub_uptime_us() -> u64 {
    0
}
fn stub_trigger_sgi(_: u32) {}
fn stub_wake_core(_: u8) {}
fn stub_wake_remote_idle() -> bool {
    false
}
fn stub_eoi(_: u32) {}
fn stub_print(s: &str) {
    // Route to std so `cargo test -- --nocapture` still shows scheduler prints.
    #[cfg(not(target_os = "none"))]
    std::print!("{s}");
    #[cfg(target_os = "none")]
    let _ = s;
}

fn hook_none_u32(_: usize) -> Option<u32> {
    None
}
fn hook_false() -> bool {
    false
}
fn hook_unit() {}
fn hook_unit_usize(_: usize) {}
fn hook_dump_info(_: u32) -> Option<(u64, u32, usize)> {
    None
}

pub fn ensure_test_runtime() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        crate::register(
            ThreadRuntime {
                uptime_us: stub_uptime_us,
                trigger_sgi: stub_trigger_sgi,
                wake_core: stub_wake_core,
                wake_remote_idle: stub_wake_remote_idle,
                end_of_interrupt: stub_eoi,
                print_str: stub_print,
            },
            ThreadConfig {
                reserved_threads: 8,
                kernel_stack_size: 64 * 1024,
                system_thread_stack_size: 32 * 1024,
                user_thread_stack_size: 64 * 1024,
                boot_stack_base: 0,
                boot_stack_top: 0,
                enable_stack_canaries: false,
                stack_canary: 0xDEAD_BEEF_DEAD_BEEF,
                canary_words: 4,
                network_thread_ratio: 4,
                prioritize_never_scheduled: false,
                deferred_thread_cleanup: false,
                thread_cleanup_cooldown_us: 10_000,
                syscall_debug_info_enabled: false,
                enable_sgi_debug_prints: false,
            },
        );
        crate::register_process_hooks(ProcessHooks {
            clear_draining: hook_unit_usize,
            lifecycle_trace_on: hook_false,
            pid_for_thread: hook_none_u32,
            find_pid_by_thread: hook_none_u32,
            is_current_interrupted: hook_false,
            proc_dump_info: hook_dump_info,
            dump_orphan_processes: hook_unit,
        });
    });
}
