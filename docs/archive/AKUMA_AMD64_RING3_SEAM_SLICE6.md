# amd64 ring-3 entry seam, slice 6: the fold — `sys_fork` calls `fork_process`

**Date:** 2026-09-11
**Status:** landed, all rigs green.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's
**ring-3 entry seam** row.
**Slice:** 6 — the fold the five slices before it existed to make possible.

`amd64::usermode::sys_fork` is 20 lines of pid allocation and errno
translation. Everything else it did is
`akuma_exec::process::fork_process`, and 366 lines of `usermode.rs` are gone
with it.

## 1. What each slice bought, in the order the fold needed them

The fold is not one change; it is the last one. Reading the list backwards is
the honest account of why this could not have been done first:

| slice | what it removed from the way |
|---|---|
| 1 | `UserContext` got an x86_64 arm, so `fork_process` **compiles** for `x86_64-unknown-none` |
| 2 | `ExecRuntime::enter_user`, so a child spawned by shared code reaches ring 3 through this target's *returning* lifecycle |
| 3 | every child got an exit `ProcessChannel` in `CHILD_CHANNELS`, so `ChildReaping::Reapable` has somewhere to register |
| 4 | `ExecRuntime::fork_share_memory`, so step 4 walks x86 page tables instead of reading ARM descriptor bits off a PML4 |
| 5 | `get_saved_user_context` and `spawn_user_closure_initializing` got real x86_64 arms, so steps 6 and 7 stop failing |

What was left over is this slice, and it is three things.

## 2. `ExecRuntime::bind_child_task` — the `SPAWN` row and the `CR3` root

Two facts about an amd64 child are keyed on its **task slot**, which does not
exist until the spawn returns, and must be in place before the slot is
published. There was nowhere to put them.

- **`space_root`.** The scheduler writes it to `CR3` on every switch into the
  task. The crate's spawn leaves it `0` — kernel `CR3` — because it has no
  `Process` to read a root from, and the first switch into an unbound child
  would run ring-3 code against the kernel's page tables.
- **the `SPAWN` row, and `UserCtx::proc_slot` naming it.** That index is what
  `run_process` uses for `thread::drain` and `spawn_record_exit`; it is how the
  target tears a process down at all, and `proc_entry` refuses to enter ring 3
  without it.

So `spawn_child_thread_and_publish` gained one hook, called in the window after
the tid exists and before `THREAD_PID_MAP` is written. AArch64 registers
`|_, _| Ok(())`.

**It is fallible, and that is the interesting part.** `SPAWN` is a fixed array;
a full one has to reach the user as an error, not as a child with no exit path.
That made it the *first* fallible step after a thread slot is claimed — and
shared code had no way to hand an `INITIALIZING` slot back. Hence
`akuma_threading::release_initializing_thread`, whose two arms disagree on
purpose:

- AArch64 stores `FREE`, matching what `spawn_user_closure_initializing` does
  on its own stack-allocation failure — the stack went back to the PMM, so the
  slot is pristine.
- x86_64 stores `TERMINATED` (`x86_abandon`), because there a slot owns two
  leaked 32 KiB stacks its next occupant reuses. Handing it back as pristine
  would leak the pair, and `x86_claim_slot` treats a `TERMINATED` slot no core
  is executing as claimable anyway.

Placing the hook *before* `thread_pid_map_insert` is what keeps that unwind to
one line: a failure has nothing to undo but the slot.

## 3. `ExecRuntime::fork_alloc_process_info` — a capability difference, pinned

`fork_process` steps 2 and 5 allocate a frame, map it read-only at
`PROCESS_INFO_ADDR`, and write a `ProcessInfo` into it. On AArch64 that page is
`read_current_pid`'s fallback when `THREAD_PID_MAP` cannot answer — and step 5
re-maps it *after* the share pass because the pass covers `PROCESS_INFO_ADDR`
and would otherwise leave the child reading its parent's pid (the bug that broke
`vfork_complete` and sent CoW faults to the wrong address space).

**amd64 has no such page and registers `|_| Ok(0)`.** Identity there resolves
through the process table and `THREAD_PID_MAP` alone; nothing reads the page,
and mapping one leaks 4 KiB per process past the frame ledger. That is exactly
what `register_exec_process` has always written into `process_info_phys`, so
this hook is not a new decision — it is the existing one moved to where `fork`
can see it.

This is the warning the hand-off prompt gives for the *next* slice
(`replace_image` "maps a mandatory `ProcessInfo` page… adopting it is a
capability change wearing a refactor's clothes") arriving one slice early. It is
answered the same way: a hook that states the divergence, not a silent adoption.
One hook and not two, because step 5's re-map and write are gated on the same
`0` the hook returned.

The AArch64 side is `akuma_exec::process::fork_alloc_process_info`, step 2
lifted verbatim.

## 4. Three behaviours the shared path brings, none silent

`Process::inherit_from` does not describe a child exactly the way
`register_exec_process` did. All three differences are gains or neutral, and all
three are stated in `sys_fork`'s comment rather than discovered later:

- **The child inherits the parent's `brk`.** This target passed `image_top: 0`
  and gave a reason: "a `brk` naming the parent's heap would answer a grow
  request into a space the child does not own". The CoW share pass makes that
  reason obsolete — the child owns a copy of the parent's heap pages, so a
  `brk` naming them names its own memory. `inherit_from` carries the parent's,
  which is what Linux does.
- **`signal_actions` is a `clone_for_fork` copy**, not a fresh table. POSIX.
  This target has no dispositions to carry (`rt_sigaction` is a stub), so it is
  a gain that costs nothing today and is correct the moment signals work.
- **`LifecycleGuard`** now wraps the whole operation. A no-op unless
  `kernel_smp_shared`, which this target does not build.

The parent-slot bound `sys_fork` used to check (`>= PROC_SLOTS`) went with the
hand-rolled slot search; `current_process()` inside `fork_process` asks the same
question of the authoritative table.

## 5. What the fold deleted

`amd64/src/usermode.rs`: **6413 → 6247 lines**, and four items became
unreferenced — which is the check that the fold is a fold and not a second
implementation living beside the first:

| deleted | why it was there |
|---|---|
| `Image::fork_of` | built a child address space; the `fork_share_memory` hook does it now, over the same `share_parent_memory_into` walk |
| `ProcEntry`, `ProcEntry::name` | one surviving field (`cmdline`), for one caller |
| `proc_entry_of`, `proc_by_pid` | that caller: `sys_fork` asking a parent for its name and command line, which `Process::inherit_from` reads off the parent's own `image` |

`share_parent_memory_into` stays — slice 4 gave it two callers precisely so this
deletion would not take it.

## 6. Verification

### 6.1 AArch64 — measured, clean

This slice changes code the AArch64 kernel executes (steps 2 and 5 of
`fork_process`, and a hook call in `spawn_child_thread_and_publish`), so a
binary comparison alone is not enough. Both were done.

**Binary,** against committed `HEAD`. `.text` **+456 bytes**, `.data` **+16**,
and every changed symbol is accounted for:

| symbol | before | after |
|---|---|---|
| `akuma_exec::process::fork_process` | 5212 → (slice 4) 2128 | **2036** (−92) |
| `akuma_exec::process::fork_alloc_process_info` | — | **216** (new) |
| `spawn_child_thread_and_publish::{fork_process}` | 840 | **968** |
| `spawn_child_thread_and_publish::{clone_thread}` | 1812 | **1924** |
| `vfork_process` | 2292 | **2352** |
| `akuma_kernel_glue::kernel_main` | 55644 | **55668** |
| `akuma_exec::runtime::RUNTIME` (`.data`) | 328 | **344** |

The `.data` +16 is the two new function pointers; nothing else moved. (Normalise
LLVM's `.llvm.NNN` internal-symbol hashes before comparing — they track the
build directory, so a worktree A/B's raw hashes always differ.
`AKUMA_AMD64_B3_GATE_GREEN.md`.)

**Boot,** `scripts/lima_aarch64_run.sh` (KVM inside Lima, `SMP=1`), committed
`HEAD` and the change side by side:

| | before | after |
|---|---|---|
| `PASSED` occurrences | 306 | **306** |
| distinct `[Test] … PASSED` | 298 | **298**, identical set (`diff` empty) |
| failures | 0 | **0** |

### 6.2 amd64 — every rig

| gate | before | after |
|---|---|---|
| Firecracker/KVM `SMP=4` | 619/0 | **619/0** |
| OVMF+GRUB q35 `SMP=4` (the metal's own multiboot2 path) | — | **631/0** |
| microvm on a real tap `SMP=4` | — | **639/0**, ssh auth works |
| **bare metal `SMP=4`, `root=/dev/sda1`** | 641/0 | **641/0** |
| metal ring-3 workload, 60 sessions | 59/60 on both pre-fold arms | **60/60** |
| host tests | 1373 | **1373** |
| pre-commit hook (73 crates, both profiles, tests) | — | **exit 0** |

The Firecracker run is not a smoke test for this change specifically: the boot
suite's whole `fork:` block passes (`sh -c "uname; echo DONE"`, the child's
output coming back, the parent finishing its command list, teardown leaking
nothing) and so does `redirect:`, including a **12-stage pipeline** — eleven
forks — and `yes | head -n 1` terminating.

The metal workload came back **60/60 in 33.8 s**, against 59/60 in ~49 s on both
arms of slice 5's A/B; `free` unmoved (2296924 → 2296924 kB), `Slab:` 1750 →
3035 KiB (+1285, against an 8192 tolerance and *less* than either pre-fold arm),
`ps` 5 → 5, and **no `NO-TRAPFRAME` and no `no free process slot` line** in the
whole boot.

### 6.3 The lockout in the middle of this, and why it was not the fold

The first metal boot after staging the fold locked itself out: 2222 answered and
refused **every** key, 22 refused. That is the signature
`AMD64_SSHD_INTERMITTENT_LOCKOUT.md` records as **signature A, still open**, and
it cost a physical reboot — so it is worth writing down how it was cleared
without a second one.

Three things were ruled out from Ubuntu, in this order:

1. **Key drift** — the trap the bare-metal runbook warns about, where a rebuilt
   `target/` regenerates the client key. The box's own
   `amd64-ssh-test-key.pub` is dated Sep 8 and its fingerprint
   (`SHA256:xkOlaHkt…`) is **identical to the laptop's**, and the laptop's public
   key is the **first line** of sda1's `/etc/sshd/authorized_keys`. Correct key,
   present, unchanged.
2. **The tmpfs.** `target/` had been moved onto a ramdisk that session (§7.3),
   which is exactly the sort of thing that eats a "generated once" key. It did
   not: both the tmpfs copy and `target.disk`'s carry the Sep 8 file.
3. **The fold** — answered with the box's own KVM rigs rather than a reboot,
   which is the point of them. `/root/taprun.sh` puts the guest on a real tap, so
   a genuine ssh round trip is possible: `AUTH_OK`,
   `[SSH Keys] authorized_keys: 98 bytes, 1 usable key(s)`,
   `Signature verified successfully`, 639/0, and the full 60-session ring-3
   workload **60/60** with `free` unmoved. amd64's sshd is the *non-forking*
   build anyway (its log says `[SSHD] Accepted connection`, without the
   `-> session pid N` the `fork-sessions` build prints), so auth never forks.

The boot after the reboot ran the same kernel with the same cmdline and served
ssh fine, which is what "intermittent" means. **Reach for the rigs before the
power button**: `/root/ovmf5.sh` is the same multiboot2 path the metal takes and
`/root/taprun.sh` can actually be logged into, and between them they answered
every question the locked box would have.

Two smaller things worth keeping:

- **`dmesg` records your own `[SSH] Exec:` line**, so `dmesg | grep -c
  NO-TRAPFRAME` answers `1` on a clean boot and `2` on the next try — it is
  counting the greps. Bracket tricks do not help (the recorded line is the
  literal you typed). Slice `dmesg` positionally instead:
  `dmesg | sed -n '1,/all self-tests/p' | tail -6`.
- **At `SMP=4` the summary line arrives torn**, because a `[BKL] stuck` line
  from another core interleaves mid-print: `Akuma/amd64 self-test: ` /
  `641 passed, ` / `0 failed` on three separate lines. A `grep` for
  `[0-9]+ passed, [0-9]+ failed` finds nothing and reads as a boot that never
  finished the suite. Same family as the torn `sshd started` marker
  `CLAUDE.md` § "Waiting for a VM" warns about, and the same fix — do not grep
  a single line out of SMP console output.

## 7. Two method notes

### 7.1 `hpbox.build` returned 0 on a failed compile

`build()` ran `cargo build … 2>&1 | tail -25`, so the pipeline's exit status was
`tail`'s — **0 whatever rustc did**. A caller then went on to `stage()`, which
does not stop on a build failure either: it installed the *previous* kernel and
armed GRUB for it. Measured this session, an `E0063` reported `build rc 0` and
`stage rc 0`, and the only evidence was an `error:` line inside the tail.

Both now `set -o pipefail` and, as a belt, fail on a line starting with
`error`. Same silent-success family as `BOX_CARGO` itself, whose own comment one
line up says a missing `cargo` "reads as a *successful* build with no output".

### 7.2 `git checkout -- .` does not undo `hpbox.deploy`

`deploy` applies with `git apply --3way`, which writes into the **index**, so a
checkout restores the patch rather than reverting it. The tell was a "base"
build finishing in **1.86 s** with an md5 **identical to the kernel already
installed** — two readings of the same fact, either of which alone reads as an
incremental-build artifact. `git reset --hard HEAD` is the revert there, and is
sanctioned for that checkout specifically (`hpbox.sync_from_git`'s docstring).
Same family as `AKUMA_AB_STALE_BAKED_ARTIFACTS.md`: **check the two arms'
binaries differ before believing either number.**

## 8. What is next

- **`clone`.** `clone_thread` already calls `get_saved_user_context` and
  `spawn_child_thread_and_publish`, both of which now work here, so this should
  be small — but it is its own step with its own baseline.
- **`execve`'s `replace_image`,** with the `ProcessInfo` decision §3 answers for
  `fork` re-answered for `exec`.
- **The `Spawn` row's last two fields** (`stdin_pipe`, `stdout_pipe`), now the
  only reason a `fork` child needs a row at all — §2's hook writes `None` for
  both, and if `sys_spawn` stops needing them the row and `proc_slot` could go
  with them.

## Background

- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE5.md` — the two stubs, and §7's list
  of the three decisions this slice makes.
- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE4.md` — the memory pass hook §1 row
  4 refers to, and `share_parent_memory_into`'s second caller.
- `docs/archive/AMD64_SSHD_INTERMITTENT_LOCKOUT.md` — signature A, §6.3.
- `docs/runbooks/amd64-bare-metal-loop.md` § "Rigs on the box" — the no-reboot
  path §6.3 used, and the key-drift trap it ruled out.
