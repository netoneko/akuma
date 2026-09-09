# amd64 C1 step 4b, batch 2a: aligning the local arms with glue's semantics

**Date:** 2026-09-09
**Status:** landed (amd64 only — nothing AArch64-side is touched).
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessor:** `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH1.md` — the five
path-only `*at` arms.
**Plan:** `proposals/NEXT_AGENT_AMD64_4B_VFS_FOLD.md` step 2, batch 2
(`openat`/`close`).

Batch 2's arms are `openat` and `close`, and asking what a folded one would
actually touch — the method that found batch 1's `fs::mark_initialized` gap and
the `sys_setsockopt` SMAP fault — turned up **four prerequisites and one hard
blocker** before a single arm could move. This document is those prerequisites,
landed and verified; the fold itself is 2b.

**Align first, then swap.** Every item below makes this target behave the way
glue's arm already behaves, while the local arm is still the one running. Each
is separately observable, so the fold that follows can be a behavioural no-op —
and two of them turned out to be live bugs on their own.

## 1. A console descriptor is a console (`fd::console_end`)

**Two spellings reach the same device.** The by-number one is this target's
own: an *unbound* 0/1/2 is the console, which is what a task with no stdio
descriptors has (the boot suite's kernel row). The descriptor one is the
tree's: `SharedFdTable::with_stdio` puts `Stdin`/`Stdout`/`Stderr` at 0/1/2,
and glue's `openat` hands out the same family for `/dev/tty`.

Only the first existed here. Every arm asked for it as
`fd < FIRST_FILE_FD && !is_bound(fd)` — a test that is **false for a process
whose stdio is bound**, so the write fell through to `sys_write_file`, which
has no arm for a `Stdout` descriptor and answers `EBADF`.

Measured, not reasoned: with the variant arms disabled, `INIT=/bin/hello`
prints `-- running /bin/hello --`, then `-- init exited --`, and **not one byte
of the program's own output** — the ELF probe's `write(1)` is refused. With
them, its line appears. That A/B is the negative control for the whole item.

`console_end(fd) -> Option<ConsoleEnd>` answers both spellings in one place;
`read`, `write`, `pread`, `lseek`, `fstat`, `fstatfs`, `poll_ready` and
`ioctl` ask it instead of open-coding the number test. `ioctl` keeps
`fd < FIRST_FILE_FD` as its first term on purpose — a spawned child's 0/1/2
are **pipes**, and answering `TCGETS` on them is what makes `isatty(0)` true
for an interactive shell over ssh — and gains `/dev/tty` as a third term.

## 2. Registered processes get `with_stdio()` again

The 4b flip had set the default table to `SharedFdTable::new()` (empty), with a
comment recording exactly why: the stdio triple made `is_bound(1)` true and
"every test process went silent". That was a missing *arm*, not a wrong table,
and item 1 supplies the arm.

**Flipping it back is a prerequisite for the fold, not a tidy-up.** Glue
allocates with `alloc_fd`, which is `alloc_fd_from(0)`. With 0/1/2 absent, the
first `open` in such a process returns **fd 0**, and every later write to fd 1
lands in whatever that process opened next. Occupying the triple is what makes
the tree's allocator safe on this target.

## 3. `dev_node_of` knows glue's `/dev` descriptors

amd64's `openat` answers `/dev/null` with a `File` carrying the path; glue's
answers it with a dedicated `FileDescriptor::DevNull` (and `DevZero`,
`DevUrandom`, `DevTty`). `read`/`write`/`lseek`/`fstat` all route through
`dev_node_of`, so mapping the variants back to the node name there is the
whole of what the still-local arms need in order to serve a descriptor the
folded `openat` will hand them.

## 4. `O_APPEND` re-derives the position per write

`sys_openat` seeded a starting cursor and `sys_write_file` wrote at the
descriptor's own position, which its comment pinned as a divergence: two
descriptors appending to one file both start at the same offset and the second
clobbers the first. Glue's `sys_write` derives the append position from the
live file size **per call**, and glue's `openat` seeds no position at all — so
folding `openat` without this would make every `>>` start at 0.

Now aligned, with the check that separates them: seed `AAA`, open two append
descriptors, write `B` through the first and `C` through the second. Five bytes
`AAABC` with the fix; four bytes `AAAC` without it, which is what the
negative-control build produced.

## What still blocks the fold (2b)

- **The boot suite has no process identity.** Glue's `openat` ends in
  `if let Some(proc) = current_process_shared() { … } else { Err(ESRCH) }`, and
  the suite runs on the boot task, which is registered nowhere — so a folded
  `openat` answers `ESRCH` to every one of the ~50 kernel-side checks in
  `fd::smoke_test` and `proc_consistency_check`. Batch 1 slipped past this only
  because its five arms are path-based and need no descriptor.

  **Prototyped and measured 2026-09-10, then reverted**, because the answer is
  "a batch, not a line". The mechanism is available and cheap:
  `akuma_exec::process::make_test_process(pid)` is a `pub fn` in the crate that
  owns `Process` (unconditional, LTO drops it when unused), so the suite can do
  what the AArch64 `register_at_syscall_process` helper does —
  `register_process` + `register_thread_pid(current_thread_id(), 1)` around the
  fd block, handing pid 1 back before `run_init` claims it. **pid 1 and not a
  spare number**: `usermode::current_pid` already answers 1 for an unmapped
  thread, and `/proc/self` resolves through it, so any other pid points
  `/proc/self` at a process the synthetic view has never heard of.

  With that in place the suite went `572 passed, 2 failed`, and both failures
  are the batch's real content:

  1. `proc: all three refuse a file /proc does not serve` — with pid 1
     registered, the **mounted `ProcFilesystem`** starts answering for paths the
     synthetic view deliberately refuses (`/proc/self/smaps`). Giving the boot
     row an identity is exactly what wakes the mounted filesystem up, so the
     `/proc` serving split has to be decided *with* the identity, not after it.
  2. `identity: probe teardown leaks nothing` — one page. `make_test_process`
     builds a real `UserAddressSpace`, and unregistering it does not return
     every frame within the window that check measures.

  The alternative — moving that coverage into a ring-3 probe binary of the
  `userspace/amd64/hello` shape — remains open, and is where the plan says a
  folded arm's verification belongs anyway.
- **`/proc` interception: a decision, and one path that should go the other
  way.** `fd.rs` answers `/proc` from this kernel's own spawn table. The
  mounted `ProcFilesystem` renders from `akuma-exec`'s table instead, is asleep
  during the suite window (no registered process — see the identity item above,
  which is what wakes it), and on the metal's persistent root its mount fails
  outright (`[FS] WARN: /proc mount failed`, pre-existing). So most of the
  interception stays ahead of `to_glue` for now, per path.

  **`/proc/<pid>/fd/0` is the exception, and this document first said the
  opposite.** It claimed the path "hands back a pipe descriptor that no
  filesystem can produce". That is wrong: `akuma-vfs-glue`'s `ProcFilesystem`
  serves it already — its header names it in the first three lines, and
  `write_fd_data` parses `<pid>/fd/0`, checks the caller is the target's
  spawner, and calls `akuma_exec::process::write_to_process_stdin`. It is
  ordinary bytes through `write_at`, and it is how the **AArch64** kernel
  serves the identical `open("/proc/<pid>/fd/0", O_WRONLY)` in
  `userspace/sshd/src/protocol.rs`.

  What actually differs is the **stdin sink**, not the VFS.
  `write_to_process_stdin` delivers into the target's
  `StdioBuffer`/`ProcessChannel`; amd64's spawned children read fd 0 from a
  **pipe** (`bind_stdio`, C2 slice 6), so bytes delivered that way land where
  nobody reads. Hence the short-circuit that hands back the pipe's write end.

  Lifting it is therefore work in `akuma-exec`, and it *deletes* code here:
  teach the sink to find the target's real stdin — the target's own `get_fd(0)`
  already names it, and a `PipeRead(id)` means "write into that pipe" — then
  stop `sys_write_file` dropping `/proc` writes (`proc_rest_of` → "Synthetic:
  accepted and dropped", which the `write` fold fixes anyway), and delete
  `open_proc`'s `/fd/0` case with `stdin_pipe_for_pid`. amd64 gains three
  things the shared path already carries and the interception has never had:
  the spawner permission check, `delegate_pid` indirection, and the terminal
  line discipline that turns an INTR byte into `SIGINT` on the foreground
  process group instead of delivering it as data.
- **Four `open` flags live here and nowhere else.** `O_DIRECTORY`, `O_EXCL`,
  `O_NOFOLLOW` and the `O_TMPFILE`-adjacent directory guard are enforced by
  amd64's `sys_openat`; glue's arm enforces none of the first three (it always
  resolves symlinks, ignores `O_EXCL` on an existing file, and hands a regular
  file to `open(…, O_DIRECTORY)`). They are Linux's answers and belong in glue
  — **but that is an AArch64 behaviour change, and the AArch64 verification
  loop does not run on this machine**: under HVF the boot asserts in QEMU
  (`hvf_handle_exception … isv`, the writeback-form MMIO trap `CLAUDE.md`
  warns about) and under TCG it panics in a pre-existing self-test
  (`process_tests.rs:3411`, `test_spawn_ext_passes_env: 1 of 2 cases failed`,
  reproduced on a tree with no AArch64-side changes). Until that loop works,
  the flags stay on this side, in the shim.
- **`close` cannot fold at all yet: the pipe tables are different.** Glue's
  `sys_close` closes a `PipeRead`/`PipeWrite` through
  `akuma_syscalls_glue::pipe`, whose table is its own `static`. This target has
  its own — `amd64/src/pipe.rs`, a `PipeTable<()>` in its own `static`, with
  its own ids. A folded `close` would hand this kernel's pipe id to the other
  kernel's table: not an error, a **wrong pipe**. Sharing the pipe table is its
  own extraction and its own batch.

## Verification

| gate | before 2a | after 2a |
|---|---|---|
| QEMU/TCG `SMP=1` | 553/0 | **563/0** (+10 checks) |
| QEMU/TCG `SMP=4` | 563/0 | **573/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK** |
| `INIT=/bin/hello` prints its line | yes (unbound stdio) | **yes (bound stdio)** |
| negative control: variant arms off | — | **silent init, as predicted** |
| negative control: per-write append off | — | **`AAAC`, 2 checks red** |

## Background

- `docs/archive/AKUMA_AMD64_4B_FLIP.md` — the refcount flip, and the
  `SharedFdTable::new()` decision item 2 reverses.
- `docs/archive/AKUMA_AMD64_C2_SLICES_6_AND_7.md` — `bind_stdio`, which is why
  spawned children were never affected by item 1's bug.
- `proposals/NEXT_AGENT_AMD64_4B_VFS_FOLD.md` — batch 2b and the rest.
