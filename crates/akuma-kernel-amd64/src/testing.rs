//! What the boot suite (`crates/akuma-kernel-tests-amd64`) may reach, and nothing else.
//!
//! The suite left this crate on 2026-11-11 (multiarch, the tests crate step). Its tests reach
//! kernel state no other crate should: the spawn table, `Image`'s fields, the inline
//! `smoke_test`s each module still carries. The modules stay private; the items the suite
//! uses are `pub` inside them and re-exported **here, one by one**, so this file is the
//! complete list of what the kernel exposes for testing. `#[doc(hidden)]` at its declaration,
//! and absent from a `no-tests` build.
//!
//! **Not an API.** Nothing but the suite may name this module; a driver or a front crate that
//! needs one of these items needs its own, narrower export.

/// The suite's seam and its context.
pub mod boot {
    pub use crate::boot::{
        bring_up_secondaries, register_self_tests, suite_emit, wire_console_and_syscalls,
        SuiteCtx, Verdict,
    };
}

pub mod blk {
    pub use crate::blk::smoke_test;
}

pub mod clock {
    pub use crate::clock::sync_via_sntp;
}

pub mod console {
    pub use crate::console::terminal_state;
}

pub mod fd {
    pub use crate::fd::{
        boot_row_register, boot_row_release, console_nonblock_test, smoke_test, sys_close,
        sys_fcntl, sys_ioctl, sys_read, table_get,
    };
    pub mod errno {
        pub use crate::fd::errno::{
            is_err, EAFNOSUPPORT, EBADF, ECHILD, EFAULT, EINVAL, ENOTSOCK, ESRCH,
        };
    }
}

pub mod fs {
    pub use crate::fs::{create_dir, read_file, remove_dir, smoke_test, write_file};
}

pub mod idt {
    pub use crate::idt::{smoke_test, user_copy_smoke_test};
}

pub mod lapic {
    pub use crate::lapic::{
        clock_rate_check, init, smoke_test, start_timer, stop_timer, US_PER_TICK_TARGET,
    };
}

pub mod loader {
    pub use crate::loader::{load, read_user, LoadedImage, MAX_ARGV, MAX_ENVP};
}

pub mod mem {
    pub use crate::mem::smoke_test;
}

pub mod mm {
    pub use crate::mm::{demand_paging_report, smoke_test};
}

pub mod net {
    pub use crate::net::{
        enable_probe, netpoll_drain_selftest, netpoll_spawn_selftest, smoke_test, uptime_us,
    };
}

pub mod paging {
    pub use crate::paging::{smoke_test, translate};
}

pub mod pci {
    pub use crate::pci::{find_class, smoke_test};
}

pub mod phys {
    pub use crate::phys::FreshFrame;
}

pub mod pipe {
    pub use crate::pipe::smoke_test;
}

pub mod reboot {
    pub use crate::reboot::smoke_test;
}

pub mod sched {
    #[cfg(feature = "ring0-preempt")]
    pub use crate::sched::ring0_preempt_smoke_test;
    pub use crate::sched::{
        all_user_tasks_finished, backstop_wakes, block_smoke_test, blocks, current_task,
        publish_task, reaper_smoke_test, smoke_test, user_context_smoke_test, wakes, yield_now,
        STACK_SIZE,
    };
}

pub mod serial {
    pub use crate::serial::puts;
}

pub mod shootdown {
    pub use crate::shootdown::smoke_test;
}

pub mod signal {
    pub use crate::signal::smoke_test;
}

pub mod smp {
    pub use crate::smp::{cpu_cap, online_cpus, smoke_test, MAX_CPUS};
}

pub mod sock {
    pub use crate::sock::smoke_test;
}

pub mod thread {
    pub use crate::thread::live_count;
}

pub mod uaccess {
    pub use crate::uaccess::smoke_test;
}

/// Ring 3: the process machinery the `usermode` tests drive by hand.
pub mod usermode {
    pub use crate::usermode::{
        alloc_pid, current_pid, current_process, reap_exec_process,
        register_exec_process, run_init, spawn_process_task,
        syscall_dispatch, sys_spawn, sys_waitpid, sys_write, to_glue, with_current_address_space,
        with_current_regions, Image, AF_UNIX, elf_stack_max_pages, ELF_STACK_PAGES, ELF_STACK_TOP,
        SPAWN_FLAG_PTY, SYSCALL_TRACE, WRITE_CPU, WRITE_SEQ,
        WRITE_SEQ_LEN,
    };
}

pub mod xhci {
    pub use crate::drivers::usb_smoke_test as smoke_test;
}
