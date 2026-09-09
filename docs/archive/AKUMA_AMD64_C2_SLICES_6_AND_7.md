# amd64 C2, slices 6–7: stdio becomes descriptors, and three things slice 5 broke

**Date:** 2026-09-09
**Status:** landed, uncommitted. QEMU/TCG `SMP=1` **533/0**, `SMP=4` **543/0**,
Firecracker `SMP=4` **530/0**, bare metal **533/0**, host **1360/0**.
**Parent:** `proposals/NEXT_AGENT_AMD64_C2_FD.md`, slices 6 and 7.
**Predecessor:** `docs/archive/AKUMA_AMD64_C2_SLICES_1_TO_4.md` (which also
covers slice 5).

Every tally above is its baseline plus the eighteen checks this work added
(seventeen on the metal, which correctly skips the `/dev/vda` one — it has no
virtio-blk). Nothing regressed.

---

## Slice 6 — a spawned child's stdio is descriptors, not a router

`Spawn` carried four fields that existed only because a spawned child's fd
0/1/2 were **routed by number**, below the descriptor table:

```rust
stdout_pipe   // the child writes fd 1/2 here
stdin_pipe    // the child reads fd 0 here
borrowed_io   // ...unless it is a fork child, which must not close them
console_io    // ...unless its parent runs on the serial line
```

Every unbound-0/1/2 read, write and poll asked `spawn_stdio(proc_slot)` which
pipe served this task; the exit path closed the child's stdout end by hand,
guarded by `borrowed_io`; the reap freed the stdin pipe by hand. Slice 6 gives
the child **real descriptors** at birth (`fd::bind_stdio`) — fd 0 the read end
of its stdin pipe, fd 1 *and* fd 2 the write end of its stdout pipe — and every
one of those jobs is then done by machinery that already existed:

| the field did | what does it now |
|---|---|
| routing | `pipe_read_id` / `pipe_write_id`, like any other pipe |
| `borrowed_io` | `inherit_fds` bumps the shared ends' refcounts; only the last name's close reaches the pipe |
| `console_io` | a console parent's row is empty, so the child's 0/1/2 are unbound, which now means exactly one thing |
| exit EOF | `close_owned_by` unrefs the child's ends before `spawn_record_exit` — a manual `close_write` there would now be a *second* close of a ref the child no longer holds |

`spawn_stdio`, `current_stdin_pipe` and `current_stdout_pipe` are deleted.

### `stdin_pipe` stays, and that is the interesting half

Three fields went; the fourth is a carried decision. The stdin pipe's *read*
end is the child's fd 0 and dies with the child's row. Its **write** end is
reached by *path* — `sshd` opens `/proc/<pid>/fd/0` — and a path is not a
reference, so nothing refcounts it. Left to the end counts alone, a spawn whose
stdin nobody ever opened (every `run_sh_capture` in the boot suite) keeps one
writer forever and the pipe is never destroyed: a leak against a 64-pipe
ceiling. The reap is the one place that knows the child is gone, so the reap
drops it.

What changed is *how*. The old reap called `pipe::free`, which destroys the
pipe whatever the end counts say — including under `sshd`'s still-open
`/proc/<pid>/fd/0` descriptor, whose later `close` then landed on a pipe that
was already gone. (It landed *harmlessly*, and only because
`PipeTable::create` hands out a monotonically increasing id and never recycles
one; that is the property the old code was relying on without saying so.) It is
now `pipe::close_write`, and the end counts decide — the rule every other pipe
in the module already follows. A strict improvement, in the one field the slice
could not delete.

`stdin_pipe_for_pid` therefore stays on the spawn row rather than moving to the
child's fd table, and the direction of the question is why: `sshd` asks for the
end **it** writes — the one no descriptor names — and the child's fd 0 is the
*other* end. Reading the child's table for it works only for as long as fd 0
still names that pipe; a shell that redirects its own stdin, or a child already
past `clear_table_mirror` on the exit path, would answer `ENOENT` to a live
bridge.

### fd 2 is a second *name*, not a second description

`bind_stdio` binds fd 2 to the **same `FILES` entry** as fd 1, `refs = 2` —
exactly what `dup2(1, 2)` would build. This is load-bearing and it is where the
first attempt at this slice went wrong.

The old router answered fd 1 *and* fd 2 out of `Spawn::stdout_pipe`, so
`prog > file` kept sending stderr to the session. A version of this slice that
binds only fd 0 and 1 and answers fd 2 by *looking up fd 1* puts the second
source of truth back in, one indirection along — and it breaks at exactly the
redirect it was meant to survive, because after `dup2(f, 1)` fd 1 is a file and
the lookup finds no pipe. Two names on one description reproduce the old
behaviour and survive the redirect: one name goes, the end stays open under the
other.

Measured on the committed kernel, over ssh:

```
sh -c "echo O; echo E 1>&2" > /tmp/o.txt
  HEAD:  file has "O";  E never appears anywhere      (stderr lost to the console)
  now:   file has "O";  E comes back over the session
```

### The regression that made this a rescue, not a review

The first pass at slice 6 failed six boot checks — the whole `redirect` group —
with `echo REDIROK > /tmp/redir.txt` exiting 1 and creating no file. The cause
was not in any of the code the slice touched.

`busybox ash` saves a descriptor before redirecting onto it:

```c
newfd = fcntl(from, F_DUPFD_CLOEXEC, 10);
err = newfd < 0 ? errno : 0;
if (err != EBADF) { if (err) ash_msg_and_raise_perror(...); close(ofd); }
```

`EBADF` is the one error it forgives. While fd 1 was unbound, `fcntl` resolved
nothing and answered exactly that, so ash recorded "there was no fd 1" and
carried on to the `open` and the `dup2`. `echo x > file` worked **by accident**.
The moment `bind_stdio` gave the child a real fd 1, the same call resolved,
fell through `sys_fcntl`'s missing `F_DUPFD` arm to `_ => EINVAL`, and ash
raised — *before* `openredirect` ran, so the file was never created at all.

`F_DUPFD`/`F_DUPFD_CLOEXEC` are now implemented (`fd::dup_from`). The general
lesson is worth more than the fix: **making a descriptor real makes every
descriptor operation on it reachable.** Three more turned up the same way and
are fixed here:

- `fstat` on a pipe or socket answered `EBADF` — `entry.file()` is `None`, so
  the size/path arm fell through. Now `S_IFIFO` / `S_IFSOCK`. A program that
  `fstat`s its own stdout is ordinary, and being told the descriptor it is
  holding does not exist is not an error it recovers from.
- `lseek` on a bound 0/1/2 answered `EBADF` because its guard was `fd < 3`
  rather than `fd < 3 && !is_bound(fd)`. Now `ESPIPE` for a pipe or socket —
  the *seekability* answer, which musl's `FILE` layer treats as "unbuffered"
  where `EBADF` is fatal.
- `sys_spawn`'s `spawn_process_task` failure path dropped an unregistered
  `Arc<SharedFdTable>` holding three descriptors. `SharedFdTable::drop` runs
  `close_all()`, which fires the `ExecRuntime` close hooks — several of which
  were `not_wired!`, i.e. `panic!`. See slice 7.

## Slice 7 — `/proc` moves nothing, and the hooks stop being a landmine

The plan says slice 7 moves "only the parts § *the plan will get wrong* says
can move". Re-checked against the code as it now stands, that section is four
bullets and all four are still *cannot*:

- **`meminfo`, `mounts`, `net/dev`** read this target's PMM, mount table and
  interface list; the crate's same-named files read the AArch64 kernel's. A
  shared format is not a shared source. Unchanged.
- **per-pid `stat`/`status`/`cmdline`** are blocked by the boot suite running
  before `run_init` — the trap that has now bitten three times. Unchanged.
- **`self/maps`, `self/statm`** have no equivalent in the crate's procfs;
  serving them for any pid is possible but is a capability change, which the
  plan says must not ride along. Not done.
- **`/proc/<pid>/fd/0`** was recorded as blocked by "the same wall as `Spawn`'s
  four stdio fields, which C2 is the thing that removes". **That reason is now
  wrong, and its replacement is narrower:** three of the four fields are gone
  and the bridge is still local, because it needs the stdin pipe's *write* end
  — the one no descriptor names, the same field slice 6 kept. The wall is one
  field wide, not four.

So the slice's substance is the `ExecRuntime` table, where the plan's list of
"nine `not_wired!` stubs C2 unblocks" had already drifted (the three pipe hooks
were wired in slice 4). Wired now:

- **`remove_socket`, `socket_clone_ref`** — by the argument the pipe hooks
  already carry. `fd.rs`'s `alloc_socket_fd` builds a
  `FileDescriptor::Socket(idx)` whose payload *is* an `akuma_net::socket`
  index, and slice 4 mirrors that descriptor into the registered table. The two
  namespaces do not meet; there is one socket table.
- **`read_at`** — `fs::read_at`, the surface slice 5 rebuilt every `read` and
  `pread` on this target on top of. It is the same byte path ring 3 gets, not a
  second one.

Not wired, with **narrower reasons than "C2"**: `flock_release` (nothing
dispatches `flock(2)` here), `resolve_file_id` and `read_at_by_inode` (they
name a file by `(mount id, inode)`; this target has one filesystem and no mount
table — inventing a pair is the `[0,0,0,0]` zero-page class of bug).

**Ten stubs are left, from 16 when the plan was written, and none of the ten
still says "C2".** That is the useful outcome of walking the table: a stub
whose stated reason is a *step* silently stops being true when the step lands,
and nothing in the type system notices — the plan's own list of "nine C2 stubs"
had been wrong since slice 4.

The point of wiring the two socket hooks is not a feature. It is that a
`SharedFdTable` holding a socket was one forgotten `clear_table_mirror` away
from a kernel panic, and slice 6 found a path that forgot. Wired, that mistake
is a double close instead of a dead machine.

The module header's claim that the whole fd-lifecycle family "cannot fire" was
true when written and is not any more — slice 4 made it fire — and has been
rewritten rather than left as a comment that is now the opposite of the code.

## The three things slice 5 broke, found from ring 3

The plan's § "The lesson C1 step 3 paid for" says a folded arm is verified by a
real ring-3 caller, not by a boot check. It was right three more times. None of
these was visible to the boot suite, and all three were found by typing
ordinary shell into an ssh session.

### 1. `O_TRUNC` never truncated

```
echo AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA > f   # 31 bytes
echo x > f
wc -c < f
  HEAD:  31        # file is "x\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
  now:    2
```

Slice 5 replaced the close-time whole-file persist with
`write_at(path, 0, &[])` at open. `akuma_ext2::write_at`'s **first statement**
is `if data.is_empty() { return Ok(0) }` — before it resolves the path, creates
a missing inode or touches a length. It reported success and did nothing.

The new head of a shorter write therefore sat in front of the old tail. Silent
data corruption, in the commonest shell idiom there is.

This is the **same short-circuit slice 5 already paid for in the other
direction**: its existence probe was a zero-length `read_at`, which also
succeeds before resolving anything, so every missing file opened successfully
(its write-up's "The probe bug"). One byte fixed the read. The write needed an
API without the short-circuit at all: `write_file(path, &[])`, which truncates
an existing inode and allocates a missing one, and is a whole-file write of
**zero** bytes — no heap, no cache.

### 2. `O_CREAT` never created a zero-length file

```
: > f          ; ls -l f     ->  HEAD: No such file or directory
false 2> err   ; ls -l err    ->  HEAD: No such file or directory
```

Same cause, same fix. Nothing wrote bytes, so nothing ever reached the
filesystem. `2>` on a command that prints no errors is the everyday form of
this.

The boot suite could not see either one, and the reason is worth keeping: its
`redirect` checks only ever write to a path that did not exist yet, and always
write something. Five new checks close that — `>` over a longer file, its
leftovers, `: >`, its length, and `2>` — and all five were confirmed red
against the unfixed openat before being kept.

`O_CREAT | O_EXCL` on an existing path is now `EEXIST` too. It was silently
succeeding, which tells two callers claiming the same lock file that they both
won.

### 3. `/dev/null` (and `/dev/zero`, and the entropy nodes)

`/dev` on this target is synthetic — `akuma_vfs_glue::dev_node` answers
`ls -la /dev`, `stat` and `getdents` — but nothing had ever wired a
**descriptor** to one. Every `open` under `/dev` fell through to the ext2 path,
where `/dev` is not a directory and the node is not a file:

```
echo hi > /dev/null
  HEAD:  sh: write error: No such file or directory      (exit 1)
cat /dev/null
  HEAD:  cat: can't open '/dev/null': No such file or directory
```

Before slice 5 the write case worked *by accident* — the bytes went into the
descriptor's buffer and were dropped at `close` when the whole-file persist
failed, one `[close] persist failed` console line per use. Slice 5 deleted the
buffer and the write went straight at `write_at`, which cannot resolve `/dev`.

**`ls > /dev/null` still looked fine**, which is how a broken `/dev/null`
survived both a 543-check boot suite and a ring-3 harness that run `>/dev/null`
on almost every line: busybox `ls` swallows its write error. `echo` does not.

The four character nodes are now served for real. The descriptor carries the
node's path and `dev_node_of` — a `&'static` name out of the table, no
allocation — is what `read`, `write`, `lseek` and `fstat` ask instead of the
VFS:

- `null` reads EOF and accepts every byte;
- `zero` reads zeros (and *fills* the buffer — the check seeds it `0xAA` first,
  which is the difference between that assertion and a no-op);
- `random`/`urandom` read the same entropy `getrandom(2)` gets, and are
  deliberately not distinguished: there is one source and no entropy accounting
  to block on, so a "blocking" `/dev/random` would be a fiction with a hang in
  it;
- `tty` is the console.

A **block** node is `ENODEV` rather than a fall-through — present, and this
target will not serve it. `is_regular_file` refuses a device node so
`mmap(MAP_PRIVATE, fd)` cannot ask `file_bytes_at` for bytes it has no inode to
hold.

**The ordering is the substance.** The device arm has to run *before* the
existence probe: a node has no byte path, so the probe answers "absent" for
every one of them. Behind the probe, `open("/dev/zero")` was `ENOENT` while
`open("/dev/null", O_CREAT)` — which skips the probe's guard — went on to try
to *create* a node. Two different wrong answers to the same question, from one
ordering; the self-test caught it as an `EBADF` on the first read.

## A method correction, recorded because it nearly reached the tree

The first version of this document's `/dev/null` paragraph said the pre-fix
kernel "printed its listing to the console with the redirect silently ignored.
Measured, not inferred." **It was not measured.** The check had been run as

```sh
FEATURES=no-tests INITARGS='sh,-c,ls /bin > /dev/null; echo rc=$?' sh amd64/run.sh
```

and `run.sh` splices `INITARGS` into the kernel command line, which is
whitespace-delimited: only `sh` ever reached init, and everything after the
first space became unrelated cmdline tokens. The console output being read as
evidence came from a plain interactive `sh`, not from the command under test.

**`INITARGS` cannot carry spaces.** A one-shot applet (`INITARGS=uname,-a`) is
what it is for; anything with a redirect, a `;` or a quoted string has to be
run over ssh against a booted guest. The corrected measurements above were
taken that way, against `git checkout HEAD -- amd64/src/` in the worktree.

## Verification

| gate | baseline | now |
|---|---|---|
| QEMU/TCG `SMP=1` | 515/0 | **533/0** |
| QEMU/TCG `SMP=4` | 525/0 | **543/0** |
| Firecracker `SMP=4` | 512/0 | **530/0** |
| bare metal | 516/0 | **533/0** (skips `/dev/vda`: no virtio-blk) |
| host tests | 1360/0 | **1360/0** |
| `amd64_mem_trials --smp 4` (TCG **and** Firecracker) | 8/10, 0 unexpected | **8/10, 0 unexpected on both arms** |
| `amd64_ring3_check --smp 1 -n 60` | OK | **OK** — 60/60, `free` unmoved, heap drift +15 kB (tolerance 8192) |
| AArch64 `cargo clippy --release` | clean | clean |
| `no-tests` clippy + boot | boots | boots |

Every new check was **falsified before being kept**: the five redirect checks
against the unfixed `sys_openat` (5 red, `got 0x1f` — the 31 bytes), the fd-2
check against a `bind_stdio` that binds only 0 and 1 (1 red).

Ring-3 witnesses, on QEMU **and** on the metal:

- `> /dev/null`, `2> /dev/null`, `cat /dev/null`, `head -c 8 /dev/zero`, two
  reads of `/dev/urandom` that differ;
- `>` truncating a longer file to two bytes; `: >` leaving a zero-byte file;
- stderr reaching the session across `sh -c '…' > file`;
- `cmd | cmd | cmd`, `busybox yes | busybox head -n 1` (EPIPE, terminates);
- `readlink /proc/self/fd/7` after `exec 7< file`;
- **the heap ladder**: 45 MB (QEMU) and 68 MB (metal) written through one held
  fd with `Cached:` flat either side — 1895 → 1894 kB on the metal. The
  whole-file cache class stays dead.
- **~320 ssh sessions on the metal**, `( ls /bin >/dev/null; ls /bin
  >/dev/null ); echo r$$` each: `free` unmoved, `ps` rows constant at 5, kernel
  heap plateauing at 2.6–3.3 MB rather than climbing. Two sessions failed to
  connect (~0.6%), which is the pre-existing signature-A intermittent lockout
  (`project_amd64_sshd_intermittent_lockout.md`), not new.

## What is left of C2

`fd.rs` is not retired. What this pass changed is which reasons are left:

- The **mirror still owns no references.** `fork_table_mirror` copies without
  bumping and `clear_table_mirror` must still run at exit, because `FILES` is
  the refcount authority. Flipping that is the remaining structural step, and
  it is what would let `close_all()` be the teardown instead of something to be
  defended against. Both socket hooks and all three pipe hooks are now wired,
  which is the prerequisite.
- **`Spawn` is three fields**: `pid`, `stdin_pipe`, `exec_slot`. Deleting it
  needs the stdin bridge to stop being reached by path.
- **`wait4`'s fold** still needs the `CHILD_CHANNELS`-vs-`Process::exited`
  decision the plan sets out; nothing here made it.
- **`fork`/`clone`** are still out of scope — they need an x86 arm of the
  ring-3 entry path.

## Background

- `proposals/NEXT_AGENT_AMD64_C2_FD.md` — the plan and its slice order.
- `docs/archive/AKUMA_AMD64_C2_SLICES_1_TO_4.md` — slices 1–5, including the
  zero-length `read_at` probe bug this pass found the mirror image of.
- `proposals/AMD64_FD_WHOLE_FILE_HEAP.md` — the crash slice 5 removed, and the
  ladder re-run here.
- `docs/archive/AKUMA_AMD64_STEP5B_SLICE3_PROCFS.md` — the `/proc` union, and
  the four reasons slice 7 re-checked.
- `docs/archive/AKUMA_AMD64_STEP5B_SLICE2_LIFECYCLE.md` — `Spawn` 9 → 6; this
  takes it to 3.
