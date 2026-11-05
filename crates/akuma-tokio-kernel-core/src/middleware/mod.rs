//! Syscall middleware: one optional table of hooks around the kernel's syscall dispatch.
//!
//! The guide is `docs/middleware.md` in this crate; the design record is `docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md`.
//! Off unless the `middleware` feature is on; a kernel that
//! does not ask for it compiles none of this and pays nothing.
//!
//! # The shape
//!
//! A kernel registers **one** [`Middleware`] (a `static`). It is a flat table, not a stack: every hook is a
//! plain `fn`, there is no `next`, no priority and no registration order, and two entries for the same call in
//! the same tier are refused when the table is armed. Someone who wants layers writes a function that calls
//! their own functions and registers that; the framework gives them nothing to make it feel supported.
//!
//! | tier | runs | sees | may |
//! |---|---|---|---|
//! | `kernel_pre`  | inside the big kernel lock, before the call | [`KernelCtx`] (read-only): args, caller's memory, tid, pid | `Pass`, `Return`, `Errno` |
//! | `fallback`    | inside the lock, only where the answer is `-ENOSYS` | [`KernelCtx`] | supply the answer (a *new* syscall) |
//! | `kernel_post` | inside the lock, after the call | [`KernelCtx`] + the result | replace the result |
//! | `own_pre` / `own_post` | **outside** the lock, around it | [`OwnCtx`]: plain integers + the middleware's private state | observe only |
//! | `exec` (one policy) | wherever code reaches a process: an image (`load_task`), a `PROT_EXEC` `mmap`, a `PROT_EXEC` `mprotect` | [`ExecEvent`] | [`ExecVerdict::Allow`] or `Deny(errno)` |
//!
//! A hook never sees a lock. A `kernel` hook runs under the kernel's own lock, taken by the kernel. An `own` hook
//! is serialised by an *autolock* the framework owns (a spinlock with preemption off, held for exactly the hook
//! call, never while the big lock is taken). An `own` hook therefore must be short, must not block, allocate or
//! print, and gets **no user memory**: the page-table walk behind a user copy races a sibling thread's `munmap`
//! when the big lock is not held, which is why anything that reads the caller's memory is a `kernel` hook.
//!
//! # What it costs
//!
//! Which numbers a tier cares about is decided once, when the table is armed: a 512-bit mask per tier. A call
//! nobody hooked costs one relaxed load and a bit test per tier. A hook may return [`Verdict::PassAndForget`] to
//! clear its own bit for that number. The mask is read **once** per call and latched ([`Latch`]), so a pre-hook
//! that prunes itself cannot make its own post-hook vanish. Numbers from 512 up are always asked.
//!
//! # Not interceptable
//!
//! `exit`, `exit_group`, `clone`, `execve` and `rt_sigreturn` rewrite the frame or never return, so a hook sees them
//! ([`observe_only`]) but cannot serve them with a `fallback` or replace their result. A `kernel_pre` *refusal*
//! (`Return`/`Errno`) is different: it skips the call, so it cannot corrupt the frame. It is honoured for `execve`
//! ([`may_refuse_before`]) and ignored for `exit`, `exit_group`, `clone` and `rt_sigreturn`, whose callers do not
//! expect a return value (`exit`) or are left half-done (`rt_sigreturn`).
//!
//! # `#![forbid(unsafe_code)]`
//!
//! This module has none and the API needs none. Raw access means patching the attribute out of your own crate.

mod calls;
mod ctx;
mod dispatch;
mod exec;
mod table;
#[cfg(test)]
mod tests;

pub use akuma_syscalls_abi::Syscall;
pub use crate::params::{self, ParamSource, Params};
pub use calls::{observe_only, may_refuse_before, Machine, On, Verdict, CUSTOM_BASE, ENOSYS_RET, MASK_BITS};
pub use ctx::{Fallback, KernelCtx, KernelPost, KernelPre, OwnCtx, OwnPost, OwnPre, UserError};
pub use dispatch::{active, init, install, own_enter, own_leave, run_kernel, Dispatch};
pub use exec::{exec_event, exec_sidecar, exec_verdict, ExecEvent, ExecKind, ExecPolicy, ExecVerdict};
pub use table::{Hooks, InitHook, Latch, Middleware, Route};
