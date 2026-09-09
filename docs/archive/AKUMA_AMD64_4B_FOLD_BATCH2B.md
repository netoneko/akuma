# amd64 C1 step 4b, batch 2b: `close` folds, and `/proc` starts converging

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessors:** `AKUMA_AMD64_4B_FOLD_BATCH2A.md` (the four alignments),
`AKUMA_AMD64_PIPE_TABLE_UNIFICATION.md` (one `PipeTable`).

`close(2)` is `akuma-syscalls-glue`'s arm on this target now. Getting there
took two prerequisites the plan never named, one of which turned up a
wrong-answer bug in the **shared** procfs that both kernels have been carrying.

## 1. The boot row has a process identity

Every glue arm that hands out or frees a descriptor ends in
`if let Some(proc) = current_process_shared() { … } else { Err(ESRCH) }`. The
boot suite runs on the boot task, which is registered nowhere. Batch 1's five
arms slipped past that only because they are path-based.

`fd::boot_row_register()` / `boot_row_release(tid)` are the pair, and they are
the amd64 spelling of what the AArch64 kernel has had all along —
`register_at_syscall_process`, used **25 times** in `src/process_tests.rs`.
`akuma_exec::process::make_test_process` is a `pub fn` in the crate that owns
`Process`, so there was nothing to build.

Two details that are not free:

- **pid 1, not a spare number.** `usermode::current_pid` already answers 1 for
  an unmapped thread, and `/proc/self` resolves through it. Any other pid
  points `/proc/self` at a process the synthetic `/proc` view has never heard
  of, and every `proc_consistency_check` case becomes `ENOENT`.
- **The release must drain.** `unregister_process` *retires* a slot rather than
  dropping it (Phase 7e's deferred reclamation), so the `Process` and its
  address space are still held when it returns — a permanent page, which
  `identity: probe teardown leaks nothing` reported, correctly.
  `reclaim::drain_retired()` is what makes the registration a loan.

`sock::smoke_test` needed the same pair, and finding that out is what the fold
is for: `sock: close` came back `-ESRCH` where it wanted `0`.

## 2. The bug the identity exposed: `metadata` in the shared procfs

Registering a process is what wakes the **mounted** `ProcFilesystem` during the
suite window, and the first thing it did was serve a file nothing serves:
`/proc/self/smaps` passed `open`, `stat` **and** `access`.

The cause is a fall-through in `akuma-vfs-glue`'s `ProcFilesystem::metadata`.
`parse_pid_path` accepts anything whose first component parses as a pid, so
after the specific arms it answered, for `<pid>/<anything>`:

```rust
return Ok(Metadata { is_dir: true, … });
```

`stat("/proc/1/smaps")` reported a **directory** for a path the same
filesystem's `exists` and `read_at` both deny — and amd64's `sys_openat`,
seeing `is_dir`, skipped its existence check and handed out a descriptor.

It is fixed to answer only for the two real directories (`<pid>` and
`<pid>/fd`) and `NotFound` otherwise. This is exactly the class of defect
amd64's own `proc_consistency_check` was written after — *open, stat and access
must agree* — living in the shared filesystem the whole time. **AArch64 has it
too**, and gets the fix.

## 3. `/proc/<pid>/exe` moved up, and grew a real target

Two interceptions in `akuma-syscalls-glue` (`sys_openat` and `sys_readlinkat`,
`if path == "/proc/self/exe"`) were the syscall layer patching a gap in procfs.
The gap is closed in `ProcFilesystem` instead — `read_symlink`, `is_symlink`,
`exists`, `metadata` and the pid directory listing all know `<pid>/exe` now —
so the generic paths serve it, for **any visible pid** rather than only `self`.

Two behaviours changed on purpose:

- `readlinkat` with no current process answered the fabricated target
  `/bin/unknown`; it answers `ENOENT` now.
- **The link names a path.** `image.name` was `argv[0]` on this target, so
  `readlink /proc/self/exe` answered `readlink` and `ls -l` showed
  `/proc/self/exe -> ls` — a symlink that opens nothing. `sys_spawn` and
  `sys_execve` register the resolved path now; `ps` is unaffected because
  `akuma_procfs::ProcStat::comm` takes the basename. Pinned by a ring-3 check
  that reads four bytes through the link and finds `\x7fELF` — the first
  assertion (an absolute path) passed while the file was still useless.

## 4. `close` folds

`Syscall::Close => to_glue(…)`. `fd::sys_close` stays as a **forward** —
`akuma_syscalls_glue::fs::sys_close(fd as u32)` — because ~40 kernel-side
self-test call sites spell it that way; it is a name, not an implementation.

One divergence adopted: an *unbound* 0/1/2 used to answer `0` and do nothing;
glue answers `EBADF` for an fd its table does not hold. That is Linux's answer,
and it is now reachable only by a task whose stdio is unbound (the boot row —
every registered process has the triple since batch 2a). The console itself is
unaffected: `read`/`write` answer it by number through `fd::console_end`.

## Verification

| gate | before | after |
|---|---|---|
| QEMU/TCG `SMP=1` | 573/0 | **579/0** |
| QEMU/TCG `SMP=4` | 583/0 | **589/0** |
| Firecracker/KVM `SMP=1` / `SMP=4` | 560/0 / 570/0 | **565/0 / 575/0** |
| bare metal | 573/0 | **579/0**, `/proc/self/exe` reads `\x7fELF`, `stat /proc/self/smaps` is ENOENT |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK** |
| ring-3: 200 spawn+open+close cycles | — | **no memory lost** (free rose slightly) |
| ring-3: `apk update` + `apk add file` | OK | **OK**, `file-5.47` runs |
| ring-3: `/proc/self/exe` | `-> ls`, opened nothing | **`-> /bin/readlink`, reads `\x7fELF`** |
| clippy (amd64 + AArch64 kernel) | clean | **clean** |

## What is left of the convergence

amd64 still answers `/proc` from its own synthetic view, ahead of the mount.
Four things must move before it can be deleted:

1. **`maps` and `statm`.** Only amd64 serves them, through the *shared*
   `akuma_procfs::render_maps_line`/`render_statm`, because the leaf walk they
   need (`UserAddressSpace::for_each_user_leaf`) is **x86-only** — it is
   `x86_walk_leaves` under the hood, and AArch64 has no equivalent. So this one
   needs a hook: procfs asks a registered callback for a pid's map rows, amd64
   registers its walker, AArch64 registers nothing and keeps today's behaviour.
2. **`/proc/net/dev`** — confirm the shared `net` directory serves `dev`, or
   wire it the same way.
3. **`/proc/<pid>/fd/0`** — the stdin-sink generalization
   (`AKUMA_AMD64_4B_FOLD_BATCH2A.md` § `/proc`): teach the shared sink to find
   the target's real stdin (`get_fd(0)` — a `PipeRead` means write into that
   pipe), which deletes amd64's interception and gains the spawner permission
   check, `delegate_pid` and INTR→SIGINT.
4. Then delete `open_proc`, `render_proc_file`, `render_pid_file`,
   `normalise_proc`, `pid_files`, `proc_is_dir`, `proc_metadata`,
   `proc_rest_of` and the `/proc` arms in `sys_openat`, `sys_newfstatat`,
   `sys_access`, `sys_getdents64` and `sys_read`.

After that, `openat` folds with nothing left to keep on this side except the
four flags glue does not enforce.
