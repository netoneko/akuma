//! Resource-sizing policy, shared by every kernel.
//!
//! Extracted from `akuma-kernel-glue` (2026-11-11, config flag sweep): these four functions
//! decided the AArch64 root kernel's memory layout alone, while `akuma-boot-amd64::mem::heap_size_for`
//! carried a cruder, unrelated copy for amd64 — a flat 512 MiB/1 GiB step with no floor
//! proportional to actual RAM, which FATALs outright on a genuinely small machine (511 MiB usable
//! → wants a 512 MiB heap that cannot fit). See `docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md` § "Open
//! issue: heap auto-sizing".
//!
//! **Every call site already has the inputs.** amd64 depends on `akuma-config` directly (unlike
//! `akuma-kernel-core`/`-glue`, which it does not yet consume), so both kernels read the exact same
//! `RESERVED_THREADS`/`MAX_THREADS`/`USER_THREAD_STACK_SIZE`/`KERNEL_HEAP_SIZE_MB`/… constants and
//! pass them in as plain arguments — this crate has no opinion on where those constants live or
//! what they're called, only on the arithmetic once it has them. A user who wants to bake in a
//! fixed value for their own build edits those `akuma-config` constants (the existing
//! `KERNEL_HEAP_SIZE_MB`/`THREAD_LIMIT_OVERRIDE` manual-override pattern); this crate reads the
//! override the same way it reads everything else, as a parameter.
//!
//! **Extension point**: a future sizing decision (socket-table ceiling, a stack-size policy, …)
//! joins this crate the same way — a pure `fn(inputs...) -> usize` with its own unit tests,
//! independent of the ones already here. `akuma-net::MAX_SOCKETS` (currently `pub(crate)`, a flat
//! 256 regardless of RAM) is the next obvious candidate, but it is not moved here yet: unlike heap
//! size, its ceiling is baked into a static array's const generic, so widening it to a real
//! function needs `akuma-net` itself to take the ceiling as a const parameter first — a bigger,
//! separate change.
//!
//! Pure, `forbid(unsafe_code)`, no dependencies, no allocation — like `akuma-machine-facts`, which
//! this crate's output (a [`MemoryLayout`]) sits one step downstream of.
#![no_std]
#![forbid(unsafe_code)]

/// Kernel physical-RAM layout: three contiguous regions starting at the machine's `ram_base` —
/// `[.. heap_start)` code+boot-stack, `[heap_start ..)` heap, then user pages.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MemoryLayout {
    pub code_and_stack: usize,
    pub heap_start: usize,
    pub heap_size: usize,
    pub user_pages_start: usize,
    pub user_pages_size: usize,
}

/// Compute the kernel memory layout for a detected RAM region.
///
/// All profile-specific policy is read from the caller's own config constants and passed in as
/// `min_code_and_stack`/`stack_guard`/`mem_calc_clamp_mb`, rather than this crate importing any
/// one kernel's `config` module — that is what keeps it callable from both AArch64 and amd64
/// without either depending on the other's crate.
///
/// CRITICAL: `code_and_stack` must cover the caller's *boot stack*
/// (`boot_stack_top` = the absolute VA of the initial SP) or the heap is placed atop the live boot
/// stack and the allocator hands out the stack's own pages under pressure — kernel-corrupts-kernel.
/// The caller passes `boot_stack_top` precisely so this function can enforce that; it does not
/// re-derive it.
#[must_use]
pub fn compute_memory_layout(
    ram_base: usize,
    ram_size: usize,
    boot_stack_top: usize,
    min_code_and_stack: usize,
    stack_guard: usize,
    mem_calc_clamp_mb: usize,
    kernel_heap_size_mb: usize,
) -> MemoryLayout {
    let stack_cover = (boot_stack_top - ram_base) + stack_guard;
    let calc_ram = reserve_calc_ram(ram_size, mem_calc_clamp_mb);
    let code_and_stack = core::cmp::max(core::cmp::max(calc_ram / 16, min_code_and_stack), stack_cover);
    let heap_start = ram_base + code_and_stack;
    let heap_size = compute_heap_size(calc_ram, code_and_stack, kernel_heap_size_mb);
    let user_pages_start = heap_start + heap_size;
    let user_pages_size = ram_size.saturating_sub(code_and_stack + heap_size);
    MemoryLayout { code_and_stack, heap_start, heap_size, user_pages_start, user_pages_size }
}

/// RAM size used to compute the kernel's own reserves (code+stack, heap).
///
/// `clamp_mb == 0` → return the real `ram_size` (historical behaviour, used on release/size).
/// Otherwise cap at `clamp_mb` MiB, so on a big box the kernel's reserve math stays pinned to the
/// small-machine numbers it was tuned for and the surplus RAM flows to the user-page pool.
#[must_use]
pub fn reserve_calc_ram(ram_size: usize, clamp_mb: usize) -> usize {
    if clamp_mb != 0 {
        core::cmp::min(ram_size, clamp_mb * 1024 * 1024)
    } else {
        ram_size
    }
}

/// Decide the kernel heap size (bytes) for a given RAM size and code+stack reserve.
///
/// - `manual_override_mb != 0` → use that fixed value (the caller's `KERNEL_HEAP_SIZE_MB`-style
///   escape hatch).
/// - **RAM ≥ 256 MiB** → the historical generous heap: `1/8 of RAM, clamped to [64, 256] MiB`.
///   Unchanged so the common 256 MiB+ configs and memory-hungry workloads (go build, bun, rustc
///   metadata) behave exactly as before.
/// - **RAM < 256 MiB** → scale down: target `1/8 of RAM` with a 4 MiB floor (the kernel boots on
///   ~2.2 MiB of heap; thread stacks come from the PMM pool, not this heap, so it does not have to
///   cover them — see [`compute_thread_limit`]), but never more than what is left after
///   `code_and_stack` and a small user-page reserve, so user pages always survive. A flat 64 MiB
///   floor here left 0 user pages below ~72 MiB (no boot) and starved user RAM at 128 MiB — the
///   bug this scaling exists to avoid, and the same shape of bug amd64's old flat 512 MiB/1 GiB
///   step had at the small end (see the module doc).
#[must_use]
pub fn compute_heap_size(ram_size: usize, code_and_stack: usize, manual_override_mb: usize) -> usize {
    const MB: usize = 1024 * 1024;
    if manual_override_mb != 0 {
        return manual_override_mb * MB;
    }
    if ram_size >= 256 * MB {
        (ram_size / 8).clamp(64 * MB, 256 * MB)
    } else {
        const SMALL_FLOOR: usize = 4 * MB;
        const MIN_USER: usize = 4 * MB;
        let cap = ram_size.saturating_sub(code_and_stack).saturating_sub(MIN_USER);
        core::cmp::min(core::cmp::max(ram_size / 8, SMALL_FLOOR), core::cmp::max(cap, MB))
    }
}

/// Decide a user thread/process stack size for a given RAM size.
///
/// Extracted from `akuma-config::compute_user_stack_size` (AArch64-only until now; amd64 never
/// called it and instead hardcoded a flat ceiling, `ELF_STACK_MAX_PAGES * 4096` = 8 MiB,
/// regardless of RAM — see `crates/akuma-kernel-amd64/src/exec_runtime.rs`). `manual_override`, like every other
/// function here, is the caller's own escape hatch (`USER_STACK_SIZE_OVERRIDE`).
///
/// `ram / 2048` clamped to `[128 KiB, 8 MiB]`, rounded up to a 4 KiB page:
/// 256 MiB RAM → 128 KiB (floor), 1 GiB → 512 KiB, 4 GiB+ → 2 MiB (unchanged from history up to
/// the 8 MiB ceiling). Below 256 MiB this is the one function in the crate whose floor is a hard
/// *minimum* workable stack (128 KiB), not a "never below what's left" budget like
/// [`compute_heap_size`] — a thread needs some usable stack to run at all, so this never scales
/// below that no matter how little RAM is left.
#[must_use]
pub fn compute_user_stack_size(ram_size_bytes: usize, manual_override: usize) -> usize {
    if manual_override != 0 {
        return manual_override;
    }
    const MIN_STACK: usize = 128 * 1024;
    const MAX_STACK: usize = 8 * 1024 * 1024;
    let computed = ram_size_bytes / 2048;
    if computed < MIN_STACK {
        MIN_STACK
    } else if computed > MAX_STACK {
        MAX_STACK
    } else {
        (computed + 0xFFF) & !0xFFF
    }
}

/// Decide how many thread slots get a stack allocated (`thread_limit`, capped at `max_threads`).
///
/// Thread stacks come from the PMM (the user-pages pool), so on a small machine the full thread
/// pool is the real boot floor, not the heap. Give the pool at most ~1/4 of user pages (processes'
/// ELF images, heaps and page tables need the rest — one process ELF load OOMs if the pool is too
/// greedy), keeping `reserved` system threads plus at least 6 user threads (a minimal session:
/// shell + SSH thread + tcc + a couple of sub-processes).
#[must_use]
pub fn compute_thread_limit(
    user_pages_size: usize,
    reserved_threads: usize,
    system_thread_stack_size: usize,
    user_thread_stack_size: usize,
    max_threads: usize,
    manual_override: usize,
) -> usize {
    if manual_override != 0 {
        return manual_override;
    }
    let sys_total = reserved_threads.saturating_sub(1) * system_thread_stack_size;
    let stack_budget = user_pages_size / 4;
    let user_budget = stack_budget.saturating_sub(sys_total);
    let n_user = user_budget / user_thread_stack_size;
    (reserved_threads + n_user).clamp(reserved_threads + 6, max_threads)
}

/// A workload's minimum resource ask, checked against a computed [`MemoryLayout`].
///
/// Checked at boot — a non-fatal warning, not a gate: the workload may still run degraded or fail
/// later on its own terms, but the operator gets told *why* up front instead of guessing from a
/// later OOM.
///
/// Extension point for the boot-hook the user asked for (2026-11-11): each runtime that cares
/// about its own minimum declares one of these, and the kernel that hosts it calls [`shortfall`]
/// against its own computed layout. `TOKIO_MIN` below is the first one, with a real number behind
/// it (every tokio kernel's current fixed boot-heap constant); `JAVA_MIN` is a deliberately unset
/// placeholder — see its own doc.
#[derive(Copy, Clone, Debug)]
pub struct Requirement {
    pub name: &'static str,
    pub min_heap_bytes: usize,
    pub min_user_pages_bytes: usize,
    pub min_threads: usize,
}

/// What a [`Requirement`] found short, if anything. `None` from [`shortfall`] means every minimum
/// was met.
#[derive(Copy, Clone, Debug)]
pub struct Shortfall {
    pub name: &'static str,
    pub heap_short_bytes: usize,
    pub user_pages_short_bytes: usize,
    pub threads_short: usize,
}

/// Check a computed layout and thread limit against a [`Requirement`]. Pure, no printing — the
/// caller's boot path turns a `Some` into whatever warning line its console wants.
#[must_use]
pub fn shortfall(layout: &MemoryLayout, thread_limit: usize, req: &Requirement) -> Option<Shortfall> {
    let heap_short = req.min_heap_bytes.saturating_sub(layout.heap_size);
    let user_short = req.min_user_pages_bytes.saturating_sub(layout.user_pages_size);
    let threads_short = req.min_threads.saturating_sub(thread_limit);
    if heap_short == 0 && user_short == 0 && threads_short == 0 {
        None
    } else {
        Some(Shortfall {
            name: req.name,
            heap_short_bytes: heap_short,
            user_pages_short_bytes: user_short,
            threads_short,
        })
    }
}

/// A tokio-based kernel's (Taoxichuan, Shanghai, Kyoto) minimum, read off what already works.
///
/// Every `akuma-tokio-kernel-{aarch64,amd64}` boots today with a flat 16 MiB `HEAP_BYTES` boot
/// heap and whatever RAM the VMM gave it past that for the PMM pool. 16 MiB is therefore a
/// **measured floor that already boots**, not a guess — but it has never been pressure-tested
/// below the RAM sizes these kernels are normally run at (2026-11-11: open question, see the
/// convergence doc). `min_user_pages_bytes`/`min_threads` here are placeholders at "boots at all"
/// generosity (32 MiB, 16 threads) until someone measures what tokio's own worker pool and task
/// set actually need; tighten them once that's known.
pub const TOKIO_MIN: Requirement = Requirement {
    name: "tokio kernel",
    min_heap_bytes: 16 * 1024 * 1024,
    min_user_pages_bytes: 32 * 1024 * 1024,
    min_threads: 16,
};

/// A JVM's minimum — **deliberately unset**.
///
/// No JVM runs on Akuma yet (see
/// `docs/archive/BUILDING_KERNEL_WITH_LOCAL_AI_PAIN_AND_SUFFERING.md` and the JVM-requirements
/// work), so there is no measurement to put here and no honest placeholder number: HotSpot's own
/// `-Xmx`/metaspace/thread-stack defaults do not transfer to Akuma's allocator or its PMM-backed
/// thread pool. Fill this in from the syscall-profile/collection work once a JVM actually boots
/// far enough to measure, not from a Linux-side guess.
pub const JAVA_MIN: Option<Requirement> = None;

#[cfg(test)]
mod tests {
    use super::*;
    const MB: usize = 1024 * 1024;

    /// Matches the doc comment's own worked examples, and the hard 128 KiB floor at tiny RAM
    /// (never scales below a workable stack, unlike every other function here).
    #[test]
    fn user_stack_size_matches_history_and_has_a_hard_floor() {
        for &(ram_mb, exp_kb) in &[(256, 128), (512, 256), (1024, 512), (2048, 1024)] {
            assert_eq!(compute_user_stack_size(ram_mb * MB, 0), exp_kb * 1024, "{ram_mb}MB RAM");
        }
        for &ram_mb in &[1usize, 8, 24, 64, 128] {
            assert_eq!(compute_user_stack_size(ram_mb * MB, 0), 128 * 1024, "{ram_mb}MB RAM floor");
        }
        assert_eq!(compute_user_stack_size(64 * 1024 * MB, 0), 8 * MB, "ceiling at huge RAM");
        assert_eq!(compute_user_stack_size(24 * MB, 7), 7, "manual override wins");
    }

    /// Large RAM (>= 256MB): exact historical values must be preserved.
    #[test]
    fn heap_size_large_ram_matches_history() {
        let cs = |ram: usize| core::cmp::max(ram / 16, 8 * MB);
        for &(ram_mb, exp_mb) in &[(256, 64), (512, 64), (1024, 128), (2048, 256), (4096, 256)] {
            let ram = ram_mb * MB;
            assert_eq!(compute_heap_size(ram, cs(ram), 0), exp_mb * MB, "{ram_mb}MB RAM");
        }
    }

    /// Small RAM (< 256MB): heap must leave > 0 user pages and keep a sane floor (>= 4 MiB; boot
    /// uses ~2.2 MiB). Thread stacks live in the PMM, not the heap, so the heap no longer has to
    /// cover them (see `compute_thread_limit`).
    #[test]
    fn heap_size_small_ram_leaves_user_pages() {
        let cs = |ram: usize| core::cmp::max(ram / 16, 8 * MB);
        for &ram_mb in &[32usize, 48, 64, 96, 128, 192] {
            let ram = ram_mb * MB;
            let c = cs(ram);
            let h = compute_heap_size(ram, c, 0);
            let user = ram.saturating_sub(c + h);
            assert!(user > 0, "{ram_mb}MB RAM -> 0 user pages (heap {}MB)", h / MB);
            assert!(h >= 4 * MB, "{ram_mb}MB RAM -> heap {}MB below 4MB floor", h / MB);
        }
    }

    #[test]
    fn heap_size_manual_override_wins() {
        assert_eq!(compute_heap_size(4096 * MB, 8 * MB, 7), 7 * MB);
        assert_eq!(compute_heap_size(16 * MB, 8 * MB, 7), 7 * MB);
    }

    /// `reserve_calc_ram`: clamp 0 is the identity at every RAM size; a non-zero clamp caps above
    /// it and never inflates below it.
    #[test]
    fn reserve_calc_ram_clamp() {
        for &ram_mb in &[4usize, 8, 16, 64, 256, 1024] {
            let r = ram_mb * MB;
            assert_eq!(reserve_calc_ram(r, 0), r, "clamp 0 changed {ram_mb}MB");
        }
        for &(ram_mb, exp_mb) in &[(2, 2), (4, 4), (8, 4), (64, 4), (256, 4)] {
            assert_eq!(reserve_calc_ram(ram_mb * MB, 4), exp_mb * MB, "clamp 4, {ram_mb}MB RAM");
        }
    }

    /// End to end: on a plausible small-RAM box, the three regions tile `ram_base..ram_end`
    /// exactly, the code+stack reserve covers the boot stack, and both the heap and the user-page
    /// pool are non-empty.
    #[test]
    fn memory_layout_tiles_ram_and_covers_boot_stack() {
        let ram_base = 0x4000_0000usize;
        let boot_stack_top = ram_base + 2 * MB; // plausible: kernel image + boot stack
        for &ram_mb in &[16usize, 32, 64, 128, 256, 1024] {
            let ram = ram_mb * MB;
            let l = compute_memory_layout(ram_base, ram, boot_stack_top, 4 * MB, MB, 0, 0);
            let ram_end = ram_base + ram;
            assert_eq!(l.heap_start, ram_base + l.code_and_stack, "{ram_mb}MB");
            assert_eq!(l.user_pages_start, l.heap_start + l.heap_size, "{ram_mb}MB");
            assert!(l.user_pages_start + l.user_pages_size <= ram_end, "{ram_mb}MB overruns RAM");
            assert!(boot_stack_top <= l.heap_start, "{ram_mb}MB heap overlaps boot stack");
            assert!(l.heap_size > 0, "{ram_mb}MB empty heap");
            assert!(l.user_pages_size > 0, "{ram_mb}MB empty user-page pool");
        }
    }

    /// `compute_thread_limit`: large user-pages gets the full `max_threads`; small user-pages
    /// stays within `[reserved+6, max_threads]` and the stack pool fits in 1/4 of the user pages
    /// unless already at the `reserved+6` floor.
    #[test]
    fn thread_limit_scales_with_user_pages() {
        let (reserved, maxt, sys_stack, user_stack) = (8usize, 64usize, 64 * 1024usize, 64 * 1024usize);
        let sys_total = reserved.saturating_sub(1) * sys_stack;
        assert_eq!(
            compute_thread_limit(1024 * MB, reserved, sys_stack, user_stack, maxt, 0),
            maxt,
            "large RAM should hit MAX_THREADS"
        );
        for &up_mb in &[4usize, 8, 16, 32, 64, 128] {
            let up = up_mb * MB;
            let n = compute_thread_limit(up, reserved, sys_stack, user_stack, maxt, 0);
            assert!(n >= reserved + 6 && n <= maxt, "{up_mb}MB userpages -> {n} threads");
            let pool = sys_total + (n - reserved) * user_stack;
            if n > reserved + 6 {
                assert!(pool <= up / 4, "{up_mb}MB userpages -> pool {pool} > quarter {}", up / 4);
            }
        }
    }

    #[test]
    fn thread_limit_manual_override_wins() {
        assert_eq!(compute_thread_limit(4 * MB, 8, 64 * 1024, 64 * 1024, 64, 23), 23);
    }

    #[test]
    fn shortfall_reports_nothing_when_met() {
        let layout = MemoryLayout {
            code_and_stack: MB,
            heap_start: MB,
            heap_size: 32 * MB,
            user_pages_start: 33 * MB,
            user_pages_size: 64 * MB,
        };
        assert!(shortfall(&layout, 32, &TOKIO_MIN).is_none());
    }

    #[test]
    fn shortfall_reports_each_short_dimension() {
        let layout = MemoryLayout {
            code_and_stack: MB,
            heap_start: MB,
            heap_size: 8 * MB,
            user_pages_start: 9 * MB,
            user_pages_size: 8 * MB,
        };
        let s = shortfall(&layout, 4, &TOKIO_MIN).expect("should be short");
        assert_eq!(s.heap_short_bytes, 8 * MB);
        assert_eq!(s.user_pages_short_bytes, 24 * MB);
        assert_eq!(s.threads_short, 12);
    }

    #[test]
    fn java_min_is_unset_not_a_guess() {
        assert!(JAVA_MIN.is_none());
    }
}
