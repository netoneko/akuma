//! The per-slot machine record: what a switch installs on the CPU for one thread.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

use akuma_primitives::OnceCopy;
use akuma_primitives::cell::BorrowCell;

use crate::FxArea;

/// A thread's two stacks, as memory the kernel owns and the CPU also writes.
///
/// Words of `AtomicU64` rather than bytes: the stack is used by running code through the
/// stack pointer, which Rust cannot see, so the allocation is `UnsafeCell`-shaped from the
/// start - and that is what lets a canary at its base be read and written through a shared
/// reference with no `unsafe`. The stack proper is addressed by [`Stacks::kernel_base`] /
/// [`Stacks::trap_base`], which are only numbers handed to the context switch.
#[derive(Clone, Copy)]
pub struct Stacks {
    pub kernel: &'static [AtomicU64],
    pub trap: &'static [AtomicU64],
    size: usize,
}

impl Stacks {
    /// Two fresh zeroed stacks of `size` bytes, leaked: a thread's stack must outlive the
    /// frame that made it. Bounded by the slot count, because a slot is given its pair once.
    #[must_use]
    pub fn leak_new(size: usize) -> Self {
        fn one(size: usize) -> &'static [AtomicU64] {
            let words: Vec<AtomicU64> = (0..size / 8).map(|_| AtomicU64::new(0)).collect();
            // No alignment is promised beyond 8, and none is needed: the context builder
            // masks the entry slot to 16 (`x86_build_closure_context`) and the trap stack's
            // top is masked by the caller.
            words.leak()
        }
        Self { kernel: one(size), trap: one(size), size }
    }

    #[must_use]
    pub fn kernel_base(self) -> usize {
        self.kernel.as_ptr() as usize
    }

    #[must_use]
    pub fn trap_base(self) -> usize {
        self.trap.as_ptr() as usize
    }

    /// Bytes in each stack.
    #[must_use]
    pub fn size(self) -> usize {
        self.size
    }
}

/// The machine state `akuma-threading` deliberately knows nothing about, one entry per
/// crate thread slot. `U` is the kernel's own per-thread user-context record.
pub struct Machine<U> {
    /// Page-table root to install when this thread runs. `0` means "the kernel's", which is
    /// what every kernel thread uses.
    pub space_root: AtomicU64,
    /// The kernel's record of this thread's ring-3 state. A [`BorrowCell`] rather than a bag
    /// of atomics because the syscall assembly may address it by offset through
    /// [`BorrowCell::as_ptr`]; the cell cannot see that, and the kernel lock is what keeps
    /// the assembly and Rust off the same slot.
    pub uctx: BorrowCell<U>,
    /// Stack the CPU switches to when this thread traps from ring 3.
    pub trap_stack_top: AtomicU64,
    /// This slot's kernel stack and trap stack, once it has been given them. Kept so a
    /// **recycled** slot reuses the stacks it owns - re-leaking two stacks per recycle would
    /// turn a slot leak into a memory leak. Set once, never replaced.
    pub stacks: OnceCopy<Stacks>,
    /// This thread's FP/SIMD registers while it is not running.
    pub fx: BorrowCell<FxArea>,
    /// Runs for the life of the kernel and never finishes (a netpoll loop, a core's idle
    /// thread): "all user tasks finished" ignores these.
    pub daemon: AtomicBool,
    /// The thread that runs a core when nothing else will. Never chosen by the picker's scan;
    /// switched to explicitly, by its own core.
    pub idle: AtomicBool,
    /// The only core allowed to run this thread, or [`crate::NO_CPU`].
    pub pinned: AtomicU32,
    /// The cores this thread may use (bit `c` = core `c`), or 0 for any. Inherited by every
    /// slot this thread creates; the decision is `akuma_vm_spec::affinity::can_run`.
    pub affinity: AtomicU64,
}

impl<U> Machine<U> {
    /// A free slot's record, with `user` as its initial user context.
    #[must_use]
    pub const fn new(user: U) -> Self {
        Self {
            space_root: AtomicU64::new(0),
            uctx: BorrowCell::new(user),
            trap_stack_top: AtomicU64::new(0),
            stacks: OnceCopy::new(),
            fx: BorrowCell::new(FxArea::initial()),
            daemon: AtomicBool::new(false),
            idle: AtomicBool::new(false),
            pinned: AtomicU32::new(crate::NO_CPU),
            affinity: AtomicU64::new(0),
        }
    }
}
