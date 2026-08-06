# Debug a freshly-cloned thread that SIGSEGVs before its first syscall

**Symptom.** A `pthread_create`d thread dies within ~10-50 ms of birth, at a
**fixed** PC, with a near-null `FAR`, and the kernel kills the whole thread
group:

```
[signal] deliver sig=11 slot=26 handler=0x37fbd1a4 fault_pc=0x3801c58c user_sp=0x146bc7510 alt_sp=0x0 alt_size=0x0 sa_flags=0xc000004
[signal] sig 11 needs sigaltstack but slot 26 has none — re-pending
[Fault] Data abort from EL0 at FAR=0x0, ELR=0x3801c58c, ISS=0x7
[Fault]  x0=0x1 x1=0x0 x2=0x0 x3=0x8
[Fault]  x19=0x1468b2650 x20=0x0 x29=0x146bc7530 x30=0x37fbd400
[Fault]  SP_EL0=0x146bc7510 SPSR=0x0 TPIDR_EL0=0x146bc7688
[Fault] Process 48 (/usr/local/bin/rustc) SIGSEGV after 0.00s
[Fault] SIGSEGV in clone_thread, calling exit_group
```

> **The dominant cause was found and fixed 2026-08-06 — read §2e first.**
> `clone_thread` wrote the child TID into the `CLONE_CHILD_CLEARTID` pointer at
> clone time. For musl that pointer is `&__thread_list_lock`, so every thread
> spawn stamped a live tid into musl's thread-list mutex and handed the lock to
> the wrong thread. That is the `FAR=0x0`/`FAR=0x8` fault above. Deterministic
> regression probe: `userspace/forktest/c_stress/tidflags.c` (4 FAIL before,
> 8 PASS after, 8 PASS on Linux).
>
> A **second, distinct** fault survives the fix and is now the live one: a
> thread executing in *foreign page tables* (`AS MISMATCH: L0 BASE DIFFERS`).
> That is theory **T1** in §3c, and §2f records the first direct evidence for it.
> Do not treat a post-2026-08-06 fault as "the same bug" without checking which.
>
> **Update, later 2026-08-06:** a concrete mechanism for §2f was found and
> fixed — a family of lock-free check-then-store races on `THREAD_STATES` that
> could revive a recycled/half-built thread slot (§2g). Plausible-cause fix,
> pending `-j4` verification; §2g's TTBR0 tripwires now catch any residual
> instance at the moment of corruption.

It is *not* a futex bug and not a lost wakeup — if you arrived here from a
process parked forever in `futex`, read
[`debug-futex-lost-wakeup.md`](debug-futex-lost-wakeup.md) §4 first: the parked
`pthread_join` is the *survivor*, this fault is the cause.

## 0. Confirm it is this bug and not a lost wakeup

### 0a. `SIGSEGV in clone_thread` does not mean "a pthread" (2026-08-06)

Read the message as **"a process whose address space is shared"**, which is a
strictly larger set. The line is emitted from a single predicate:

```rust
// src/exceptions.rs
let is_clone_thread = proc.address_space.is_shared();
```

and `vfork_process` builds its child with `UserAddressSpace::new_shared` on the
parent's L0 (`crates/akuma-exec/src/process/mod.rs`, `vfork_process`) exactly
like `clone_thread` does. So a **vfork-fastpath child that faults before it
execs** prints the same line, and so does any CLONE_VM process. Before pooling
occurrences into one population — every session so far has — separate them:

| the victim is | evidence |
|---|---|
| a real pthread | `ELR` symbolizes inside `Thread::new::thread_start` / a thread entry, and the process is *post*-exec (`ELR` in the exec'd image or one of its `.so`s) |
| a vfork child | `ELR` is in the interpreter (`0x30000000`+) or in the parent's image at a `fork`/`posix_spawn` return, and `SIGSEGV after 0.00s` on a pid the parent just created |

This matters because §2's class (`librustc_driver`, `thread_start`) and the
"second, separate crash" (ld-musl `__dls2`) may not be the same *kind* of process
at all, and averaging their evidence produces theories that fit neither.

### 0b. Three checks that tell this apart from a lost wakeup

Each is cheap and each has fooled a previous session:

| Check | This bug | Not this bug |
|---|---|---|
| `[FUTEX-ORPHAN]` lines in the log | **zero** | any ⇒ `debug-futex-lost-wakeup.md` §3, or §4a if the orphans are all one tgid with no `[kill]`/`[Fault]` anywhere in the boot |
| `SIGSEGV after N.NNs` on the victim | `0.00`-`0.05` | seconds ⇒ ordinary crash, not a spawn bug |
| the stuck waiters' `uaddr` | musl `detach_state` (`pthread_join`) | `__thread_list_lock` (`0x300c2340`) ⇒ §5 of the futex runbook |

The wedge shape: the SIGSEGV'd thread is killed before it can publish
`detach_state`, so every joiner parks forever on a correctly-queued futex. The
futex layer is behaving; the thread it is waiting for is dead.

## 1. Symbolize the faulting PC — do this first, it is 5 minutes

The PC is in the guest's own `librustc_driver`, which lives on the self-host
disk image and carries **`.symtab` and `.debug_info`**. Never guess at it.

Find the load base in the log — the `[IA-DP] file region` lines print it:

```
[IA-DP] file region: fault_va=0x33abab60 seg_va=0x30100000 filesz=0xcd6c000 file_off=0x0
```

`seg_va` is the load base; `filesz` (0xcd6c000) matches the `.so`'s total memsz,
which is how you confirm you have the right object. Then, against a **clone** of
the disk (never the image a live VM has open — see
[`recover-wedged-vm.md`](recover-wedged-vm.md)):

```bash
docker run --rm --privileged -v "$PWD/orphan.img:/disk.img:ro" alpine sh -c '
  apk add -q binutils
  mount -o loop,ro /disk.img /mnt
  SO=/mnt/usr/local/lib/librustc_driver-*.so
  addr2line -f -C -i -e $SO 0x7f1c58c 0x7ebd400'   # PC-base, LR-base
```

`objdump -d --start-address=… --stop-address=…` on the same object gives the
faulting instruction and the register that fed it.

## 2. What the symbols say (2026-08-05)

`ELR = base + 0x7f1c58c` → `__aarch64_ldadd8_relax` (compiler-rt `lse.S`), and
`x30 = base + 0x7ebd400` → `Arc<thread::Inner>::clone` inlined into
`ThreadInit::init`, inlined into `std::sys::thread::unix::Thread::new::thread_start`.

The whole prologue of the new thread is four instructions:

```asm
thread_start:
    ldr  x20, [x0]            ; x0 = the clone argument (Rust's thread packet)
    mov  x19, x0
    mov  w0, #1
    mov  x1, x20
    bl   __aarch64_ldadd8_relax   ; strong-refcount fetch_add(1) at [x20]  ← FAULTS
```

So the sequence is: **the child loads the first 8 bytes of its own clone
argument and gets a value that is not a pointer.** `[x19]` itself read fine
(x19 is a valid heap address); its *contents* were wrong. Observed contents:
`0x0` and `0x7fff0`.

Consequences that follow directly, and that kill three earlier theories:

- It is **not** a corrupted/garbage pointer (those are large). The child is
  reading memory the parent wrote milliseconds earlier and seeing stale or
  zeroed content.
- It is **not** a TLS/`TPIDR_EL0` fault: `TPIDR_EL0` is sane (0x178 above SP,
  exactly where musl puts the thread struct at the top of a fresh thread stack),
  and the first TLS access is *after* the faulting instruction (`mrs x22,
  tpidr_el0` at `+0x44`).
- It is **not** the sigaltstack line above it. That line is a *consequence*:
  Rust registers its SIGSEGV handler with `SA_ONSTACK` (`sa_flags=0xc000004`)
  and a brand-new thread has no altstack yet, so the kernel re-pends and the
  fault falls through to the kill. Fixing the re-pend changes the message, not
  the crash.

The same fault reproduces with **byte-identical** `SP_EL0`, `x19`, `x29`, `x30`
and `TPIDR_EL0` across *different processes* minutes apart — there is no ASLR,
so this is a deterministic code path, not a wild race in userspace.

## 2a. Ruled out by measurement: the clone memory handoff

The obvious reading of §2 — "the parent's stores are not visible to the child" —
is **wrong**, and it was cheap to disprove. `userspace/forktest/c_stress/clonearg.c`
clones raw in musl `__clone`'s exact register shape and has the child check,
before its first syscall: the argument popped off its own stack, sentinel words
the parent wrote below it, and a page the parent `mmap`'d microseconds earlier
(including the `PROT_NONE`-then-`mprotect` shape musl uses for thread stacks).

**144,260 children across 4 concurrent processes, 0 divergences** — and 0 on
real Linux, so the probe is calibrated. The handoff is sound; do not spend
another session on it.

## 2b. FIXED 2026-08-05: `mprotect` could not downgrade a cached translation

Found while chasing the above, in the same subsystem, and it is a real defect on
its own merits — though it is **not yet proven** to be the cause of the
thread-spawn SIGSEGV.

`flush_tlb_range` invalidated with `tlbi vale1is, va>>12`. That instruction
takes its target ASID from operand bits **[63:48]**, which `va >> 12` of any
user VA leaves **zero**, while every user process runs under a non-zero ASID.
The invalidation matched nothing. `sys_mprotect` (`src/syscall/mem.rs`)
publishes its PTE edits through exactly that function, so a permission
*downgrade* on an already-touched page never reached the TLB:

- musl's `pthread_create` — `mmap` the stack, then `mprotect(guard, PROT_NONE)`
  — left the guard page **writable**, so a stack overflow silently scribbled on
  the next mapping instead of faulting.
- a dynamic loader's RELRO `mprotect(GOT, PROT_READ)` left the GOT **writable**.

The fix widens it to `vaae1is` ("VA, All-ASID"), which is *required* rather than
merely conservative: `UserAddressSpace::new_shared` allocates a fresh ASID while
reusing the parent's `l0_frame`, so one L0 table can be live under several ASIDs
at once (CLONE_VM threads, vfork-fastpath children) and a single PTE edit has to
invalidate all of them.

Regress with `userspace/forktest/c_stress/mprotectlb.c` — deterministic, one
mmap/touch/mprotect/access per phase. Measured **3 FAIL before, 3 PASS after,
3 PASS on Linux**.

## 2c. Ruled out 2026-08-06 by code reading — do not re-derive these

Four mechanisms that each explain the evidence perfectly and are each false. All
four were checked against the source, not against a log; the file:line is given
so the next session can re-check them in a minute rather than re-reason them in
an hour.

**The kernel never reports failure for a thread it has already started.** This is
the cleanest possible explanation for a freed packet: Rust's `Thread::new` does
`drop(Box::from_raw(p))` when `pthread_create` returns nonzero, so a kernel that
made the child runnable *and* returned an error would hand the child a freed
argument, which is precisely the observed shape. It does not happen —
`clone_thread` (`crates/akuma-exec/src/process/mod.rs`) has no fallible step
between `spawn_user_thread_initializing` and its final `mark_thread_ready`, and
`sys_clone_pidfd` (`src/syscall/proc.rs`) only converts an `Err` that was
returned before the slot was ever marked READY. The only `clone_thread failed`
lines in the `-j4` logs are `No free user thread slots`, a pre-spawn error, and
they land *after* the faults, not before.

**SA_RESTART cannot re-run `clone`.** A silently replayed `svc` would clone twice
against one packet — candidate 1 below, delivered. But the rewind at
`src/exceptions.rs` (`(*frame).elr_el1 -= 4`) is gated on the syscall having
returned `-EINTR` or `-ERESTARTSYS`; `clone` returns a positive tid or `EAGAIN`,
neither of which arms it.

**The trampoline cannot pick a stale `Process`.** `entry_point_trampoline`
resolves its process by *scanning* for `thread_id == Some(tid)`
(`process/mod.rs`) — a uniqueness assumption with no enforcement — and `sys_exit`
deliberately leaves an exited thread's `Process` ACTIVE as a zombie ("Do NOT
unregister_process — leave as zombie for wait4", `src/syscall/proc.rs`), while
nothing clears `thread_id`. That is a genuine-looking collision: a recycled slot
whose old `Process` still claims it. It is closed by the recycler's ordering —
`cleanup_terminated_internal` runs the cleanup callback (`on_thread_cleanup` →
`unregister_process`, retiring the zombie and making it invisible to the scan)
**before** it stores `FREE` (`crates/akuma-exec/src/threading/mod.rs`), and only
`FREE` lets a spawn claim the slot. The window is closed by construction, not by
timing. (The scan is still worth replacing with `THREAD_PID_MAP` — see
§"Hardening" — but it is not the bug.)

**ASID reuse is flushed before the ASID is reallocated.** `UserAddressSpace::drop`
issues `flush_tlb_asid(self.asid)` and only then returns the ASID to the
allocator, with the ordering documented in place (`crates/akuma-exec/src/mmu/mod.rs`);
`AsidAllocator` is a plain round-robin bitmap over 256 IDs with no generation
counter to get wrong (`mmu/asid.rs`).

**No kernel relocation writer can accumulate `base`.** For the ld-musl class
below: all three appliers — `load_elf`, `load_interpreter`, and the size-profile
`load_interpreter_from_path` (`crates/akuma-exec/src/elf/mod.rs`) — write
`*ptr = base + addend`, the idempotent form, and `INTERP_BASE`
(`elf/types.rs`) is a `const`. So the `base*N` accumulation is not produced by
any kernel write, which moves the hunt onto musl's own `GETFUNCSYM` static
pointer being written by two parties.

## 2d. The evidence that reframes the question (2026-08-06)

In the archived `-j4` log (`selfhost_vm_smp4.log`, 2026-08-03) two faults
**0.32 s apart** are byte-identical in every field:

```
[T1684.46] Process 428 (/usr/local/bin/rustc)   [T1684.78] Process 433 (/usr/local/bin/rustc)
tid=26  x19=0x3d96f980  SP_EL0=0x24477d510  FAR=0x2d7463697274732b  ELR=0x3801c58c
```

`0x2d7463697274732b` is `"+strict-"` — rustc target-feature string heap, the same
family as `"libder-8"` / `"+outline"`. Both are preceded by a clean
`trampoline ENTRY tid=26` with a proper `[Cleanup] Thread 26 recycled` between
them, so this is **not** a double-start of one thread: two different processes,
two legitimately fresh occupants of slot 26.

Two readings, and the register dump cannot choose between them:

1. **Determinism.** No ASLR, two rustc invocations with the same arguments, so the
   same allocation sequence puts `"+strict-"` at the same VA in both, and the
   hand-off is corrupted the same way both times. This is what §2 assumed.
2. **Aliasing.** The two processes are reading the *same memory* — same physical
   page under the same VA in two address spaces.

Reading 2 was never on the table before and is the more economical explanation of
"a pointer slot that holds another process's string". It is also the reading that
unifies this class with the "victim is sometimes running in another pid's address
space" observation in the second crash below. Note the honest constraint: this
pair predates `src/file_page_cache.rs` (2026-08-05), so the *file* page cache
cannot be the aliasing route for these two — CoW fork, the vfork fastpath's
shared L0, or a demand-fault double-install are the candidates that were already
present.

Deciding between the two readings is what §3a's instrumentation exists for.

## 2e. FIXED 2026-08-06: the kernel wrote a live TID into musl's thread-list lock

**This was the dominant cause.** Neither reading in §2d was right; both were
downstream of a syscall-semantics bug.

Linux keeps three `clone(2)` tid flags strictly separate:

| flag | what it does | when |
|---|---|---|
| `CLONE_PARENT_SETTID` (`0x0010_0000`) | write child tid to `ptid` | at clone, in the parent |
| `CLONE_CHILD_SETTID` (`0x0100_0000`) | write child tid to `ctid` | when the child first runs, **in the child's context** (`schedule_tail`) |
| `CLONE_CHILD_CLEARTID` (`0x0020_0000`) | write **zero** to `ctid`, then futex-wake | at child **exit** |

`CLEARTID` says nothing whatsoever about clone time. Akuma's `clone_thread`
ignored the flags and wrote unconditionally:

```rust
if child_tid_ptr != 0 {
    unsafe { core::ptr::write(child_tid_ptr as *mut u32, child_tid); }   // BUG
}
```

musl's `pthread_create` passes `CLEARTID` **without** `CHILD_SETTID`, and the
pointer it passes is `&__thread_list_lock` — a global mutex word, not a tid
slot. So every single thread spawn stamped the new thread's own tid into musl's
thread-list lock. That is uniquely destructive because of `__tl_lock`'s
recursion fast path:

```c
int val = __thread_list_lock;
if (val == tid) { tl_lock_count++; return; }   /* "already mine" */
```

The value the kernel wrote is *exactly* the child's tid, so the lock appeared to
be already held **by the one thread that must not hold it**. The child's first
`__tl_lock()` — the one at the top of `__pthread_exit` — returned without
acquiring, and the child unlinked itself from the thread list while its parent
was still linking it:

```
4071cc: ldp x0, x1, [x19, #8]   ; x0 = self->next, x1 = self->prev  (both NULL)
4071d0: str x0, [x1, #8]        ; prev->next = next  ->  write to 0x8
```

which is exactly the `FAR=0x0` / `FAR=0x8`, `x3=0x8`, near-null fault this
runbook is named after. Second-order damage: the bogus `tl_lock_count++` is
never undone, so the parent's `__tl_unlock` only decrements the counter and
**never releases** — every later pthread call in that process blocks forever.
That is a plausible contributor to the "wedged with threads parked in futex"
reports, and it is why the fault and the hang always travelled together.

**The fix** (`crates/akuma-exec/src/process/mod.rs`, `clone_thread`) takes the
raw `flags` word and gates all three writes on their actual flags. Go's
`newosproc` passes 0 for both pointers, so Go is unaffected either way.

**Regression probe:** `userspace/forktest/c_stress/tidflags.c`. One clone and
one load per check, no stress loop — because the stress repro (`spawnalias`)
reproduces this only about one run in three, which is nowhere near enough to A/B
a fix on (the trap described in
[`../archive/`](../archive/) and in §3a). Calibrated on real Linux.

| | Linux | Akuma before | Akuma after |
|---|---|---|---|
| `tidflags` | 8 PASS | **4 FAIL** — `ctid=0xc` where the child tid was 12 | **8 PASS** |

The three extra FAILs were the same defect seen from other angles: `ctid`
written with no tid flags set at all, and `ctid` cleared at exit with no
`CLEARTID` set.

> One calibration subtlety worth keeping: `CHILD_SETTID`'s write is performed
> **by the child**, so a parent that reads `ctid` the instant `clone` returns
> sees the old value on *Linux too*. The probe polls for it. An earlier version
> asserted it synchronously and produced a false FAIL on Linux — which is
> precisely why every probe in `c_stress/` is calibrated before it is trusted.

## 2f. STILL OPEN after 2e: a thread running in foreign page tables

The first `-j4` build on the fixed kernel got far further — one fault in the
whole run instead of a steady stream — but it was a **different** fault, and the
§3a instrumentation caught the thing T1 predicted:

```
[Fault] Data abort from EL0 at FAR=0x7, ELR=0x3801c58c, ISS=0x7
[Fault]  tid=28 ttbr0_live=0xf7000088f1d000 ttbr0_proc=0x4000088b1d000  *** AS MISMATCH ***
[Fault]  clone-handoff tid=28 stack=0x145f4d590 from pid=1988/tid=20 ttbr0=0x2000088b1d000
[Fault]  [x19=0x3ceb84f0] = 0x7 0x3ceb8ef0 0x6f666e69657363 0x10a80000000000
[Fault]    ascii: "cseinfo."
```

Decode the three `ttbr0` values as `ASID:base`:

| | ASID | L0 base |
|---|---|---|
| parent (pid 1988) | 2 | `0x88B1D000` |
| this thread's `Process` | 4 | `0x88B1D000` |
| **live `TTBR0_EL1`** | **0xF7** | **`0x88F1D000`** |

The parent and the child's `Process` agree on the L0 base, as they must — a
CLONE_THREAD child shares the parent's L0 under a fresh ASID. The **live**
register points somewhere else entirely. The core was executing this thread in a
third party's page tables. Every user pointer it loaded was resolved against the
wrong translation, which is why `x19` dereferences to a plausible-looking but
foreign heap word (`"cseinfo."`).

**Read the flag carefully.** `ttbr0_live != ttbr0_proc` on its own means
nothing: `clone_thread` hands the child `shared_ttbr0` — the parent's ttbr0
*including the parent's ASID* — while `new_shared` gives the child's `Process` a
fresh ASID over the same L0, so the ASIDs differ by design on every cloned
thread. The diagnostic in `src/exceptions.rs` now separates the two and only
flags **`L0 BASE DIFFERS`**; a bare ASID difference prints
`(asid differs only — normal for a cloned thread)`. An earlier revision flagged
both, which would have made every cloned-thread fault look like a smoking gun.

Where to look: the switch path in `crates/akuma-exec/src/threading/mod.rs`
restores `TTBR0_EL1` from `THREAD_CONTEXTS[new_idx].ttbr0`, which is only
refreshed when a thread is switched *out*. `clone_thread` already carries a
comment about that entry being stale for a thread that activated a new address
space since its last switch-out. That is the first place to look, and note the
observed live value belongs to *neither* participant, so it is whatever last ran
on that core.

Also still open, and possibly related: the same run logged **81 `[BKL] stuck`
events** and the build wedged with rustc processes alive but making no progress.
Do not assume that is the same bug.

## 2g. A mechanism for §2f found and fixed: lock-free check-then-store on THREAD_STATES (2026-08-06)

Chasing "how does a thread get switched in under a third party's L0" through the
switch path (`sgi_scheduler_handler_with_sp` saves live TTBR0 into the outgoing
context and restores `ctx.ttbr0` for the incoming one — sound on its own) leads
to the question: *who can corrupt a parked thread's saved context?* Answer: any
path that makes a slot schedulable when its context does not belong to a
runnable thread. Four were found, all the same shape — a **check-then-store on
`THREAD_STATES` with no lock** — and all are now single atomic transitions
(CAS / `fetch_update`, `crates/akuma-exec/src/threading/mod.rs`):

1. **`ThreadWaker::wake`** (every futex/IO/timer wake funnels through it):
   loaded `state == WAITING`, then stored `READY` as a second step. The waker
   runs preemptible with no lock, so between the two steps it can be switched
   out for *milliseconds* — while the target wakes by timeout, runs, exits, and
   its slot is reclaimed and re-claimed by a new `clone_thread` (slot churn
   under `-j4` is constant; see "Amplifier" in §3c). The stale `READY` then
   lands on an INITIALIZING slot whose context is still the previous
   occupant's. A peer core picks it and restores the previous occupant's
   `ttbr0` — a **third party's L0, which is exactly the §2f signature** — and
   its kernel stack, possibly still in use (double-run; BKL ledger corruption;
   `[BKL] stuck` storms). Now: `compare_exchange(WAITING, READY)`; a wake can
   make no other transition.
2. **The same waker cleared `WAKE_TIMES` after its check.** In the same stale
   window that clear can erase a *fresh* deadline the slot's next occupant just
   published — a thread parked forever with no timeout, i.e. the
   "live-but-idle rustc" wedge shape. The waker no longer touches `WAKE_TIMES`
   at all (every sleep entry rewrites it; the value is inert on a non-WAITING
   thread).
3. **`commit_switch` / the net-boost path** re-READYed the outgoing thread via
   load-then-store. `mark_thread_terminated` is called cross-thread with no
   lock (`kill_thread_group`), so a TERMINATED landing in that window was
   overwritten — resurrecting a killed thread onto page tables its group exit
   is freeing. Same fix in `publish_waiting_and_take_pending_wake` and both
   park-loop resume arms (one of which stored RUNNING *unconditionally*), and
   in `mark_thread_ready` (the spawn publish), which could resurrect a child
   killed between context setup and publish.

Regression tests: host —
`threading::state_transition_guard_tests` (waker refuses INITIALIZING /
TERMINATED / FREE / RUNNING; deadline untouched by a successful wake; publish
and park-resume refuse TERMINATED); boot-suite — `wake_transition_guards` in
`src/process_tests.rs` (refusal semantics only; it never flips a contextless
slot READY in a live kernel).

**Status: plausible-cause fix, not yet proven against the reproducer.** The
§2f `AS MISMATCH` fired once in one `-j4` run; nothing this cheap to observe
can confirm the fix. What CAN falsify it fast is the new tripwire pair below.

### TTBR0 tripwires (new instrumentation, always on)

`EXPECTED_L0[tid]` tracks the L0 base each thread is *supposed* to run under
(written by `update_thread_context` for child inits and by
`UserAddressSpace::activate`/`deactivate` for self-installs; ASID deliberately
masked out — see "Read the flag carefully" in §2f). The switch path checks it
both ways and prints at the moment of corruption, with both tids:

| line | meaning |
|---|---|
| `[TTBR SAVE-MISMATCH] core=… old_tid=… live=… expected_l0=…` | the outgoing thread was RUNNING under foreign tables — and the save just wrote that foreign value into its context |
| `[TTBR LOAD-MISMATCH] core=… new_tid=… ctx=… expected_l0=…` | the incoming thread's saved context was corrupted while it was off-CPU (wrong-old_idx save, or a stale-slot revival) |

Zero of either across a full SMP=4 boot suite (259 PASSED). A `-j4` build that
produces an EL0 `AS MISMATCH` fault with **zero** tripwire lines before it
means the corruption enters by a path other than the switch/context machinery —
that result would be as valuable as a hit.

### Tid generations (`WakeHandle`) — the once-and-for-all half (same day)

The CAS fixes close the *transition* races but leave the design asymmetry that
bred them: **pids are effectively generational** (`allocate_pid` is a monotonic
counter — a stale pid misses in the table) while **tids were bare indices into
a recycled 256-slot array** — a stale tid is indistinguishable from a live one,
and every per-thread structure is keyed by the ambiguous kind. Two residuals
survived the CAS alone: a stale waker could still spuriously wake a slot's new
occupant when that occupant happened to be legitimately WAITING, and its sticky
`WOKEN_STATES` store could spend a phantom wake on the new occupant's next park.

Now (`crates/akuma-exec/src/threading/mod.rs`): `SLOT_GEN[tid]` bumps once per
slot lifetime in `scrub_thread_slot` (every claim path runs it under the winning
FREE→INITIALIZING CAS). A **`WakeHandle`** packs `(generation << 16) | tid`;
wait registrations store handles, not tids, minted by the waiter itself at
enqueue time (`current_wake_handle` / `wake_handle_for_thread`), and
`wake_by_handle` refuses a stale generation *before any side effect*. The
`core::task::Waker` plumbing packs the handle into the raw-waker data pointer,
so `Waker`-storing registries (terminal input, ssh) got incarnation-binding for
free. Converted queues: futex `FUTEX_WAITERS` (`src/syscall/sync.rs`), pipe
pollers, msgqueue send/recv pollers, eventfd pollers, `VFORK_WAITERS`,
`ProcessChannel` pollers. Same-tid queue *scans* (dequeue, purge, self-locate)
still key on `handle.tid()` — they identify entries, they never act on the
thread through the bare index.

Left on bare tids deliberately: `pend_signal_for_thread`, `request_thread_kill`
and the kill paths — their callers resolve the tid from live process state at
call time, and their per-thread *array stores* (pending-signal bits) are a
staleness surface the wake layer cannot fix. If those ever need it, the
mechanism is in place (`thread_generation`, `WakeHandle::is_current`).

Regression test: `a_stale_handle_is_refused_even_against_a_waiting_new_occupant`
(host) — the exact case the CAS alone cannot defend.

### Do not chase `[SGI-S STACK]` lines for idle tids

The stack-aliasing tripwire fired ~20k times per SMP=4 boot for
`new_tid=1..3`: per-core idle threads are seeded at bringup on their per-core
*boot* stacks, not the pool stacks registered for their slots, so every switch
into an idle thread tripped it. Those lines were pure noise (present on known-
good boots) and are now gated by `IS_IDLE_THREAD`. A remaining `[SGI-S STACK]`
line for a non-idle tid is a real finding.

## 3. Where to look in the kernel

With the handoff ruled out (§2a), the live candidates are:

1. **Rust's packet is read correctly but has already been freed.** `[x0]`
   resolving to a small integer is what a *reused* heap slot looks like, not
   what corruption looks like. That points at the thread being started against
   an argument the parent has already dropped, or started twice.
2. **Stale TLB on the child's core** for the packet's page. §2b closed one such
   hole; `mark_thread_ready` ordering is *not* one of them — `THREAD_STATES`
   uses `SeqCst`.
3. **Demand-fault double-allocation** in a shared address space. Partly
   defended already: the fault path serializes per page via
   `fault_slot_acquire` and `map_user_page` reports `installed=false` on a lost
   race.

`clone_thread` itself is `crates/akuma-exec/src/process/mod.rs`.

### Measured after the §2b fix (2026-08-05, ~12 min of `-j4` build, SMP=4)

| | pre-fix baseline (~300 s) | with the §2b fix (~720 s) |
|---|---|---|
| `SIGSEGV in clone_thread` (the crash above) | 3 | **0** |
| `Instruction abort from EL0` (the class below) | 6 | 2 |
| `[FUTEX-ORPHAN]` | 0 | 0 |
| what killed the build | — | `could not compile primeorder … (signal: 11)` |

Not a controlled A/B — the runs differ in length, and a build's work changes as
it progresses — so read it as "class 1 stopped appearing and class 2 did not",
not as proof. A plausible mechanism for class 1 going away: with the guard page
unwritable again, a thread stack overflow *faults* where it used to scribble
silently into the neighbouring mapping — and a freshly-malloc'd thread packet is
exactly the kind of neighbour that would land in.

**The remaining blocker is the class below.**

### A second, separate crash lives in the same logs — do not conflate them

The `-j4` logs also contain freshly-exec'd rustc processes taking an
**instruction abort** at a PC whose low bits are constant and whose high half
grows by exactly `0x30000000` (`INTERP_BASE`) per occurrence:

```
ELR=0x6006c964 → 0x9006c964 → 0xc006c964 → 0xf006c964 → 0x12006c964 → 0x15006c964
```

Symbolized, `ld-musl+0x6c964` is a **function prologue**, entered with
`x0 = 0x30000000` (the loader base) and `x1 = sp+8` — the `__dls2(base, sp)`
signature. `x19 = x20 = x29 = x30 = 0` throughout: the process is at its very
first instructions, and `x30 = 0` says it arrived by a *tail* branch, not a call.

Facts established about it, so nobody re-derives them:

- **The branch is indirect.** `objdump -d` over the whole of ld-musl finds no
  direct branch to `0x6c964` at all, and a PC-relative `b` could not reach a
  target `0x30000000` away regardless (`b` is ±128 MB). So `__dls2` is entered
  through a pointer *read from memory* — which is what carries the corruption.
- **N is a per-boot sequence, not per-process noise.** It starts at 2 and
  increments by exactly 1 per occurrence, across different processes and
  different address-space owners, and resets to 2 on the next boot. That means
  one thing is accumulating, and each fault leaves it one step worse — the shape
  of a *shared* object being re-relocated, not of independent corruption.
- **The victim is sometimes running in another pid's address space.**
  `[IA] pid=247` while the fault block says `Process 270` — a process at its new
  image's entry point whose address space is owned by pid 247, i.e. a
  vfork-fastpath child that reached `_dlstart` without its own AS. But not
  always: the other fault in the same run had `[IA] pid=268` == `Process 268`.
- **It survives the §2b fix and is now what kills the build**
  (`could not compile primeorder … (signal: 11)`).
- Care with the relocation story: aarch64 uses **RELA**, and musl's stage-1
  relocation is `*rel_addr = base + rel[2]` — an *assignment*, which is
  idempotent. So "relocations applied twice" cannot by itself produce
  `base*N + offset`; either `base` is wrong at some point, or the pointer is
  being written by two parties.

  Refinement worth keeping (2026-08-05): that idempotence holds *only* because
  the addend comes from the RELA entry. Any path that takes the addend from the
  **slot itself** — musl's `DT_REL` implicit-addend form, `*slot = base + *slot`
  — produces exactly this `base*N` accumulation. That shape is the thing to hunt
  for. The kernel's own pass (`crates/akuma-exec/src/elf/mod.rs:381`) is
  `*ptr = base + addend`, i.e. the safe form.

New constraints from the 2026-08-06 `-j4` run (instrumented kernel, N ran 2→6):

- **The carrier is global kernel-side state, not any file's pages.** N=2,3,4
  hit `rustc`, `rustc`, `cargo` in a 2 s window at boot; N=5 and N=6 hit
  `/bin/busybox.static` at T872 and T1096 — *different inodes*, so per-inode
  page-cache/file-buffer theories can't explain one shared counter. All five
  kernel relocation appliers were re-audited the same day: absolute writes to
  private `alloc_and_map` frames, applied after the copy — a corrupted GOT
  byte in the source buffer would be *overwritten* by the absolute relocation,
  so the interp-GOT-in-shared-cache family is ruled out too.
- **N advances once per CRASH, not per exec.** Hundreds of successful execs
  sit between N=4 (T35) and N=5 (T872) without advancing it.
- **The register fingerprint is byte-identical across victims** —
  `x0=0x30000000 x2=0x30000000 x3=0x300c03d8 x1/SP=0x203ffff588/80`,
  callee-saved all zero, `SIGSEGV after 0.00s`. A process at its very first
  instructions with an already-poisoned branch target reads as much like
  "ERET'd at the wrong PC from birth" as "branched through a corrupted slot" —
  instrument `do_execve`'s final entry/ELR value before trusting the branch
  story.
- **Cheap live reproducer:** on a devbox-smoltcp guest under `-j4` load, plain
  `busybox <anything>` invocations over ssh crash with the next N every ~dozen
  execs. No build harness needed to iterate.
- **The `+= base` writer exists and is now located (2026-08-06).** ld-musl's
  `_dlstart` at `0x69f14-0x69f20` does `ldr x3,[x2,x5]; add x3,x3,x2;
  str x3,[x2,x5]` — *slot += base*, the exact accumulation form. It is musl's
  **RELR** apply loop: `readobj --dynamic-table` on the disk's ld-musl shows
  `DT_RELR 0x13580, RELRSZ 48` — Alpine links everything with compressed
  relative relocations, and **RELR entries are implicit-addend by design**.
  The kernel has *no* RELR handling anywhere (`grep -ri relr` over the kernel:
  one unrelated comment), which is fine for private pages (musl/crt applies
  its own, once) — so the remaining question is ONLY: *which physical page do
  the faulting execs share?* One `+= base` per sharing exec on one shared
  frame produces the observed global N sequence exactly. Prime suspect: a
  file-page-cache frame reached through a write that should have CoW'd
  (`is_shareable_mapping` admits RO PTEs; the victims' RELR slots must be
  written by their own startup code through what should be a private page —
  same neighbourhood as the FIXED madvise-WILLNEED zero-fill bug). Decide it
  by logging the slot value + frame PA at exec / at the CoW fault for the
  slot's page: same PA across two processes = case closed.
- **This class is what wedges `-j4` builds — the "live-but-idle rustc" wedge
  is downstream of it, not a scheduler bug.** Live capture 2026-08-06 T1200:
  jobserver `pipe=3 bytes=0 readers=7 writers=7 pollers=7`, seven futex
  waiters frozen since T≈35 — the exact moment three of these instruction
  aborts killed two rustcs and a cargo child, taking their jobserver tokens
  with them. The kernel pipe/futex machinery is exonerated by its own
  decision table (`bytes=0, writers>0`). Fixing THIS class is the remaining
  blocker for a green `-j4`.

### Ruled out 2026-08-05 — do not re-derive these

Both of the obvious carriers for the growing value are eliminated by direct
measurement:

- **Not `e_entry`.** `/lib/ld-musl-aarch64.so.1` reads back clean from a running
  guest with `e_entry = 0x69de8` (`busybox od -A x -t x8 -N 32`). So `0x6c964`
  is not the entry point at all — consistent with §"the branch is indirect"
  above, it is the `__dls2` call — and the interpreter file/page-cache is not
  corrupted.
- **Not `AT_BASE`.** `load_elf_with_stack` pushes
  `AT_BASE = interp.base_addr` (`crates/akuma-exec/src/elf/mod.rs:821`), which is
  the `INTERP_BASE` **constant** and cannot accumulate. The faulting frame
  agrees: `x0 = 0x30000000`, the correct base. So `base` is right at the moment
  of the bad branch; it is the branch *target* that is wrong.

Also note the cadence precisely: **N increments once per *fault*, not per exec.**
One run had ~270 s and many hundreds of successful execs between N=4 and N=5.
Whatever holds the value is shared across processes *and* only advances when the
fault happens — which rules out anything recomputed per-exec from the file.

### A data-abort variant of the same class

Same `ELR`, but a **data** abort where `FAR` is an 8-byte ASCII string:

```
[Fault] Data abort from EL0 at FAR=0x382d72656462696c, ELR=0x3801c58c, ISS=0x21
[Fault]  x0=0x1 x1=0x382d72656462696c x2=0x0 x3=0x8
```

`0x382d72656462696c` is `"libder-8"` and `0x656e696c74756f2b` is `"+outline"`
(little-endian) — both rustc heap content: a `deps/` filename and the
`+outline-atomics` target feature. `x1 = x20 = FAR` is a pointer slot holding a
freed block that was reused as a string. Decode `FAR` as ASCII before assuming a
wild pointer; a printable `FAR` names the data that overwrote the slot and is a
strong hint about *which* allocator neighbour it was.

One such run also took **QEMU itself** down:

```
qemu-system-aarch64: Assertion failed: (isv), function hvf_handle_exception, file hvf.c, line 1883
```

That is HVF failing to decode a guest MMIO access — i.e. the kernel dereferenced
garbage that landed in the MMIO window. It exits QEMU with status 134, so a
supervisor loop must treat "QEMU died" as a normal outcome and reboot.

Where to start: `sys_execve` calls `vfork_complete(pid)` **before**
`proc.address_space.activate()` (`src/syscall/proc.rs`), waking the vfork parent
while the child is still between address spaces; and `vfork_process` gives the
child the parent's L0 table under a **new ASID**
(`child_ctx.ttbr0 = new_proc.address_space.ttbr0()`), so parent and child run the
same page tables under two ASIDs.

Not reproduced yet by `userspace/forktest/c_stress/dynspawn.c` (posix_spawn —
musl implements it with `CLONE_VM|CLONE_VFORK` — of a dynamically linked child,
4 threads): 600 spawns of a tiny dynamic ELF and 100 of `rustc --version` were
all clean. It currently needs the real build load.

## 3a. The instrumentation that decides it — read this before reproducing

Landed 2026-08-06, always on, so **any** repro answers the question instead of
motivating another instrumented build. The class costs 2-4 minutes of `-j4` per
occurrence; a run that produces a fault and still cannot say which theory is live
is a wasted run, and three sessions have had one.

The mechanism: musl's `__clone` pushes the child's entry and argument onto the
new stack before the `svc` (`stp x0,x3,[x1,#-16]!`) and the child pops them
(`ldp x1,x0,[sp],#16`) — so the `stack` value the kernel is handed **is the
address of that pair**. `clone_thread` snapshots the two words there, in the
parent's address space, into per-slot statics (`record_clone_snapshot`,
`crates/akuma-exec/src/process/mod.rs`). The fatal EL0 data-abort dump re-reads
them in the child's context and prints the comparison
(`print_spawn_fault_diag`, `src/exceptions.rs`):

```
[Fault]  tid=26 ttbr0_live=0x1a000067f00000 ttbr0_proc=0x1a000067f00000
[Fault]  FAR as ASCII: "+strict-" (freed block reused as string?)
[Fault]  clone-handoff tid=26 stack=0x24477d510 from pid=417/tid=22 ttbr0=0x...
[Fault]    at clone: entry=0x37fbd3c0 arg=0x24477d520
[Fault]    now:      entry=0x37fbd3c0 arg=0x24477d520  (intact)
[Fault]  [x19=0x3d96f980] = 0x2d7463697274732b ...
```

Read the flags:

| what prints | what it means | where to go |
|---|---|---|
| `*** AS MISMATCH: L0 BASE DIFFERS ***` | the thread is executing in someone else's page tables — every "corrupt pointer" in the dump is a correct pointer resolved in the wrong space | §2f, then the switch path in `threading/mod.rs` and the vfork/exec ordering below |
| `(asid differs only — normal…)` | **not a finding.** A cloned thread is handed the parent's ttbr0 while its `Process` gets a fresh ASID over the same L0 | ignore it |
| both clean, `[x19]` is a string | the argument was delivered correctly and the packet behind it was freed | a genuine lifetime bug — candidate 1 |

`FAR`-as-ASCII is printed unconditionally now, so nobody has to remember the
`"libder-8"` precedent by hand.

**The `at clone:` / `now:` handoff lines are informational, not a verdict.**
An earlier revision flagged any difference as `*** HANDOFF CHANGED ***`; that was
a false positive and has been removed. musl's child starts with
`ldp x1, x0, [sp], #16` — it pops both handoff words and then uses that same
address as its stack, so its own first frame legitimately overwrites them within
a few instructions. The `at clone:` values are the trustworthy half (they record
what the parent actually handed over); `now:` only means anything for a fault
taken before the child ever ran. The first fault ever caught by this
instrumentation printed `HANDOFF CHANGED` and it was pure noise — the real
signal in that dump was `[x19] = 0x10a66f20 0x0 0x0 0x0`, i.e. a NULL
`self->prev`, which is what led to §2e.

## 3b. Hardening worth doing regardless of the outcome

`entry_point_trampoline` resolves its `Process` with
`table::find_process(|p| p.thread_id == Some(tid))` — a linear scan resting on an
invariant (one ACTIVE process per tid) that nothing enforces, sitting on the exact
path where every fault in this runbook happens. `THREAD_PID_MAP[tid]` is the
authoritative mapping, `clone_thread`/`vfork_process` both populate it before the
child can run, and `current_process_shared()` already trusts it. Preferring it
(scan as fallback) removes a whole class from consideration for free. §2c shows
the scan is *currently* safe; it is safe by an ordering two subsystems away, which
is not where you want a hot-path invariant to live.

## 3c. Live theories, ranked (2026-08-06)

> **Outcome, same day.** None of T1-T4 was the dominant cause — a clone(2)
> flag-semantics bug was (§2e), and it was found by reading the faulting PC's
> disassembly rather than by testing any theory here. The ranking below is kept
> because **T1 was independently confirmed** as a real, separate bug once the
> §2e fix removed the noise (§2f): a thread was caught executing in foreign page
> tables. T1 is now the live hunt. T2 and T3 are weakened — much of what
> motivated them (the byte-identical string faults of §2d) is explained by §2e
> instead.
>
> The methodological lesson is worth more than the ranking: three sessions
> theorised about memory aliasing from register dumps, and five minutes of
> `objdump` on the faulting PC — §1, which the runbook already told you to do
> first — named the exact musl function and line. Symbolize before you theorise.

Ordered by how much of the evidence each one explains, not by how easy it is to
test.

**T1 — Cross-address-space aliasing (CONFIRMED 2026-08-06 as a real bug — see §2f).** The child dereferences a
pointer that is *correct*, in an address space that is *wrong*: same VA, different
physical page, so the packet slot reads back as another rustc's string heap. This
is the only theory that explains, without coincidence, why two different processes
fault byte-identically on the same string (§2d), why `FAR` keeps decoding as
another process's data rather than as garbage, and why the second crash class has
a victim "running in another pid's address space". It also predicts §2a's result:
`clonearg.c` measured the *handoff* and would pass regardless, because the handoff
is not what is broken. **Decided by:** `*** AS MISMATCH: L0 BASE DIFFERS ***`,
which has now fired once (§2f) — a thread whose live `TTBR0_EL1` named neither
its own `Process` nor its parent, but a third address space entirely.
Suspects, in order: `sys_execve` calling `vfork_complete(pid)` **before**
`proc.address_space.activate()` (`src/syscall/proc.rs`), which wakes the vfork
parent while the child is between address spaces; `vfork_process` giving the child
the parent's L0 under a *new* ASID; a demand-fault double-install (`map_user_page`
reports `installed=false` on a lost race — check every caller honours it).

**T2 — Packet use-after-free (the runbook's original candidate 1).** The argument
is delivered intact and the memory behind it has already been recycled. §2c kills
the mechanism that made this attractive (kernel-reports-failure-after-success), so
it now needs a *different* freeing party — a double-start, or a child that
completes and is re-entered. **Decided by:** handoff `(intact)` **and**
`ttbr0_live == ttbr0_proc` **and** `[x19]` printing string bytes. If all three
hold, this is the answer and the hunt moves into thread-slot lifetime.

**T3 — Stale stack page under the child (residual §2b).** The child's stack VA
resolves to a page that is not the one the parent wrote — the mprotect/TLB family
that §2b closed one instance of, possibly with another instance left under
shared-AS/new-ASID. **No longer decidable by the handoff lines** — see the note
at the end of §3a explaining why a changed `now:` value is expected. Note this is the
one thing §2a's 144,260-child probe did *not* cover: it proved stores are
*visible*, not that the child's mapping is the same mapping.

**T4 — Lazy-region snapshot staleness (new, secondary).** `clone_thread` copies
the parent's lazy-region map into the child (`clone_lazy_regions`), a *snapshot*
taken at clone time. Fault-time lookups normally resolve through
`address_space_owner_pid_for_fault()` (TTBR0 → the non-shared owner), which is
correct — but `lazy_region_lookup_for_page_fault` **falls back to the caller's
pid**, and that fallback reads the frozen copy. A thread demand-paging a region
its parent mmap'd after the clone would then get obsolete flags/source, or nothing
at all. Explains an unmapped-page abort in the mmap range (the third fault in
`selfhost_vm_smp4.log` has `FAR=0x242b33727`, squarely in the `0x240000000` mmap
window) better than it explains the `thread_start` class.

**Amplifier, not a cause: thread-slot pressure.** The same log carries
`clone_thread failed: No free user thread slots` — rustc's rayon pool under `-j4`
exhausts the 256-slot table. Slot exhaustion means slots are recycled as fast as
they are freed, which is the precondition every reuse-race theory needs. If a fix
attempt appears to work, check it did not merely lower slot churn.

**Dead, do not revive:** kernel-reports-failure-after-success, SA_RESTART replay,
stale-`Process` scan in the trampoline, ASID reuse without flush, implicit-addend
relocation in the kernel — all five with file:line in §2c.

## 4. Reproduce

Roughly 1 fault per 2-4 minutes of `-j4` build, with 3 in a 190 s window seen:

```bash
# your OWN cloned disk + your OWN ports; two other agents' VMs use 2322/2422
INSTANCE=N DISK=<your-clone>.img MEMORY=4096 SMP=4 SNAPSHOT=0 \
  cargo run --profile release-smp-shared --features devbox-smoltcp,no-tests
```

then in the guest:

```sh
cargo build -p akuma --profile release-smp-shared --features devbox-smoltcp,no-tests -j4
```

`scripts/`-side driver: the retry loop pattern in
[`selfhost-kernel-build.md`](selfhost-kernel-build.md) — and note that a
`Compiling`-line stall heuristic is **not** a liveness signal
(`debug-futex-lost-wakeup.md` §0).

### Getting the in-guest build to actually start (2026-08-06)

Three things cost a session's worth of time before a single fault was observed.
None of them are about the bug; all of them are about the harness.

- **The workload has to be a cold build.** `cargo build` on the guest's warm
  `target/` finishes in ~1 minute and spawns almost no threads. Delete
  `target/aarch64-unknown-none/release-smp-shared` first — the fault rate is a
  function of concurrent `rustc` processes, not of wall time.
- **`cargo` cannot reach crates.io from the guest even though the network is
  fine.** `curl -o /dev/null -w '%{http_code}' https://index.crates.io/config.json`
  returns `200` in 0.3 s while cargo reports
  `Failed to connect to index.crates.io:443 after 420 ms`. It is libcurl's HTTP/2
  multiplexing; `[http] multiplexing = false` in `/root/.cargo/config.toml` (or
  `CARGO_HTTP_MULTIPLEXING=false`) fixes it. Don't debug the net stack.
- **`--offline` can fail with sources fully cached.** `no matching package named
  arm_pl031 found` while `~/.cargo/registry/{cache,src}` both hold it and
  `~/.cargo/git/db` holds the `embedded-tls` checkout: what is stale is the
  *index* cache, which a cargo upgrade invalidates. Refresh it once online
  (`cargo fetch`), then run every subsequent pass `--offline` so a long repro loop
  never touches the network again.
- **Detach properly.** `ssh host 'cmd &'` dies with the session. Write the loop to
  a file on the guest and start it with
  `busybox setsid busybox sh /root/loop.sh > log 2>&1 < /dev/null &`. Prefer that
  form over `busybox sh -c '…'`: the inner quoting does not survive the trip
  through `ssh` and busybox reports `loop.sh: applet not found`.
- **`scp` to the guest hangs.** Pipe base64 over the existing ssh channel
  instead — `busybox base64 -d > /root/prog && chmod +x /root/prog` with the
  encoded bytes on stdin. 145 KB lands in 0.1 s.
- **The guest has almost no standalone coreutils.** `nproc`, `sleep`, `timeout`,
  `head`, `nohup`, `which` and `uname` all need a `busybox ` prefix.
- **Downloads are flakier than the index.** Even with multiplexing off,
  `static.crates.io` connection failures are common. Add `[net] retry = 20` and
  run `cargo fetch` in a retry loop until it is clean *before* starting the
  timed repro, so a network stall is never mistaken for a kernel hang.

## Verify

**Start here — it takes 30 seconds and needs no build load.** Run
`userspace/forktest/c_stress/tidflags` on the guest. It must print
`[tidflags] PASS (0 failures)`; anything else means the §2e regression is back
and there is no point running a `-j4` build yet. `spawnalias` is the
corroborating stress test, but do not A/B on it — it reproduces this class only
about one run in three and has passed on a known-broken kernel.

A fully fixed kernel must then produce, across a full `-j4` build:

- zero `[Fault] SIGSEGV in clone_thread` lines,
- zero `signal: 11` errors in cargo's output,
- zero `AS MISMATCH: L0 BASE DIFFERS` lines,
- no `[BKL] stuck` storm,
- and the build reaching `rust-lld` rather than stalling with `.rmeta` files
  that have no matching `.rlib`.

**Status as of 2026-08-06:** the first two are not yet met. The §2e fix took the
run from a steady stream of faults down to a single one, but that one was an
`AS MISMATCH` (§2f), and the run also logged 81 `[BKL] stuck` events before
wedging with live-but-idle `rustc` processes. The §2g state-transition fixes
target both (same corruption family) but are unproven against the `-j4`
reproducer — a verifying run must also show **zero `[TTBR SAVE-MISMATCH]` /
`[TTBR LOAD-MISMATCH]`** lines (§2g). Note the boot-time `[BKL] stuck` storm
(onset at the "NEON registers across preemptive scheduling" boot test,
owner-core near-permanent hold) reproduces on both fixed and unfixed kernels —
measure it as a rate, not a boolean, when A/B-ing.

Regression probes for the neighbourhood (all should stay green):
`userspace/forktest/c_stress/tidflags`, `futexkey`, `threadmax`, `tlsdirty`,
`futextest_rs`.

## Background

- [`../archive/CLONE_TIDFLAGS_THREAD_LIST_LOCK.md`](../archive/CLONE_TIDFLAGS_THREAD_LIST_LOCK.md)
  — the full 2026-08-06 investigation record behind §2e and §2f: disassembly,
  mechanism, before/after probe tables, the two instrumentation flags that had
  to be corrected, and the methodology lessons.
- [`../archive/THREAD_STATES_RACES_TID_GENERATIONS.md`](../archive/THREAD_STATES_RACES_TID_GENERATIONS.md)
  — the full record behind §2g: the check-then-store race family, the tid
  generation / `WakeHandle` design, the boot-probe A/B tables, and why the
  `[SGI-S STACK]` noise had to be gated.
- [`../archive/SELFHOST_DEVBOX_SMOLTCP.md`](../archive/SELFHOST_DEVBOX_SMOLTCP.md)
  — "Open issue: thread-spawn SIGABRT under real `-j4` parallelism", "The crash
  has a fixed address (2026-08-04)", and the ASID free/flush ordering fix that
  was *not* the cure.
- [`../reference/subsystems/thread-lifecycle.md`](../reference/subsystems/thread-lifecycle.md)
  — the spawn/teardown state machines and their locks.
- [`debug-futex-lost-wakeup.md`](debug-futex-lost-wakeup.md) — the symptom this
  bug wears in `[THR-DUMP]`.
