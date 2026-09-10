# amd64 ring-3 entry seam, slice 5: the two loud stubs get real x86_64 arms

**Date:** 2026-09-11
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's
**ring-3 entry seam** row.
**Slice:** 5 — the two stubs slice 4's §4 named as all that stood between
`sys_fork` and `fork_process`.

`akuma_threading::get_saved_user_context` and
`ThreadPool::spawn_user_closure_initializing` both had x86_64 arms that failed
unconditionally. They have real ones now, and **`amd64::usermode::sys_fork`
calls both today** — not as a registered-but-unreached hook, but on the path
every `fork` on this target takes. The fold itself is still the next step; what
this slice removes is the reason it could not work.

## 1. What the two stubs were, and why they are one slice

They are the *same* seam seen from two sides. `fork_process` reads its parent's
register file at step 6 and spawns the child's thread at step 7, and until this
slice the first returned `None` and the second returned `Err`. Neither could
corrupt a child — both fail the syscall — which is why slice 4 could land the
memory pass with them still standing.

They are also both **mirrors of things this target already had**, which is what
made the slice small:

| shared entry point | this target's existing spelling |
|---|---|
| `get_saved_user_context(tid)` | `usermode::current_user_context()`, reading `crate::smp::current_uctx()` |
| `spawn_user_closure_initializing(fn, ptr)` | `sched::spawn_unpublished(entry, root, daemon)` |

So neither arm is new code. Each is the existing code moved behind the name the
shared path uses, with the one piece it cannot supply left as a hook.

## 2. `get_saved_user_context` — `X86ArchHooks::read_user_context`

The read mirror of the `write_user_context` slice 2 built, and registered beside
it. `amd64::sched::read_user_context(slot)` reads `machines()[slot].uctx` and
returns the shared `UserContext`:

```rust
UserContext {
    regs: uctx.saved_regs,          // rdi rsi rdx r10 r8 r9 rbx rbp r12..r15
    sp: uctx.user_rsp,
    pc: uctx.user_rip,
    fs_base: uctx.fs_base,
    gs_base: uctx.gs_base,
    rax: 0,
}
```

**The refusal is preserved and is the same refusal.** `user_rip == 0 ||
user_rsp == 0` means the `syscall_entry` assembly has never run for this slot —
a kernel thread, or a process task published moments ago that has not reached
its first instruction. That is precisely what "no live EL0 trap frame" means on
the other kernel, and the consequence of answering anyway is the same class of
silent birth its doc comment spends thirty lines on. `sys_fork` made this exact
check by hand, one field at a time, immediately after building its context; the
check now lives with the reader and `clone` inherits it for free.

The `NO_TRAP_FRAME_CHILDREN` counter and its rate-limited `[NO-TRAPFRAME]` line
are shared with the AArch64 arm rather than re-invented: a non-zero count means
fork/vfork/clone syscalls are being refused, and that has to be one greppable
line on either kernel.

`rax` is `0` and cannot be anything else — the assembly does not save it, since
it carries the syscall number in and the return value out. That is also the
value a `fork` child wants, but callers say so themselves with
`set_child_return_zero` rather than lean on it.

**Off both kernels** (a host `cargo test`, where `target_arch` is the host's)
the function still returns `None`: there is no slot table to read. Same shape
`update_thread_context`'s host arm takes, and the reason the arm is
`cfg(all(target_os = "none", target_arch = "x86_64"))` rather than
`cfg(target_arch = "x86_64")`.

## 3. `spawn_user_closure_initializing` — `X86ArchHooks::prepare_task_slot`

The harder of the two, per slice 4, and the shape that made it easy is a split
rather than a translation. A spawn on this target is three things:

1. claim a slot — already the crate's (`x86_claim_slot`);
2. give it somewhere to stand — **the target's**, because `amd64` owns thread
   stacks (it leaks a pair per slot on first use and a recycled slot reuses the
   pair it already has), where AArch64 takes them from the crate's own
   PMM-backed pool;
3. seed a context so the first switch enters `trampoline(closure_ptr)` —
   already the crate's (`x86_build_closure_context`).

Only (2) needed a hook. `prepare_task_slot(slot) -> Option<usize>` does it and
answers the kernel stack top.

**`amd64::sched::spawn_unpublished` now calls the same function.** That is the
part worth more than the hook: this target's own spawn path and the shared one
give a slot **one** initial machine state, so a field reset in one and forgotten
in the other cannot exist. And every field the picker or the switch reads is in
it, not just the two stacks — `space_root`, `pinned`, `daemon`, `idle`, the
saved `UserCtx`, the FPU area. A recycled slot inheriting the previous
occupant's `space_root` runs the new task in a freed address space; a stale
`uctx` hands it a dead process's `proc_slot`.

Three differences from the AArch64 arm, all of them this target's and all
stated in the code:

- **`self` is unused and the `POOL` lock buys nothing.** What this claims from
  is the lock-free `THREAD_STATES`; the `POOL`-guarded `stacks`/`slots` arrays
  are the AArch64 stack pool. The method stays on `ThreadPool` so both
  architectures present one signature to `spawn_user_thread_initializing`.
- **Exhaustion is final, and the error string says so.**
  `spawn_user_thread_initializing` answers the string `"No free user thread
  slots"` with a reclaim-and-retry pass. That pass exists to move cooled-down
  `TERMINATED` slots to `FREE` — and `x86_claim_slot` *already* takes a
  `TERMINATED` slot no core is executing, which is exactly why `MAX_TASKS` is
  not a ceiling on processes-per-boot on this target. There is nothing left for
  the reclaim to free (its stack-return half is `kernel_profile_extreme`, i.e.
  AArch64's), so `"No free x86 task slots"` opts out of a retry that could only
  fail again.
- **A failed `prepare_task_slot` abandons with `x86_abandon`, not `FREE`.** The
  slot may already own one of the two stacks; `TERMINATED` is what says
  "reusable, but not pristine". The AArch64 arm stores `FREE` on its own
  stack-allocation failure and is right to — there the stack went back to the
  PMM.

## 4. Both are on the live path, deliberately

Neither arm waits for the fold. `sys_fork` was rewritten to reach them:

| was | now |
|---|---|
| `let mut child_ctx = current_user_context();`<br>`if user_rip == 0 \|\| user_rsp == 0 { return ENOSYS }` | `let Some(mut child_ctx) = akuma_threading::get_saved_user_context(sched::current_task()) else { return ENOSYS };` |
| `sched::spawn_in_space_unpublished(proc_entry, child_root)` | `akuma_threading::spawn_user_thread_initializing(proc_entry, null_mut())`<br>`sched::set_task_space_root(task_slot, child_root)` |

This is slice 2's method — it put `write_user_context` on the fork path ahead of
the shared caller that would need it — and it is what makes the gate below mean
something. A hook that is registered and unreached is verified by nothing.

`usermode::current_user_context` is deleted; it had one reader.

`sched::set_task_space_root` is new and is the one thing the crate's spawn
cannot do: it has no `Process`, so a child arrives with `space_root` 0 — kernel
`CR3`. Forgetting it is not subtle (the first switch into the child runs ring-3
code against the kernel's page tables), which is why it is a named function next
to `set_current_space_root` rather than a raw write at the call site. It
deliberately has no `mov cr3`: the task is not running, so writing `CR3` on its
behalf would install a foreign address space on the core doing the spawning.

### 4.1 The `[threads] new high-water` lines are the proof

`note_user_thread_highwater` is called from `spawn_user_thread_initializing` and
from nowhere else, and nothing on this target called it before this slice. The
lines appearing in an amd64 boot log are therefore direct evidence that `fork`
now goes through the crate's spawn — not an inference from a passing test.

They are also the free diagnostic that came with the change: a slot-exhaustion
failure now names live/terminated counts and the ceiling instead of reporting a
bare `ENOMEM`.

## 5. Verification

### 5.1 The AArch64 kernel is untouched

Every hunk is inside an x86-gated region (`X86ArchHooks` is
`#[cfg(target_arch = "x86_64")]`; the two arms are `cfg(not(aarch64))`), so the
check is a binary one. Worktree A/B against committed `HEAD`:

- **every ELF section size identical**;
- **all 4882 symbol sizes identical**, once LLVM's internal `.llvm.NNN` hashes
  are normalised away.

Raw section *hashes* differ, and that is the worktree trap
(`AKUMA_AMD64_B3_GATE_GREEN.md`): the two builds live at different absolute
paths, so LLVM's internal-symbol hashes — a function of the build directory —
differ, and every address that names one moves with it. Compare sizes, per
section and per symbol; a hash comparison across worktrees answers a question
nobody asked.

### 5.2 amd64

QEMU skipped this slice by request; the gate was Firecracker at `SMP=4` and the
metal, which is the stricter pair anyway.

| gate | before | after |
|---|---|---|
| Firecracker/KVM `SMP=4` | 619/0 | **619/0** |
| host tests | 1373 | **1373** |
| clippy — aarch64 `release` + `extreme-size`, amd64 ±`no-tests` | clean | **clean** |

**The metal was A/B'd rather than compared to a recorded figure**, which the
hand-off prompt asks for in as many words: its `634 + 3 xHCI` no longer
describes the box. Both arms were built on the box from the same tree state,
staged the same way (`init=/bin/sshd root=/dev/sda1`) and measured back to back:

| bare metal, `SMP=4` | base (`HEAD` 6dcfc91e) | slice 5 |
|---|---|---|
| boot self-test | **641 passed / 0 failed** | **641 passed / 0 failed** |
| 60 ssh sessions of the two-exec subshell | 59/60 | 59/60 |
| `free`, free column | 2296936 → **2296936** | 2296924 → **2296924** |
| `Slab:` (live kernel heap) | 1709 → 3248 KiB (+1539) | 1604 → 3184 KiB (+1580) |
| `ps \| wc -l` | 5 → **5** | 5 → **5** |
| `[threads] new high-water` lines | **0** | present |

Two things fall out of running the base arm rather than trusting the recorded
number, and both were worth the reboot cycle:

- **The one failed session in sixty is pre-existing.** It appeared on *both*
  arms (session 44 on base, session 35 on the change), as `rc=255` with empty
  stdout and stderr — an ssh transport failure, not a workload one. The kernel
  log carries no `NO-TRAPFRAME`, no `SPAWN FAILED` and no slot exhaustion across
  it, and the live-thread high-water for the whole run is **10 of 512**. Same
  family as `AMD64_SSHD_INTERMITTENT_LOCKOUT.md` signature A but at a far lower
  rate; not this slice's, and worth a run of its own.
- **The zero in the high-water row is the negative control** for §4.1. Nothing
  on this target called `note_user_thread_highwater` before the change, so its
  absence on the base arm and its presence on the change arm is direct evidence
  that `fork` reaches the crate's spawn — measured, not inferred. (Read the
  count carefully: `dmesg | grep -c high-water` answers `1` on the base arm
  because `dmesg` contains the `[SSH] Exec:` line echoing the pattern. The
  self-match is the whole of it.)

The three xHCI failures the recorded baseline carries do not appear on either
arm, so they were a property of the previous boot configuration, not of the box.

## 6. Two method notes, both of which cost time

### 6.1 `git checkout -- .` does not undo `deploy`

`hpbox.deploy` applies the working-tree diff with `git apply --3way`, and
`--3way` writes its result into the **index**. `git checkout -- .` restores the
working tree *from the index*, so on the box it restores the patch — it reverts
nothing.

The tell was loud and nearly missed: the "base" build finished in **1.86 s** and
produced an md5 **identical to the kernel already installed**. Both readings
say the same thing — nothing was rebuilt because nothing changed — and either
one alone would have been dismissed as an incremental-build artifact. `git reset
--hard HEAD` is the correct revert there, and is sanctioned for that checkout
specifically (`hpbox.sync_from_git`'s docstring: `/root/akuma` is a deployment
checkout, not anyone's working tree).

This is the same family as `AKUMA_AB_STALE_BAKED_ARTIFACTS.md`: an arm that
never ran reporting the other arm's result as fresh. **Check that the two arms'
binaries differ before believing either number.**

### 6.2 `target/` on tmpfs, and the prime-once rule

The box's root is rotational (`/sys/block/sda/queue/rotational` is 1) and a
kernel build is thousands of small object writes. `hpbox.ramdisk` mounts a
tmpfs **over** `/root/akuma/target` — over, not `CARGO_TARGET_DIR`, so
`BOX_KERNEL`, `BOX_DISK` and `/root/stage_akuma.sh`'s GRUB install all keep
pointing at the same paths. Builds went to ~23–29 s.

The loop reboots constantly, and a tmpfs does not survive that, hence
`target.disk/` beside it and `hpbox.ramdisk_sync`. The subtle part is that
`ramdisk` seeds `target.disk` from `target/` **only when `target.disk` is
empty**: after a reboot the tmpfs is gone and `target/` is once more the
*underlying* directory, frozen at whatever it held before the first mount.
Seeding from that unconditionally — which the first version did — throws away
every build since. `target.disk` is the authority once it exists, and
`ramdisk_sync` is its only writer.

## 7. What is left before `sys_fork` can call `fork_process`

Nothing structural. The fold is next, and the three things it must decide are
already known:

- **`Process::inherit_from` vs `register_exec_process`.** The shared path builds
  the child with `inherit_from` and `register_process`; this target has its own
  registration taking a `name`/`cmdline`/`terminal_state` triple. They agree on
  what a `fork` child should carry (the parent's), so this is a deletion — but
  `image_top` is passed `0` here **as a stated decision** (a `fork` child has no
  heap of its own until its `execve`) and `inherit_from` must be checked against
  it.
- **The `ProcessInfo` page.** `fork_process` steps 2 and 5 allocate one, map it
  at `PROCESS_INFO_ADDR` and write it; this target registers
  `process_info_phys: 0` as a stated decision because the page is never read
  here and mapping one leaks 4 KiB per process past the ledger. Identical in
  kind to the `replace_image` warning the hand-off prompt gives for the *next*
  slice, and it arrives one slice early.
- **`proc_slot` and the `SPAWN` row.** `run_process` reads its process index out
  of `UserCtx::proc_slot` and uses it for `thread::drain(idx)` and
  `spawn_record_exit(idx, …)`; `fork_process` knows nothing about either. The
  natural place is a fallible bind hook called from
  `spawn_child_thread_and_publish` in the window after the tid exists and before
  the process is registered — which is also where `set_task_space_root` has to
  move. Fallible because the row table can be full, so the shared side needs a
  way to release an `INITIALIZING` slot on that path; it has none today.

## Background

- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE4.md` §4 — the two stubs, named.
- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE2.md` — `write_user_context`, of
  which §2 here is the mirror, and the put-it-on-the-live-path method §4 reuses.
- `docs/archive/AKUMA_THREADING_X86_SWITCH.md` — the x86 `Context`, the stacks
  `prepare_task_slot` hands out, and `x86_claim_slot`'s recycling rule.
- `docs/archive/AKUMA_AMD64_B3_GATE_GREEN.md` — why §5.1 compares sizes and not
  hashes.
