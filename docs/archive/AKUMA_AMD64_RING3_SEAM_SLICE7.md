# amd64 ring-3 entry seam, slice 7: `clone` folds, and finds four identity bugs

**Date:** 2026-09-11
**Status:** landed, all rigs green.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's
**ring-3 entry seam** row.
**Slice:** 7 — `clone`, the step slice 6's §8 named next.

`amd64::thread::sys_clone_thread` is its three argument checks and an errno
map over `akuma_exec::process::clone_thread`. Getting there cost four bugs, and
**all four are the same bug**: amd64 had one answer where AArch64 has two.

## 1. The shape of it

Before the fold a `CLONE_THREAD` child on this target had **no `Process` at
all** — a `THREADS` row, a task slot, and a `THREAD_PID_MAP` entry published
under its *leader's* pid. That last detail is what made everything else work by
accident: "my own pid" and "my thread group's pid" were the same number, so
every question that should have asked the second could ask the first and be
right.

`clone_thread` gives each thread a real registered `Process` — its own pid,
`tgid` = the leader's. The two answers separate, and every place that had only
ever needed one broke at once:

| question | amd64 had | correct (and what AArch64 already did) |
|---|---|---|
| which counter mints pids | its own `NEXT_PID`, starting at **2** | `akuma-exec`'s, starting at 1 — **two counters, one table** |
| `getpid` | the task's own pid | the **tgid** (`read_current_pid`) |
| futex namespace | the task's own pid | the **tgid** |
| whose regions / address space | the task's own `Process` | the thread-group **leader's** |

None of these is an amd64 design choice that the fold trampled. Each is a
distinction this target never had to draw. `akuma-exec` already ships the split
as a cached pair — `table::current_thread_own_process()` (per-thread identity)
and `table::current_thread_tgid_process()` (the group leader's `Process`), both
resolved through `THREAD_IDENTITY` — so every fix was to stop hand-rolling and
call the group-half accessor.

## 2. The four, in the order they were found

Each was found by the boot suite's own `threadprobe`, which reports a nine-bit
status. That probe is why this slice cost hours and not days: every failure
below arrived as a specific bit going out.

### 2.1 Two pid counters minting into one table

`clone_thread` allocates its child pid internally, from `akuma-exec`'s
`NEXT_PID` — which starts at **1**. amd64 minted from `usermode::NEXT_PID`,
starting at 2, and since 5b both register into the same process table. Nothing
had collided only because no shared allocator had ever been called here.

The first call that was drew **pid 1** — init's. `thread_pid_map_insert(tid, 1)`
then pointed the new thread at init's `Process`, and `entry_point_trampoline`
ran *init's* image: the thread entered ring 3 at init's `context.pc`, which is
`0`.

```
[Fault] #PF ... rip=0x400023 rsp=0x390f00 cr2=0x390f00 task=5 pid=11
```

Loud only because `pc = 0` faults. A collision with any *other* live pid would
have run the wrong program silently.

**Fixed by removing the second counter**: `usermode::alloc_pid` forwards to
`akuma_exec::process::allocate_pid`. The "children start at 2" reservation
survives as `reserve_pid_floor(2)`, called once from `exec_runtime::init` —
amd64's init is pid 1 by convention (it is not drawn from a counter), where
AArch64's init *is* the first allocation and needs no floor.

### 2.2 The argument order

`clone_thread`'s signature is `(stack, tls, parent_tid_ptr, child_tid_ptr,
flags)` — neither x86_64's syscall order (`flags` first, `tls` **last**) nor
asm-generic's. Passing the syscall's order handed `flags` to `stack`.

It did not fail; it produced a plausible-looking `sp` that was the *parent's*.
The probe pushes the child's entry point onto the child stack before the
`syscall` and the child pops it back off, so a wrong `sp` faults on the very
first instruction — `cr2 == rsp`, which is what named it. The call site now
spells every argument with a `/* name */` comment rather than trusting
position.

### 2.3 The futex namespace — a lost wakeup, 2 runs in 3

`amd64::futex::namespace` keys `(tgid, uaddr)` and read the tgid from
`usermode::current_pid()`. With each thread now carrying its own pid, a thread
enqueued on one key and its waker used another.

The symptom was the probe's parent parking in `FUTEX_WAIT` forever and the
suite reporting `0xffffffffffffffff` — `EXIT_STATUS`'s never-stored sentinel —
on **two runs in three**, passing on the third only when the parent happened to
see the value before it parked.

**AArch64's `read_current_pid` already predicted this**, in a comment written
long before: *"That matters most for `futex_key_tgid`: a non-leader thread
degrading to its own pid enqueues on `(own_pid, uaddr)` while its waker uses
`(tgid, uaddr)`, which is a lost wakeup."*

Fixed by making `current_pid()` answer the **tgid**, through
`current_thread_tgid_process()` — which is also what `getpid(2)` means on
Linux, with `gettid` (`thread::current_tid`) as the per-thread number.

### 2.4 The regions and the address space

`with_current_regions` / `with_current_address_space` resolved through
`current_process()` — the own-half. A thread's own `Process` has an **empty**
`mmap_regions` by construction (`inherit_from` gives every child a fresh one),
so demand paging asked the wrong list.

`.bss` is demand-paged here, so the first `clone` child faulted on the first
push to its own stack: the handler asked the thread's empty region list whether
`cr2` was mapped, got "no", and killed the process. Both accessors now resolve
through `current_mm_process()` = `current_thread_tgid_process()`. Linux's model:
identity is per-thread, the `mm` is per-thread-group.

The address-space half matters for a second reason: a thread's own
`address_space` is a `new_shared` *view* of the same page tables — the walk
would work — but its frame ledger is separate, so a page mapped through the
view is charged to a `Process` that is reaped before the leader is.

## 3. A `Process` leaked per `pthread_create`

Found on the metal, after everything above was green, by looking at `ps`:

```
PID   USER     TIME  COMMAND
    1 0         0:00 /bin/sshd
   11 0         0:00 threadprobe      <- the boot self-test, long finished
   12 0         0:00 threadprobe      <- its clone child
```

Nothing on this target released a thread's `Process`. A process is retired by
`sys_waitpid`; a thread is never waited for. AArch64 does it from
`akuma_exec::process::on_thread_cleanup`, registered as
`threading::set_cleanup_callback` and run when a thread slot is recycled — and
**that callback never fires here**, because x86 slots are recycled by
`x86_claim_slot` taking a `TERMINATED` one directly rather than through the
crate's collector.

So `thread::teardown` — the one point this target knows a thread is finished —
now retires it:

```rust
if let Some(pid) = akuma_exec::process::thread_pid_map_remove(t.task) {
    akuma_exec::process::unregister_process(pid);
}
```

No remaining-thread count, unlike the shared callback: the map row just removed
was this thread's own pid, and a thread's `Process` has exactly one thread by
construction.

**The second symptom is what made it findable**, and is worth knowing about:
a dead `Process` keeps `thread_id = Some(task)`, so
`resolve_thread_process`'s table scan starts finding it for whoever inherits
that task slot, and logs `[TRAMP-MISMATCH]`. `ps` is back to three rows on the
metal after the fix.

### 3.1 The `[TRAMP-MISMATCH]` lines that remain are benign, and are new since slice 6

Six on a Firecracker boot, eight on the metal, all naming **fork** children:

```
[TRAMP-MISMATCH] tid=5 THREAD_PID_MAP=33 but table scan found 30 — using 33
```

A dead `fork` child's `Process` stays ACTIVE until its parent reaps it, so if
its task slot is recycled first, the slot briefly matches two processes.
`resolve_thread_process` prefers `THREAD_PID_MAP` — which is published before
the child can be scheduled, so it is never missing for a task reaching the
trampoline — and the map is right, so the resolution is correct and the line is
the diagnostic doing its job.

They are **new on this target since the `fork` fold (slice 6)**, not since this
one: `register_exec_process` set `thread_id: None`, so the scan never matched
and the line could not fire. Slice 6's verification checked pass counts and did
not grep for new log lines — worth doing next time.

## 4. What amd64 keeps, and why

The `THREADS` row and `run_thread` stay, and this is the one place the fold
stops on purpose.

`enter_ring3` now dispatches: a task with a `UserCtx::thread_slot` runs
`thread::run_thread`, everything else `run_process`. That is not tidiness — the
process teardown closes the fd table, drains the thread group, publishes an
exit status a parent's `wait4` will believe, and retires the process. Running
it when *one thread* returns would report the whole process dead while its
siblings execute.

Folding that away is the "move amd64 onto the never-returns lifecycle" step
(`proposals/NEXT_AGENT_AMD64_RING3_ENTRY_SEAM.md` §4 option 2), which deserves
its own baseline.

The row shrank to what only it can answer: `tid` (now **the kernel thread
slot**, which is what `clone(2)` returns and what every per-thread array is
indexed by — it was `alloc_pid()`), `proc_slot`, `task`, and
`clear_child_tid`. Its `rip`/`rsp` are gone: the child's entry point is
`ProcessImage::context`, read back by `Process::run`, and a second copy is the
staleness `sched::write_user_context`'s doc refuses one field along.

## 5. Two new hooks

- **`ExecRuntime::bind_child_task` gained a `ChildKind`.** amd64 must build a
  *different thing* per primitive — a `SPAWN` row for a process, a `THREADS`
  row plus `thread_slot` for a thread — and a `SPAWN` row written for a thread
  would make `spawn_record_exit` publish an exit status a parent's `wait4`
  would believe. `ChildKind` is derived from `ChildReaping` rather than passed
  beside it (`ChildReaping::child_kind`), so a caller cannot disagree with
  itself.
- **`ExecRuntime::write_user_tid`** — the `CLONE_*_SETTID` store, and the two
  kernels need *different* stores that each refuse the other's. AArch64 uses
  `mmu::write_current_user_val`, a single aligned EL1 `str`, and deliberately
  **not** `copy_to_user` (whose `strb` loop returned a spurious `EFAULT` for
  exactly these musl/Go stores). x86_64 **cannot use that at all**: SMAP is on,
  so a ring-0 store to a user page without `stac` faults, and
  `write_current_user_val`'s plain `write_unaligned` would have taken the
  kernel down on every `pthread_create` that asks for a tid. It goes through
  `amd64::uaccess::write_val`, which brackets with `stac`/`clac` and has a
  `#PF` fixup in the IDT.

  **Caught before the first boot, not after** — the §7 caution of the hand-off
  prompt ("does the shared code read a hook amd64 never registered?") applied
  to a *memory access* rather than a hook. `record_clone_snapshot`'s
  `copy_from_user_with` was checked the same way and is fine:
  `akuma-user-access` has a real x86_64 SMAP-aware arm.

## 6. Verification

### 6.1 AArch64

`clone_thread` is on this kernel's `pthread_create` path, so both checks were run.

**Binary**, against the pre-fold commit. `.text` **−56 bytes**, and every
changed symbol is accounted for:

| symbol | before | after | why |
|---|---|---|---|
| `spawn_child_thread_and_publish::{clone_thread}` | 1924 | **1648** | the `set_tid` closure calls a hook instead of inlining the page walk |
| `…::{fork_process}` | 968 | 972 | `bind_child_task` gained an argument |
| `vfork_process` | 2352 | 2356 | same |
| `kernel_main` | 55668 | **55680** | two more hook registrations |
| `RUNTIME` | 344 | **352** | one more fn pointer |
| glue closure (new) | — | **208** | `write_user_tid`'s body |

**Boot**, `scripts/lima_aarch64_run.sh` (KVM inside Lima, `SMP=1`), both arms:

| | before | after |
|---|---|---|
| `PASSED` occurrences | 306 | **306** |
| distinct `[Test] … PASSED` | 298 | **298**, identical set (`diff` empty) |
| failures | 0 | **0** |

### 6.2 amd64

| gate | result |
|---|---|
| QEMU/TCG `SMP=4` | **641/0, three runs in a row** |
| Firecracker/KVM `SMP=4` | **619/0** |
| bare metal `SMP=4`, `root=/dev/sda1` | **641/0** |
| `thread: probe reported every check` | **[OK]** on all three |
| metal ring-3 workload, 60 sessions | 58/60, `free` unmoved, `Slab:` +1379 KiB (tol. 8192), `ps` **5 → 5** |
| metal `ps` after the boot suite | **3 rows** (was 5, with two leaked `threadprobe`) |
| host tests | **1375** |
| clippy — aarch64 `release`, amd64 ±`no-tests` | clean |

"Three runs in a row" is the gate that matters for §2.3: the futex bug passed
one run in three, so a single green run proves nothing here. The two failed ssh
sessions on the metal are the transport intermittent, unchanged either side and
tracked separately with the sda1 disk stall.

## 7. Method note: the probe's bitmask is the whole story

`threadprobe` reports nine independent bits and the suite names each one when
the total is wrong. Every bug above was read off that list before any
instrumentation was added:

- `0x8b` (bits 0,1,3,7 set) with `pc = 0` — clone returned, `CLONE_PARENT_SETTID`
  wrote, but the child did nothing: §2.1 and §2.2.
- `0x8b` again after the pid fix, now with the right `pc`: §2.4, and the `#PF`
  line named `cr2 == rsp`.
- `0xffff…` — the sentinel, meaning the probe never reported at all, on 2 runs
  in 3: §2.3.

One caution learned the hard way: **bit 3 (`from_child != 0`) is a false
positive when the child never runs**, because the parent reads garbage rather
than a zero. Bits 0, 1 and 7 are the ones that genuinely pass on a dead child —
bit 7 trivially, since `CLONE_CHILD_SETTID` is not requested and the join word
starts at 0 ≠ tid.

## Background

- `docs/archive/AKUMA_AMD64_RING3_SEAM_SLICE6.md` §8 — where this slice was
  named, and the `fork` fold whose `thread_id` this one's §3.1 explains.
- `crates/akuma-exec/src/process/table.rs` — `current_thread_own_process` vs
  `current_thread_tgid_process`, the pair §1 is about.
- `crates/akuma-exec/src/process/children.rs` — `read_current_pid`, whose
  comment predicted §2.3.
- `docs/runbooks/debug-thread-spawn-segv.md` — the `[TRAMP-MISMATCH]` line's
  original subject.
