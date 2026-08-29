// Unsafe-free by design, and `forbid` so no module can opt back in with a
// local `allow`. Same reasoning as `akuma-net-yarn` and `akuma-syscalls`, and
// spelled here rather than in Cargo.toml for the same reason: a crate-local
// `[lints]` table and `[lints] workspace = true` are mutually exclusive.
#![forbid(unsafe_code)]
#![no_std]
//! The futex family's pure logic.
//!
//! One syscall family, extracted on the `akuma-syscalls-time` model — the
//! "opportunistic family implementation" that `AKUMA_EXTRACT_SYSCALLS.md` §8
//! describes, chosen because `src/syscall/sync.rs` had the most pure logic per
//! line of anything left in `src/syscall/`.
//!
//! # Why this family, and why it was worth the move
//!
//! Not for size — the family is 1,131 lines and much of that is diagnostic
//! machinery that stays in the kernel. For **falsifiability**. Every bug this
//! family has produced was found by running a `-j4` rustc self-host build in
//! QEMU for several minutes and reading a wedged thread dump afterwards:
//!
//! | incident | what was actually wrong |
//! |---|---|
//! | `pthread_join` hangs forever | `futex_wake` only published to the `tgid=0` queue, so `FUTEX_WAIT_PRIVATE` waiters were never reached |
//! | `typenum` build stalls | a requeued waiter that left by timeout stranded its tid on the requeue target, where it absorbed a later wake |
//! | rustc "futex deadlock", worse the longer the VM had been up | `FUTEX_WAIT_BITSET`'s absolute deadline was treated as relative, so every Rust std timed wait slept ~2x uptime |
//! | a wake landing on a thread that was never waiting | a dead tid left queued by a thread killed while parked, then the slot was recycled |
//! | cross-process lost wakeups under `-j4` only | musl's `__thread_list_lock` is a fixed address with `priv = 0`, so with no ASLR every process shared one queue |
//!
//! Every one of those is a property of the queue algebra, the key namespace or
//! the deadline arithmetic — and every one is now a host test that runs in
//! milliseconds. That is the entire argument for this crate. The bugs it cannot
//! catch (the scheduler handshake, the in-hold user read) are exactly the ones
//! that stayed in the kernel.
//!
//! # The shape: decisions and one data structure
//!
//! Following `akuma-net-yarn` and `akuma-syscalls`: no `trait Effects`, no
//! generic effect parameter, no `dyn`. The kernel performs every effect —
//! taking the spinlock, masking IRQs, validating and reading user memory,
//! firing wakers, tracing — and calls pure functions in between. What moved is
//! the part that decides, plus [`table::WaiterTable`], which is a container
//! with no lock inside it.
//!
//! The one generic is [`table::WaiterId`], and it exists so this crate cannot
//! act on a thread even by accident: it can compare and find waiters, and has
//! no way to wake one.
//!
//! # What deliberately did not move
//!
//! - **The spinlock and the IRQ masking.** `FUTEX_WAITERS` is reachable from a
//!   BKL-free syscall window, so every access masks local IRQs to avoid an
//!   AB-BA against a nested IRQ that hard-spins for the BKL. That is a locking
//!   argument about *this kernel's* IRQ discipline, not about queue algebra.
//! - **The in-hold read of the futex word.** Doing it inside the hold is what
//!   makes enqueue atomic against a concurrent wake; doing it with `Prefault::No`
//!   is what stops it demand-paging under masked IRQs. Both are effects with a
//!   correctness argument attached, and both stay next to the lock they concern.
//! - **The diagnostic machinery** — the per-tid event ring, the bucketed wake
//!   ring, the orphan check. They read kernel-global state (thread states, the
//!   current syscall number, uptime) and exist to explain a wedge in a live VM.

extern crate alloc;

pub mod deadline;
pub mod key;
pub mod op;
pub mod table;
pub mod wakeop;
pub mod waitloop;

pub use deadline::{NEVER, REVALIDATE_US, deadline_us, expired, park_deadline_us};
pub use key::{Namespace, namespace};
pub use op::{Action, decode};
pub use table::{Key, Located, MATCH_ANY, WaiterId, WaiterTable};
pub use waitloop::{Step, step};
pub use wakeop::WakeOp;

#[cfg(test)]
mod tests;
