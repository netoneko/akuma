# `ETXTBSY`: a file being executed cannot have its data changed (2026-11-05)

**Status: done and verified on all four kernels:** the tokio-kernel family (Shanghai, Taoxichuan) on amd64 and aarch64 through the gate, and the
two Akuma main kernels (`src/` aarch64, `amd64/`) by booting each and running the probe's `txtbusy` mode (below).

## Why

A process's code is read from its executable's inode *after* `execve` returns: whole-page segment pages are lazy file records filled from
the file on first touch, and a program run by path can be demand-loaded the same way. Nothing stopped a writer from rewriting that file.
Measured on the stock tokio kernel, both machines, before this change: a child that rewrote its own file and then kept running died with
SIGSEGV (wait status 11); a kernel that checks a signature at exec (`akuma-mw-signed-exec`) was checking bytes that no longer ran.
Linux refuses the write (`ETXTBSY`, "Text file busy"); this kernel had no such rule (`git grep ETXTBSY` found nothing).

## Design

- **Where it is enforced: the filesystem, by inode.** `akuma-ext2` asks `text_busy::is_busy(inode)` in every path that changes a regular
  file's data or size: `write_file` (existing file), `write_inode_locked` (all `write_at*`), `truncate_inode_locked`
  (`truncate`/`ftruncate`/`truncate_by_inode`), `fallocate_inode_locked`. The error is the new `FsError::TextBusy`, mapped to `ETXTBSY` (26)
  by `fs_error_to_errno`. This kernel has no open-file object to count writers on, so checking at `open` alone cannot stop a descriptor
  opened for writing **before** the exec; checking at the write can. (Linux fails the *exec* in that case; this is stricter on the write
  and equivalent in effect.)
- **`open(2)` fails too, as on Linux:** `openat` in `akuma-syscalls-glue` refuses `O_WRONLY`, `O_RDWR` and `O_TRUNC` on a busy inode with
  `ETXTBSY`. Before this the `O_TRUNC` path discarded the result of `write_file` (`let _ =`), so a refused truncate would have reported
  success having done nothing.
- **`akuma_primitives::text_busy`:** `TextPin` (RAII; clone for a fork child, drop to release) over an exact 256-slot table keyed on
  **`(filesystem identity, inode)`**: `Filesystem::identity()` is unique per mounted instance and never 0 (0 = not trackable: procfs, the
  memory filesystem); each `Ext2Filesystem` takes the next number at construction. **No overflow
  mode, unlike `InodePin`:** when the table is full `TextPin::try_new` returns `None` and the `execve` fails with `ENOMEM`, so a lookup never
  says "busy" for a file that is not and never "not busy" for one that is. Lookups are lock-free; insert/remove take a spinlock with
  preemption off. A `TextPin` also holds an `InodePin`, so unlinking or renaming over a running binary stays allowed (as on Linux) and the
  blocks live until the last executor exits.
- **Who holds the pin (tokio kernels):** the task record (`Task::{text, interp}`). Taken in `loader::read_image_pinned` **before the image
  is read** (no window between read and mark), installed by `loader::adopt_text` after a successful load, replaced on `execve`, cloned for
  a fork child in `bind_child_task`, released in `zombie_root` (the one idempotent point both the normal exit and the signal-kill purge
  hook reach), and for init by the machines' init spawn. A `#!` script's own file is data and is not pinned, its interpreter is; a dynamic
  image's loader gets `Task::interp`.

## The Akuma main kernels (added the same day)

The filesystem check, `FsError::TextBusy`, `Filesystem::identity` and the `openat` refusal are shared, so what these kernels needed was the pin.
It lives in `akuma_exec::process::ProcessImage::text` (lock-protected, next to `exe`, which has the same lifecycle):

- **Taken before the read, moved in after the install.** aarch64 `do_execve` (`akuma-syscalls-glue`) and amd64 `do_execve` (`amd64/src/usermode.rs`)
  call `akuma_vfs_glue::text_pin(path)` right after the execute-permission check and hand it to `Process::set_text` once the image is
  installed. **On success both functions `eret`/abandon their frame, so a pin left in a local would be permanent**: the shebang branch drops it
  explicitly (the script is data; the recursion pins the interpreter) and the success path moves it into the process.
- **Spawn:** `spawn_process_with_channel_ext` takes it through a new `ExecRuntime::text_pin` hook (the two main kernels register
  `akuma_vfs_glue::text_pin`; the tokio kernels and the tests register an inert one, since their mark lives in the task record).
- **Fork, vfork, thread:** `Process::inherit_from` (the one constructor behind all three) clones the pin.
- **Exit:** `publish_child_exit`, the helper every exit site routes through, calls `Process::release_text`, so the file is writable again when
  the process exits, not when a `wait4` finally reaps the zombie. Dropping the `Process` is the backstop.
- **Not changed:** `akuma-bkl`, the main kernels' lock. It already masks interrupts across the whole ticket wait, with a re-check after masking,
  so the nested-ticket deadlock in `TAOXICHUAN_BKL_NESTED_TICKET_DEADLOCK.md` cannot happen there; its own comment records a rare leaked ticket
  with the same signature (`owner == 0`, `next_ticket == now_serving + 5`, four cores spinning) whose root cause it calls "not yet pinned".

**Verified by booting both and running `/bin/mwprobe txtbusy` (the probe copies itself to `/tmp` and runs the T cases on the copy):**
`RESULT TXTBUSY PASS`, T-1..T6 all passing, on the aarch64 Akuma kernel (under HVF, with its full boot self-test suite: 166 PASS, 0 FAIL) and on
the amd64 Akuma kernel (QEMU microvm, TCG). How to repeat: clone `disk.img` (`cp -c`), `debugfs -w` the probe into `/bin`, boot with
`INSTANCE=9` (snapshotted, ssh on 3122) and `ssh ... /bin/mwprobe txtbusy`; amd64: `DISK=<img> INIT=/bin/mwprobe INITARGS=txtbusy sh amd64/run.sh`.
The pin-taking sites were each compiled but only these two paths were *exercised* (`execve` and the probe's spawn); the kernel-initiated
`spawn_process*` route was not given its own probe.

## Verification

Host: 5 tests for the table (pin/clone/drop balance, a full table refuses and never lies, 4 threads), 2 for ext2 (every write route refused
while pinned; unlink and rename-over still allowed with the data readable; refusal lifts on drop).

Gate (`sh shanghai/mw_gate.sh all`, SMP=4, both machines, 206 PASS lines: control 17, `middleware` 23, `no-dynamic-loading` 39,
`no-dynamic-elf` 6, `signed-exec` 18, each x2). Cases T0-T6 and X1/S10 run in the control and `signed-exec` kernels; the other kernels
share the code path and were not given their own copies of the cases:

| | |
|---|---|
| T0 | a file nobody executes can be opened for writing |
| T1, T3b | a running executable: `open(O_WRONLY)` and `open(O_TRUNC)` -> `ETXTBSY` |
| T2 | a descriptor opened for writing **before** the program started: a write through it -> `ETXTBSY` |
| T3 | `ftruncate` of a running executable -> `ETXTBSY` |
| T4 | its bytes are untouched |
| T5 | after the process exits the same descriptor can write again (the pin is released) |
| T6 | renaming a new file over a running executable is allowed |
| X1, S10 | the child that used to die with SIGSEGV now gets `ETXTBSY` from its own `open` and keeps running |

## Found by a test run on the way (fixed)

The first version keyed the table on the inode number alone. The pre-commit test run failed three unrelated `akuma-ext2` tests with
`TextBusy`: tests mount separate filesystems that all number their first files alike, and my new tests had pinned those numbers. The same
flaw would have made a binary running from one mount block writes to same-numbered files on every other mount, and it would have
broken the Akuma main kernels (several ext2 mounts) the moment the pin was wired there. Fixed by the `(identity, inode)` key above, with
a test for it (`a_busy_inode_on_one_filesystem_does_not_block_the_same_number_on_another`).

## Observed on the way (not changed)

- A refused `write(2)` still **advances the descriptor's offset** (`sys_write` reserves the offset before writing): the probe measured
  offset 0 -> 1 on a write that failed with `ETXTBSY`. Linux leaves the offset alone on error. Pre-existing, general, not fixed here.
- The gate image was 32 MB and the probe binary is 5 MB; five copies filled it and a `/tmp` write silently produced an empty file.

## Open

- On the Akuma main kernels the binaries the kernel starts on its own (init, `herd`) are pinned through the spawn route, which has no probe of its
  own; the `execve` route does.
- Only the **root** filesystem's executables are pinned in the tokio kernels (`loader::read_image_pinned` uses `root_fs()`); a binary run
  from another mount is not marked. The Akuma main kernels resolve the mount from the path, so a binary on any mounted ext2 is marked.
- A writable `MAP_SHARED` mapping made **before** the exec is not stopped by this (writes go through the mapping, not `write(2)`); a
  signed image is immune (it is mapped from the verified bytes), an unsigned one is not. No probe for it.
- An executable that is itself a shared library mapped by the dynamic loader is not pinned.

## Background

`crates/akuma-primitives/src/text_busy.rs`; `crates/akuma-ext2/src/ext2.rs` (`deny_if_text_busy`); `crates/akuma-syscalls-glue/src/fs.rs`
(`openat`); `crates/akuma-tokio-kernel-core/src/{loader,proc,task}.rs`; probe `shanghai/mwprobe/mwprobe.rs` (`txtbusy_cycle`).
Context: `shanghai/docs/MIDDLEWARE_COMPOSITION.md` (why signing needed it).
