# amd64 C1 step 4b, batch 3a: `read`, `pread64`, `write`, `lseek`, `getdents64`

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessor:** `AKUMA_AMD64_4B_FOLD_BATCH2D.md` (`openat`), which ends with
the line this batch is: *"`read`, `pread64`, `write`, `lseek` and `getdents64`
are the cluster that reads what `openat` now produces, and they are the natural
next fold."*

Five arms moved. `amd64/src/fd.rs`: **+324 / −589**, 3 846 → 3 581 lines.

What is left of each on this side is a preamble, and every one of them is a
divergence stated rather than assumed:

| arm | what this kernel keeps | why it cannot move |
|---|---|---|
| `read` | the serial console (`console_end`), the `MAX_IO` clamp | glue's `Stdin` arm reads a `ProcessChannel`, and no process here has one |
| `pread64` | `ESPIPE` for a pipe/socket/console, the clamp | glue answers `_ => EBADF`; musl's `FILE` layer falls back on the first and gives up on the second |
| `write` | the serial console + `WRITE_SEQ` instrumentation, **and the `O_ACCMODE` refusal** | glue's `Stdout` arm writes through a channel; and its `File` arm does not check the access mode at all (see §5) |
| `lseek` | a `/dev` character node | Linux seeks one to the offset asked for; glue answers `0`/`ESPIPE` |
| `getdents64` | **nothing** | — |

`fstat`/`newfstatat` are deliberately **not** in this batch: `struct stat` is a
third vocabulary and needs its own hop (§7).

---

## 1. The two prerequisites, neither of which was in the plan

This is the fourth batch in a row whose real content was a prerequisite found by
asking what a folded arm actually touches. Both of these had to land before a
single arm could move, and both were invisible until something broke.

### 1a. The untimed park had no backstop in the shared crate

`amd64/src/sched.rs` gives every park with **no deadline of its own** a 1 s
tripwire (`BACKSTOP_US`), and its comment says exactly why:

> *AArch64 parks untimed all the time and has an interrupt-driven scheduler to
> recover; this target reaches its scheduler only by being called, so a lost
> wake is terminal here in a way it is not there.*

`akuma-syscalls-glue` parks `schedule_blocking(u64::MAX)` in **22 places** —
including the `PipeRead` and `PipeWrite` arms this batch folds `read` and
`write` onto. Folding onto them while the backstop was local to `sched.rs` would
have traded a 1 Hz degradation for a silent, unrecoverable hang, on the paths a
shell pipeline runs through. The tripwire that survives review is the one that
covers the code that was written after it.

So the mechanism moved into `akuma-threading`:
`park_indefinitely()` + `set_untimed_park_backstop_us()` +
`untimed_park_backstop_wakes()`. Unregistered — which is every AArch64 build —
`park_indefinitely()` **is** `schedule_blocking(u64::MAX)`, so the other kernel
is unchanged by construction and a host test asserts that degradation contract.
The number is still amd64's; `boot::install_shared_sinks` registers it, on both
boot protocols, which is that function's founding reason.

`sched::block_current` keeps only its own `BLOCKS` counter now, and
`sched::backstop_wakes()` forwards to the crate — so that number covers all 22
glue arms and not just the four this kernel wrote itself.

Four host tests, over a split-out `untimed_park_deadline(backstop, now)`, because
the decision is the whole of it and both ways of getting it wrong are silent:
`0` must mean *no backstop* (not "a deadline of now", which turns every wait loop
in the tree into a spin) and the addition must **saturate** (a wrap puts the
deadline in the past, with the same result).

### 1b. amd64 had no prefault hook — and `apk` said the database was corrupt

**This is the batch's real finding.** `amd64/src/uaccess.rs`'s header said, and
it was true when written:

> *There is no "is it mapped" walk here and no prefault: this target has no lazy
> user regions yet, so the copy either succeeds or faults, and the fault is
> recovered. When lazy regions arrive, the walk goes here.*

Lazy regions arrived with **B1, on 2026-09-07**, and nothing came back to that
sentence — because nothing had to. This kernel's own `copy_to_user` still just
copies, and `idt.rs` services the `#PF` inline.

The shared arms do not work that way: every one of them opens with
`validate_user_ptr`, which is
`akuma_user_access::validate_user_range(.., Prefault::Yes)` — it *walks the page
table first*, and on a miss calls a hook that `akuma_exec::init` registers and
this target never called. The hook is **fail-closed**. So the answer for a page
ring 3 had not touched was `EFAULT`.

Folding `read(2)` made that reachable in the most ordinary way there is: a
program `mmap`s a buffer and reads a file into it. What surfaced was

```
ERROR: Unable to read database: v2 database format error
ERROR: Failed to open apk database: v2 database format error
```

— a **file-format** complaint about a file the kernel had refused to read, three
layers away from the defect. The fix is `mm::prefault_user_range`, registered
from `install_shared_sinks`: a page-at-a-time loop over `mm::fault_in`, skipping
pages already present (re-populating one would leak the frame under it, and a
`PROT_NONE` guard must stay a guard) and failing on the first page that will not
come in.

**Neither standing gate could see it, and that is structural.** The boot suite
runs inside `BypassValidationGuard`, which returns from `validate_user_range`
before the walk. `scripts/utils/amd64_ring3_check.py` reads into libc heap
buffers, which are resident because the allocator wrote a header into them.
Hence §4's probe.

---

## 2. And a third: the boot suite's `usermode` block needed an identity

`spawn`, `busybox`, `execve`, `fork` and `redirect` all went `EBADF` at once the
moment `read`/`write` moved, because each reads a spawned child's stdout — and
every glue arm resolves `current_process_shared()` first, while this suite runs
on the boot task, which is registered nowhere.

`fd::boot_row_register` has existed since batch 2b for exactly this; what was
missing was the bracket around `boot::self_tests`' userspace block. It is
**outside** the per-test `free_count()` windows on purpose: `make_test_process`
builds an address space, so a register/release inside one would read as the leak
that check is looking for.

That is the fourth time "the boot suite runs before `run_init`" has been the
hidden prerequisite (batch 2b's `close`, 2d's `alloc_fd` starting at 0, 2d's
`MAX_FDS` ceiling, and now this).

---

## 3. What the fold gained, arm by arm

Everything here is behaviour this kernel did not have and did not have to write.

- **`read`/`pread64` go by inode.** Glue asks `read_at_open_file`, so a
  descriptor resolves through the `(mount id, inode)` `open(2)` pinned rather
  than re-walking the path on every call — and an unlinked-but-open fd keeps
  reading. The batch-2d `openat` fold is what bound those inodes; nothing read
  them until now.
- **`read` on a real file runs BKL-free**, scoped to that one arm.
- **`EPOLLET` edges are re-armed** after a pipe or socket read, and after a short
  socket write. This kernel's arms never did.
- **Concurrent writers cannot corrupt each other.** Glue reserves the file
  position with `reserve_write_pos` — read-and-advance in one lock hold — before
  any I/O. Two `CLONE_FILES` siblings each read a stale cursor here and wrote
  over each other on disk.
- **`lseek`'s `SEEK_END` knows an unlinked file's size**, and **`rewinddir`
  works**: glue clears `KernelFile::dir_cache` on a seek to 0, where this arm
  reset the entry index and left the first snapshot in place for the life of the
  descriptor.
- **`lseek` on a console descriptor is `ESPIPE`, not `EBADF`** — the kind answer
  rather than the closed-descriptor one. An fd naming *nothing* is still
  `EBADF`; this arm's console guard fired before it ever looked in the table and
  could not tell the two apart.
- **A `/dev` node lists as a device.** `getdents64` reported every character node
  as `DT_REG`, because `DirEntry` carries only `is_dir`/`is_symlink`; glue asks
  `dev_node_named`.
- **`/dev/urandom` keeps working**, which needed a fix in glue — see §5.

Two bounds were **moved into glue rather than dropped**, because a fold must not
lose one: `getdents64`'s 64 KiB clamp (that function's kernel allocation is a
ring-3 number; every caller loops, so filling fewer records is the syscall's own
contract) and `read`/`pread64`'s `MAX_IO` clamp, which stays in the preamble
because glue's pipe, stdin and `/dev/zero` arms allocate `count` unclamped.

---

## 4. `lazybuf.c` — the probe that would have caught §1b

`userspace/forktest/c_stress/lazybuf.c` (new): every syscall that **writes into**
user memory, aimed at a page the process has never touched, with a `TOUCHED`
control that does the identical call after one store and must pass either way.

| where | result |
|---|---|
| Akuma/amd64, QEMU | **6 PASS, 0 FAIL** |
| Akuma/amd64, bare metal | **6 PASS, 0 FAIL** |
| the same kernel with the hook unregistered | **3 FAIL** — `read`, `pread64`, `getdents64`, all `Bad address`; the control still passes |

**The size is the probe, and the first draft got it wrong.** It used a 64 KiB
mapping — which is exactly `akuma_config::MMAP_EAGER_MAX_PAGES` (16 pages), so
this kernel populated it eagerly and every probe passed 6/6 against the very
kernel it was written to fail on, while `apk` was still broken three feet away.
It is 1 MiB now, and the comment says why. *A probe that passes on the broken
build has not been falsified, it has been fooled.*

`fstat` is in the list and passes against the broken build too — correctly, it
is not a glue arm on this target yet. It is there so that the day `fstat` folds
(§7) the line starts asking the question rather than having to be remembered.

Not run on real Linux: no Linux host was reachable this session (Docker down,
and the box's Ubuntu side is the build machine). The probe is written to run
there and every line should be PASS.

---

## 5. Two things wrong in the *shared* crate, found by folding onto it

### `/dev/urandom` was `EIO` on any machine with no virtio-rng

Glue's `DevUrandom` read arm named `akuma_virtio::rng` outright — the same
mistake `getrandom(2)` made and that C1 step 3 batch 3 fixed one function along.
Every amd64 rig takes its entropy from `RDRAND` through
`akuma_primitives::rng`, so a folded read of `/dev/urandom` would have answered
`EIO` to every caller. `dev_urandom_fill` is the seam now, in the shape
`sys_getrandom` already had: `Some(ok)` from the hook, `None` → the device.
Additive; AArch64 registers no hook and behaves as before.

### `write(2)` on an `O_RDONLY` descriptor succeeds — on **both** kernels

Glue's `sys_write` `File` arm does not look at `KernelFile::flags` at all.
`O_ACCMODE` is checked at `open(2)` for whether the *file* may be written
(`may_open`) and then never again for whether this *description* may — so on the
AArch64 kernel a descriptor obtained with `open(path, O_RDONLY)` is a **write
capability**: the bytes reach the filesystem and `write(2)` reports success.

This target has always refused it, and the fold would have imported the gap
silently. `fd::write_mode_refusal` keeps the refusal here, ahead of the
delegation, and names the finding.

**It is not fixed in glue, and that is a decision, not an oversight.** It is a
behaviour change on the other kernel, whose verification loop does not run on
this machine (batch 2a and 2d recorded the same constraint for the same reason),
and a program that has been writing through a read-only descriptor deserves a
boot behind the change rather than a blind edit. **Open issue** — see §8.

---

## 6. A pre-existing ceiling this batch's long runs exposed

**One pipe leaks per ssh session, and at 64 the machine can no longer spawn.**

Symptom: `sshd: failed to spawn '/bin/sh' for exec`, exit 127, for every session
from about the 44th onward — permanently, on QEMU and on the metal.

Measured, not inferred. A temporary `[spawn] DIAG live-before=` print of
`akuma_syscalls_glue::pipe::pipe_live_count()` at each `sys_spawn` reads
**15, 16, 17, … one more per session**, monotonic from boot, and the failing
allocation reports `pipe::alloc failed, live=64` — `amd64/src/pipe.rs`'s
`MAX_PIPES`, a machine-wide policy.

**It is not this batch.** The identical 150-session run against `02b9166f` — the
commit before the fold, built in a worktree — fails at exactly the same count:
**43/150 returned**, both arms, same message.

The leading suspect is the **stdin pipe's write end**. `sys_spawn` allocates two
pipes; `fd::bind_stdio` consumes the stdin pipe's *read* reference (the child's
fd 0) and both stdout references (fd 1, fd 2), and the parent takes the stdout
read end. Nothing consumes the stdin pipe's initial **write** reference: only
`open("/proc/<pid>/fd/0")` does, and the exec path never opens it. So the child
exits, readers reach 0, writers stay at 1, and the pipe is never destroyed. The
fix is a design question rather than a line — dropping that reference at spawn
would leave the child's fd 0 at zero writers, i.e. immediate EOF — which is why
it is written down here rather than guessed at.

**`amd64_ring3_check.py`'s default of 40 sessions sits one session under the
cliff**, which is why ~320 metal sessions were reported clean in batch 2d: they
were run in batches. `-n 60` reproduces it in about four minutes.

---

## 7. Why `fstat`/`newfstatat` are not in this batch

`struct stat` is the **third architecture vocabulary**, after the syscall
numbers (C1 step 1) and `open(2)`'s flag word (4b prerequisites):

| field | x86_64 | asm-generic (aarch64) |
|---|---|---|
| `st_nlink` | 16 (8 bytes) | 20 (4 bytes) |
| `st_mode` | 24 | 16 |
| `st_uid`/`st_gid` | 28 / 32 | 24 / 28 |
| `st_rdev` | 40 | 32 |
| `sizeof` | **144** | **128** |

`akuma_syscalls_linux::Stat` is the aarch64 layout — its own header says so —
and `amd64/src/fd.rs`'s `encode_stat` writes the x86_64 offsets, under a comment
that already calls this "proposal item 5 territory". Folding the `stat` family
therefore needs a converter in `akuma-syscalls-abi` (the vocabulary crate, where
`open_flags` lives) with `offset_of!` assertions, **and** a split of glue's
`sys_fstat`/`sys_newfstatat` at the user-copy seam — `…_into() -> Result<Stat,
u64>` plus the write — exactly as batch 2d split `sys_openat`/`openat_path`.
That is a batch, not a hunk. `statx` needs none of it: it is arch-neutral.

---

## 8. Open, and stated rather than left as a gap

1. **`write(2)` on an `O_RDONLY` descriptor succeeds on the AArch64 kernel**
   (§5). amd64 refuses it in its preamble. Wants the AArch64 loop.
2. **One pipe leaks per spawn; 64 bricks the machine** (§6). Pre-existing,
   measured, reproduced on `02b9166f`.
3. **`/dev/tty` cannot be opened here** (`ENODEV`, from batch 2d), so `read`'s
   preamble has no `DevTty` arm. The day it can be opened, it belongs in the
   console guard.
4. **The stdin-sink interception** (`/proc/<pid>/fd/0`) is still this kernel's
   own, unchanged since batch 2d, for the same reason.
5. **`akuma-net-yarn`'s wait loop can still park untimed** — `poll.rs`'s three
   `schedule_blocking(deadline_us)` sites take `u64::MAX` when the caller passed
   no timeout. Left alone: that loop has its own escapes (drain budget,
   fruitless-progress escape) and its `WaitPolicy` fields are a measured set.

---

## Verification

| gate | before 3a | after |
|---|---|---|
| QEMU/TCG `SMP=1` / `SMP=4` | 579/0 / 589/0 | **590/0 / 600/0** |
| Firecracker/KVM `SMP=1` / `SMP=4` | 565/0 / 575/0 | **576/0 / 586/0** |
| bare metal (HP 500-502nj) | 579/0 | **590/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK**, heap 1573 → 1574 kB |
| memory probes, both transports | 8/10, 0 unexpected | **8/10, 0 unexpected** |
| `lazybuf` (QEMU / metal / hook removed) | — | **6/6 · 6/6 · 3 FAIL** |
| `openflags` (QEMU) | 20/20 | **20/20** |
| `apk update` + `apk add file` (QEMU + metal) | OK | **OK**, `file-5.47` runs |
| host tests | 1367 | **1371** (+4, the backstop's) |
| clippy, both kernels | 1 warning | **clean** |

+11 checks on every arm, the same eleven: one `lseek` on an unbound descriptor,
nine for the `getdents64`/`rewinddir` gains, one for the borrowed boot identity.
Every new check was falsified against the code it tests — the two `getdents64`
gains by disabling `is_dev_dir` and the `dir_cache` reset in glue, which turned
them red as `DT_REG` and a stale listing.

**AArch64 is touched this time** and does not claim byte-identical sections:
`park_indefinitely` replaces 22 `schedule_blocking(u64::MAX)` calls with an
atomic load and a branch, and `sys_read`/`sys_pread64`'s `/dev/urandom` arm gains
the hook check. Both are behaviour-preserving with nothing registered, which is
every AArch64 build, and the AArch64 kernel builds clippy-clean.

---

## Background

- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH2D.md` — `openat`, whose fold bound the
  inodes this batch's reads finally use.
- `docs/archive/AKUMA_AMD64_4B_PREREQUISITES.md` — the `open(2)` flag
  permutation, i.e. vocabulary number two.
- `docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md` § "batch 3" — the
  `getrandom` entropy seam this batch copies for `/dev/urandom`.
- `proposals/AMD64_FD_WHOLE_FILE_HEAP.md` § "And a method correction" — why the
  ring-3 check reads the kernel heap at all.
