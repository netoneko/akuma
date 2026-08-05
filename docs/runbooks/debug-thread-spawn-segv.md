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

This is the **open** blocker for the `-j4` in-VM self-host build (2026-08-05).
It is *not* a futex bug and not a lost wakeup — if you arrived here from a
process parked forever in `futex`, read
[`debug-futex-lost-wakeup.md`](debug-futex-lost-wakeup.md) §4 first: the parked
`pthread_join` is the *survivor*, this fault is the cause.

## 0. Confirm it is this bug and not a lost wakeup

Three checks, in order — each is cheap and each has fooled a previous session:

| Check | This bug | Not this bug |
|---|---|---|
| `[FUTEX-ORPHAN]` lines in the log | **zero** | any ⇒ futex table defect, go to `debug-futex-lost-wakeup.md` §3 |
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

`clone_thread` itself is `crates/akuma-exec/src/process/mod.rs:2723`.

### A second, separate crash lives in the same logs — do not conflate them

The `-j4` logs also contain freshly-exec'd rustc processes taking an
**instruction abort** at a PC whose low bits are constant and whose high half
grows by exactly `0x30000000` (`INTERP_BASE`) per occurrence:

```
ELR=0x6006c964 → 0x9006c964 → 0xc006c964 → 0xf006c964 → 0x12006c964 → 0x15006c964
```

Symbolized, `ld-musl+0x6c964` is a **function prologue**, entered with
`x0 = 0x30000000` (the loader base) and `x1 = sp+8` — i.e. `_dlstart_c` calling
`__dls2` through a GOT entry that has had `+= base` applied **N times**. That is
the signature of ld-musl's `R_AARCH64_RELATIVE` self-relocation running against
data that was **already relocated**, with N incrementing once per occurrence
across the boot.

Both victims are processes whose address space is owned by a *different* pid
(`[IA] pid=119` while the fault block says `Process 125`), i.e. the vfork
fastpath. Note also `sys_execve` calls `vfork_complete(pid)` **before**
`proc.address_space.activate()` (`src/syscall/proc.rs`), which wakes the vfork
parent while the child is still between address spaces.

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
