# Syscall middleware: a guide

**Stability: B** (written 2026-11-05; the behaviour is booted on both machines under QEMU TCG, the cost of having it on is not
measured, and the pieces in "Not built" are not there). The incident-style record, with what was argued and what was
found by booting, is [`docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md`](../../../docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md);
the code is [`src/middleware.rs`](../src/middleware.rs); a working table is
[`shanghai/src/middleware.rs`](../../../shanghai/src/middleware.rs).

Middleware lets a kernel built on `akuma-tokio-kernel` answer a syscall of its own, watch one, change one, or refuse one,
without touching the machine halves' dispatch. It is **off unless the `middleware` cargo feature is on**, for every kernel;
with it off none of it is compiled and nothing in dispatch changes.

Two worked examples follow, both from Shanghai and both booted by its gate: **Example 1** adds a syscall of your own, **Example 2** refuses an
exec by its environment.

## Example 1: add a syscall of your own (five minutes)

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

A syscall that takes a pointer reads or writes the caller's memory through the checked helpers and answers `-EFAULT` for a bad
address; it never trusts the pointer. This one (Shanghai's number 1001, `shanghai/src/middleware.rs`) writes the caller's pid
to the address in its first argument:

```rust
const EFAULT_RET: u64 = (-14i64) as u64;

Route::new(&[On::Nr(CUSTOM_BASE + 1)], |ctx| {
    Some(match ctx.user_write_u64(ctx.args[0], u64::from(ctx.pid)) {
        Ok(()) => 0,
        Err(_) => EFAULT_RET,
    })
}),
```

`syscall(1001, &mut v)` fills `v`, and `syscall(1001, 8)` returns `-14` instead of crashing the kernel: `mwprobe` checks both.

To see it run end to end, `sh shanghai/mw_gate.sh [x86_64|aarch64|all]` (`SMP=4` for four cores) builds Shanghai with and
without the feature, boots each under QEMU TCG with [`shanghai/mwprobe`](../../../shanghai/mwprobe) as `init`, and checks the
probe's own result line. It needs only `e2fsprogs`, no sshd or key.

## The table

One `Middleware<S>` per kernel, a `static`: `Middleware::new(state, Hooks { .. })`. `Hooks` has five fields, each a slice of
`Route::new(&[On::..], hook)`; leave the ones you do not use at `..Hooks::new()`.

| tier | hook type | runs | it may |
|---|---|---|---|
| `kernel_pre` | `fn(&KernelCtx) -> Verdict` | inside the kernel lock, before the call | `Pass`, `PassAndForget`, `Return(v)`, `Errno(e)` (a refusal: not for `exit`/`clone`/`rt_sigreturn`) |
| `fallback` | `fn(&KernelCtx) -> Option<u64>` | inside the lock, only where the answer is `-ENOSYS` | `Some(answer)` to serve the call, `None` to leave the `-ENOSYS` |
| `kernel_post` | `fn(&KernelCtx, u64) -> u64` | inside the lock, after the call | return the result to use (result in, result out) |
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
- A hook cannot change what the call sees: `KernelCtx` is read-only, and the call gets exactly the arguments the program passed.
- A `Return`/`Errno` from `kernel_pre` skips the call (and the fallback) but `kernel_post` still runs on that result.

## What a hook gets

`KernelCtx`: `nr` (native number), `call: Option<Syscall>`, `args: [u64; 6]` (read-only, as passed), `tid`, `pid`, and checked access to the
**caller's** memory:

| method | does | error |
|---|---|---|
| `user_read(addr, &mut [u8])` / `user_write(addr, &[u8])` | copy in / out | `UserError::Fault` if a page does not translate |
| `user_read_u64(addr)` / `user_write_u64(addr, v)` | little-endian word | `Fault` |
| `user_str(addr, &mut [u8]) -> &str` | NUL-terminated UTF-8, read bytewise so a string that ends just before an unmapped page does not fault | `Fault`, `TooLong` (no NUL in your buffer), `NotUtf8` |

`UserError::errno()` gives the positive errno to hand back (`Verdict::Errno(e.errno())`). A bad user pointer is an error value,
never a crash: write `?`-style code and return `-EFAULT`.

`OwnCtx<S>`: `nr`, `call`, `args` (plain integers: **not pointers you may follow**), `tid`, and `state: &mut S`, your private state.

## Example 2: refuse an exec by its environment

The goal: **refuse to start any program whose environment names an `LD_*` variable** (`LD_PRELOAD`, `LD_LIBRARY_PATH`,
`LD_AUDIT`, ...), the knobs the dynamic loader reads from the environment. It is the exec half of the "no dynamic loading"
demo in [`shanghai/src/nodl.rs`](../../../shanghai/src/nodl.rs); the code below is that file's, not a paraphrase.

**Where the data is.** A new process's environment is the third argument of `execve`: `ctx.args[2]` points at a
NULL-terminated array of pointers to `NAME=value` strings, all in the *caller's* memory, which is still the old image's when a
`kernel_pre` hook runs. So this is a `kernel_pre` hook (it needs user memory) on `Syscall::Execve`. It reads the pointer array
with `user_read_u64`, and for each string only the first three bytes, one at a time: a long value costs nothing, and a short
string at the end of a page cannot fault. The environment of a process that is already running is not reachable this way.

**Why it can refuse.** `execve` is observe-only: a hook cannot serve it with a `fallback` or replace its result. A
`kernel_pre` *refusal*, though, skips the call before it touches the process, so it is honoured for `execve`
(`may_refuse_before`; it stays ignored for `exit`, `exit_group`, `clone` and `rt_sigreturn`). A refused `execve` returns the
errno to the caller, which is still running its old image.

```rust
use akuma_tokio_kernel::middleware::{KernelCtx, UserError, Verdict};

const EPERM: i32 = 1;
const E2BIG: i32 = 7;
/// More entries than any real environment; past it the call is refused (fail closed).
const MAX_ENV: usize = 512;

/// Whether the NUL-terminated string at `s` starts with `LD_`. Bytewise and bounded to three bytes, so a long
/// string costs nothing and a short one at the end of a page does not fault.
fn starts_with_ld(ctx: &KernelCtx, s: u64) -> Result<bool, UserError> {
    let mut b = [0u8; 1];
    for (i, want) in (0u64..).zip(*b"LD_") {
        ctx.user_read(s + i, &mut b)?;
        if b[0] != want {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `execve(path, argv, envp)`: refuse a call whose `envp` has an `LD_*` entry. `envp` may be NULL (an empty
/// environment). A bad pointer or an over-long list is refused too: unknown is not allowed.
pub fn exec_guard(ctx: &KernelCtx) -> Verdict {
    let mut slot = ctx.args[2];
    if slot == 0 {
        return Verdict::Pass;
    }
    for _ in 0..MAX_ENV {
        let s = match ctx.user_read_u64(slot) {
            Ok(0) => return Verdict::Pass, // the NULL that ends the array
            Ok(s) => s,
            Err(e) => return Verdict::Errno(e.errno()),
        };
        match starts_with_ld(ctx, s) {
            Ok(false) => {}
            Ok(true) => {
                akuma_primitives::safe_print!(96, "[nodl] pid {} refused an execve with an LD_* variable\n", ctx.pid);
                return Verdict::Errno(EPERM);
            }
            Err(e) => return Verdict::Errno(e.errno()),
        }
        slot += 8;
    }
    Verdict::Errno(E2BIG)
}
```

Register it like any hook (Shanghai's table, `shanghai/src/middleware.rs`). In Shanghai the three guards are a **feature, off by
default**: `no-dynamic-loading` (it implies `middleware`), so the sample table alone does not have them:

```rust
kernel_pre: &[
    Route::new(&[On::Call(Syscall::Openat)], open_guard),
    #[cfg(feature = "no-dynamic-loading")]
    Route::new(&[On::Call(Syscall::Execve)], crate::nodl::exec_guard),
    // ... mmap_guard and mprotect_guard the same way
],
```

```text
cargo build -p shanghai --features no-dynamic-loading --target x86_64-unknown-none --release     # or aarch64-unknown-none
```

**Decisions in it, and why**

- **It refuses; it does not strip.** Dropping the `LD_*` entries and letting the call proceed would need a different environment
  array. Rewriting the caller's array in place is wrong (after `vfork` the parent shares that memory), and a hook has no scratch
  user memory to build a new one. An exec-specific hook that gets a mutable list, applied inside the exec path, is the clean
  way to strip, and it does not exist.
- **Unknown is refused.** A bad pointer is `EFAULT`, an over-long list is `E2BIG`, and both are returned instead of guessed
  past. A NULL `envp` is a valid empty environment and passes.
- **Bounded.** At most `MAX_ENV` entries and three bytes of each: the cost under the kernel lock is fixed.
- **Only at `execve`.** After exec a program can `setenv` what it likes; the dynamic loader reads its variables at startup, which
  is why exec is the right place.

**What the gate proves** (`mwprobe`, both machines at 4 cores, amd64 also at 1): an `execve` with `LD_PRELOAD=...` in its environment returns
`-EPERM`; an `execve` with only `PATH=/bin` is allowed and the new image runs (it prints the run's result line, which is how the
gate knows exec still works); the console shows the `[nodl] ... refused an execve with an LD_* variable` line; the control kernel,
built without the feature, has no such behaviour.

**Going further: the rest of the demo.** `nodl.rs` also has `mmap_guard` (refuse a file-backed `PROT_EXEC` mapping, which is
what `ld-musl` does for every library beyond libc, and `dlopen` does later) and `mprotect_guard` (refuse adding `PROT_EXEC`,
which would otherwise turn a read-only file mapping executable). `mwprobe` checks that each refusal fires and that the
neighbouring cases stay allowed: anonymous `PROT_EXEC` memory, a read-only file mapping, `mprotect(PROT_READ)`.

**What it does to real dynamic programs** (Alpine's `musl` loader, `busybox` and `xz`, fetched by `shanghai/mw_fetch_dyn.sh`; amd64
and aarch64 at 4 cores):

| child | stock kernel | with the three guards |
|---|---|---|
| `xz --version` (needs `liblzma.so.5`, a library besides libc) | prints `xz (XZ Utils) 5.8.4` | the loader fails: `Error loading shared library liblzma.so.5: Permission denied (needed by /usr/bin/xz)` |
| `busybox true` (dynamic, needs only libc) | runs | **still runs** (known gap, pinned by a probe case) |

What each child goes through, and where the guards can and cannot see it (the xz line is from the gate's console: the `[nodl]`
refusal under the child's pid, then the loader's error; the "opens the file" step is inferred, the log does not record opens):

```text
             xz --version  (needs liblzma.so.5)                   busybox true  (needs only libc)
             ─────────────────────────────────────                ──────────────────────────────────
mwprobe      fork + execve("/usr/bin/xz", env: empty)             fork + execve("/bin/busybox", env: empty)
             exec_guard: no LD_* in env -> allowed                exec_guard: no LD_* in env -> allowed
                          |                                                     |
                          v                                                     v
KERNEL       the kernel's own loader, inside execve (no syscall from the program involved):
 (its own    +- maps the main binary                              +- maps the main binary
  loader)    +- reads PT_INTERP, maps /lib/ld-musl-<arch>.so.1    +- reads PT_INTERP, maps /lib/ld-musl-<arch>.so.1
             +- starts the program at ld-musl's entry             +- starts the program at ld-musl's entry
                          |                                                     |
                          v                                                     v
ld-musl      reads the binary's list of needed libraries          reads the binary's list of needed libraries
(userspace,
 in the      1. libc.musl-<arch>.so.1                             1. libc.musl-<arch>.so.1
 child)         = ld-musl itself (a symlink to it)                   = ld-musl itself, already mapped
                -> already mapped, no syscall                        -> no syscall
                                                                  nothing else needed -> nothing left to map
             2. liblzma.so.5
                opens the file, then
                mmap(fd, PROT_READ|PROT_EXEC, MAP_PRIVATE)
                          |
                          v
MIDDLEWARE   kernel_pre on mmap: file-backed + PROT_EXEC          no executable file mapping is requested, so
             mmap_guard -> Errno(EACCES)                           mmap_guard and mprotect_guard never run for
             console: "[nodl] pid 9 refused a                      this process: nothing to refuse
             file-backed PROT_EXEC mmap"
                          |                                                     |
                          v                                                     v
RESULT       ld-musl: "Error loading shared library               runs `true`, exits 0
             liblzma.so.5: Permission denied" -> the child fails  (on the stock kernel both run)
```

The gap is structural: the kernel's own loader maps the interpreter and the main binary without a syscall, and `ld-musl` *is*
libc, so a program with no other dependency never asks for a second mapping. This is **not a wholesale ban**: it stops a program
loading libraries beyond libc and the `LD_*` knobs, nothing more.

**Children are bound, because the policy is the kernel's.** The probe checks four kinds: a forked child that never execs (a raw
`clone`), an exec'd static child in its own pid (the `[nodl]` lines in the console carry the child's pid, not 1), a child exec with
`LD_PRELOAD` (refused with `EPERM`, so the child never starts), and the two dynamic children above.

What is still outside it: a shebang script is data for a permitted interpreter; `memfd_create` was not checked; and a check by
number cannot tell a file mapping from an anonymous one in `mprotect`, so that guard is stricter than the mmap one on purpose.
A policy that must hold needs the verdict at the loader (a file becoming executable), not at the syscalls; this example is the
shape of the hook, not the enforcement.

**A lesson from writing it:** the first test subject was `bzip2`, and the probe passed it as "dynamic" because it ran under the
guards without a single refusal. It had never asked for a second library: Alpine builds libbz2 into it, so it needs only libc. Check
what a test subject actually loads (`strings -a <binary> | grep '^lib.*\.so'`) before trusting a result about it;
`mw_fetch_dyn.sh` now fails if `xz` does not name `liblzma` or if `busybox` needs more than libc.

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
  return. For them a `fallback` and a changed `kernel_post` result are ignored (`observe_only`). A `kernel_pre` refusal
  (`Return`/`Errno`) skips the call, so it cannot corrupt the frame: it is honoured for `execve` and ignored for `exit`,
  `exit_group`, `clone` and `rt_sigreturn` (`may_refuse_before`).
- **Hooks cannot mutate the kernel context.** Every `kernel` hook takes `&KernelCtx`, so a hook can allow, refuse or answer a
  call but never change what runs. Rewriting arguments is how one hook defeats another's check (a guard validates a path, a
  later hook swaps the pointer), and it would make combined hooks depend on their order. (`OwnCtx` has `&mut` for one thing: your
  own `state`.)
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
- **Children and real programs:** a policy about processes is only shown by processes. `mwprobe` forks (raw `clone`), execs a static
  child, and runs real dynamic binaries (`shanghai/mw_fetch_dyn.sh` fetches Alpine's loader, `busybox` and `xz`; it needs network the
  first time and caches under the work dir). Run the same children on the control kernel: that is what shows the refusal is the
  middleware's.
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

An `exec_image` tier (a hook the loader calls with the path, the interpreter path and the image bytes, read-only, able to refuse): the
place to close the libc-only gap above by refusing any `PT_INTERP` executable, and the place a signed-binaries check belongs, since the
loader holds the bytes it is about to map. Not built, and whether a loader refusal leaves a calling process intact was not checked.
Refusing `clone`/`fork` before they run (as safe as refusing `execve`, but nobody has asked for it); stripping variables from an exec's
environment (see Example 2); a runtime `arm`/`disarm` handle for interest bits; a lock-protected state cell for `kernel` hooks (use
atomics, or a `BorrowCell` static, which the kernel lock serialises); typed `UserPtr<T>` (there are byte reads and `u64`/string helpers); a
framework-provided blocking wait for `kernel` hooks; `Return` from an `own` hook (signal delivery on the way out runs under the
kernel lock, and whether a cheap lock-free "signal pending" test exists has not been checked); a measured cost. Hosting
rump's interceptor would need the `arm`/`disarm` handle, the state cell, typed pointers and the blocking wait, and is out of scope
while rump is deferred.

## Background

[`docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md`](../../../docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md) (design, what was
verified, deviations), [`crates/akuma-tokio-kernel/README.md`](../../akuma-tokio-kernel/README.md) § Syscall middleware,
[`taoxichuan/docs/reference/syscalls.md`](../../../taoxichuan/docs/reference/syscalls.md) § Who answers, in order,
`crates/akuma-bkl/src/policy.rs` (the per-syscall bitmap and the latch-at-entry rule this follows).
