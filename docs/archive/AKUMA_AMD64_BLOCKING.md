# amd64: a blocked state, and the `CR0.WP` hole that turned up next to it

**Status, 2026-09-07: landed and boot-verified.** Two changes that arrived
together because they live in the same handler. They are unrelated in cause and
both are prerequisites for self-hosting on this target.

- § 1 — `CR0.WP` was never set, so `copy_to_user` onto a copy-on-write page
  corrupted the *parent*. Silent, no error, no fault.
- § 2 — the scheduler gained a blocked state, and pipes, `futex` and `wait4`
  stopped polling.

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

## 2. The scheduler grew a blocked state

`State` was `Unused | Reserved | Runnable | Finished`. A waiter was therefore
always runnable, and every wait in this kernel was a poll: `pipe::fire` was an
empty function, `futex` span on `yield_now` re-testing table membership, and
`wait4` span on `yield_now` re-calling `sys_waitpid`.

That is survivable on a bring-up target and is not survivable on one expected to
run `cargo -j4`, where it is most of a core per blocked thread.

### What was added

`State::Blocked`, invisible to the picker. That is the whole mechanism: a task
the round-robin cannot choose burns no CPU, and `wake` is the transition back to
`Runnable`.

Three things had to come with it.

**`Task::wake_pending`, and the arm-before-you-test discipline.** The flag is
needed even though every writer runs under the BKL. A pipe reader registers in
the pipe's poller set, the pipe lock is released, and only then does it park; a
wake landing in that gap finds the task Runnable, does nothing, and **drains the
poller set** — so the waiter parks with no registration and nobody to wake it.
Recording the wake as durable state rather than an edge closes that. The shape
every caller uses is Linux's `prepare_to_wait`:

```rust
sched::prepare_block();                 // arm
if !pipe::check_set_reader(id) {        // test AND register, in one step
    sched::block_current();             // park
}
```

Arm-then-test has no window; test-then-arm does.

**Deadlines.** `block_until_deadline` takes an absolute `uptime_us`, and
`try_switch` sweeps expired parks before it picks — before, not after, or a core
with nothing else runnable parks in `hlt` and serves the timeout a tick late.
The sweep is gated on a single atomic (`NEXT_DEADLINE_US`, maintained with
`fetch_min`) so the common path never walks 512 slots.

**`all_user_tasks_finished` had to change.** It asked "is this slot not
Runnable", which is the same question as "is it dead" only while `Blocked` does
not exist. Left alone, the boot's drive loop would have declared the shell
finished the moment it waited for a keystroke.

### `BACKSTOP_US`: a tripwire, not a design

`block_current` — the untimed form — still installs a deadline, one second out.
A correct wait has a wake path; if that path is missing, an untimed park is an
unrecoverable hang with no output, which `thread::drain`'s own comment already
calls the failure mode that costs the most to diagnose and says the least. With
a backstop the same bug degrades to what this kernel did before: a poll, at 1 Hz
instead of at scheduler frequency. `sched::backstop_wakes()` counts them and the
boot prints the total.

**It has read 0 on every run.** That is the number to watch: it climbing names a
wait whose wake path is missing, and it is reported at the end of the suite
alongside the park and wake totals for exactly that reason.

### What now parks, and what deliberately does not

| | before | now | wake path |
|---|---|---|---|
| pipe read / write | `yield_now` | park | `akuma_pipes` `Wakes`, fired by `pipe::fire` |
| `futex` wait | `yield_now` | park | dequeue + `sched::wake` from `wake`/`requeue`/`wake_op` |
| `wait4` | `yield_now` | park | `spawn_record_exit` → `wait4_wake_all` |
| stdin-pipe `poll_input_event` | `yield_now` | park | as pipes |
| **netpoll daemon** | `yield_now` | **unchanged** | none exists |
| **`read_console`** | `yield_now` | **unchanged** | none exists |

The last two are deliberate and are not oversights: **this target takes no
device interrupts at all** — the LAPIC timer is the only vector. There is
nothing to wake a NIC poller or a UART reader, so both must keep polling. An
IOAPIC is what changes that, and it is what would let those two rows move.

Notes on the three conversions:

- **pipes.** `fire` was written as a real function and called on every path that
  produces wakes precisely so this would be a change to one body. It was. The
  token type stays `()`: `Wakes<W>` yields `(tid, W)` pairs and this kernel's
  `tid` **is** the scheduler task slot, so the identity a wake needs is already
  the key.
- **futex.** The signal did not change — "am I still queued?" is still the whole
  test, still durable state rather than an edge. What was added is that the
  waiter parks between tests and a waker also calls `sched::wake`, in that order
  of authority: get the `sched::wake` wrong and the waiter is merely late (the
  backstop releases it); get the table wrong and it is incorrect. Requeued
  waiters are deliberately **not** resumed — they were moved, not woken, and
  waking them would undo the point of a requeue.
- **`wait4`.** Waiters go in a bitmap over task slots rather than a per-child
  parent link, because `wait4(-1)` waits for *any* child and the waiter is often
  not in the spawn table at all (`sshd` is pid 1). Any child's exit wakes the
  whole set; each parked task re-runs `sys_waitpid` and parks again if the exit
  was not its child.
- **`thread::drain`** now wakes the group. The exit flag is only tested at
  syscall entry and inside the futex wait loop, and that loop parks now — without
  the wake an `exit_group` while a sibling holds an untimed `FUTEX_WAIT` would
  wait out the full backstop.

### Not done: folding this into `akuma-threading`

`AKUMA_PIPES_EXTRACTION.md` framed this as "two pieces, not a rewrite": a
blocked state the picker skips, and `schedule_blocking` routed through the
cooperative x86 switch. Only the first piece was needed. `amd64/src/sched.rs`
runs its own switch (`switch_context`), not `akuma-threading`'s, so nothing here
goes through `schedule_blocking` at all and there was no `trigger_sgi` + `wfi`
to replace.

**The finding that doc recorded still stands and is still unfixed**:
`akuma-threading`'s x86 switch is a port of `amd64/src/sched.rs`'s that dropped
the `pushfq`/`popfq`, and therefore reintroduces the interrupt-flag leak that
switch's own comment exists to record. Nothing consumes it yet. **Fix it before
anything adopts it** — the symptom is an unrelated intermittent hang.

---

## 3. Verification

Host tests:

```bash
cargo test -p akuma-pipes -p akuma-cow -p akuma-mmap -p akuma-syscalls-sync \
    --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```
→ 8 / 54 / 27 / 42 passed, 0 failed.

Boot suite (QEMU/TCG, `amd64/run.sh`), including `redirect_test`'s real busybox
`>`, `>>`, `cmd | cmd` and 12-stage pipeline:

| | passed | failed | parks | wakes | backstop |
|---|---|---|---|---|---|
| baseline, before any of this | 276 | 0 | — | — | — |
| `SMP=1` | 295 | 0 | 28 | 27 | **0** |
| `SMP=4` | 304 | 0 | 28 | 27 | **0** |

The park/wake totals are the evidence that matters: the blocking self-test
performs 3 parks of its own, so the rest came from real pipe, futex and `wait4`
waits — and **every one was released by a real wake**, none by the backstop.

Interactive check, driven over the console (the boot suite does not cover it):
`ls /bin | head -3`, a three-iteration `for` loop, `sleep 1`, `exit` — all
correct.

`aarch64 is untouched.` Only `amd64/src/` changed, so the AArch64 `kernel_tests`
that are the oracle for the pipe shim (`test_pipe_close_read_wakes_blocked_writer`,
`run_pselect6_registers_waker_test`, `test_sigpipe_terminate_no_deadlock`) are
unaffected by this pass.

Not run here: the HP box (`hpbox.firecracker`), and `/bin/futexops`, which is not
on the disk `amd64/mkdisk.sh` generates.

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
