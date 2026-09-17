# amd64: wiring the box/container syscalls, and three real bugs in a spawn path nothing had ever exercised

**Date:** 2026-09-17
**Status:** in progress, **uncommitted** (on top of the user's own "amd64 containers"
checkpoint commit, `cbbe1a6a`) — the user drives commits on this repo. First-boot
`register_box`/`spawn_ext`/`waitpid` verified working end to end with a real
process on local QEMU. A second, distinct crash on a *second* invocation in the
same boot is open and not yet investigated — see "What's left".
**Trigger:** continuing the same day's `docs/archive/AKUMA_AMD64_SC_CONTAINERS_MOUNT.md`
work, the user asked what actually blocks amd64 from having real "boxes"
(containers) the way AArch64 does, and then to wire it.

## The corrected picture: boxes are mostly already arch-neutral

The first answer given in this session was wrong in one respect and
incomplete in another; both were checked against the code rather than left
as assumptions, and the correction matters for anyone picking this up:

- **Wrong:** that `sys_spawn`'s six-argument ABI would need to grow a
  box-targeting argument. It doesn't — a **separate**, already-existing
  Akuma-private syscall, `spawn_ext`(315), takes a `SpawnOptions` struct (88
  bytes, `#[repr(C)]`, pinned by a `const` assertion on both the kernel side —
  `crates/akuma-syscalls-glue/src/proc.rs` — and the userspace side —
  `userspace/box/src/sys.rs`) with a `box_id: u64` field at a fixed offset
  (64). `box`/`herd` on AArch64 already use it for exactly this. Plain
  `sys_spawn`(301) always births box 0 and that is correct, unrelated
  behavior.
- **Incomplete:** the claim that amd64 has no per-box network namespace.
  `crates/akuma-net/src/socket.rs` already tags every socket with a `box_id`
  and filters visibility by it (`current_box_id != 0 && slot.box_id !=
  current_box_id`, line ~1751) — arch-neutral, already linked into amd64.
  Nothing is missing there; it's simply unexercised, because nothing on this
  target has ever created a process with `box_id != 0`.

Verified by reading, not assumed: `Process::inherit_from`
(`crates/akuma-exec/src/process/mod.rs:888`) already copies `box_id:
parent.box_id` and `namespace: parent.namespace.clone()` on **every** `fork`,
arch-neutral, unmodified — so once a process is born into a box, its
descendants stay in it for free, on amd64 exactly as on AArch64. And
`spawn_process_with_channel_ext` (`crates/akuma-exec/src/process/spawn.rs`)
already resolves `box_id != 0` to the box's real registered namespace via
`(runtime().get_box_namespace)(box_id)` and assigns it to the new process
permanently (not just for the duration of the spawn's own file reads), and
amd64's own native `crate::fd::sys_openat` resolves paths through
`akuma_syscalls_glue::fs::resolve_path_at`, which reads the calling process's
own `.namespace` via `current_process_shared()` — the same function glue
itself uses. **The isolation mechanism was never the gap.** The gap was three
concrete bugs in a code path nothing on this target had ever run.

## What was wired

Six Akuma-private syscall numbers (300-series, dispatched through
`amd64/src/usermode.rs`'s `AKUMA_PRIVATE_BASE` match — `nr - 0x1000`), all
forwarding raw arguments straight to already-implemented, arch-neutral
`akuma-syscalls-glue` functions:

| syscall | nr | glue implementation | gate |
|---|---:|---|---|
| `spawn_ext(path, options_ptr, options_len)` | 315 | `proc::sys_spawn_ext` | none — dispatched even with `sc-containers` off |
| `register_box(id, name, name_len, root, root_len, primary_pid)` | 316 | `container::sys_register_box` | `sc-containers` |
| `kill_box(box_id)` | 317 | `container::sys_kill_box` | `sc-containers` |
| `reattach(pid, force)` | 318 | `container::sys_reattach` | `sc-containers` |
| `set_box_stack(box_id, stack)` | 324 | `proc::sys_set_box_stack` | none |
| `mount_in_ns(box_id, target, target_len, fstype, fstype_len, data)` | 325 | `container::sys_mount_in_ns` | `sc-containers` |

`sc-containers` was already turned on for amd64 earlier the same day
(`AKUMA_AMD64_SC_CONTAINERS_MOUNT.md`), so four of these six needed nothing
beyond the dispatch arm. Every argument order was checked against
`userspace/box/src/sys.rs`'s actual wire calls before wiring, not assumed
from the glue signature alone.

**`set_box_stack(box_id, 1)`** (NetBSD rump stack) is a real, silent no-op
divergence on amd64: this target has no `akuma-rump` dependency at all, so a
box that asks for the rump stack gets accepted (`Ok(0)`) but nothing routes
its sockets anywhere different. Not fixed — there is nothing to route to on
this target — and worth a real errno (`ENOSYS` or similar) if this ever
matters in practice, rather than a silent accept.

Confirmed against `nr.rs`'s own comment, checking rather than assuming: 316-318
and 325 land in the same Akuma-private block the mount work's own doc
already covered; `amd64/src/usermode.rs`'s `AKUMA_PRIVATE_BASE` match had no
arms for any of these six before this session.

## Bug 1: `run_registered_process` bypassed the `enter_user` hook entirely

`akuma_exec::process::spawn::run_registered_process` — the function every
`spawn_ext`-created process (i.e. every boxed process) runs through on its
very first entry into ring 3 — ended with a **direct** call to
`akuma_el0_entry::enter_user_mode_checked`:

```rust
// before
let ctx = proc.image.lock().context;
enter_user_mode_checked(&ctx)
```

That function is AArch64's raw `eret`, gated `#[cfg(all(target_os = "none",
target_arch = "aarch64"))]`; every other target gets:

```rust
#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
pub fn enter_user_mode_checked(_ctx: &UserContext) -> ! {
    panic!("enter_user_mode_checked on a host build")
}
```

— which is exactly the panic hit, verbatim, the first time this session ran
a real `spawn_ext` call on amd64:

```
[PANIC] crates/akuma-el0-entry/src/lib.rs:78
        enter_user_mode_checked on a host build
```

`akuma-exec`'s **other** first-run/resume path, `Process::run`
(`crates/akuma-exec/src/process/mod.rs:1111`), already does this correctly:
`(runtime().enter_user)(&ctx)` — the registered `ExecRuntime` hook, whose own
doc comment (`crates/akuma-exec/src/runtime.rs:186`) states plainly that this
is *the* architecture seam for exactly this reason: "On AArch64 it is
`akuma_el0_entry::enter_user_mode_checked`... On x86_64 `sysret` does
return". `fork_process` reaches ring 3 through `Process::run` and was
completely unaffected. **This bug is not amd64-specific in principle** — any
non-AArch64 target using `spawn_process_with_channel_ext` would hit it — it
was silent on AArch64 only because that target's registered hook happens to
be the exact function this call site hardcoded.

**Fix** (`crates/akuma-exec/src/process/spawn.rs`, `run_registered_process`):
replaced the direct call with `(runtime().enter_user)(&ctx)`, matching
`Process::run` exactly, and dropped the now-unused `enter_user_mode_checked`
import.

## Bug 2: `spawn_ext`'s one-phase thread spawn never called `bind_child_task`

Fixing bug 1 traded the panic for a different failure, printed once and then
hung (QEMU still alive, ssh session stuck):

```
  [proc] ring-3 entry with no slot
```

`amd64/src/usermode.rs`'s `enter_ring3` — the concrete function registered as
`ExecRuntime::enter_user` on this target — reads `current_proc_slot()` from
the running task's own per-CPU `UserCtx` and refuses to proceed if it is
unset. That field is seeded by `ExecRuntime::bind_child_task`
(`crates/akuma-exec/src/runtime.rs:275`), whose own doc names precisely what
it does for amd64: "the task's `space_root`... its `SPAWN` row and the
`UserCtx::proc_slot` naming it." It is called from exactly **one** place in
the whole tree — `spawn_child_thread_and_publish`
(`crates/akuma-exec/src/process/mod.rs:2458`), fork's own child-publish path.

The reason `spawn_process_with_channel_ext` never called it is structural,
not an oversight of "forgot a line": fork's spawn primitive,
`crate::threading::spawn_user_thread_initializing`, is **two-phase** — it
allocates a task slot that stays `INITIALIZING` (unable to run) until the
caller explicitly calls `mark_thread_ready`, so `bind_child_task` runs from
the *parent*, in the window between slot allocation and the child ever being
scheduled. `spawn_ext`'s primitive,
`crate::threading::spawn_user_thread_fn_for_process`, is **one-phase** — the
closure passed to it *is* the child, already running the moment the thread is
created. There is no window for a parent to bind anything into before the
child runs, because there is no parent-side step at all.

**Fix:** the child thread binds itself, as its own first action, before it
publishes into `THREAD_PID_MAP` (the same ordering `bind_child_task`'s own
contract requires, and satisfiable here because nothing before this line
depends on the state it seeds):

```rust
let bound = crate::process::table::with_process(pid, |p| {
    p.thread_id = Some(tid);
    (runtime().bind_child_task)(tid, p, ChildKind::Process)
});
```

Verified this is safe to call from the *child itself* rather than a parent by
reading amd64's own `bind_child_task` implementation
(`amd64/src/usermode.rs:4683`): it is keyed purely on `task_slot`/`child.pid`
via global tables (`spawn_table()`, `crate::sched::set_task_space_root`,
`crate::sched::seed_proc_slot`) and never reads "who is calling" — `kind ==
ChildKind::Thread` is the only branch that would (`current_proc_slot()`, the
*caller's* slot, for `clone`'s parent-child pairing), and a `spawn_ext`
process always passes `ChildKind::Process`, which does not take that branch.

This alone was not sufficient — see Bug 3 — but it is a real, necessary fix:
without it, `current_proc_slot()` is permanently unset for every
`spawn_ext`-created process.

## Bug 3: a lock-order hazard, and a stale `thread_slot`, found together

With bugs 1 and 2 fixed, a real ring-3 attempt no longer panicked or hung —
it **crashed the whole QEMU guest outright**, no panic banner, no fault
trace, the process just gone (consistent with a triple fault under
`run.sh`'s `-no-reboot`). This is the harder of the two remaining bugs and
both fixes landed together; they were not isolated from each other, stated
plainly rather than overclaimed:

**3a — lock ordering.** The fix for Bug 2 called `bind_child_task` from
*inside* `crate::process::table::with_process(pid, |p| { ... })`'s callback.
`with_process` takes `PROCESS_TABLE`'s own **exclusive** lock
(`with_active_mut`) for the callback's duration. `bind_child_task`'s amd64
implementation walks the `SPAWN` table and takes further locks (the BKL,
implicitly, per its own `unsafe`-block comments: "under the BKL"). Fork's own
call to `bind_child_task` (`spawn_child_thread_and_publish`) never has this
problem: it runs on a `Box<Process>` that is **not yet registered in the
table at all**, so no table lock is held. Calling a lock-taking hook from
inside a narrower, exclusive lock than the one it was designed to be called
without is exactly the kind of hazard `run_registered_process`'s own
existing comment already named for a different function ("this first-run
window reaches its process through a safe shared borrow instead of
`with_process_exclusive`" — `AKUMA_EXEC_AUDIT.md` §6.E group 2).

**Fix:** split the two operations. `p.thread_id = Some(tid)` (a plain field
write) stays inside the short `with_process` call; `bind_child_task` moved
outside it, reached through `lookup_process_shared(pid)` — the same
`&'static Process`, lock-light shared borrow `run_registered_process` itself
already uses for exactly this reason:

```rust
let _ = crate::process::table::with_process(pid, |p| {
    p.thread_id = Some(tid);
});
let bound = lookup_process_shared(pid)
    .map(|p| (runtime().bind_child_task)(tid, p, ChildKind::Process));
```

**3b — `UserCtx::thread_slot` never reset for this path.** Independently
(and possibly redundantly — never isolated from 3a, see below),
`bind_child_task`'s `ChildKind::Process` arm called
`crate::sched::seed_proc_slot(task_slot, slot)`, which — by its own doc
comment — sets **only** `proc_slot`, deliberately, leaving
`UserCtx::thread_slot` whatever it was before this task slot was claimed. For
every existing caller (fork/vfork/clone, all through the two-phase
`spawn_user_thread_initializing`), this has apparently never mattered in
practice — the pool's own recycling never happened to hand a `clone`
thread's old slot to a `ChildKind::Process` bind in a way any existing test
exercises. `spawn_ext` is the first caller reaching this bind through the
*one-phase* primitive, on a task slot that could equally have been last used
by anything. Left stale, `enter_ring3` reads `current_thread_slot() !=
NO_THREAD` and branches into `crate::thread::run_thread` — a **thread-exit
teardown path** — for a process that has not run a single instruction.

**Fix:** `crate::sched::seed_thread_slots(task_slot, slot,
crate::thread::NO_THREAD)` in place of the narrower `seed_proc_slot` call,
explicitly stating what is always true for this arm — a process is never a
thread — rather than relying on an assumption about which slots get recycled
into which role.

**Both 3a and 3b were applied together and tested together**; the crash was
not reproduced with only one of the two in place at a level of confidence
worth stating as fact. Whoever next touches this code should not assume
either alone is sufficient without re-isolating.

## Live verification

Local QEMU only, per the user's explicit instruction this session (another
agent was using the Firecracker/bare-metal box in parallel) — `amd64/run.sh`,
`SSH_PORT=2322`, `INIT=/bin/sshd`.

| check | result |
|---|---:|
| `cargo build --release --target x86_64-unknown-none` (amd64) | clean, every stage of this session |
| `cargo build --release` (AArch64 kernel, repo root) | clean, unaffected |
| `cargo test -p akuma-exec` (host) | 87/87 pass, every stage |
| `cargo clippy -p akuma-exec`, both `x86_64-unknown-none` and `aarch64-unknown-none` | clean |
| Boot suite, every stage of this session including the final one | **745 passed, 0 failed** — unchanged from before any box work; the new syscalls are Akuma-private numbers with no `dispatch_smoke_test` coverage added yet (see "What's left") |
| **Attempt 1** (bug 1 present): real ring-3 probe, `spawn_ext` into a fresh box | kernel panic, `enter_user_mode_checked on a host build` |
| **Attempt 2** (bug 1 fixed, bug 2 present): same probe | `[proc] ring-3 entry with no slot`, then hang — ssh session stuck, QEMU alive |
| **Attempt 3** (bugs 1+2 fixed, bug 3 present): same probe | silent full QEMU exit, no panic, no fault trace — consistent with a triple fault under `-no-reboot` |
| **Attempt 4** (bugs 1+2+3b only, no 3a): same probe | identical silent crash to attempt 3 — this is the basis for not claiming 3b alone was sufficient |
| **Attempt 5** (bugs 1+2+3a+3b, i.e. everything above): same probe, **first boot, first call** | `register_box(900, root=/box-jail)` → `Ok`; `spawn_ext` into box 900 → real pid (60) + real `stdout_fd`; `waitpid` → real exit status (not a hang, not a crash) |
| **Attempt 6**: same probe run a **second time** in the **same boot**, without a reboot | silent full QEMU exit again — a **distinct, unrelated crash**, not yet investigated (see "What's left") |

The attempt-5 child process itself exited 1 — its own isolation self-checks
(`open("/inside.txt")`, `open("/outside.txt")` expecting `ENOENT`,
`open("/box-jail")` expecting `ENOENT`) were never read back, because the
probe's first version didn't drain the `stdout_fd` `spawn_ext` returns in the
result's high 32 bits (`SpawnResult.stdout_fd`, per
`userspace/box/src/sys.rs`). A second probe revision that reads it was
written but not yet run to completion before attempt 6's crash — **the
isolation logic itself (`/inside.txt` visible, `/outside.txt` and
`/box-jail` invisible from inside the box) is therefore still unconfirmed**,
separate from "does spawn_ext run a process at all," which attempt 5 did
confirm.

### Repro (attempt 5/6's probe)

Cross-built `x86_64-linux-musl-gcc -static`, pushed over ssh with `cat` (the
guest busybox has no `base64`). Registers box 900 rooted at `/box-jail`,
seeds `/box-jail/inside.txt` and a sibling `/outside.txt` outside the jail,
`spawn_ext`s itself back into the box with `--child`, and the child checks
whether the box's namespace actually hides `/outside.txt` and `/box-jail`
while exposing `/box-jail/inside.txt` as `/inside.txt`:

```c
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <stdint.h>

#define AKUMA_PRIVATE_BASE 0x1000
#define NR_SPAWN_EXT    (AKUMA_PRIVATE_BASE + 315)
#define NR_REGISTER_BOX (AKUMA_PRIVATE_BASE + 316)
#define NR_KILL_BOX     (AKUMA_PRIVATE_BASE + 317)

struct spawn_options {
    uint64_t cwd_ptr, cwd_len;
    uint64_t root_dir_ptr, root_dir_len;
    uint64_t args_ptr, args_len;
    uint64_t stdin_ptr, stdin_len;
    uint64_t box_id;
    uint64_t env_ptr, env_len;
};

static const char *SELF = "/tmp/box_probe";

static int child_main(void) {
    int fd = open("/inside.txt", O_RDONLY);
    if (fd < 0) { printf("CHILD FAIL open(/inside.txt) errno=%d\n", errno); return 1; }
    char buf[32] = {0};
    read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (strncmp(buf, "INSIDE", 6) != 0) { printf("CHILD FAIL wrong content: %s\n", buf); return 1; }

    errno = 0;
    fd = open("/outside.txt", O_RDONLY);
    if (fd >= 0) { printf("CHILD FAIL /outside.txt visible from inside the box!\n"); return 1; }
    if (errno != ENOENT) { printf("CHILD FAIL wrong errno for /outside.txt: %d\n", errno); return 1; }

    errno = 0;
    fd = open("/box-jail", O_RDONLY);
    if (fd >= 0) { printf("CHILD FAIL /box-jail (host path) exists inside the box!\n"); return 1; }

    printf("CHILD ALL OK\n");
    return 0;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "--child") == 0) return child_main();

    unlink("/box-jail/inside.txt"); rmdir("/box-jail"); unlink("/outside.txt");
    mkdir("/box-jail", 0755);
    int fd = open("/box-jail/inside.txt", O_CREAT | O_WRONLY, 0644);
    write(fd, "INSIDE", 6); close(fd);
    fd = open("/outside.txt", O_CREAT | O_WRONLY, 0644);
    write(fd, "OUTSIDE", 7); close(fd);

    long box_id = 900;
    const char *name = "probe-box", *root = "/box-jail";
    syscall(NR_REGISTER_BOX, box_id, (long)name, (long)strlen(name),
            (long)root, (long)strlen(root), 0L);

    const char *cargv[] = { SELF, "--child", NULL };
    struct spawn_options opts;
    memset(&opts, 0, sizeof(opts));
    opts.args_ptr = (uint64_t)(uintptr_t)cargv;
    opts.args_len = 2;
    opts.box_id = (uint64_t)box_id;

    long spawn_rc = syscall(NR_SPAWN_EXT, (long)SELF, (long)&opts, (long)sizeof(opts), 0L, 0L, 0L);
    uint32_t pid = (uint32_t)((uint64_t)spawn_rc & 0xFFFFFFFFu);
    int stdout_fd = (int)(((uint64_t)spawn_rc >> 32) & 0xFFFFFFFFu);

    int status = 0;
    waitpid((pid_t)pid, &status, 0);
    char cbuf[512];
    ssize_t cn = read(stdout_fd, cbuf, sizeof(cbuf) - 1);
    if (cn > 0) { cbuf[cn] = 0; printf("child stdout: %s\n", cbuf); }

    syscall(NR_KILL_BOX, box_id, 0L, 0L, 0L, 0L, 0L);
    return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}
```

Run from the scratch directory, not `userspace/` — this family has no
existing probe tree the way `userspace/epollprobe/` does, matching the same
call made in `AKUMA_AMD64_SC_CONTAINERS_MOUNT.md`.

## What's left

- **Attempt 6's crash is a separate, open bug.** Same probe, same boot, run
  twice: the first run gets a real pid and a real exit status (attempt 5);
  the second run — which re-registers `box_id 900` (the first run's
  `kill_box` never ran, since the child's own exit code made the orchestrator
  return before reaching it) — crashes the guest the same silent way bugs 1-3
  did. Not yet investigated. Candidates worth checking first: re-registering
  an already-live `box_id` (does `akuma_isolation::box_registry::register_box`
  or `akuma_vfs_glue::create_box_namespace` assume the id is unused?), a stale
  `SPAWN` row from `spawn_ext`'s first, un-`kill_box`-ed process, or the
  `stdout_fd` `read()` this session's second probe revision added blocking on
  a channel state the first revision never touched.
- **The isolation logic itself is unconfirmed.** `/inside.txt` visible,
  `/outside.txt` and `/box-jail` invisible from inside the box — the probe
  above checks this, but attempt 6's crash means it has not yet completed a
  run with the `stdout_fd`-draining revision. Given the arch-neutral proof
  already done by reading (`Process::inherit_from`, `get_box_namespace`,
  `resolve_path_at`'s `current_process_shared()` use), this is expected to
  pass — but "expected" is not "verified," and this doc should be corrected
  once it runs.
- **`dispatch_smoke_test` has no coverage for any of the six new syscalls.**
  The mount/umount2 pass added ten checks including a full live round trip
  run from the boot task itself; nothing analogous exists yet for
  `spawn_ext`/`register_box`/etc., partly because — unlike `mount` — a
  meaningful `spawn_ext` check needs a registered process (`current_process_shared()`
  must be `Some` for the channel-registration step to succeed, the same
  `io_setup` caveat from `AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md`), so it cannot
  run from the boot task the way `mount`'s could.
- **Firecracker/bare-metal verification** — deliberately not attempted this
  session; another agent was using that box for unrelated work, and the user
  asked explicitly to stay on local QEMU. Once attempt 6 and the isolation
  check are closed out, this is the natural next verification step, following
  `AKUMA_AMD64_SC_CONTAINERS_MOUNT.md`'s already-proven recipe for reaching
  that box's Ubuntu/Firecracker personality.
- **`box run` / `herd` end to end on amd64** — the user's actual target
  ("verify that boxes via box/herd work the same way as on aarch64"). Not
  attempted yet: `box`/`herd` are real userspace binaries with real OCI image
  handling, overlay mounts and process supervision on top of everything
  above; this session's probe exercises only the two kernel primitives
  (`register_box`, `spawn_ext`) directly. Cross-building and running the real
  `box`/`herd` binaries against this kernel is the next real milestone once
  the two open items above are closed.
- **`set_box_stack(box_id, 1)`'s silent no-op** on this target (see above) —
  a real divergence, not urgent, not fixed.

## Background

- `docs/archive/AKUMA_AMD64_SC_CONTAINERS_MOUNT.md` — the same day's earlier
  work: `mount`/`umount2` wired, `sc-containers` turned on for amd64, and the
  Firecracker/bare-metal box access recipe this session deliberately did not
  use.
- `docs/archive/AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md` — the `to_glue`/dispatch
  fold shape this session's six syscalls follow, and the `io_setup`
  boot-task-vs-registered-process caveat cited above.
- `docs/archive/AKUMA_EXEC_AUDIT.md` §6.E group 2 — the shared-vs-exclusive
  borrow split (`lookup_process_shared` vs `with_process`) bug 3a's fix
  depends on.
- `proposals/NEXT_AGENT_AMD64_RING3_ENTRY_SEAM.md` — the original fork/exec
  ring-3 entry seam design (`ExecRuntime::enter_user`) that this session's
  bug 1 found a second, un-ported caller of.
