# amd64: do boxes work the way they are supposed to? (audit and eight fixes)

**Date:** 2026-10-12/13. **Trigger:** "make sure boxes actually work on amd64; I
am sure `box grab` does not work and probably a bunch of other features."
**Method:** read what a box is supposed to do (`docs/reference/subsystems/containers.md`,
`userspace/box/README.md`), then drove every `box` command against a live amd64
guest (QEMU `microvm`, `SMP=2`) until each behaved. Regression:
`scripts/box_amd64_suite.py` (14 checks, offline — it builds its own image from
the guest's busybox, because the sandbox had no outbound TCP).

**Result before:** `box open` of any binary over 1 MiB reset the machine; a
small one worked once and then corrupted the heap; the container saw the host
filesystem; `box use -i`/`box run` never returned; `box grab` attached to the
wrong process and could wedge sshd; `box close` killed nothing. Everything is
`dispatch`-wired (315-318, 324, 325) and the syscalls are shared with AArch64, so
none of this was "missing syscalls" — each bug was a place where the shared code
met a fact about this kernel.

## The faults, in the order they hid each other

1. **No jail.** `exec_runtime.rs` answered `get_box_namespace: |_| None` and
   no-op'd `set_spawn_namespace`/`clear_spawn_namespace` ("containers are not
   built for this target" — true until `sc-containers` was turned on, and then
   never revisited). `spawn_ext(box_id != 0)` therefore gave the child the
   **caller's** namespace and resolved its entry binary against the host mounts.
   `AKUMA_AMD64_BOX_SPAWN_EXT.md` § "The corrected picture" asserted the opposite
   by reading; it was never run. Wired to `akuma_vfs_glue`'s registry.
2. **Binaries over 1 MiB reset the machine.** `spawn_ext` loads by path above
   `HEAP_SLURP_MAX`, which is *deferred* (lazy regions) and amd64's `#PF` handler
   serves `Process::regions` only. The September doc named this correctly. Fixed
   by `akuma_exec::process::image::set_eager_path_loads()` +
   `akuma_elf::load_elf_with_stack_eager_from_path` — the same windowed eager
   load `execve` already uses, so kernel heap stays flat.
3. **Kernel stack overflow in `spawn_ext`.** amd64 kernel stacks are 32 KiB
   (`sched::STACK_SIZE`, no guard page); `Process` carried an inline 8 KiB
   `ProcessSyscallStats` and `spawn_ext` holds it by value three or four times.
   Symptom: `[STACK CANARY] slot=N … w0=<a return address>` then a `talc` free
   fault in whichever thread next drained a dead address space — this is the
   September doc's "second `spawn_ext` crashes". The tables are now one boxed
   slice (`stats.rs`); AArch64 benefits too (stacks there are 128 KiB+, so it
   never noticed).
4. **Any ring-3 trap in a `spawn_ext` child triple-faulted.**
   `spawn_user_thread_fn_internal`'s x86 arm claimed a slot and a PMM stack and
   never ran `prepare_task_slot`, so `trap_stack_top` (→ `TSS.rsp0`) was 0. The
   child lived only while it stayed inside syscalls. The September doc blamed the
   lazy-region fault for the "`CR2 = 0xfffffffffffffff8`, double then triple
   fault"; that is this. Now the x86 arm uses `spawn_user_closure_initializing`,
   the same path fork takes. Found with `-d int,cpu_reset` (QEMU).
5. **`waitpid` never saw a `spawn_ext` child.** amd64's private `waitpid` (303)
   matches on `parent_pid`; `spawn_ext` children have 0 there and a
   `spawner_pid`. `box use -i` and `box run` polled forever. `is_child_of`.
6. **`box run` lost output, and `box grab` lost a forked shell's.** `reattach`
   repointed only the target's `channel` field. A short command had already
   written to its birth channel; a shell's earlier children kept theirs.
   `ProcessChannel::forward_to` makes the old channel forward (and moves what it
   buffered), with cycle clearing for A→B→A regrabs.
7. **A killed grabber wedged sshd.** The grabbed container kept the grabber's
   session channel and wrote a tick a second into a channel nobody read after the
   session ended; amd64's sshd is one cooperative process draining it forever, so
   no later connection was accepted. Writes to an `exited` channel are now
   discarded (a closed tty).
8. **`box close`/`--rm` killed nothing** (and the obvious fix hung the machine).
   `kill_box` → `kill_process` marks threads TERMINATED from another core, which
   this kernel cannot honour (`AKUMA_AMD64_SIGKILL_NATIVE_PATH.md`); the process
   went on running. `akuma_exec::process::set_box_member_killer` lets amd64
   register `kill(2)`'s own path (pend `SIGKILL`, the death runs on the dying
   thread). **It must not wait**: the first version yielded until the member
   exited, inside a syscall, and stalled sshd for most of a minute.

Smaller: `box grab` with no pid took the lowest-slot process in the box (a
transient `sleep`) — now the box's recorded primary first; `box show`/`grab`
scanned 64 of 512 thread slots (`thread_stats`); a symlinked entry (`/bin/sh` →
`busybox`, in every busybox/alpine image) failed "failed to spawn" because
amd64's `resolve_symlinks` hook was the identity; `box run --name X` under a
live `X` re-registered, failed at the overlay (a box with processes cannot be
re-rooted) and then `kill_box`ed the id — the first container's — and a closed
name could never be reused because its zombie counted as "live" to the re-root
guard (orphans reparent to init, which on this target is sshd and never reaps);
`libakuma::print_hex` printed a leading NUL (`ID=\0d07`).

## Still open

- **Outbound network was not testable** in this sandbox (DNS answers, TCP does
  not), so `box pull`/`box run <registry image>` on amd64 are unverified since
  2026-09-17. `box run` over a hand-built layer store is verified.
- **No pty `box grab`/`box run -i`.** Everything above is exec-mode. A full-screen
  app under `grab` (SIGWINCH repaint) and `run -i` stdin need an interactive
  ssh session; not exercised.
- **Orphans are never reaped**: init is `sshd`, which does not `wait`. Every
  `box close`d or `-d` container leaves zombie rows until the 256-row table
  fills. Not box-specific.
- **`set_box_stack(box, rump)` is accepted and ignored** on amd64 (no rump).
- **Kernel stacks have no guard page** and are 32 KiB. Fix 3 removed the thing
  that overflowed, not the hazard.
- The host test `boxlib::manifest` tests assume an aarch64 build (5 fail on an
  x86_64 host) and `akuma-threading`'s host tests do not build on x86_64; both
  predate this work.

## Verify

```bash
SSH_PORT=2290 INIT=/bin/sshd SMP=2 amd64/run.sh &        # any free SSH_PORT
SSH_PORT=2290 scripts/box_amd64_suite.py                 # expect ALL PASSED
```

## Background

- `docs/archive/AKUMA_AMD64_BOX_SPAWN_EXT.md` — the first wiring; its "wall" and
  "second spawn crash" are items 2-4 here.
- `docs/archive/AKUMA_AMD64_SIGKILL_NATIVE_PATH.md` — why a cross-core terminate
  is wrong on this kernel (item 8).
- `docs/reference/subsystems/containers.md` — the model.
