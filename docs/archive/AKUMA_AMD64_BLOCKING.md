# amd64: folding the scheduler into `akuma-threading`, and the `CR0.WP` hole next door

**Status, 2026-09-07: landed and boot-verified.** Two changes that arrived
together because they live in the same handler. They are unrelated in cause and
both are prerequisites for self-hosting on this target.

- § 1 — `CR0.WP` was never set, so `copy_to_user` onto a copy-on-write page
  corrupted the *parent*. Silent, no error, no fault.
- § 2 — `amd64/src/sched.rs` stopped being a scheduler. `akuma-threading` is now
  the scheduler on both architectures; what is left here is the machine effects
  it is not allowed to know about. Pipes, `futex` and `wait4` park instead of
  polling as a consequence, not as the goal.

Verification for both is § 3. **The RAM root and Firecracker only** — do not
verify against `root=/dev/sda1`, which has a separate open failure (Akuma cannot
read files off the ext2-on-USB root) that makes every result here unreadable.

---

## 1. `CR0.WP` was clear, and copy-on-write was not enforced against the kernel

### What was measured

`grep`ped directly, 2026-09-07: bit 16 of `CR0` is set nowhere in the tree.
`boot.s` sets `PG|PE`, then `CR0.EM=0`/`CR0.MP=1` for SSE; `smp.rs`'s
`enable_sse` does the same four bits for each AP. `WP` appears in neither.

Intel SDM Vol. 3A §4.6.2: *"If CR0.WP = 0, supervisor-mode accesses may write to
any linear address with a translation, regardless of the R/W flag."*

### Why that is a correctness bug and not a missing hardening

`copy_to_user` writes through the **user** virtual address with `rep movsb` from
ring 0 (`akuma-user-access`). After a `fork` the child's pages are mapped
read-only and CoW-marked. So:

1. the child calls `read(2)` into a buffer it has not written since the fork;
2. the kernel's `rep movsb` writes a read-only page from ring 0;
3. with `WP` clear this **takes no fault at all**;
4. the copy-on-write break therefore never runs;
5. the bytes land in the frame the **parent** is still mapping.

The parent's memory changes underneath it, with no error anywhere. For a target
whose goal is to build its own kernel, this is the shape of bug that produces a
wrong `.o` and a green build.

### The second half: the arm that could never fire

`idt::page_fault_dispatch` already had an arm for this, and its comment
described the behaviour as if it worked:

> *"a `copy_to_user` landing on a CoW page is a legitimate write the kernel
> should break the sharing for, not an `EFAULT` to hand back — sending it to the
> fixup would make `read(2)` into a forked child's buffer fail with no
> explanation."*

The code under that comment required `PF_USER`:

```rust
const PF_PRESENT: u64 = 1 << 0;
const PF_WRITE: u64 = 1 << 1;
const PF_USER: u64 = 1 << 2;
if code & (PF_PRESENT | PF_WRITE | PF_USER) == (PF_PRESENT | PF_WRITE | PF_USER)
```

Bit 2 reflects the **CPL of the access**, not the `U/S` bit of the page, so a
`copy_to_user` fault arrives with it clear. The arm was unreachable from ring 0.
It was also unreachable *at all*, because with `WP` clear no such fault existed
to classify — which is why nothing ever noticed.

Both halves are fixed together. Fixing either alone moves the bug rather than
removing it: `WP` alone turns the corruption into an unexplained `EFAULT` from
`read(2)`, and the predicate alone changes nothing.

### What changed

- `akuma_user_access::init_smap` now also sets `CR0.WP`. That function is the
  right home because it is already called on **every** core — the BSP from
  `kmain`, each AP from `ap_entry64` — and all three bits (`SMAP`, `SMEP`, `WP`)
  make a ring-0 access to a user page obey a rule it was previously exempt from.
  A secondary that missed any one of them would enforce a different rule than
  the boot core.
- The fault predicate is `is_write_to_present_page()`: a write to a mapped page,
  **from either ring**. Letting a supervisor write through costs nothing —
  `cow_write_fault` re-checks `prot.user` and refuses a kernel page, and
  `akuma_cow` answers `Fault` for a page that is read-only on purpose rather
  than CoW-marked, so a ring-0 write to either still falls through to the
  user-copy fixup and the `EFAULT` those cases deserve.

### Blast radius, checked rather than assumed

`WP` only bites where the kernel writes through a read-only mapping. Checked:

- the ELF loader writes segment bytes through `phys_ptr` (the physmap alias,
  kernel-RW), not through the user VA — `init_smap`'s own comment already said
  so, and it is still true;
- `Prot::KERNEL_RO`/`KERNEL_RX` have no live call site (only `encode`'s test);
- every other kernel write to user memory goes through `write_bytes`, whose
  `rep movsb` is already fixup-covered.

Boot-confirmed: 284 passed / 0 failed at `SMP=1`, 293/0 at `SMP=4`, with
`fork`, `execve` and the whole `redirect` pipeline suite unchanged.

### The regression test

`uaccess::write_protect_check` builds the scenario by hand, because no probe
reaches it reliably: two user VAs onto **one** CoW-marked frame with a share
count of 2, a kernel `write_bytes` through one of them, and then the question
the bug got wrong — *did the other VA see it?*

```
  wp: CR0.WP is set                                          [OK]
  wp: a kernel write to a read-only user page is refused      [OK]
  wp: and the page is unchanged                               [OK]
  wp: AC is clear after the refused write                     [OK]
  wp: a kernel write to a CoW page succeeds                   [OK]
  wp: and is visible through that mapping                     [OK]
  wp: and NOT through the peer mapping (the sharing broke)    [OK]
  wp: the pair no longer shares a frame                       [OK]
```

The first three are the other half: a page that is read-only **on purpose** (an
ELF text segment, an `mprotect(PROT_READ)` range) must still refuse a kernel
write. Without them, turning `WP` on would relocate the corruption rather than
end it.

### `PageFaultCode`, and why the bits got a type

The five `#PF` error-code bits were three `const`s declared inside a function
body plus a bare `code & 1 == 0` a few lines above them. Three problems, none of
them style:

- **No owner.** They are the fault-side counterpart of the PTE bits at the top
  of `paging.rs`, and a second private spelling in another file is the drift
  this target already paid for once with `Prot`.
- **Two of five were missing.** Bit 3 (a reserved bit set in a paging-structure
  entry — always a kernel bug, usually `NX` with `EFER.NXE` clear) and bit 4 (an
  instruction fetch) were never named, so the handler could not tell a malformed
  entry from an ordinary protection fault. § 2's successor work — lazy `mmap` —
  needs bit 4 to decide whether a demand-paged frame must be executable.
- **Nothing pinned the positions.** A one-off `1 << 2` at a call site is right
  or wrong with no test either way.

`paging::PageFaultCode` is now the decode, `describe_page_fault` prints it in
words before the fatal printer's hex, and `paging::smoke_test` pins every bit
against a decoded value. It lives in `amd64/src/paging.rs` rather than a crate
because that is where the tree's stated split already puts x86 paging bits —
`akuma-mmap`'s `types.rs` says so explicitly: *"the bits live with the walker
that writes them — `akuma_mmu::types` for AArch64, `amd64/src/paging.rs` for
x86_64."*

---

## 2. `amd64/src/sched.rs` stopped being a scheduler

`docs/archive/AKUMA_SELF_HOSTING_AMD64.md` calls this **A1**, the trunk the rest
of the amd64 self-hosting tree hangs off, and its one-line summary is
"`sched.rs` → `akuma-threading`".

### The wrong turn first, because it is the instructive part

The first attempt did **A2 without A1**: it gave `amd64/src/sched.rs` its own
`State::Blocked`, its own `wake_pending` flag, its own deadline sweep, and
pointed pipes, `futex` and `wait4` at them. It worked — 295/0 at `SMP=1`, 304/0
at `SMP=4`, every wait released by a real wake.

It was still wrong, and not for tidiness. That file had grown a **second, weaker
copy** of a state machine `akuma-threading` already had, hardened by years of
AArch64 incidents:

| the new local version | the crate | what the difference costs |
|---|---|---|
| `State::Blocked` | `thread_state::WAITING` | — |
| `Task::wake_pending` | `WOKEN_STATES` | — |
| `Task::wake_at_us` | `WAKE_TIMES` | — |
| `wake(slot)` | `WakeHandle` / `ThreadWaker` | **slot generations.** A wake held across a slot's death is *refused*; the local version would spend it on whoever inherited the slot |
| `state = Runnable` on a wake | a `WAITING → READY` **CAS** | a plain store overwrites a concurrent `TERMINATED`, resurrecting a killed thread onto page tables that are being freed |

The last two rows are the argument. Both are real, documented AArch64 failures
(`ThreadWaker::wake`'s own comment; the `THREAD_STATES` check-then-store races),
both are **silent**, and the local version had neither defence. Two
implementations of one thing meant the target aiming at `cargo -j4` — the one
that will run the most threads — was running the copy that had never been
debugged.

### What the fold actually moved

The crate schedules; `amd64/src/sched.rs` performs. Everything the scheduler
cannot know about is registered once as `akuma_threading::X86ArchHooks` and
lives in a per-slot `Machine` side table indexed by the crate's own thread id:
`CR3`, the TSS trap stack, `IA32_FS_BASE`/`GS_BASE`, the `fxsave` area, the Big
Kernel Lock's recursion depth, which slot each core runs, and which slot idles
it. `Task`, `State`, `Context`, `TASKS`, `switch_context` and `try_switch` are
gone.

The crate gained, all `#[cfg(target_arch = "x86_64")]` and inert on AArch64:

- `X86ArchHooks` and its registration;
- `x86_wake_pass` — the `WAITING` deadline sweep, mirroring the AArch64 one's
  CAS discipline. Without it `schedule_blocking` with a deadline parks forever
  on this target, because the AArch64 sweep lives inside `ThreadPool::
  schedule_indices`, which x86 never enters;
- an SMP-aware `x86_pick_next` (honours `ON_CPU` and pinning) and an
  `x86_yield_now` that runs the hooks around the switch;
- **`schedule_blocking`'s x86 arm, routed through the cooperative switch.** The
  AArch64 arm raises an SGI and `wfi`s; on x86 there is no SGI, and
  `akuma_cpu::park::wfi` is `hlt`, which inside a syscall (interrupts masked by
  `IA32_FMASK`) halts the core **forever**. So the x86 arm *is* the switch;
- a narrow, allocation-free spawn surface — `x86_claim_slot` / `x86_seed_entry`
  / `x86_publish` / `x86_adopt_running_thread` / `x86_finish_current` — because
  amd64's entries are plain `extern "C" fn() -> !` with their data in a side
  table, not boxed closures.

`MAX_THREADS` gained an `x86_64` arm of **512**, deliberately not a tidier
universal number: `amd64/src/sched.rs` raised its own table 96 → 512 against a
measurement (a real session runs dozens of commands, every one a slot, `fork`
takes a second), and folding must not walk that back to 256.

### Two bugs the fold surfaced, both found by the boot suite

Neither was reachable from a clean build, and both were caught by the park
self-test rather than by review:

1. **`current_thread_id()` answered `0` for every thread on x86.** It reads
   `TPIDRRO_EL0`, which `akuma-cpu` correctly stubs off AArch64 — so
   `schedule_blocking` published `WAITING` for the **boot thread** instead of
   its caller. The symptom was exact and immediate: `block: the worker is
   parked` `[FAIL]` while the worker ran merrily on. The identity now comes from
   the target's per-CPU block through `X86ArchHooks::current_slot`.
2. **`trigger_sgi` registered as `unreachable!()` panicked on the first wake.**
   The reasoning was that the x86 path never raises an SGI, so a field it cannot
   reach should say so loudly. It reaches it on every wake: `ThreadWaker::wake`
   raises one unconditionally after a successful `WAITING → READY` CAS, because
   on AArch64 that is how the woken thread gets *looked at*. It is now a no-op
   with the reason written down, alongside `wake_core`, `wake_remote_idle` and
   `end_of_interrupt`, which are no-ops for the same reason.

### `BACKSTOP_US`: a tripwire, not a design

`block_current` — the untimed form — still installs a deadline one second out.
A correct wait has a wake path; if that path is missing, an untimed park is an
unrecoverable hang with no output, which `thread::drain`'s own comment already
calls the failure mode that costs the most to diagnose and says the least. With
a backstop the same bug degrades to a 1 Hz poll. `sched::backstop_wakes()`
counts them and the boot prints the total.

It is spelled in `amd64/src/sched.rs` rather than in the crate on purpose:
AArch64 parks untimed constantly and has an interrupt-driven scheduler to
recover, where this target reaches its scheduler only by being called.

**It has read 0 on every run**, on every machine. That is the number to watch.

### What now parks, and what deliberately does not

| | before | now | wake path |
|---|---|---|---|
| pipe read / write | `yield_now` | park | `akuma_pipes` `Wakes`, fired by `pipe::fire` |
| `futex` wait | `yield_now` | park | dequeue + `sched::wake` from `wake`/`requeue`/`wake_op` |
| `wait4` | `yield_now` | park | `spawn_record_exit` → `wait4_wake_all` |
| stdin-pipe `poll_input_event` | `yield_now` | park | as pipes |
| **netpoll daemon** | `yield_now` | **unchanged** | none exists |
| **`read_console`** | `yield_now` | **unchanged** | none exists |

The last two are deliberate: **this target takes no device interrupts at all** —
the LAPIC timer is the only vector — so nothing exists to wake a NIC poller or a
UART reader. An IOAPIC is what moves those two rows.

Notes on the conversions:

- **pipes.** `fire` was written as a real function and called on every path that
  produces wakes precisely so this would be a change to one body. It was. The
  token stays `()`: `Wakes<W>` yields `(tid, W)` and this kernel's `tid` **is**
  the scheduler slot, so the identity is already the key.
- **futex.** The signal did not change — "am I still queued?" is still the whole
  test, still durable state rather than an edge. The waiter now parks between
  tests and a waker also calls `sched::wake`, in that order of authority: get
  the wake wrong and the waiter is merely *late*; get the table wrong and it is
  *incorrect*. Requeued waiters are deliberately not resumed — they were moved,
  not woken.
- **`wait4`.** Waiters go in a bitmap over slots rather than a per-child parent
  link, because `wait4(-1)` waits for *any* child and the waiter is often not in
  the spawn table at all (`sshd` is pid 1).
- **`thread::drain`** now wakes the group, or an `exit_group` while a sibling
  holds an untimed `FUTEX_WAIT` would wait out the full backstop.

### There is no `prepare_block`

The pre-fold version had one, as its own answer to the window between
registering as a waiter and parking. The crate closes that window without the
caller's help: a waker's first act is a sticky `WOKEN_STATES` flag, which
`schedule_blocking` tests on entry **and again atomically with publishing
`WAITING`** (`publish_waiting_and_take_pending_wake`). So the shape is two steps,
not three, and there is nothing to forget:

```rust
if !pipe::check_set_reader(id) {   // test AND register, in one step
    sched::block_current();        // park
}
```

### Not done, and named

- **`thread.rs` (391 lines) and the `smp.rs` ticket-lock BKL → `akuma-bkl`** are
  the rest of A1 in the chart. Not attempted here.
- **`thread.rs` and the BKL swap**, above, are the whole of what is left.

### The boot paths, and the seven tests nobody was running

`kmain` (PVH) and `kmain_mb2` (multiboot2/GRUB) each carried a full copy of the
boot, described in the latter's own comment as "deliberately parallel to
`kmain`, and in the same order". Patching `sched::init()` into both by hand is
what prompted looking, and the two had drifted: **the multiboot2 path was not
running seven of the PVH path's tests** —

```text
blk::smoke_test              usermode::execve_test
sched::block_smoke_test      usermode::fork_test
usermode::spawn_test         usermode::busybox_test
usermode::console_notify_test
```

— the entire process-lifecycle suite, missing from **the least-tested path in
the tree**, the one that runs on real silicon where the emulators cannot reach.
It is why bare metal reported 262 checks where QEMU reported 295, and nobody had
noticed, because both said `0 failed`.

The two blocks that were genuinely identical now live in `amd64/src/boot.rs`:

- `early_init` — descriptor tables, per-CPU block, IDT, SMAP/SMEP/WP, the
  scheduler, drop the identity map. Six steps whose *order* is load-bearing and
  which were written out twice.
- `self_tests` — one canonical list, one canonical order, both paths.

What deliberately stays per-protocol is what genuinely differs: how the console
comes up (a UART versus a GRUB framebuffer, which must exist before anything can
report a failure), where the machine description comes from, what the root
filesystem *is* (virtio disk, GRUB module in RAM, or ext2 on USB), how the
network is configured, and what happens when `init` exits. Five different
decisions with five different reasons — a single function taking five callbacks
would have been a merge in name only.

## 3. Verification

Host tests: `cargo test` over the workspace, green.

**amd64** — `scripts/utils/amd64_trials.py` runs local QEMU and the trashcan's
Firecracker in parallel (see `docs/runbooks/amd64-bare-metal-loop.md`):

| | passed | failed | parks | backstop |
|---|---|---|---|---|
| baseline, before any of this | 276 | 0 | — | — |
| QEMU/TCG `SMP=1` | 295 | 0 | 23 | **0** |
| QEMU/TCG `SMP=4` | 304 | 0 | 18 | **0** |
| Firecracker `SMP=1` (KVM, the box) | 285 | 0 | 15 | **0** |
| Firecracker `SMP=4` (KVM, the box) | 294 | 0 | 27 | **0** |
| **bare metal, the HP box** | **262** | **0** | — | — |

The park totals are the evidence: the blocking self-test performs 3 of its own,
so the rest came from real pipe, `futex` and `wait4` waits — and **every one was
released by a real wake**, none by the backstop.

One flake, on QEMU/TCG at `SMP=4` only: `net: the netpoll daemon is being
scheduled` wants >100 daemon laps per 4000 boot-task yields and got fewer, once,
on a run that shared the laptop with a `cargo build`. Re-runs on an idle host
measure 3977–3989 laps against a bar of 100. Timing, not mechanism — but it is
the first thing to re-check if it recurs on an idle machine.

**aarch64 must be unaffected**, since the crate change is `cfg`-gated. Measured
rather than asserted, on one accelerator so there is no second variable:

| kernel | PASSED | failures |
|---|---|---|
| `main` @ b7c89d47 | 307 | `retired_reclaim_ab` |
| branch, pre-fold @ dbbbb986 | 307 | `retired_reclaim_ab` |
| **branch, post-fold** | **307** | `retired_reclaim_ab` |

Identical, including the failure — which is a standing bug on `main` and is
written up separately in `docs/archive/POST_EXIT_PMM_RECLAIM.md`.

### Two things about running the aarch64 suite that cost time here

- **HVF needs `MEMORY=2048M`.** Below it, this suite dies with
  `Assertion failed: (isv) ... hvf.c` and QEMU exit 134 on the user-copy EFAULT
  probe, whose faulting instruction is an LDP and carries no syndrome. That is
  the configuration, not the kernel — `scripts/cargo_runner.sh` prints a warning
  saying exactly this, and it is easy to mistake for a crash you just caused.
- **Lima runs it under KVM**, which is the fast correct option on an Apple
  Silicon laptop: `scripts/lima_aarch64_run.sh` (a wrapper around
  `cargo_runner.sh`, which now selects KVM on its own). The laptop builds, Lima
  runs — Lima has no Rust toolchain and the host has no KVM.

## Background

- `docs/archive/AKUMA_PIPES_EXTRACTION.md` — the pipe table both kernels share,
  and the two `akuma-threading` findings § 2 closes one of.
- `docs/archive/AKUMA_AMD64_COW.md` — the copy-on-write decision § 1 makes
  reachable from ring 0.
- `docs/archive/AKUMA_USER_ACCESS_X86_FIXUP.md` — the `rep movsb` fixup that
  a `WP` fault falls through to when the page is read-only on purpose.
- `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` — why a CoW-demoted page and
  an `mprotect(PROT_READ)` page must be told apart, which is what the `COW`
  marker bit is for.
