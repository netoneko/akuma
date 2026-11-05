# Syscall middleware: a guide

**Stability: B** (written 2026-11-05; the behaviour is booted on both machines under QEMU TCG, the cost of having it on is not
measured, and the pieces in "Not built" are not there). The incident-style record, with what was argued and what was
found by booting, is [`docs/archive/TAOXICHUAN_B22_SYSCALL_MIDDLEWARE.md`](../../../docs/archive/TAOXICHUAN_B22_SYSCALL_MIDDLEWARE.md);
the code is [`src/middleware.rs`](../src/middleware.rs); a working table is
[`shanghai/src/middleware.rs`](../../../shanghai/src/middleware.rs).

Middleware lets a kernel built on `akuma-tokio-kernel` answer a syscall of its own, watch one, change one, or refuse one,
without touching the machine halves' dispatch. It is **off unless the `middleware` cargo feature is on**, for every kernel;
with it off none of it is compiled and nothing in dispatch changes.

## Your first syscall (five minutes)

1. Turn the feature on in your kernel's `Cargo.toml` and make it optional the way Shanghai does:

   ```toml
   [features]
   middleware = ["akuma-tokio-kernel/middleware", "dep:akuma-primitives"]   # akuma-primitives only for safe_print!

   [dependencies]
   akuma-primitives = { path = "../crates/akuma-primitives", optional = true }
   ```

2. Write the table in a module of your kernel crate (`src/middleware.rs`):

   ```rust
   #![forbid(unsafe_code)]
   use akuma_tokio_kernel::middleware::{CUSTOM_BASE, Hooks, Middleware, On, Route};

   pub static MW: Middleware<()> = Middleware::new((), Hooks {
       // 1000 is above every number Linux assigns. This runs only where the kernel would have said -ENOSYS.
       fallback: &[Route::new(&[On::Nr(CUSTOM_BASE)], |_ctx| Some(42))],
       ..Hooks::new()
   });
   ```

3. Register it, once, in `main`:

   ```rust
   #[cfg(feature = "middleware")]
   let kernel = kernel.middleware(&middleware::MW);
   ```

4. From the guest, `syscall(1000)` returns 42. Everything else behaves as before.

To see it run end to end, `sh shanghai/mw_gate.sh [x86_64|aarch64|all]` (`SMP=4` for four cores) builds Shanghai with and
without the feature, boots each under QEMU TCG with [`shanghai/mwprobe`](../../../shanghai/mwprobe) as `init`, and checks the
probe's own result line. It needs only `e2fsprogs`, no sshd or key.

## The table

One `Middleware<S>` per kernel, a `static`: `Middleware::new(state, Hooks { .. })`. `Hooks` has five fields, each a slice of
`Route::new(&[On::..], hook)`; leave the ones you do not use at `..Hooks::new()`.

| tier | hook type | runs | it may |
|---|---|---|---|
| `kernel_pre` | `fn(&mut KernelCtx) -> Verdict` | inside the kernel lock, before the call | `Pass`, `PassAndForget`, `Return(v)`, `Errno(e)`; rewrite `ctx.args` |
| `fallback` | `fn(&mut KernelCtx) -> Option<u64>` | inside the lock, only where the answer is `-ENOSYS` | `Some(answer)` to serve the call, `None` to leave the `-ENOSYS` |
| `kernel_post` | `fn(&mut KernelCtx, u64) -> u64` | inside the lock, after the call | return the result to use (result in, result out) |
| `own_pre` | `fn(&mut OwnCtx<S>)` | **outside** the lock, before it | observe, and update `ctx.state` |
| `own_post` | `fn(&mut OwnCtx<S>, u64)` | outside the lock, after it | observe, and update `ctx.state` |

`On::Call(Syscall::Openat)` names a Linux call by identity (the machine maps it to its architecture's number);
`On::Nr(1000)` names a raw number, the same on both architectures (use it from `CUSTOM_BASE` = 1000 up).

**One table, flat, no chaining.** There is no `next`, no priority and no ordering. Two entries naming the same call in the same
tier panic when the table is armed at boot, with the number. A second `Kernel::middleware(..)` panics. To layer behaviours, write
a function that calls your own functions and register that; the framework gives you nothing to make that feel supported.

## Where it runs

```text
amd64    dispatch():  own_pre -> [take the kernel lock] -> legacy fold -> kernel_pre -> the call
                      -> fallback (if -ENOSYS) -> kernel_post -> signal delivery -> [release] -> own_post
aarch64  el0_sync():  own_pre -> [take the kernel lock] -> kernel_pre -> the call -> fallback -> kernel_post
                      -> signal delivery -> [release] -> own_post
```

- The `kernel` tier sees the **canonical** number on amd64: the legacy spellings (`mkdir`, `unlink`, ...) have already been
  folded into their `*at` call.
- The `own` tier sees the **raw** number the program issued, before any folding, and has no user memory (below).
- `kernel_pre` may rewrite `ctx.args`; the call and `kernel_post` see the new values.
- A `Return`/`Errno` from `kernel_pre` skips the call (and the fallback) but `kernel_post` still runs on that result.

## What a hook gets

`KernelCtx`: `nr` (native number), `call: Option<Syscall>`, `args: [u64; 6]` (rewritable), `tid`, `pid`, and checked access to the
**caller's** memory:

| method | does | error |
|---|---|---|
| `user_read(addr, &mut [u8])` / `user_write(addr, &[u8])` | copy in / out | `UserError::Fault` if a page does not translate |
| `user_read_u64(addr)` / `user_write_u64(addr, v)` | little-endian word | `Fault` |
| `user_str(addr, &mut [u8]) -> &str` | NUL-terminated UTF-8, read bytewise so a string that ends just before an unmapped page does not fault | `Fault`, `TooLong` (no NUL in your buffer), `NotUtf8` |

`UserError::errno()` gives the positive errno to hand back (`Verdict::Errno(e.errno())`). A bad user pointer is an error value,
never a crash: write `?`-style code and return `-EFAULT`.

`OwnCtx<S>`: `nr`, `call`, `args` (plain integers: **not pointers you may follow**), `tid`, and `state: &mut S`, your private state.

## The rules, and why each is there

- **No hook touches a lock.** `kernel_*` and `fallback` run under the kernel lock, which the kernel takes. `own_*` run outside it,
  with `state` behind an autolock the framework owns: a spinlock held with preemption off for exactly the hook call and never
  while the kernel lock is taken, so there is no lock-order question and nothing for a hook to name.
- **`own` hooks must be short, must not block, allocate or print, and get no user memory.** The walk behind a user copy races a
  sibling thread's `munmap`/`mprotect` (which run under the kernel lock) and could read a frame that now belongs to another
  process. Anything that reads the caller's memory is a `kernel` hook. If you need a lock-free counter, an atomic works from
  any tier.
- **`kernel` hooks run under the big kernel lock.** Do not spin on something another core's syscall holds, and do not try to
  park the caller: there is no sanctioned way to block yet (see "Not built").
- **Some calls are observed, never replaced:** `exit`, `exit_group`, `clone`, `execve`, `rt_sigreturn` rewrite the frame or do not
  return. For them a `Return`/`Errno` verdict, a fallback, or a changed `kernel_post` result is ignored (`observe_only`).
- **A result is a value or `-errno`.** A `kernel_post` that does arithmetic must leave an error alone:
  `if r.cast_signed() < 0 { r } else { r + 1000 }`. (The first Shanghai sample added 1000 to a `-ENOSYS` and returned 962.)
- **A hook that panics is a kernel panic.** Treat it as kernel code.
- **No `unsafe`.** `middleware.rs` is `#![forbid(unsafe_code)]` and the API needs none; put the same attribute at the top of your
  middleware module. Raw access means patching the attribute out of your own crate.
- **No runtime switch.** A production kernel does not get a command-line hatch to change what a syscall does (the same decision
  as the removed `wx=0`).

## What it costs, and the caches

Which numbers a tier wants is decided once, when the table is armed: a 512-bit mask per tier. A call nobody hooked costs one
relaxed load and a bit test per tier, and no hook runs. Numbers from 512 up are always asked (that is where custom calls live).
The mask is read **once per call** and latched, so a hook that prunes itself cannot make its own post-hook vanish. A
`kernel_pre` may return `Verdict::PassAndForget` to clear its own bit for that number for good. Verdicts themselves are never
cached: they depend on the arguments.

The feature-on cost against the syscall floor is **not measured**; do not quote a number for it.

## Testing a table

- **On the host** (no kernel): `Middleware::arm(machine)` with a fake `Machine` (`decode`, `encode`, `copy_in`, `copy_out`,
  `pid`), then `run_kernel`, `run_own_pre` and `run_own_post` directly. [`src/middleware.rs`'s `tests`](../src/middleware.rs) does
  exactly this for every rule above: `cargo test -p akuma-tokio-kernel-core --features middleware --target $(rustc -vV | sed -n
  's/host: //p')`.
- **In a guest:** copy Shanghai's pattern. Write a probe that makes raw `syscall` instructions (libc would hide an `-ENOSYS`
  behind `errno`), run it as `init`, and judge the boot by its result line. Always boot a **control** built without the feature
  too: it proves the calls your table touches are stock when it is off. `shanghai/mw_gate.sh` is the harness for both machines.
- **Contention:** the `own` autolock is only exercised by threads on more than one core. `mwprobe` runs 4 threads x 500 `getpid`
  at `SMP=4` against a hook that reads, spins and writes its state; a lost update would leave the state below the atomic count.

## Pitfalls found by booting it

- Struct-update syntax needs public fields: write `Middleware::new(state, Hooks { fallback: .., ..Hooks::new() })`, not a
  `Middleware { .. }` literal.
- Hook a call the kernel actually serves. `getuid` is only served with the `shell` feature; a table on it sees `-ENOSYS` and, if
  it is a `kernel_post`, adds to an error.
- On aarch64, QEMU's `-kernel` needs the flat `.bin`; when cargo reuses a cached ELF it does not relink and the `.bin` can be the
  other build's. Make it from the ELF (`sh scripts/mkbin.sh <elf> <bin>`), as the gate does
  (`docs/archive/LINK_KERNEL_BIN_LAYOUT.md`).

## Not built (yet)

A runtime `arm`/`disarm` handle for interest bits; a lock-protected state cell for `kernel` hooks (use atomics, or a `BorrowCell`
static, which the kernel lock serialises); typed `UserPtr<T>` (there are byte reads and `u64`/string helpers); a
framework-provided blocking wait for `kernel` hooks; `Return` from an `own` hook (signal delivery on the way out runs under the
kernel lock, and whether a cheap lock-free "signal pending" test exists has not been checked); a measured cost. Hosting
rump's interceptor would need the first four and is out of scope while rump is deferred.

## Background

[`docs/archive/TAOXICHUAN_B22_SYSCALL_MIDDLEWARE.md`](../../../docs/archive/TAOXICHUAN_B22_SYSCALL_MIDDLEWARE.md) (design, what was
verified, deviations), [`crates/akuma-tokio-kernel/README.md`](../../akuma-tokio-kernel/README.md) § Syscall middleware,
[`taoxichuan/docs/reference/syscalls.md`](../../../taoxichuan/docs/reference/syscalls.md) § Who answers, in order,
`crates/akuma-bkl/src/policy.rs` (the per-syscall bitmap and the latch-at-entry rule this follows).
