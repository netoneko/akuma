# Syscall middleware: a guide

**Stability: B** (written 2026-11-05; the behaviour is booted on both machines under QEMU TCG, the cost of having it on is not
measured, and the pieces in "Not built" are not there). The incident-style record, with what was argued and what was
found by booting, is [`docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md`](../../../docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md);
the code is [`src/middleware/`](../src/middleware/mod.rs) (`calls`, `ctx`, `exec`, `table`, `dispatch`); a working table is
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

   pub static MW: Middleware = Middleware::new(Hooks {
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

One `Middleware` per kernel, a `static`: `Middleware::new(Hooks { .. })`. `Hooks` has two tiers, each a slice of
`Route::new(&[On::..], hook)`, and one `exec` policy (below); leave the ones you do not use at `..Hooks::new()`.

| tier | hook type | runs | it may |
|---|---|---|---|
| `kernel_pre` | `fn(&KernelCtx) -> Verdict` | inside the kernel lock, before the call | `Pass`, `PassAndForget`, `Return(v)`, `Errno(e)` (a refusal: not for `exit`/`clone`/`rt_sigreturn`) |
| `fallback` | `fn(&KernelCtx) -> Option<u64>` | inside the lock, only where the answer is `-ENOSYS` | `Some(answer)` to serve the call, `None` to leave the `-ENOSYS` |

`On::Call(Syscall::Openat)` names a Linux call by identity (the machine maps it to its architecture's number);
`On::Nr(1000)` names a raw number, the same on both architectures (use it from `CUSTOM_BASE` = 1000 up).

**One table, flat, no chaining.** There is no `next`, no priority and no ordering. Two entries naming the same call in the same
tier panic when the table is armed at boot, with the number. A second `Kernel::middleware(..)` panics. To layer behaviours, write
a function that calls your own functions and register that; the framework gives you nothing to make that feel supported.

## Where it runs

```text
amd64    dispatch():  [take the kernel lock] -> legacy fold -> kernel_pre -> the call
                      -> fallback (if -ENOSYS) -> signal delivery -> [release]
aarch64  el0_sync():  [take the kernel lock] -> kernel_pre -> the call -> fallback
                      -> signal delivery -> [release]
```

- The `kernel` tier sees the **canonical** number on amd64: the legacy spellings (`mkdir`, `unlink`, ...) have already been
  folded into their `*at` call.
- A hook cannot change what the call sees: `KernelCtx` is read-only, and the call gets exactly the arguments the program passed.
- A `Return`/`Errno` from `kernel_pre` skips the call (and the fallback).

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

## Example 2: refuse an exec by its environment

The goal: **refuse to start any program whose environment names an `LD_*` variable** (`LD_PRELOAD`, `LD_LIBRARY_PATH`,
`LD_AUDIT`, ...), the knobs the dynamic loader reads from the environment. It is the exec half of the "no dynamic loading"
demo in the crate [`akuma-mw-no-dynamic-loading`](../../akuma-mw-no-dynamic-loading/src/lib.rs) (it was `shanghai/src/nodl.rs` until 2026-11-05; the crate is the worked example of a middleware that lives outside the kernel that uses it: two plain functions, no table, host-tested); the code below is that crate's, not a paraphrase.

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
    Route::new(&[On::Call(Syscall::Execve)], akuma_mw_no_dynamic_loading::exec_guard),
],
exec: EXEC,   // Some(akuma_mw_no_dynamic_loading::exec_policy) with the feature, None without it
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

**Going further: the exec policy, and what it blocks.** The environment guard is an ordinary `kernel_pre` hook. The rest of the demo is
a different kind of hook, because "no dynamic loading" is not about one syscall: code reaches a process in exactly three ways, and the
kernel asks **one** question at each of them, `Hooks::exec: Option<ExecPolicy>` (`fn(&ExecEvent) -> ExecVerdict`, `Allow` or
`Deny(errno)`, read-only like every hook):

| how code arrives | the event | asked from |
|---|---|---|
| `execve`, init, a `#!` interpreter | `ExecKind::Image { path, interp, image }` | `loader::load_task`, **before** the task's layout is reset, so a refusal leaves the caller untouched (`EACCES`) |
| `mmap` with `PROT_EXEC` | `ExecKind::Map { inode }` (`None` = anonymous) | `mm::mmap_arm` |
| `mprotect` adding `PROT_EXEC` | `ExecKind::Protect` | `mm::mprotect_arm` (the kernel cannot say which file a present page came from) |

A policy is one `match`, so it cannot be bypassed by a path nobody hooked. The crate's (`exec_policy`) refuses an image that
has a `PT_INTERP`, refuses a file-backed executable mapping, refuses `mprotect(+PROT_EXEC)`, and allows the rest: static programs run,
anonymous executable memory is allowed (W^X already forbids the writable kind). The policy is registered as `exec: Some(..)`, and
only with `--features no-dynamic-loading`.

**What it does to real dynamic programs** (Alpine's `musl` loader, `busybox` and `xz`, fetched by `shanghai/mw_fetch_dyn.sh`;
amd64 and aarch64 at 4 cores; on the stock kernel both run):

```text
             xz --version  (needs liblzma.so.5)                   busybox true  (needs only libc)
             ─────────────────────────────────────                ──────────────────────────────────
mwprobe      fork + execve("/usr/bin/xz", env: empty)             fork + execve("/bin/busybox", env: empty)
                          |                                                     |
                          v                                                     v
KERNEL       load_task: the exec policy is asked FIRST, with ExecKind::Image { path, interp: Some("/lib/ld-musl-<arch>.so.1"), .. }
                          |                                                     |
                          v                                                     v
MIDDLEWARE   exec_policy: an image with a PT_INTERP        ->      the same: a dynamic image is a dynamic image
             console: "[nodl] pid 6 refused a dynamic             console: "[nodl] pid 7 refused a dynamic
             executable /usr/bin/xz (interpreter ...)"             executable /bin/busybox (interpreter ...)"
                          |                                                     |
                          v                                                     v
RESULT       execve returns EACCES; the child never starts;        execve returns EACCES; the child never starts;
             nothing was mapped, the caller is untouched           the libc-only gap of the syscall guards is closed
```

The first cut guarded `mmap` and `mprotect` as syscalls. It stopped `xz` inside the loader (`Error loading shared library
liblzma.so.5: Permission denied`) but let `busybox` run, because the kernel's own loader maps the interpreter and the main binary
without a syscall and `ld-musl` *is* libc, so a program with no other dependency never asked for a second mapping. A hook at the
loader sees what a syscall hook cannot; that is why the exec policy exists.

**Every avenue the probe tries, with the result** (`mwprobe`, `--features no-dynamic-loading`, amd64 and aarch64, identical):

| | avenue | result |
|---|---|---|
| A1 | `execve` of a dynamic program with an extra library (`xz`) | refused, `EACCES` |
| A2 | `execve` of a dynamic program with only libc (`busybox`) | refused, `EACCES` |
| B | the loader run directly, `ld-musl /bin/busybox true` (no `PT_INTERP`, so the image check passes it) | cannot load it: its mapping of `busybox` is refused (`Not a valid dynamic program`) |
| C | a `#!` script whose interpreter is dynamic | refused, `EACCES` (the kernel resolves the interpreter; it goes through the same image check) |
| D1-D5 | `mmap(file, PROT_EXEC)`; `mprotect(+X)` on a file mapping | refused; read-only file mappings and anonymous executable memory still allowed |
| E1, E2 | bytes written into anonymous memory, then `mprotect(+X)`; `mmap(RWX)` | refused (policy, and W^X) |
| F | `memfd_create` | not served (`ENOSYS`): closed by absence, not by policy |
| G1-G4 | `execveat`, `ptrace`, `process_vm_writev`; opening `/proc/self/mem` for writing | not served / cannot be opened: closed by absence |
| H1-H3 | a forked child that never execs; an exec'd static child (own pid); a child exec with `LD_PRELOAD` | all bound by the same policy; the static child still runs |

"Closed by absence" is only as good as the absence. If `memfd_create`, `execveat` or `ptrace` is ever implemented, those probe lines
fail (the `memfd_create` line already has the branch that tests a mapping of the memfd), which is the point of pinning them.

**A build-time alternative that needs no middleware at all.** If all you want is "no dynamically linked programs", the elf crate can say
no by itself. `akuma-elf-exec` has a `refuse-dynamic` cargo feature (off by default): with it, `program()` returns `ExecErr::Dynamic`
for any image that names an interpreter, and `ExecErr::errno()` for that is `EACCES`. The loader turns the answer into the errno
(`LoadError::Refused(13)`) before anything is torn down. There is no hook, no table, no dispatch arm and no runtime switch: a kernel
built with it cannot load a dynamic image, and one built without it carries none of the code. In Shanghai it is `--features
no-dynamic-elf`; the chain is `no-dynamic-elf` -> `akuma-tokio-kernel/refuse-dynamic-elf` -> the machine crates ->
`akuma-tokio-kernel-core/refuse-dynamic-elf` -> `akuma-elf-exec/refuse-dynamic` (`cargo tree -e features -i akuma-elf-exec` shows
it on or off). It is independent of `middleware` and of `no-dynamic-loading`.

| | `no-dynamic-elf` (build-time, elf crate) | `no-dynamic-loading` (exec policy, middleware) |
|---|---|---|
| mechanism | `akuma-elf-exec::program()` refuses; compiled in or out | a `match` registered as `Hooks::exec`; needs `middleware` |
| errno | `EACCES` (aligned with the policy, 2026-11-05) | `EACCES` (the policy's choice) |
| dynamic `execve`, both kinds; a `#!` script with a dynamic interpreter | refused | refused |
| the loader run directly (`ld-musl <binary>`, no interpreter of its own) | **still loads the program** (pinned in the probe) | refused: its mapping is refused |
| `mmap(file, PROT_EXEC)`, `mprotect(+X)`, code written to anonymous memory | not touched | refused |
| runtime cost | none beyond the header scan | one hook call per exec / executable mapping |

So the build-time feature closes the *image* avenue and nothing else, and the probe pins the rest as open (`B`, `D1`) so the scope
cannot drift. Use it when images are the whole threat and you want a kernel that cannot be talked out of it; use the policy when the
mapping avenues matter. They compose (the image check runs first).

**What this does not claim.** It bans dynamically linked programs and every route the probe found for turning a file or bytes into
native code. It does not stop a static program from doing anything else, and it does not police interpreted code: a script run by a
*static* interpreter (the script is data), or a static program's own bytecode, is not "loading a binary". `mprotect(+PROT_EXEC)` is
refused outright, so nothing can JIT. A policy that must also check *which* binaries may run (signatures) uses the same `Image` event,
which carries the image bytes; that is not written.

**A lesson from writing it:** the first test subject was `bzip2`, and the probe passed it as "dynamic" because it ran under the
guards without a single refusal. It had never asked for a second library: Alpine builds libbz2 into it, so it needs only libc. Check
what a test subject actually loads (`strings -a <binary> | grep '^lib.*\.so'`) before trusting a result about it;
`mw_fetch_dyn.sh` fails if `xz` does not name `liblzma` or if `busybox` needs more than libc.

## Boot parameters (`Hooks::init`)

A table may read configuration off the boot command line, once, at boot. The machine captures the line whole into a write-once
buffer (`params::capture`: 2048 bytes, taken exactly once, handed out afterwards only as `&'static str`, so nothing can change it
by API) and then calls `middleware::init()`, which runs the table's `Hooks::init: fn(&Params<'static>, &mut S)`. It runs
single-threaded, before the first user instruction, with no kernel context.

- **The view is narrowed to `mw.`.** `init=`, `epoch=`, `smp=` are not visible. `Params::scoped("sample.")` narrows again and has
  no inverse, so each crate in a composed table is handed only `mw.<name>.`. Keys are whole words (`key` does not match `keyring`)
  and prefixes end at their dot (`mw.nodl.` is not `mw.nodlx.`).
- **What a crate reads from is the `ParamSource` trait** (`get(key) -> Option<&str>`), implemented by the cmdline view and by
  `params::Table` (a fixed list, for host tests and compiled-in defaults), so a crate's parser cannot tell where its input came
  from. `Params::value` is `get` with a value that outlives the view; use it to keep a `&'static str` in a `OnceCopy`.
- **A line that does not fit is not kept as a prefix.** amd64 reads it with `StartInfo::cmdline_complete`, which says `TooLong`
  instead of truncating (`cmdline` still truncates, for readers that want `smp=` off the front of a long line). `params::failed()`
  is then true and the view is empty. **A policy that needs a value must read an absent or malformed one as "refuse", never as
  "unset".** The 512-byte limit the kernel used to read with was its own stack buffers, not PVH's (PVH gives only a NUL-terminated
  string); whether a VMM caps the line before the kernel sees it is not checked.
- **Software guarantee only.** A raw-pointer write by the kernel itself would not be stopped; mapping the page read-only after boot
  is the stronger version and is not built.

Shanghai's sample table reads `mw.sample.also=/path` (one more path `kernel_pre` refuses) and records what it could and could not
see (call 1006); the probe checks that the kernel's own words and another namespace's keys were invisible.

## Allowing from the checked bytes (`ExecVerdict::AllowFromImage`, `Hooks::exec_sidecar`)

A policy that *verifies* an image (a hash, a signature) has a problem that one that merely refuses does not: the kernel hands it its own
copy of the whole file, which nothing can change while it reads it, but the loader would otherwise map whole-page segment pages as lazy
records read from the inode at fault time, and a file rewritten (or a path renamed) after the check would run unchecked bytes. A policy
answers `AllowFromImage` to say "I checked `image`; map from it": every page is placed from those bytes, no inode is attached and the path
is not looked up again. The cost is the shared file-page cache for that image. `Hooks::exec_sidecar: Some(".sig")` makes the loader also read
`<path>.sig` (at most 4 KiB) and pass it as `ExecKind::Image { sidecar }`, so the policy stays a pure function of its event. The worked
example is `crates/akuma-mw-signed-exec`; the proof is the probe child that rewrites its own file while running (stock kernel: SIGSEGV;
signed kernel: survives).

## The rules, and why each is there

- **No hook touches a lock.** `kernel_pre` and `fallback` run under the kernel lock, which the kernel takes, so there is no lock-order question and nothing for a hook to name. If you need a counter, an atomic works.
- **`kernel` hooks run under the big kernel lock.** Do not spin on something another core's syscall holds, and do not try to
  park the caller: there is no sanctioned way to block yet (see "Not built").
- **Some calls are observed, never replaced:** `exit`, `exit_group`, `clone`, `execve`, `rt_sigreturn` rewrite the frame or do not
  return. For them a `fallback` is ignored (`observe_only`). A `kernel_pre` refusal
  (`Return`/`Errno`) skips the call, so it cannot corrupt the frame: it is honoured for `execve` and ignored for `exit`,
  `exit_group`, `clone` and `rt_sigreturn` (`may_refuse_before`).
- **Hooks cannot mutate the kernel context.** Every `kernel` hook takes `&KernelCtx`, so a hook can allow, refuse or answer a
  call but never change what runs. Rewriting arguments is how one hook defeats another's check (a guard validates a path, a
  later hook swaps the pointer), and it would make combined hooks depend on their order. 
- **A hook that panics is a kernel panic.** Treat it as kernel code.
- **No `unsafe`.** `middleware/` is `#![forbid(unsafe_code)]` and the API needs none; put the same attribute at the top of your
  middleware module. Raw access means patching the attribute out of your own crate.
- **No runtime switch.** A production kernel does not get a command-line hatch to change what a syscall does (the same decision
  as the removed `wx=0`).

## What it costs, and the caches

Which numbers a tier wants is decided once, when the table is armed: a 512-bit mask for `kernel_pre`. A call nobody hooked costs one
relaxed load and a bit test, and no hook runs. Numbers from 512 up are always asked (that is where custom calls live).
The mask is read **once per call**. A
`kernel_pre` may return `Verdict::PassAndForget` to clear its own bit for that number for good. Verdicts themselves are never
cached: they depend on the arguments.

The feature-on cost against the syscall floor is **not measured**; do not quote a number for it.

## Testing a table

- **On the host** (no kernel): `Middleware::arm(machine)` with a fake `Machine` (`decode`, `encode`, `copy_in`, `copy_out`,
  `pid`), then and `run_kernel` directly. [`src/middleware/tests.rs`](../src/middleware/tests.rs) does
  exactly this for every rule above: `cargo test -p akuma-tokio-kernel-core --features middleware --target $(rustc -vV | sed -n
  's/host: //p')`.
- **In a guest:** copy Shanghai's pattern. Write a probe that makes raw `syscall` instructions (libc would hide an `-ENOSYS`
  behind `errno`), run it as `init`, and judge the boot by its result line. Always boot a **control** built without the feature
  too: it proves the calls your table touches are stock when it is off. `shanghai/mw_gate.sh` is the harness for both machines.
- **Children and real programs:** a policy about processes is only shown by processes. `mwprobe` forks (raw `clone`), execs a static
  child, and runs real dynamic binaries (`shanghai/mw_fetch_dyn.sh` fetches Alpine's loader, `busybox` and `xz`; it needs network the
  first time and caches under the work dir). Run the same children on the control kernel: that is what shows the refusal is the
  middleware's.

## Not built (yet)

Refusing `clone`/`fork` before they run (as safe as refusing `execve`, but nobody has asked for it); stripping variables from an exec's
environment (see Example 2); a runtime `arm`/`disarm` handle for interest bits; a lock-protected state cell for `kernel` hooks (use
atomics, or a `BorrowCell` static, which the kernel lock serialises); typed `UserPtr<T>` (there are byte reads and `u64`/string helpers); a
framework-provided blocking wait for `kernel` hooks; an observe-only tier outside the kernel lock (the `own_pre`/`own_post` tier was removed: nothing used it, and its state could not reach a decision); a measured cost. Hosting
rump's interceptor would need the `arm`/`disarm` handle, the state cell, typed pointers and the blocking wait, and is out of scope
while rump is deferred.

## Background

[`docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md`](../../../docs/archive/TAOXICHUAN_SYSCALL_MIDDLEWARE.md) (design, what was
verified, deviations), [`crates/akuma-tokio-kernel/README.md`](../../akuma-tokio-kernel/README.md) § Syscall middleware,
[`taoxichuan/docs/reference/syscalls.md`](../../../taoxichuan/docs/reference/syscalls.md) § Who answers, in order,
`crates/akuma-bkl/src/policy.rs` (the per-syscall bitmap and the latch-at-entry rule this follows).
