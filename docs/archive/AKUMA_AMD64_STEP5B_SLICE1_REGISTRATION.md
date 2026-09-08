# amd64 C1 step 5b, slice 1: every process is a registered `akuma_exec::Process`

**Date:** 2026-09-08
**Status:** landed. Five rigs at baseline, ring-3 verified on the metal.
**Parent:** `proposals/NEXT_AGENT_AMD64_STEP6_AND_5B.md` § "5b slice 1
(registration), spec'd and started" — this is that slice, written to that spec.
**Predecessors:** `docs/archive/AKUMA_AMD64_STEP5A_ONE_WALKER.md` (one walker),
`docs/archive/AKUMA_AMD64_STEP6_ONE_LOADER.md` (one loader). 5a is what made
this writable at all: `ProcAddressSpace::new` takes a `UserAddressSpace` and
nothing else.

---

## What this slice did

`akuma_exec::process::current_process_shared()` returned `None` on this target,
because nothing had ever put a `Process` in `akuma-exec`'s `PROCESS_TABLE`.
Every glue path that asks the kernel "who is running?" therefore fell back or
refused. Now `sys_spawn`, `sys_fork`, `sys_execve` and `run_init` each build a
real `akuma_exec::Process` and register it, and each reap unregisters it:

```
register_exec_process(pid, ppid, task_slot, root, brk, name)
    -> Process { .. }                       // the 45-field literal
    -> register_process(pid, proc)          // akuma-exec's PROCESS_TABLE
    -> thread_pid_map_insert(task_slot, pid)

reap_exec_process(pid, task_slot)
    -> unregister_process(pid)              // RETIRED, then collected
    -> thread_pid_map_remove(task_slot)
```

**Register order is table-then-map**, and the map key is the **sched task
slot**: amd64's tid *is* that slot (`gs:[32]`, what `current_tid` returns under
`smp-shared`), and `spawn_process_task` already returns it. `Spawn` gained an
`exec_slot` field so the reap — which runs when the shell collects the child,
long after the task slot could otherwise be reissued — can still name the key it
inserted.

`amd64/src/usermode.rs`'s own `Process`/`PROCS`/`Spawn` tables **stay**. This
slice adds a second, authoritative registration beside them; deleting the local
ones is slices 2-4. That is deliberate: it makes the slice reversible and keeps
the diff readable against a 4 000-line file.

### The field decisions, stated

`akuma_exec::Process` has 45 `pub` fields and this target has no answer for
eight of them. Each is a **carried decision**, not a default:

| field | what amd64 registers, and why |
|---|---|
| `channel` | `None` — sshd's stdio bridge is `crate::pipe`, a different namespace |
| `stdin`/`stdout` | empty `StdioBuffer` — same reason |
| `fds` | `SharedFdTable::with_stdio` (empty) — `fd.rs`'s `FDS`/`FILES` remain the real table until C2 |
| `namespace` | `akuma_isolation::global_namespace()` — there are no boxes here (new dependency, and the only one this slice adds) |
| `signal_actions`/`signal_mask` | empty — no signal delivery on this target |
| `image.context` | zeroed `UserContext` — amd64 never `eret`s from it; `run_process`/`enter_user` is its own entry path |
| `lazy_regions` | empty `LazyRegionMap` — amd64 demand-pages from `Process::regions` |
| `address_space` | `ProcAddressSpace::new(UserAddressSpace::new_shared(cr3))` |
| `memory` | `ProcessMemory::new(image_top, stack_bottom, ELF_STACK_TOP, mm::MMAP_BASE)` |

`new_shared` is the **non-owning, no-`Drop`** x86 view (`akuma-mmu` ~line 3332),
which is what lets the registered `Process` wrap the live root without a
double-free. `MMAP_BASE` became `pub` for the same-drift reason: the registered
`ProcessMemory` and `mm.rs`'s placer now state the same window in one place.

### The two open questions from the hand-off, resolved

- **The ProcessInfo page.** Registered with `process_info_phys: 0`. Safe here,
  and measured rather than assumed: `read_current_pid`'s page-walk tail reads
  `ttbr0_el1()`, which is **0 on x86_64**, so that tail is inert on this target
  — identity resolves through `THREAD_PID_MAP` and the identity cache, and
  `pid == 0` is already handled as "absent". Allocating and mapping a real page
  would have leaked 4 KiB per process past the ledger, which is exactly what the
  ring-3 leak check below would have caught.
- **`smp-shared` on `akuma-exec`.** Already resolved before this slice —
  the feature is *required* for amd64 since the unblock
  (`docs/archive/AKUMA_AMD64_SMP_SHARED_UNBLOCK.md`), with a real x86 `daif`
  arm, so `ProcAddressSpace::lock()` masks IRQs for real. Nothing to decide.

### Reclaim: three sites, because `unregister_process` only retires

`unregister_process` moves a slot to RETIRED; something has to collect it, or
256 slots exhaust and `register_process` panics — a long shell session is enough.
AArch64 collects from its own vetted list; this slice wires the three sites that
exist here:

1. **exit** — the terminal drain in `run_process`;
2. **idle** — `sched::idle_loop` calls `drain_retired_if_requested()`, the
   regime where the cooldown has always elapsed;
3. **pressure** — `akuma_pmm::PmmHooks::drain_retired` was `|| 0` and is now
   the real sweep, so the allocator's pressure ladder has its retired-process
   rung.

---

## The bug found on the way: `cpuid` and `rbx`

Not related to 5b, found because the boot tally moved 516 -> 514 while landing
it and the delta had to be explained. `uaccess::init_smap` read **garbage** for
the SMAP/SMEP feature word, and had for as long as the function existed.

`rbx` is callee-saved and LLVM reserves it, so it cannot be named as a clobber
and has to be saved and restored *inside* the template. Both hand-written shapes
this tree carried were wrong, in opposite directions:

```asm
; shape 1 (original) — two out(reg) operands
mov {tmp:r}, rbx      ; save
cpuid
mov {ebx:e}, ebx      ; result -> the result operand
mov rbx, {tmp:r}      ; restore
```
LLVM may allocate the **result** operand to `rbx` itself, and then the restore
overwrites the result: the function returned whatever the caller's `rbx` held.
That is the read that made SMAP detection depend on code layout — the same CPU
answered `on` on one build and `off` on the next.

```asm
; shape 2 (the 2026-09-08 repair) — one operand, result stashed first
mov {tmp:r}, rbx      ; save
cpuid
mov {tmp:e}, ebx      ; result -> the SAVE slot, destroying it
mov rbx, {tmp:r}      ; "restore" rbx FROM THE RESULT
```
This reads the right value and leaves `rbx` holding the CPUID feature word,
while LLVM believes `rbx` survived the call. Verified in the shipped image, not
inferred:

```
ffffffff802c4e67:  movq %rbx, %rsi
ffffffff802c4e6a:  cpuid
ffffffff802c4e6c:  movl %ebx, %esi
ffffffff802c4e6e:  movq %rsi, %rbx     <- rbx = the CPUID word
```

`ap_entry64` keeps the AP's cpu index in `rbx` across that call (`movq %rdi,
%rbx`, then `movq %rbx, %rdi; call put_dec` after it), so every secondary
printed the feature word as its cpu number. Cosmetic *in this build*; an
undeclared clobber of a callee-saved register is not a bug that stays cosmetic.

**The fix is `core::arch::x86_64::__cpuid_count(7, 0)`** — the core intrinsic,
which uses the `xchg` form:

```
movq %rbx, %rsi ; cpuid ; xchgq %rbx, %rsi
```

correct for *every* allocation including the degenerate one where the operand is
`rbx` (the result lands in `rbx` and LLVM knows, because LLVM chose it). It is
also a **safe** call — `cpuid` is unprivileged and baseline on x86_64 — so no
feature detection is needed to run the feature detection. `net::has_rdrand`
never read `EBX` and so never had the wrong-value bug, but carried the same
latent hazard (`mov {tmp:r}, rbx` / `mov rbx, {tmp:r}` is a pair of self-moves
when `tmp` *is* `rbx`, and then `cpuid`'s clobber escapes the template); it is
`__cpuid(1)` now for the same reason.

### Why no test caught it

`smap: CR4.SMAP follows CPUID` compares the register against **the value that
programmed it**. Both sides read the same corrupted word, so the check passed
whichever way the corruption fell. A self-test that compares a register against
its own input cannot see this class of bug; the check that would have is a CPUID
word against a second source.

### The reading, now confirmed against a second source

This was the first *trustworthy* SMAP reading each rig had ever produced, so it
was checked rather than trusted. On the bare-metal box the kernel now says
`smap: (CPUID lacks SMAP) bracket-off probe skipped`, and SMEP present. Linux on
the same machine, read on the Ubuntu side:

```
model name : Intel(R) Core(TM) i5-4460 CPU @ 3.20GHz
smep       : present
smap       : NO smap flag
```

Haswell — SMEP (Ivy Bridge) yes, SMAP (Broadwell) no. The two sources agree
exactly, which is the check the self-test cannot perform on itself. **The metal
has therefore been running with `CR4.SMAP` correctly clear and `CR4.SMEP`
correctly set**; what was broken was the reading, and on some builds it would
have set neither.

---

## Tallies

Registration adds no self-tests, so every arm should read *exactly* baseline —
and does. All `SMP=4` unless noted.

| rig | baseline | this tree |
|---|---|---|
| QEMU/TCG | 516 / 0 | **516 / 0** |
| QEMU/TCG `SMP=1` | 507 / 0 | **507 / 0** |
| Firecracker (the box, KVM) | 503 / 0 | **503 / 0** |
| OVMF/GRUB (the box, KVM) | 507 / 0 | **507 / 0** |
| bare metal | 507 / 0 | **507 / 0** |
| host tests | 1360 / 0 | **1360 / 0** |

`amd64_mem_trials.py --smp 4`, run twice, because one probe in it is a known
race and a single run cannot tell a race from a regression:

| run | QEMU/TCG | Firecracker |
|---|---|---|
| 1 | 6/10, `cowstale` "NO END MARKER" | **8/10, 0 unexpected** |
| 2 | **8/10, 0 unexpected** | arm never launched (harness, no probe output at all) |

So **both arms reached 8/10 with 0 unexpected on this tree**, in different runs.
`cowstale` is the known pre-existing no-TLB-shootdown race under `CLONE_VM`
threads that the hand-off says to score against its own rate rather than against
8/10 — 1 fail in 2 here. The run-2 Firecracker arm produced no probe output
whatsoever, which is a harness/environment miss and not a kernel result; the
same arm was clean in run 1.

### Ring-3 on the metal

The boot suite runs under `BypassValidationGuard` and cannot prove a user-facing
path, so: 40 ssh sessions (~160 process lifetimes, each a fork + exec + reap
through the new registration), `free` read either side.

```
before   used 1052045   free 2298516
after    used 1049847   free 2298516      # -2.1 MiB used, free unmoved
```

40/40 sessions returned. No leak — which is the number that would have caught a
4 KiB-per-process ProcessInfo page had one been mapped, and the number that says
the RETIRED sweep runs.

Run twice, across a power cycle (the box needed one for the unrelated subshell
wedge below), on the same kernel image — identical md5 in both `stage` runs:

| metal run | self-tests | sessions | `used` before -> after |
|---|---|---|---|
| 1 | 507 / 0 | 40/40 | 1052045 -> 1049847 |
| 2 | 507 / 0 | 40/40 | 1051727 -> 1049784 |

`free` unmoved at 2298516 in both. Secondaries printed `cpu 1/2/3 online` in
both, which is the `rbx` clobber staying fixed on real hardware; a dynamic
busybox ran through `ld-musl` in both.

---

## Not done — slices 2-4

1. **Delete the `Spawn` table**, moving its state onto the registered `Process`.
2. **Mount `akuma-vfs-glue`'s `ProcFilesystem`** — it compiles for this target
   already and is deliberately unmounted because the process table was empty.
   This is what retires `fd.rs`'s ~400 lines of synthetic `/proc`; the byte
   formats are already shared (`akuma-procfs`), so the swap is the process
   model, not the rendering.
3. **Delete `PROCS`**, transferring `Process.space` ownership.

Then 5c (`fork`/`execve`/`wait4`/`clone` onto `children.rs`/`spawn.rs`/
`exec.rs`), and only then does C1 step 4 become possible.

---

## Open issue found while verifying: a two-exec subshell wedges the kernel

Found by the ring-3 workload check, on the metal and then reproduced on local
QEMU. **Pre-existing** — `057ed0d3` (the commit *before* this slice, built in a
throwaway worktree) reproduces it identically, so it is neither 5b's nor the
`cpuid` fix's.

The ladder, each on a freshly booted `SMP=4` QEMU with `INIT=/bin/sshd`, run in
this order because the failure poisons the kernel for every later session:

```
echo hi                                       rc=0
ls /bin | wc -l                               rc=0    # plain pipe, 2 processes
( echo a ); echo done                         rc=0    # subshell, builtin only
( ls /bin >/dev/null ); echo done             rc=0    # subshell, ONE exec
( ls /bin >/dev/null; ls /bin >/dev/null )    HANGS   # subshell, TWO execs
```

So it is not the pipe and not the subshell: it is **the second exec inside a
forked shell** — a grandchild fork/exec. The console shows the `[SSH] Exec:`
line and then silence: no fault, no panic, no `[Fault] #PF`. Afterwards sshd
still accepts connections and still *runs* commands — output arrives in full —
but no session ever tears down, which is the same visible shape as the
now-closed open issue 3 in `AKUMA_SELF_HOSTING_AMD64.md` and is presumably why
that issue looked like a pipe/`wait4` problem. On the bare-metal box the same
command took the whole machine unreachable and it needed a power cycle.

Worth noting for whoever picks it up: this is the amd64 rhyme of an AArch64 bug
that was real — `( cmd; cmd ) &` losing mmap region extents across a CoW fork so
grandchildren shared nothing (`docs/archive/`, fixed 2026-07-30). Start there
before assuming it is a scheduler wake.

## Background

- `proposals/NEXT_AGENT_AMD64_STEP6_AND_5B.md` — the hand-off this slice was
  written from, including the constructor shape and the field decisions.
- `docs/archive/AKUMA_AMD64_STEP5A_ONE_WALKER.md` — the walker that made
  `ProcAddressSpace` reachable.
- `docs/archive/AKUMA_AMD64_STEP6_ONE_LOADER.md` — the loader fold.
- `docs/archive/AKUMA_AMD64_SMP_SHARED_UNBLOCK.md` — why `smp-shared` is
  required here, and the x86 `daif` arm this slice relies on.
- `proposals/AMD64_FD_WHOLE_FILE_HEAP.md` — still open, and still the reason to
  read before writing a large file on this target.
