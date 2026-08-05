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

This is still **open** (2026-08-05). It no longer *blocks* the in-VM self-host
build — that now completes, because a retry resumes and crates that compiled
stay compiled ([`selfhost-kernel-build.md`](selfhost-kernel-build.md) §5) — but
each occurrence still costs a whole crate compile, so it is worth fixing.
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

Read the three flags:

| what prints | what it means | where to go |
|---|---|---|
| `*** AS MISMATCH ***` | the thread is executing in someone else's page tables — every "corrupt pointer" in the dump is a correct pointer resolved in the wrong space | the vfork/exec ordering below, and `Process::run`'s `activate()` |
| `*** HANDOFF CHANGED ***` | the child's stack page is not the page the parent wrote | aliasing / stale TLB / demand-fault double-install — §2a's probe did *not* cover this, it only covered visibility |
| both clean, `[x19]` is a string | the argument was delivered correctly and the packet behind it was freed | a genuine lifetime bug — candidate 1 |

`FAR`-as-ASCII is printed unconditionally now, so nobody has to remember the
`"libder-8"` precedent by hand.

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

Ordered by how much of the evidence each one explains, not by how easy it is to
test. Every one of them is decided by §3a's three flags, which is why the
instrumentation went in before any of them was chased.

**T1 — Cross-address-space aliasing (new, best fit).** The child dereferences a
pointer that is *correct*, in an address space that is *wrong*: same VA, different
physical page, so the packet slot reads back as another rustc's string heap. This
is the only theory that explains, without coincidence, why two different processes
fault byte-identically on the same string (§2d), why `FAR` keeps decoding as
another process's data rather than as garbage, and why the second crash class has
a victim "running in another pid's address space". It also predicts §2a's result:
`clonearg.c` measured the *handoff* and would pass regardless, because the handoff
is not what is broken. **Decided by:** `*** AS MISMATCH ***`, or by `[x19]`
reading back differently in the fault dump than it must have at clone time.
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
shared-AS/new-ASID. **Decided by:** `*** HANDOFF CHANGED ***`. Note this is the
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
  `busybox setsid busybox sh -c '... > log 2>&1' < /dev/null &`.

## Verify

A fixed kernel must produce, across a full `-j4` build:

- zero `[Fault] SIGSEGV in clone_thread` lines,
- zero `signal: 11` errors in cargo's output,
- and the build reaching `rust-lld` rather than stalling with `.rmeta` files
  that have no matching `.rlib`.

Regression probes for the neighbourhood (all should stay green):
`userspace/forktest/c_stress/futexkey`, `threadmax`, `tlsdirty`, `futextest_rs`.

## Background

- [`../archive/SELFHOST_DEVBOX_SMOLTCP.md`](../archive/SELFHOST_DEVBOX_SMOLTCP.md)
  — "Open issue: thread-spawn SIGABRT under real `-j4` parallelism", "The crash
  has a fixed address (2026-08-04)", and the ASID free/flush ordering fix that
  was *not* the cure.
- [`../reference/subsystems/thread-lifecycle.md`](../reference/subsystems/thread-lifecycle.md)
  — the spawn/teardown state machines and their locks.
- [`debug-futex-lost-wakeup.md`](debug-futex-lost-wakeup.md) — the symptom this
  bug wears in `[THR-DUMP]`.
