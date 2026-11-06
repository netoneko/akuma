//! Ring 3 for the minimal amd64 composition (taoxichuan stage 2).
//!
//! Everything a kernel needs to build a task's initial user stack and cross
//! the ring boundary in both directions —
//! extracted as a crate rather than grown inline in `taoxichuan/`, per the
//! experiment's composition rule. The full kernel's counterparts live in
//! `amd64/src/usermode.rs` (9 k lines: demand paging, signals, threads, an
//! interpreter loader) and `amd64/src/loader.rs` (over `akuma-elf` +
//! `akuma-mmu`); none of that serves a task that only prints and exits, and
//! pulling it would pull the whole MMU. What is here is the irreducible core:
//!
//! (The `PT_LOAD` walk that was here moved to `akuma-elf-exec`'s `program`
//! in B8, 2026-11-02: Taoxichuan places program and interpreter through one
//! parser.)
//! - [`build_stack`]: argc/argv/envp/auxv laid out the way Linux does, with
//!   the auxv entries static musl reads at startup.
//! - [`enable_syscalls`] / [`SYSCALL_ENTRY`]: `EFER.SCE` + `STAR`/`LSTAR`/
//!   `SFMASK`, and the naked stub the CPU lands on for every `syscall` —
//!   switch to a kernel stack, build a [`SyscallFrame`], call the kernel's
//!   dispatcher, [`sysretq`](enter_user) back.
//! - [`enter_user`]: the first crossing in, by `sysretq`.
//!
//! # What is deliberately NOT here
//!
//! No GS handling: neither the stub nor the kernel keeps a per-CPU block, so
//! there is nothing to `swapgs` for (the IDT stubs in `akuma-trap-amd64` do
//! swap, and stay correct: swapping a pair of zero bases changes nothing).
//! No return path other than `sysretq` from the stub itself — a task that
//! faults dies in the kernel's trap handler, it does not come back here.
//!
//! # The syscall contract
//!
//! `syscall` clears `IF` itself, so the stub runs with interrupts masked until
//! `sysretq` restores `RFLAGS` from `R11`. The dispatcher receives a
//! [`SyscallFrame`] (the number, the six argument registers in Linux order —
//! `R10` carries the fourth argument, not `RCX` — and the user `RIP`/`RFLAGS`
//! for anything that must look at them) and returns the `RAX` value, negative
//! errno on failure, by Linux convention.

#![no_std]
// Same allow as taoxichuan and akuma-boot-amd64: several entry-point docs open
// with a long paragraph on purpose (the why lives in the first block).
#![allow(clippy::too_long_first_doc_paragraph)]

#[cfg(target_arch = "x86_64")]
use akuma_cpu::msr::{self, Msr};

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// The initial user stack: argc/argv/envp/auxv, the Linux layout.
// ---------------------------------------------------------------------------

// Moved to `akuma_elf_exec::stack` (B21 A5.6): the layout is the Linux ABI's, the
// same on both architectures, and the aarch64 tokio kernel builds the same stack.
pub use akuma_elf_exec::stack::*;

// ---------------------------------------------------------------------------
// The syscall machinery: MSRs, the stub, the frames it builds.
// ---------------------------------------------------------------------------

/// One kernel stack for the syscall path. One task, one core, interrupts off
/// for the whole excursion — a single static is the honest shape, and the
/// crate owns it so the stub's `mov rsp` cannot drift from the stack it names.
///
/// 32 KiB, sized by the depth-2 exec measurement (2026-10-27, session U):
/// the taoxichuan run-to-completion crossing lowers the cursor once per
/// level — spawn (STACK/2), fork (STACK/4), exec (`exec_replace`'s
/// `rsp_here - 0x1000`) — and at 16 KiB the exec'd image's band landed
/// ~0x400 BELOW the region bottom (T-0x2000 shell → T-0x3000 fork child →
/// T-0x4400 exec < bottom T-0x4000), so every exec from a fork child
/// dispatched in the statics under this array: prints went silent, then
/// garbled (`[o] open flags=0x?`), then a ring-0 `#GP` in `core::fmt::write`
/// (formatter pointer read from the clobbered neighborhood), then the
/// session-teardown poisoning. At 32 KiB the same chain bottoms out at
/// T-0x4400 of T-0x8000 — inside, with ~0x3C00 of room under the band.
/// deeper still. At 64 KiB the whole M2 depth-2 exec chain (E1 exec at
/// T-0x4600, E2 exec inside the fork child at T-0x7A00, the exec'd image's
/// syscall band dispatched at its lowered top) bottoms ~0x2C00 above the
/// floor: `spawn_arm` hands the child bands a top HALF the array down and
/// `fork_arm` a further QUARTER down, so the fixed ~0x5400 of exec/fork
/// lowering and frames must fit in SIZE/2 — at 32 KiB it did not (the E2
/// band dispatched ~0xA00 BELOW the floor, pushing zeroed user registers
/// into the read-only data under the array: a later indirect jump through
/// the clobbered jump tables fetched rip=0).
pub const SYSCALL_STACK_SIZE: usize = 64 * 1024;

#[repr(align(16))]
#[cfg(target_arch = "x86_64")]
struct SyscallStack(#[allow(dead_code)] [u8; SYSCALL_STACK_SIZE]);
#[cfg(target_arch = "x86_64")]
static SYSCALL_STACK: SyscallStack = SyscallStack([0; SYSCALL_STACK_SIZE]);

/// One core's block, reached through `%gs` in ring 0. **Layout is the contract
/// of the naked stub and of `akuma_cpu::percpu`**: `gs:[8]` the user `RSP`
/// stash, `gs:[16]` the syscall stack top, `gs:[24]` the core index (u32),
/// `gs:[32]` the running slot. Replaces the two single-core statics
/// (`USER_RSP_STASH`, `SYSCALL_STACK_TOP`) this crate used to keep.
///
/// Invariant (the amd64 kernel's, `amd64/src/smp.rs`): in ring 0 `GS_BASE` is
/// this core's block and `KERNEL_GS_BASE` is the program's; the stub, the
/// IDT stubs and [`enter_user`] `swapgs` at every ring crossing.
#[cfg(target_arch = "x86_64")]
#[repr(C, align(64))]
pub struct PerCpu {
    self_ptr: core::sync::atomic::AtomicU64,
    user_rsp: core::sync::atomic::AtomicU64,
    syscall_top: core::sync::atomic::AtomicU64,
    index: core::sync::atomic::AtomicU32,
    lapic_id: core::sync::atomic::AtomicU32,
    slot: core::sync::atomic::AtomicU64,
}

#[cfg(target_arch = "x86_64")]
const _: () = {
    assert!(core::mem::offset_of!(PerCpu, self_ptr) == 0);
    assert!(core::mem::offset_of!(PerCpu, user_rsp) == 8);
    assert!(core::mem::offset_of!(PerCpu, syscall_top) == 16);
    assert!(core::mem::offset_of!(PerCpu, index) == 24);
    assert!(core::mem::offset_of!(PerCpu, slot) == 32);
};

/// Most cores the per-CPU table holds.
pub const MAX_CPUS: usize = 16;

#[cfg(target_arch = "x86_64")]
impl PerCpu {
    const fn new() -> Self {
        Self {
            self_ptr: core::sync::atomic::AtomicU64::new(0),
            user_rsp: core::sync::atomic::AtomicU64::new(0),
            syscall_top: core::sync::atomic::AtomicU64::new(0),
            index: core::sync::atomic::AtomicU32::new(0),
            lapic_id: core::sync::atomic::AtomicU32::new(0),
            slot: core::sync::atomic::AtomicU64::new(0),
        }
    }
}

#[cfg(target_arch = "x86_64")]
static PERCPU: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

/// Point THIS core's `%gs` at `PERCPU[idx]` (and its program half at 0).
/// Call once per core before any [`user_rsp_stash`]-family accessor, with
/// interrupts off. Returns false for an `idx` past [`MAX_CPUS`].
#[cfg(target_arch = "x86_64")]
pub fn install_percpu(idx: usize, lapic_id: u32) -> bool {
    let Some(block) = PERCPU.get(idx) else { return false };
    let addr = core::ptr::from_ref::<PerCpu>(block) as u64;
    block.self_ptr.store(addr, core::sync::atomic::Ordering::Relaxed);
    block.index.store(idx as u32, core::sync::atomic::Ordering::Relaxed);
    block.lapic_id.store(lapic_id, core::sync::atomic::Ordering::Relaxed);
    // SAFETY: GS_BASE is this core's per-CPU base: a `static`, valid for the
    // kernel's life; nothing reads `gs:` before this returns.
    unsafe { msr::write(Msr::GsBase, addr) };
    let _ = msr::set_user_gs_base(0);
    true
}

/// This core's index (`gs:[24]`).
#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn cpu_index() -> usize {
    akuma_cpu::percpu::core_id() as usize
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn this_cpu() -> &'static PerCpu {
    &PERCPU[cpu_index()]
}

/// Set core `idx`'s running slot from ANOTHER core (the BSP presets an AP's idle
/// slot before it starts). Not for a core's own bookkeeping: use [`set_cpu_slot`].
#[cfg(target_arch = "x86_64")]
pub fn preset_cpu_slot(idx: usize, slot: u64) {
    if let Some(b) = PERCPU.get(idx) {
        b.slot.store(slot, core::sync::atomic::Ordering::Relaxed);
    }
}

/// The slot this core runs now (`gs:[32]`).
#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn cpu_slot() -> u64 {
    this_cpu().slot.load(core::sync::atomic::Ordering::Relaxed)
}

/// Record the slot this core runs now.
#[cfg(target_arch = "x86_64")]
pub fn set_cpu_slot(v: u64) {
    this_cpu().slot.store(v, core::sync::atomic::Ordering::Relaxed);
}

/// The kernel's dispatcher, stored as an address so the naked stub can `call`
/// it through memory (a naked function has no Rust-level names to reach).
#[cfg(target_arch = "x86_64")]
static DISPATCH: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The frame the stub builds on the kernel stack, one field per pushed
/// register in **memory order** — `syscall` pushes `r10`, `r8`, `r9` in that
/// sequence, so the *lowest* address holds the last push: `r9` sits at +48,
/// `r10` at +64. The offsets are load-bearing on both sides of the `call` —
/// the stub indexes `[rsp + N]` by hand, and `#[repr(C)]` fixes the Rust
/// side to match. `user_rip`/`user_rflags` are what `syscall` stashed in
/// `RCX`/`R11`. (Stage 2 wrote this struct in *push* order — `r10, r8, r9`
/// — which silently swapped args 4 and 6 for every syscall with more than
/// three arguments; nothing until stage 3's `openat`/`mmap` had that many,
/// so the pinned offset tests, which never named these three fields, stayed
/// green while the ABI was wrong.)
#[repr(C)]
pub struct SyscallFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    /// Sixth argument (`R9`), at the slot the stub's last of the three
    /// pushes occupies.
    pub r9: u64,
    /// Fifth argument (`R8`).
    pub r8: u64,
    /// Fourth argument. `syscall` takes it in `R10` because `RCX` carries
    /// the return address.
    pub r10: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    /// The syscall number on the way in; the stub stores the dispatcher's
    /// return value here before restoring, so the same slot carries the
    /// initial `RAX` on first entry.
    pub rax: u64,
    /// The interrupted user `RIP`.
    pub user_rip: u64,
    /// The interrupted user `RFLAGS`.
    pub user_rflags: u64,
}

/// The register file for the *first* crossing into ring 3 — what
/// [`enter_user`] loads. Distinct from [`SyscallFrame`]: that one is built by
/// the CPU's `syscall` contract; this one is built by the kernel, and names
/// the entry point, flags and stack explicitly.
#[repr(C)]
pub struct EntryFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r10: u64,
    pub r8: u64,
    pub r9: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rax: u64,
    /// Where `sysretq` jumps: the image entry point.
    pub rip: u64,
    /// What `sysretq` restores: `IF` set, reserved bit 1 set.
    pub rflags: u64,
    /// The stack `build_stack` returned.
    pub rsp: u64,
}

/// The dispatcher's type: inspect the frame, return the `RAX` value.
pub type Dispatcher = fn(&mut SyscallFrame) -> u64;

// -- Nested-task knobs (taoxichuan stage 4) --------------------------------
//
// The minimal kernel's stage-4 SPAWN runs a child to completion *inside* the
// parent's syscall. During that nested run the child's own `syscall`
// crossings use the same two statics the stub uses — the parked user `RSP`
// and the kernel stack top — and both must be redirected for the child and
// restored for the parent. The statics stay private; these accessors are the
// whole of the new surface, and they are plain loads/stores on memory the
// stub itself treats as single-core, `IF`-off scratch.

/// The user `RSP` the stub stashed for the task currently in the kernel.
#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn user_rsp_stash() -> u64 {
    // Relaxed: single core, `IF` clear inside the syscall path — the same
    // discipline that owns every write to this static (the stub's own asm
    // included). The ordering carries no inter-core promise on purpose.
    this_cpu().user_rsp.load(core::sync::atomic::Ordering::Relaxed)
}

/// Redirect the stub's user-`RSP` stash (see [`user_rsp_stash`]).
#[cfg(target_arch = "x86_64")]
pub fn set_user_rsp_stash(v: u64) {
    // Relaxed: as `user_rsp_stash` — single-core scratch, no ordering promise.
    this_cpu().user_rsp.store(v, core::sync::atomic::Ordering::Relaxed);
}

/// The stack top the stub switches to on the next `syscall`.
#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn syscall_stack_top() -> u64 {
    this_cpu().syscall_top.load(core::sync::atomic::Ordering::Relaxed)
}

/// Point the stub's kernel stack at a lower region for a nested run. The
/// caller owns the arithmetic: the value must leave the region between the
/// stack's base and the current top untouched (the live dispatch frames).
#[cfg(target_arch = "x86_64")]
pub fn set_syscall_stack_top(v: u64) {
    this_cpu().syscall_top.store(v, core::sync::atomic::Ordering::Relaxed);
}

/// Install `dispatcher` and point `EFER.SCE`/`STAR`/`LSTAR`/`SFMASK` at the
/// stub. Call once, before the first ring-3 entry.
#[cfg(target_arch = "x86_64")]
pub fn enable_syscalls(dispatcher: Dispatcher) {
    DISPATCH.store(dispatcher as usize, core::sync::atomic::Ordering::Relaxed);
    enable_syscalls_this_core();
}

/// The per-core half of [`enable_syscalls`]: this core's `EFER.SCE`, `STAR`,
/// `LSTAR`, `SFMASK`, and its syscall stack top. An AP calls it after
/// [`install_percpu`]; the dispatcher is global and already stored.
#[cfg(target_arch = "x86_64")]
pub fn enable_syscalls_this_core() {
    // The BSP's block: callers that never ran `install_percpu` still get one.
    if msr::read(Msr::GsBase) == 0 {
        let _ = install_percpu(0, 0);
    }
    // Only the BSP has the static boot band; an AP gets its stack top from
    // the first task it switches to (`set_syscall_stack_top`).
    if cpu_index() == 0 {
        this_cpu().syscall_top.store(
            (&raw const SYSCALL_STACK) as usize as u64 + SYSCALL_STACK_SIZE as u64,
            core::sync::atomic::Ordering::Relaxed,
        );
    }
    // SCE: the `syscall` instruction itself. Without it, `syscall` is `#GP`.
    let efer = msr::read(Msr::Efer) | 1;
    // SAFETY: the documented contract of each MSR write; the values are this
    // table's selectors (via `star()`), this stub, and a fixed mask.
    unsafe {
        msr::write(Msr::Efer, efer);
        msr::write(Msr::Star, akuma_gdt::star());
        msr::write(Msr::Lstar, syscall_entry as *const () as usize as u64);
        // Bits cleared on entry, over and above what `syscall` already does
        // (it clears `IF` unconditionally): `TF` (no single-step across the
        // boundary), `DF` (musl's `rep movsb` leaves it set; a kernel that
        // inherits `DF=1` faults in its own `memcpy`), `IOPL`/`NT`/`AC`
        // (ring 0 owns all three).
        msr::write(Msr::Fmask, 0x0004_7700);
    }
}

impl SyscallFrame {
    /// First argument (`RDI`).
    #[must_use]
    pub fn arg1(&self) -> u64 {
        self.rdi
    }
    /// Second argument (`RSI`).
    #[must_use]
    pub fn arg2(&self) -> u64 {
        self.rsi
    }
    /// Third argument (`RDX`).
    #[must_use]
    pub fn arg3(&self) -> u64 {
        self.rdx
    }
    /// Fourth argument (`R10`).
    #[must_use]
    pub fn arg4(&self) -> u64 {
        self.r10
    }
    /// Fifth argument (`R8`).
    #[must_use]
    pub fn arg5(&self) -> u64 {
        self.r8
    }
    /// Sixth argument (`R9`).
    #[must_use]
    pub fn arg6(&self) -> u64 {
        self.r9
    }
}

/// The `syscall` landing point. See [`enable_syscalls`] for the setup
/// contract. `no_mangle`d so the kernel can name the symbol for `LSTAR`.
///
/// # Safety
/// Not callable as a function: the CPU enters here, with `IF` clear, user
/// state in `RAX`/`RCX`/`R11`/`RDI`/`RSI`/`RDX`/`R10`/`R8`/`R9` and the
/// user's `RSP` still current.
#[doc(hidden)]
#[unsafe(no_mangle)]
#[unsafe(naked)]
#[cfg(target_arch = "x86_64")]
pub extern "C" fn syscall_entry() {
    core::arch::naked_asm!(
        // In: RAX = nr, RCX = user RIP, R11 = user RFLAGS, RSP = user RSP
        // (not switched), args in RDI RSI RDX R10 R8 R9. `IF` is clear.
        // `swapgs`: ring 0's `%gs` is this core's per-CPU block from here.
        "swapgs",
        "mov qword ptr gs:[8], rsp",
        "mov rsp, qword ptr gs:[16]",
        // 15 pushes (120 bytes) on top of 8 bytes of alignment padding:
        // 16-aligned at the call, frame at [rsp] matching `SyscallFrame`.
        "sub rsp, 8",
        "push r11",
        "push rcx",
        "push rax",
        "push rdi",
        "push rsi",
        "push rdx",
        "push r10",
        "push r8",
        "push r9",
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov rdi, rsp",
        "call qword ptr [rip + {dispatch}]",
        // The result rides back in the frame's RAX slot.
        "mov [rsp + 96], rax",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "pop r9",
        "pop r8",
        "pop r10",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        // Now: [rsp] = saved RAX, [rsp+8] = user RIP, [rsp+16] = user RFLAGS.
        "mov rcx, [rsp + 8]",
        "mov r11, [rsp + 16]",
        "mov rax, [rsp]",
        // Back to the user stack — from the stash, not from the frame: the
        // user stack was never touched.
        "mov rsp, qword ptr gs:[8]",
        "swapgs",
        "sysretq",
        dispatch = sym DISPATCH,
    );
}

/// Enter ring 3: restore `f` into the registers and `sysretq`.
///
/// `sysretq` takes the target `RIP` from `RCX`, the target `RFLAGS` from
/// `R11`, and derives `CS`/`SS` from `IA32_STAR` — which is why this crate
/// depends on `akuma-gdt`: the selectors `sysretq` computes must name the user
/// rows, and the table's row order is that contract, stated at compile time.
/// `RCX`, `R11` and `RSP` are loaded last; they carry the target, not caller
/// state. This never returns: the task runs, and comes back only through the
/// stub or the kernel's trap handler.
///
/// # Safety
/// `f` must describe a ring-3-enterable state, and the active address space
/// must map both the kernel half (the CPU is in ring 0 until `sysretq` fires)
/// and the user half (it executes there afterwards). Naked on purpose: the
/// body is the crossing, and the pointer arrives in `RDI` per the C ABI.
#[unsafe(naked)]
#[cfg(target_arch = "x86_64")]
pub unsafe extern "C" fn enter_user(f: &EntryFrame) -> ! {
    core::arch::naked_asm!(
        // The frame pointer is RDI; every load is spent against it, and the
        // frame's own R10 is restored after the pointer's last use.
        "mov rax, [rdi + 0x60]",
        "mov rdx, [rdi + 0x48]",
        "mov rsi, [rdi + 0x50]",
        "mov rbp, [rdi + 0x20]",
        "mov r8,  [rdi + 0x38]",
        "mov r9,  [rdi + 0x40]",
        "mov rbx, [rdi + 0x28]",
        "mov r12, [rdi + 0x18]",
        "mov r13, [rdi + 0x10]",
        "mov r14, [rdi + 0x08]",
        "mov r15, [rdi + 0x00]",
        "mov rcx, [rdi + 0x68]", // rip: sysretq's target
        "mov r11, [rdi + 0x70]", // rflags: sysretq's RFLAGS
        "mov r10, [rdi + 0x30]",
        "mov rsp, [rdi + 0x78]", // rsp: the user stack (rdi still points at the frame)
        "mov rdi, [rdi + 0x58]", // last: spends the frame pointer
        "swapgs",                // ring 0 -> ring 3: GS_BASE becomes the program's
        "sysretq",
    );
}

/// [`enter_user`] after checking the parts of `f` that make a `sysretq` hostile to ring 0.
///
/// The safe entry for a caller that built `f` from an ELF image and a stack: `sysretq` with a **non-canonical
/// `RCX`** faults in ring 0 (the famous `sysret` privilege bug), a kernel-half `rip`/`rsp` would run or push in
/// the kernel's own address range, and an `rflags` with `IOPL`/`VM`/`NT` set or `IF` clear hands the task more
/// than ring 3 should have or hangs the core. So it refuses (panics) unless:
///
/// * `rip` and `rsp` are in the canonical **lower** half (`< 0x0000_8000_0000_0000`);
/// * `rflags` has `IF` and the reserved bit 1 set and `IOPL`, `NT`, `VM` clear.
///
/// The active address space still has to map the kernel half and the user half; the checked
/// `akuma_pagetable::amd64::activate` establishes the first for every root this kernel loads, and a user half
/// that does not map `rip` is an ordinary page fault in ring 3.
///
/// # Panics
/// If `f` fails the checks above.
#[cfg(target_arch = "x86_64")]
pub fn enter_user_checked(f: &EntryFrame) -> ! {
    const USER_LIMIT: u64 = 0x0000_8000_0000_0000;
    const IF: u64 = 1 << 9;
    const RESERVED1: u64 = 1 << 1;
    const FORBIDDEN: u64 = (3 << 12) | (1 << 14) | (1 << 17); // IOPL, NT, VM
    assert!(f.rip < USER_LIMIT && f.rsp < USER_LIMIT, "enter_user: rip/rsp not in the user half");
    assert!(
        f.rflags & (IF | RESERVED1) == (IF | RESERVED1) && f.rflags & FORBIDDEN == 0,
        "enter_user: unsafe rflags for ring 3"
    );
    // SAFETY: the checks above make `f` a ring-3-enterable state; the kernel-half mapping of the active root is
    // the loader's `activate` (checked), and `enter_user` is `!`.
    unsafe { enter_user(f) }
}

// ---------------------------------------------------------------------------
// The x86_64 Linux ABI surface the minimal dispatcher serves: numbers, errno,
// and the `arch_prctl` requests. (`akuma-syscalls-linux` is the AArch64/`asm-
// generic` numbering — `write` 64, `exit` 93 — and the amd64 ABI is a
// different table; the full kernel translates per arch, and so does this.)
// ---------------------------------------------------------------------------

/// The x86_64 syscall numbers `akuma-syscalls-abi` cannot hold: x86-only
/// calls with no asm-generic twin. (Akuma's private block went 2026-11-02:
/// Taoxichuan serves no private call; libakuma's `linux-abi` build reads its
/// uptime from `CLOCK_MONOTONIC`.) Every other
/// number is `akuma_syscalls_abi::x86_64::*`, one row per call with both
/// architectures' numbers and round-trip tests.
///
/// Until 2026-11-01 this was a second, 93-entry x86_64 table, and it drifted:
/// `GETPID = 38` (`setitimer`; getpid is 39) and `SENDFILE = 39`, plus the
/// misleading `SELECT_PSELECT6` and `PR_LIMIT64`
/// (`docs/archive/SYSCALL_CONSTANT_AUDIT_2026_10_30.md`).
pub mod nr {
    /// `arch_prctl(2)`: x86-only (musl's `__init_tp` sets the TLS base with it).
    pub const ARCH_PRCTL: u64 = 158;
    /// `fork(2)` and `vfork(2)`: x86-only. musl issues `vfork` as the raw
    /// number with no flags word.
    pub const FORK: u64 = 57;
    pub const VFORK: u64 = 58;
    /// `pause(2)`: x86-only (asm-generic musl spells it `ppoll(0, 0, 0, 0)`).
    pub const PAUSE: u64 = 34;
}

/// `openat(2)` flag bits (x86_64) and `dirfd` special values.
pub mod open {
    /// `O_RDONLY` — 0, the only access mode a read-only root used to serve.
    pub const O_RDONLY: u64 = 0;
    /// `O_ACCMODE` mask: `O_RDONLY` is 0 — the only access mode the
    /// read-only-in-spirit root serves.
    pub const O_ACCMODE: u64 = 0b11;
    /// `O_WRONLY` — served once the root grew a write path (stage 6).
    pub const O_WRONLY: u64 = 0b01;
    /// `O_RDWR`.
    pub const O_RDWR: u64 = 0b10;
    /// `O_CREAT` — create when missing (stage 6: the root writes).
    pub const O_CREAT: u64 = 0x40;
    /// `O_TRUNC` — empty an existing file on open.
    pub const O_TRUNC: u64 = 0x200;
    /// `O_DIRECTORY` — the caller wants a directory; anything else is
    /// `ENOTDIR`.
    pub const O_DIRECTORY: u64 = 0x1_0000;
    /// `O_CLOEXEC` — recorded on the fd, meaningless until an exec family
    /// exists.
    pub const O_CLOEXEC: u64 = 0x8_0000;
    /// `AT_FDCWD`: the only `dirfd` this kernel serves (absolute paths only).
    pub const AT_FDCWD: u64 = (-100i64) as u64;
    /// `AT_SYMLINK_NOFOLLOW` — `newfstatat`'s lstat mode.
    pub const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
    /// `AT_REMOVEDIR` — `unlinkat` removing a directory.
    pub const AT_REMOVEDIR: u64 = 0x200;
    /// `AT_EMPTY_PATH` — operate on the `dirfd` itself.
    pub const AT_EMPTY_PATH: u64 = 0x1000;
}

/// errno values, negated on return.
pub mod err {
    /// Operation not permitted.
    pub const EPERM: u64 = 1;
    /// No such file or directory.
    pub const ENOENT: u64 = 2;
    /// `ENOEXEC` — exec of a file that is not a runnable image.
    pub const ENOEXEC: u64 = 8;
    /// I/O error (the filesystem refused a read the dispatcher asked for).
    pub const EIO: u64 = 5;
    /// No such device (mmap of something that is not anonymous or a device
    /// this kernel serves).
    pub const ENODEV: u64 = 19;
    /// Not a directory (`O_DIRECTORY` opened on a file; `getdents64` on an fd
    /// that is not a directory).
    pub const ENOTDIR: u64 = 20;
    /// Is a directory (`read(2)` on a directory fd).
    pub const EISDIR: u64 = 21;
    /// Permission denied (`openat` with a write access mode on a root this
    /// kernel does not write).
    pub const EACCES: u64 = 13;
    /// Bad file descriptor.
    pub const EBADF: u64 = 9;
    /// Out of memory.
    pub const ENOMEM: u64 = 12;
    /// Too many open files (the fd table's fixed slot ceiling).
    pub const EMFILE: u64 = 24;
    /// Bad address.
    pub const EFAULT: u64 = 14;
    /// Invalid argument.
    pub const EINVAL: u64 = 22;
    /// Connection timed out (a dial that outlived its wait).
    pub const ETIMEDOUT: u64 = 110;
    /// Connection refused (the dial's socket went `Closed` before it was up).
    pub const ECONNREFUSED: u64 = 111;
    /// Inappropriate ioctl for device.
    pub const ENOTTY: u64 = 25;
    /// Function not implemented.
    pub const ENOSYS: u64 = 38;
    /// Address out of range (canonical forms): returned for a bad `arch_prctl`.
    pub const ERANGE: u64 = 34;
    /// Resource temporarily unavailable / operation would block (`O_NONBLOCK`
    /// on a socket with nothing to read or write; `try_accept` with an empty
    /// backlog).
    pub const EAGAIN: u64 = 11;
    /// No child processes (`waitpid` for a pid that is nobody's child).
    pub const ECHILD: u64 = 10;
    /// Address family not supported by the protocol (`socket(2)` for a
    /// family this stack does not serve — every family but `AF_INET`).
    pub const EAFNOSUPPORT: u64 = 47;
    /// Socket operation on a non-socket (an fd that is not a socket fd).
    pub const ENOTSOCK: u64 = 88;
    /// Transport endpoint is not connected (`write(2)`/`send(2)` on a UDP
    /// socket that never saw a `connect(2)` — stage 8, M2).
    pub const ENOTCONN: u64 = 107;
    /// Protocol not available (`setsockopt(2)` for an option this stack has
    /// no behaviour behind — stage 8, M2).
    pub const ENOPROTOOPT: u64 = 92;
    /// File exists (`O_CREAT`/`mkdirat` on a path that is already there).
    pub const EEXIST: u64 = 17;
    /// Illegal seek (`lseek` on a pipe/socket fd).
    pub const ESPIPE: u64 = 29;
    /// Directory not empty (`unlinkat AT_REMOVEDIR` on a full directory).
    pub const ENOTEMPTY: u64 = 39;
    /// No space left on the device.
    pub const ENOSPC: u64 = 28;
    /// Cross-device link (`rename`/`link` across mounts — one mount here).
    pub const EXDEV: u64 = 18;
}

/// `arch_prctl(2)` requests, x86_64.
pub mod arch {
    /// `ARCH_SET_FS` — set the FS base (the thread pointer on x86_64 musl).
    pub const ARCH_SET_FS: u64 = 0x1002;
    /// `ARCH_GET_FS` — read the FS base into `*addr`.
    pub const ARCH_GET_FS: u64 = 0x1003;
    /// `ARCH_SET_GS`.
    pub const ARCH_SET_GS: u64 = 0x1001;
    /// `ARCH_GET_GS`.
    pub const ARCH_GET_GS: u64 = 0x1004;
}

/// `mmap(2)` flags and `mprotect(2)` prots, x86_64 values.
pub mod mmap {
    /// `MAP_PRIVATE`.
    pub const MAP_PRIVATE: u64 = 0x02;
    /// `MAP_FIXED`.
    pub const MAP_FIXED: u64 = 0x10;
    /// `MAP_ANONYMOUS`.
    pub const MAP_ANONYMOUS: u64 = 0x20;
    /// `PROT_READ`.
    pub const PROT_READ: u64 = 0x1;
    /// `PROT_WRITE`.
    pub const PROT_WRITE: u64 = 0x2;
    /// `PROT_EXEC`.
    pub const PROT_EXEC: u64 = 0x4;
}
