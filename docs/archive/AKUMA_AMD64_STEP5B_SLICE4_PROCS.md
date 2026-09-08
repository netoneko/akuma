# amd64 C1 step 5b, slice 4: `PROCS` is gone

**Date:** 2026-09-08
**Status:** landed. Five rigs at baseline + 3 (QEMU/TCG `SMP=1` and `SMP=4`,
the box's Firecracker, the bare metal, host tests), ring-3 verified on local
QEMU **and on the metal**.
**Parent:** `proposals/NEXT_AGENT_AMD64_5B_SLICE4_PROCS.md`.
**Predecessors:** `AKUMA_AMD64_STEP5B_SLICE1_REGISTRATION.md` (registration),
`..._SLICE2_LIFECYCLE.md` (identity + lifecycle), `..._SLICE3_PROCFS.md` (`/proc`).

---

## What landed

`amd64/src/usermode.rs`'s `static mut PROCS: [Option<Process>; 128]` and its
sibling `static mut PENDING_EXEC` are deleted. Every field of this target's
private `Process` now lives on the registered `akuma_exec::Process` that slices
1-2 put in `PROCESS_TABLE`:

| was | is |
|---|---|
| `space: ProcAddressSpace` | `Process::address_space` — and **owning**, where slice 1 registered a non-owning `new_shared` view |
| `entry: u64` | `Process::entry_point` and `ProcessImage::context.pc` |
| `stack: u64` | `ProcessImage::context.sp` |
| `regions: Spinlock<Vec<MmapRegion>>` | `Process::mmap_regions` |
| `forked: bool` | `UserCtx::forked` — per **task**, not per process |

What is left of the type is `struct Image`: an address space, an entry point, a
stack pointer and a region list. It is a **value**, never a table entry —
`register_exec_process` takes it by value and moves each field into the
registered process, and a spawn that fails simply drops it, which is what frees
the half-built space.

`.data` lost 32 768 bytes exactly (the two 128-slot arrays), `.text` 6 688, for
a net −38 008 in the image, with a new self-test added.

### Where `forked` went, and why

It is the one field with no home in `akuma-exec`, and the proposal listed three
honest options. It went to `UserCtx`, the per-task block, because **the rest of
the same fact is already there**: `sched::seed_forked_task` seeds `saved_regs`,
`fs_base` and `gs_base` into that struct, and `forked` says nothing more than
"use them". It is set in that same function rather than by a separate call, so a
path that seeds the registers and forgets the flag cannot be written. AArch64
needs no equivalent because there is nothing to say there: that kernel `eret`s
from `ProcessImage::context`, which either *is* the parent's register set or is
not.

### `PENDING_EXEC` did not become a smaller array — `execve` does the swap now

The staging array existed because the *swap* was deferred to `run_process`,
"because a page-table switch and a frame free do not belong inside the syscall
asm". They still do not, and they still are not: `sys_execve` runs on the kernel
stack in ordinary Rust, well outside `syscall_entry`, and
`sched::set_current_space_root` is documented safe mid-flight — every address
space shares the kernel's upper half, so the stack it runs on stays mapped
across the `mov cr3`. What the deferral actually bought was somewhere to *keep*
the new image.

So `sys_execve` performs it, in the order `run_process` used:

1. install the new address space on the registered process, taking the old one
   back out (`ProcAddressSpace::replace`) rather than letting it drop;
2. `mov cr3` off the old space;
3. **then** drop the old space, freeing its frames and page tables.

Nothing between (1) and (3) touches user memory — the syscall return path with
`leave` set restores a kernel stack pointer and returns into kernel code — so
there is no window in which a fault could reach a freed table. `run_process`
keeps a loop, driven by a one-shot `UserCtx::exec_pending` flag, and re-reads
`(entry, stack)` from the registration each time round.

Two details that are easy to get wrong and were: everything that **allocates**
(the new argv `Vec<String>`) is built before the `with_process` hold, which runs
with interrupts disabled and forbids heap allocation; and the old region list is
`mem::take`n out of that hold and dropped outside it, next to the address space,
for the same reason.

---

## The cost question, and why the hand-off's mitigation was not needed

The proposal's whole risk was that the fault path's lookup —
`with_current_regions`, `with_current_address_space`, `cow_swap_frame`, all three
on every demand-paged fault and every CoW break — would go from

```
current_proc_slot()   -> PROCS[slot]              # per-CPU read + array index
```

to

```
current_task() -> THREAD_PID_MAP[tid] -> PROCESS_TABLE find(pid)   # two lookups
```

**That shape was never built.** `akuma-exec` already has the mitigation the
hand-off proposed, one level up: `current_thread_own_process()` resolves through
`table::THREAD_IDENTITY`, the per-thread identity cache that took the syscall
boundary from 410 ns to 150 ns (commit `c2a0e630`). Its key is the thread id,
which on this target **is** the scheduler task slot
(`X86ArchHooks::current_slot`) — the same key `register_exec_process` inserts
under — so it resolves on the first try for every process this kernel starts.
The new accessor is a per-CPU read plus a handful of atomic loads and a pointer
deref; the naive `find_process` scan is not on the path at all.

Adding a second cache — a raw `&'static Process` beside `UserCtx::proc_slot` —
was therefore **declined**, and not only on cost grounds: it would have to
re-derive the slot generation guard `akuma-slot-table` exists to provide
(`docs/archive/IDENTITY_CACHE_SMP_REVIEW.md` Finding B), and a raw process
pointer in a recycled per-task struct is precisely the "correctness bug wearing
a performance costume" the proposal warned about.

### Measuring it: the userspace instrument does not work on this target

The hand-off says to use `userspace/memprobe/c/mem_fault_cost` and
`scripts/benchmarks/mem_ab_run.sh`. Both traps it names are real and cheap to
fix (build with `x86_64-linux-musl-gcc`; inject with `debugfs` instead of ssh),
and neither is the problem.

**The problem is the guest clock.** Every arm of that probe is timed with
`clock_gettime(CLOCK_MONOTONIC)`, and this kernel's clock has **10 ms**
granularity: `net::uptime_us` is `lapic::ticks() * US_PER_TICK` with
`US_PER_TICK = 10_000`, and every clock on the target derives from it. Measured
on QEMU/TCG, 10 passes, three runs, all identical:

```
mmap_lazy             10000 ns   (control: region record only)
mmap_eager            10000 ns   (ratio 1.00)
demand_1p                 0 ns
demand_512p               0 ns
  per_demand_fault        0 ns   [(many - one) / 511]
```

The 1000-iteration control reads exactly one tick per 1000 reps; every 512-fault
bracket reads **zero**. A per-fault cost is nanoseconds and the finest thing
ring 3 can see here is ten milliseconds, so the probe is not "not run" — it is
not *runnable*. `scripts/benchmarks/amd64_fault_cost.py` is the driver that
demonstrates it (build, `debugfs`-inject, boot as init, read the console), kept
because the next person will reach for the same instrument.

Two side findings from making it report at all: the probe said only "an arm never
completed" for any failure, and it now **names** the arm — which is how
`brk_grow_1p`/`brk_grow_512p` turned out to be the failing pair, i.e. **amd64
has no `brk` growth** (`syscall(SYS_brk, 0)` answers `<= 0`). It also prints the
sections it *did* measure instead of discarding eight working arms because of
one missing syscall. Both changes are neutral on AArch64.

### So the measurement is a boot self-test

`usermode::identity_cost_test` — three checks and three notes, in every boot,
using the TSC, which has the resolution the tick clock lacks and is only
reachable from the kernel. It registers a process for the boot task (which is
registered nowhere — the boot-suite window this target has now tripped over
three times), times 20 000 iterations of each shape, and retires it.

| | QEMU/TCG (7 boots) | **bare metal** (i5-4460) |
|---|---|---|
| `current_proc_slot()` — the per-CPU read the old lookup started with | 3 | **1** |
| `current_process()` — the whole new lookup | 28–31 | **15** |
| added per fault-path lookup | 25–29 | **14** |

Uncalibrated TSC ticks. Under TCG they are emulation ticks and only the ratio
means anything; **on the metal they are cycles**, so the real answer to the
slice's cost question is **14 cycles — about 4.4 ns at 3.2 GHz — per fault-path
lookup**, every one of them a cache hit. TCG overstates it by roughly 2x, which
is the usual shape and the reason the metal reading is the one to quote.

The absolute end-to-end effect is below this rig's noise — whole-suite guest
time at `SMP=1`, three boots each arm:

```
before   2.61  2.56  2.49 s      (median 2.56)
after    2.51  2.57  2.37 s      (median 2.51)
```

The check that is **not** a timing is the one that matters and cannot pass by
luck: `IDENTITY_FALLBACKS` must not move across the whole measured loop. Every
resolution is a cache hit; not one is the 256-slot table scan the naive fold
would have paid. A nanosecond count drifts, but which path the lookup takes does
not.

---

## The four bugs and gaps found on the way

1. **`execve` never refreshed `/proc/<pid>/cmdline`.** Slice 2's note says "the
   argv is now recorded once, at registration, and `execve` refreshes it". It
   refreshed `ProcessImage::name` — and nothing reads that: `ps`'s COMMAND
   column comes from `ProcEntry::name()`, which is `args[0]` (`proc_entry_of`).
   So every `execve`d process listed the argv of whatever spawned it. Measured
   side by side, same image, same session:

   ```
   before:  54 0  0:00 /bin/sh -c ps        after:  65 0  0:00 ps
   ```

   Slice 4 refreshes both, in one `image` lock, so no reader can pair a new
   entry point with an old argv.

2. **`execve` leaked its predecessor's mmap extents.** While `execve` replaced a
   whole `Process`, the new one simply had an empty region list. Now the process
   outlives its image, so the clear has to be explicit — without it a `fork`
   child's inherited extents survive into the program it `execve`s and reserve
   VA ranges nothing maps.

3. **The reap was not a drain site, and the boot suite proved it.** With the
   address space on the registered process, `unregister_process` only *retires*
   it. `process::reclaim`'s three vetted sites here — the exit path, the idle
   loop, the PMM pressure ladder — none of them covers the moment a **parent**
   collects a child: the child's own terminal drain ran before the retire, the
   idle loop does not run while a busy shell reaps, and parking one image is not
   pressure. `spawn`/`busybox`/`fork`'s "teardown leaks nothing" checks each
   reported a whole image outstanding. `sys_waitpid` is now the fourth site,
   with the lock context the module requires: inside a syscall, holding the BKL
   and nothing else.

4. **`register` now happens before `publish`, everywhere.** `run_process` reads
   its entry point and stack *out of* the registration, so a task published
   first could be scheduled with nowhere to start. Slices 1-2 registered after
   publishing and merely left a window where the child's identity did not
   resolve; that window is closed as a side effect, and with it `sys_execve`'s
   "register here instead" fallback, which existed only to paper over it.

Two smaller things fell out. `fork` and `sys_spawn` used to disagree about how
full the machine was — `fork` searched `PROCS` *and* `SPAWN`, `sys_spawn` only
`PROCS` — and its `ENOMEM` diagnostic printed both counts because they could
diverge. There is one array left, so they cannot. And `exec_runtime.rs`'s
`futex_wake` stub is wired: its stated reason ("`crate::futex` is keyed by its
own task ids, a different namespace from `akuma-exec`'s pids") named the wrong
half of the table — the waiter *identity* is a task slot, but the **key** is
`(tgid, uaddr)`, and since slice 2 that tgid is `current_pid()`, i.e.
`akuma-exec`'s own pid. What unblocked it was the identity fold, not this one.

### The boot-suite window, for the third time

The six self-tests that run ring-3 programs had to be registered, and this is
not tidiness: the accessors resolve through the process table now, and `fdprobe`
and `threadprobe` both `mmap`. An unregistered self-test process gets `None`
from `with_current_regions` and fails with no explanation. `start_test_process`
/ `finish_test_process` are that, in one place — including the drain that makes
"teardown leaks nothing" mean what it said, since a test that merely retired
would report a leak of the whole image.

The **boot task itself** is still registered nowhere, `current_pid()` still
answers 1 for it, and `proc_by_pid`'s pid-1 fallback is still load-bearing. The
proposal's suggestion — register pid 1 before the suite — was not taken: it is a
change to what init *is* rather than to where a field lives, and this slice was
already moving a structure on the fault path.

---

## Verification

| rig | before | after | note |
|---|---|---|---|
| QEMU/TCG `SMP=4` | 521 / 0 | **524 / 0** | +3: the new cost test |
| QEMU/TCG `SMP=1` | 512 / 0 | **515 / 0** | +3 |
| Firecracker (the box, KVM) `SMP=4` | 508 / 0 | **511 / 0** | +3 |
| bare metal `SMP=4` | 512 / 0 | **515 / 0** | +3 |
| `amd64_mem_trials --smp 4` | 8/10, 0 unexpected | **8/10, 0 unexpected** | 3 of 4 runs; see below |
| host tests | 1360 / 0 | **1360 / 0** | no crate changed |
| `cargo clippy` (AArch64 gate) | clean | **clean** | |
| `cargo clippy -p akuma-amd64` | clean | **clean** | |

Ring-3, local QEMU, `scripts/utils/amd64_ring3_check.py` — 40 ssh sessions of
`( ls /bin >/dev/null; ls /bin >/dev/null ); echo r$$`, `free` either side,
`ps | wc -l`, then `/probes/grandfork`:

| arm | sessions | `used` before → after | `free` | `ps` rows | grandfork |
|---|---|---|---|---|---|
| baseline (worktree at `0f1b715a`), `SMP=1` | 40/40 | 528223 → 528137 | unmoved | 5 → 5 | ALL PASS |
| this tree, `SMP=1` | 40/40 | 528081 → 528059 | unmoved | 5 → 5 | ALL PASS |
| this tree, `SMP=4` | 40/40 | 527953 → 527931 | unmoved | 5 → 5 | ALL PASS |
| **bare metal**, run 1 | 39/40 | 1051635 → 1049437 | unmoved | 5 → 5 | ALL PASS |
| **bare metal**, run 2 | 40/40 | 1049884 → 1049415 | unmoved | 5 → 5 | — |

The one miss on the metal was `rc=255` with **no output on either stream** — an
ssh transport drop, not a guest fault: the immediately following run took all 40
without a pause, and `free` and `ps` are unmoved across both. Read against slice
1's own metal figure (`used` 1052045 → 1049847, "-2.1 MiB used, free unmoved"),
run 1 here is the same number to three digits.

The baseline arm is there because a harness that has never been run against a
known-good tree is not yet trustworthy. `grandfork` passing is the point of
having built it before this refactor: `sys_waitpid` has now been rewritten twice
since it was fixed, and that probe is what says the rewrites preserved the fix
(`AKUMA_AMD64_WAIT4_OWNERSHIP.md`).

Also checked by hand on a live guest: `ps` renders real commands, `/proc/1/cmdline`
is `/bin/sshd`, `/proc/self/{statm,maps}`, `free`, `df`, `uptime` and
`/proc/net/dev` all answer, and the stock dynamic busybox (`/bin/busybox.dyn`,
through `ld-musl`) runs.

### `cowstale` is a known flake, and it takes the run with it

`amd64_mem_trials --smp 4` reported 8/10 with 0 unexpected on three of four runs
against this tree, and 6/10 on the fourth. The fourth reads:

```
FAIL   cowstale               NO END MARKER — the probe did not return
FAIL   eager_mprotect_probe   NOT REACHED — the run stopped before this probe
FAIL   smapsdirty             NOT REACHED — the run stopped before this probe
```

That is the pre-existing `CLONE_VM` TLB race
(`docs/archive/AKUMA_AMD64_SMP_SHARED_UNBLOCK.md` § "The open issue"), the same
`NO END MARKER` signature slice 1 recorded at 1-fail-in-2, and the hand-off says
to score it against its own rate rather than against 8/10. 1-in-4 here is inside
that. Worth stating separately because the *shape* misleads: a `cowstale` hang
takes the two probes after it down as `NOT REACHED`, so one flake reads as three
failures.

### Getting a probe onto the metal

`stage('')` rebuilds `root.img` from `mkdisk.sh`, so anything injected before it
is discarded — and Akuma's busybox has **no `base64` and no `chmod`**, so a probe
cannot be pushed into a running guest over ssh either. The order that works is:
stage first, then `debugfs`-write into the **installed** image, then reboot:

```bash
python3 -c "import hpbox; hpbox.stage('')"          # builds, installs, arms GRUB
cat grandfork | ssh box 'cat > /tmp/grandfork'
ssh box "debugfs -w -R 'mkdir /probes' /boot/akuma/root.img ; \
         debugfs -w -R 'write /tmp/grandfork probes/grandfork' /boot/akuma/root.img ; \
         debugfs -w -R 'sif probes/grandfork mode 0100755' /boot/akuma/root.img"
python3 -c "import hpbox; hpbox.reboot_to('akuma')"
```

`sif ... mode 0100755` is not optional: `debugfs write` leaves 0644 and a probe
has to be executable to be a probe.

## New instruments, and where they live

- `scripts/utils/amd64_ring3_check.py` — the ring-3 workload check, which slices
  1-3 each described in prose and retyped by hand. Note it does **not** use
  `scripts/vm_ready.py`'s probe: that presents the host's default identities and
  this image authorises exactly one key, so it would poll to the timeout while
  sshd logged `Publickey auth failed` — which reads as "the guest never came up"
  and is not. Same check, own identity.
- `scripts/benchmarks/amd64_fault_cost.py` — `mem_fault_cost` in an amd64 guest,
  over the console. Kept for the negative result above: it is the fastest way to
  re-establish that the 10 ms clock still makes a userspace fault-cost
  measurement impossible here, and the first thing to re-run if that clock ever
  gets finer.
- `usermode::identity_cost_test` — the fault-path lookup, priced in every boot.

## Background

- `proposals/NEXT_AGENT_AMD64_5B_SLICE4_PROCS.md` — the hand-off, including the
  `UserCtx` pointer-cache mitigation that turned out to be unnecessary.
- `docs/archive/AKUMA_AMD64_STEP5A_ONE_WALKER.md` — why `space` became a
  `ProcAddressSpace`, and the lock order **regions → address space → PMM**.
- `docs/archive/AKUMA_AMD64_WAIT4_OWNERSHIP.md` — the probe, and why a hang
  needs a probe that prints before it acts.
- `docs/archive/IDENTITY_CACHE_SMP_REVIEW.md` — the generation guard a second
  cache would have had to re-derive.
