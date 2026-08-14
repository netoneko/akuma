# Fork-from-a-threaded-process kills the process — SOLVED 2026-08-08

> **Status: root-caused and fixed.** Full writeup, evidence and numbers:
> [`docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md) §12.
>
> In one paragraph: `fork` demotes the address space read-only, every thread that
> writes the same page faults at once, the first one through breaks CoW and consumes
> the CoW reference — and the threads behind it arrive holding a fault for a write
> that is now legal. The handler never re-read the PTE, so it judged that fault
> against state that had moved on: no CoW reference left, and no region record
> either, because an ELF `.data`/`.bss` page has none. It killed the process for
> writing its own global. Fix: `stale_write_fault_absorbed` re-reads the PTE at the
> top of the write arm and retries when the write is already permitted, bounded on
> `(VA, PTE)`. Pristine 10/10 SEGV → 0; new probe `c_stress/bssfork` 8/8 → 0;
> boot self-test `stale_write_fault_absorbed`.
>
> **Three claims below are wrong, and each cost real time — read the corrections:**
> - "The pointer lost bit 28" (§"The pointer is wrong"). It did not. `0x420260` is
>   `g_reader_checks`, a `.bss` global; `x20` held `&g_map`, i.e. the section base,
>   not a data pointer. `readelf -sW` settles it in one command.
> - "Two or more fork rounds AND two or more threads" (§"The minimal trigger").
>   Threads are required; rounds are not. `bssfork 1 3` fails 5/25 — the table read
>   a probability as a threshold.
> - Question 3's guess that the unrecorded page is "just the binary's read-only
>   image — benign, and the fault is correct". It is the *writable* image, and the
>   fault is wrong. That question was one `readelf -S` from the whole answer.
>
> Question 1 (is this also cargo's null `Rc`?): **probably not — different bugs.**
> The `-j4` campaign finally ran (15 rounds, fresh VM each, full clean build):
> **11 GREEN at 180-181 s, zero `EXIT=139`** — ~91% confidence the rate is under
> 20%, short of the 95% this brief asked for. The mechanism argument is stronger:
> this defect signals at the store and corrupts nothing, cargo's zeroes a qword
> silently with no fault. See audit §12.8-12.9.
>
> The campaign also had to fix its own instrument first — three unconditional
> traces were saturating the UART and no build completed at all until they were
> gated ([`docs/archive/SERIAL_TRACE_TRAFFIC_AUDIT.md`](SERIAL_TRACE_TRAFFIC_AUDIT.md)).
> And 4 of the 15 rounds died of the `[BKL] stuck tag=511` class, which this brief
> calls noise and which is now the dominant instability (audit §12.7).

---

## Original brief (kept for the record)

# Fork-from-a-threaded-process kills the process — now reproducible in 0.01s

**Repo:** `/Users/netoneko/github.com/netoneko/akuma`, branch `stabilize-devbox`, HEAD `b9396f1`.
**Read first:** [`docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)
— the full investigation log, with three dead theories and the instrumentation
that killed them. This file is the task; that file is the evidence.

This supersedes [`CARGO_HEAP_NULL_RC.md`](CARGO_HEAP_NULL_RC.md) as the active
brief. That defect (cargo dereferences a null `Rc` during a `-j4` self-host build,
`EXIT=139`, ~1 in 5 runs, ~10 minutes per attempt) is still open — but there is now
a **deterministic, sub-second reproducer of a fault in the same class**, and
whether the two are the same bug is question #1 below.

---

## What's known

`userspace/forktest/c_stress/cowstale.c` kills itself with `EXIT=139` in ~0.01
seconds, **8 out of 8 attempts, on an idle VM with no build load**:

```
[Fault] Data abort from EL0 at FAR=0x420260, ELR=0x400868, ISS=0x4f
[WPF] pid=7 as_owner=7 va=0x420000 pa=0x91dac000 mapped=true cow_ref=0
      lazy_self=0xffffffffffffffff lazy_owner=0xffffffffffffffff have_owner=true
[Fault] Process 8 (/tmp/cowstale) SIGSEGV after 0.01s
[Fault] SIGSEGV in clone_thread, calling exit_group
```

`ISS=0x4f` → DFSC `0b001111` = **permission fault, level 3**, with WnR set: a write
to a page that is mapped but not writable *from this core's point of view*.
`[WPF] cow_ref=0 lazy_self=NONE` is the signature `src/exceptions.rs` names in its
own comments as "the signature that killed cargo mid-build".

### The minimal trigger

| rounds | pages | reader threads | result |
| ---: | ---: | ---: | --- |
| 1 | any | any | PASS |
| 5 | 8 | 0 | PASS |
| 5 | 8 | 1 | PASS |
| **2** | **8** | **2** | **SEGV** |
| 5 | 1 | 3 | SEGV |
| 20 | 4 | 2 | SEGV |

**Two or more `fork()` rounds AND two or more live threads.** One fork is fine.
One thread is fine. Both, twice, and it dies. That is exactly cargo's shape: a
multi-threaded process forking repeatedly.

### The pointer is wrong, not just the permission

The mapping is at `0x10420000`; the faulting write went to `0x420260` — **bit 28
missing**, landing in the binary's own read-only image instead of the heap
mapping. Registers at the fault:

```
x2=0x5041524e00000000  x3=0x5041524e00000000   ← the probe's parent pattern
x20=0x420248                                   ← the write pointer, minus 0x10000000
```

So the kernel's SIGSEGV is arguably *correct* — that address really is read-only.
The defect is upstream: a pointer the program read from its own memory came back
with its high bits gone. That is the same class as "a live pointer qword reads
back as zero", which is what kills cargo.

**Do not assume they are the same bug.** Establishing that is the first task.

---

## Reproduce

```bash
# 1. Build the probe (needs aarch64-linux-musl-gcc; it is in userspace/build.sh now)
cd userspace/forktest/c_stress
aarch64-linux-musl-gcc -static -O2 -Wall -Wextra -o cowstale cowstale.c -pthread

# 2. Calibrate on real Linux FIRST — a FAIL there means the probe is wrong,
#    not the kernel. Expect PASS.
docker run --rm --platform linux/arm64 -v "$PWD/cowstale:/cowstale:ro" alpine /cowstale 40 32 3

# 3. Boot an idle devbox (no build needed — the fault does not require load)
DEVBOX_DISK=disk_selfhost.img DEVBOX_MEMORY=14336 SMP=4 INSTANCE=1 SNAPSHOT=1 \
  bash overlays/devbox/run-smoltcp.sh > boot.log 2>&1 &
until grep -aq "Started sshd" boot.log; do sleep 2; done
```

The guest has no `scp`; stream the ELF in over ssh stdin from Python (the `ssh`
CLI is blocked by policy, so drive it from `subprocess`):

```python
import subprocess
data = open("userspace/forktest/c_stress/cowstale", "rb").read()
subprocess.run(['ssh','-o','StrictHostKeyChecking=no','-p','2322','root@localhost',
                'busybox cat > /tmp/cowstale && busybox chmod +x /tmp/cowstale'], input=data)
r = subprocess.run(['ssh','-o','StrictHostKeyChecking=no','-p','2322','root@localhost',
                    '/tmp/cowstale 5 8 3; echo EXIT=$?'], capture_output=True, text=True)
print(r.stdout)   # expect: Segmentation fault / EXIT=139
```

Working copies of these helpers are in this session's scratchpad
(`push_run.py`, `rerun.py`, `matrix.py`) but they are trivial to rewrite.

---

## The questions, in order

1. **Is this the cargo null-`Rc` defect, or a second bug sharing a signature?**
   Both are `EXIT=139` with a corrupted pointer read from the process's own
   memory; cargo's read back as 0, this one lost bit 28. If they are the same,
   fixing this fixes the build. If not, this is still worth fixing and the build
   defect needs its own handle. Cheapest discriminator: fix this, then run the
   `-j4` build ~15 times and see whether the crash rate moves.

2. **Why does the write target lose bit 28?** Establish whether the *pointer* is
   corrupt in memory (read `g_map` back and compare) or whether the *access* is
   being mistranslated. The probe writes through `g_map + p * PAGE_SIZE` where
   `g_map` is a plain global — a PC-relative load from the binary's own data.

3. **What is at `0x420000` with `mapped=true` and no region record?** The `[WPF]`
   line says the page is mapped (`pa=0x91dac000`) but neither an eager nor a lazy
   region covers it. Either the ELF loader maps image pages without recording a
   region (likely, and then this VA is just the binary's read-only image — benign,
   and the fault is correct), or something is mapping pages off the books.

---

## What is already ruled out (don't re-litigate)

Each of these was tested with instrumentation that is still in the tree; the audit
doc has the numbers.

- **Premature free / use-after-free.** `PMM-UAF=0` and `PMM-QUAR-DF=0` across four
  complete 4-way self-host builds, with a poison quarantine proven to catch a
  deliberate UAF on every boot.
- **CoW refcount desync.** `FREE=false`, `tracked=true`, no free record, contents
  intact at every anomaly; a durable one-bit-per-frame record says some frames had
  never been CoW-shared at all.
- **`mprotect` widening an upgrade across a region (D1).** `MPROT-WIDEN=0` against
  **3043** `mprotect` calls. Real latent bug, fix plan in the audit doc §7, not
  this defect.
- **A stale/overlapping region shadowing the live record (D9).**
  `[REGIONS] claimed_by=1` every time, with correct flags.
- **A lost permission of any kind.** Six `[PTE]` samples, unanimous
  `ap=AP_RW_ALL(writable)` — the page table was correct at every `[EAGER-UPGRADE]`.
- **`ENOSYS`/errno-as-pointer.** Zero `ENOSYS`/`EFAULT`/`EINVAL` in the crashing
  run's syscall ring. Enabling `SYSCALL_ERRNO_DIAG_ENABLED` adds tens of thousands
  of `readlinkat`-`EINVAL` lines per build and finds nothing.

---

## Instruments already in the tree

All are boot-suite tested (`src/process_tests.rs`) and cost little; read
`docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` §3 before adding more.

| Instrument | Reads out as |
| --- | --- |
| `pmm::is_page_free(pa)` | `FREE=` in the forensic line — `true` proves a double-owned frame |
| free ledger | `last_free=(tid= age=)`, `-1` = never freed |
| poison quarantine (`config::PMM_UAF_QUARANTINE`) | `[PMM-UAF]`, `[Mem] quar= UAF=` |
| CoW ring + durable bitset (`config::COW_REF_LEDGER`) | `[COW-HIST]`, distinguishes "never shared" from "aged out" |
| `[REGIONS]` / `[PTE]` / `[LAZY]` | region count, raw descriptor + decoded AP, lazy overlap |
| `[MPROT-WIDEN]` | `mprotect` upgrades recorded outside their range |
| `[MADV]` counters | `MADV_DONTNEED` divergence from Linux |
| `[TLB] stale_write_faults= repeats=` | write faults on already-writable pages |

The `[WPF]` diagnostic (`print_write_perm_fault_diag`) fires on the SIGSEGV path
and is what produced the evidence above. `print_page_forensics` is the richer dump
— it currently runs on `[EAGER-UPGRADE]` and `[WILD-DA]`; **wiring it into the
`[WPF]`/SIGSEGV path is probably the first thing to do**, since that is where this
reproducer lands and it would answer question #3 immediately.

---

## Fixed along the way (verified, not suspects)

- **D8 — `sys_munmap` dropped regions.** It matched one eager region by exact
  `start_va` and returned, so an unmap starting mid-region or spanning two freed
  only the first region's pages, reported success, and left the rest mapped with
  its VA never recycled; it also never reached lazy regions when an eager one
  matched. Now `detach_eager_regions_in_range` in `akuma-exec` (pure, 9 host
  tests) plus a boot self-test. Green on a full `-j4` build.
- **The `[EAGER-UPGRADE]` repair was a TLB flush in disguise** — it rewrote AP bits
  that already said writable, and `update_current_user_page_flags`'s trailing
  `flush_tlb_page` was doing the actual work. It now checks the PTE first and, when
  the write is already permitted, just invalidates (bounded, with `[TLB]` counters
  and a `[TLB-STALE]` fall-through).

---

## Traps

- The `ssh` CLI is blocked by policy — drive it from Python `subprocess` (`-p 2322`
  at `INSTANCE=1`).
- Serial logs interleave across cores and contain binary bytes: always `grep -a`.
- The guest has no `nohup`/`scp`/`tail`/`sleep` on `PATH`. `busybox --install -s
  /root/bbin` and **append** `/root/bbin` to `PATH` (append, so busybox's `ar`
  doesn't shadow binutils).
- Never wait synchronously on a QEMU process; poll its log.
- `[BKL] stuck tag=511` storms are a known separate class — noise, don't chase.
- **Two boot-suite tests fail on a pristine tree too** —
  `thread_slot_reclaim_on_spawn` (`hot_reclaim=206`) and `retired_reclaim_ab`
  (745p vs a 768p threshold). Verified by stashing everything and re-running.
  Pre-existing; not yours.
- The single-core `release` boot suite stalls after `drivers-bkl-drop` (~3130
  lines) with and without these changes. Also pre-existing.
- **Green build runs prove almost nothing.** At the documented ~1-in-5 rate, four
  consecutive greens happen ~41% of the time with the bug fully intact — that
  actually happened during this session while `cowstale` was failing 8/8 on the
  same tree. Use the deterministic probe as the oracle, not the build.
- Kernel/syscall changes need a boot-suite self-test in `src/process_tests.rs`.
- **Do not commit or push** — the user drives all commits.

---

## Deliverable

Root cause with evidence, a fix, a regression test (extend `cowstale` or add a
sibling probe, calibrated against real Linux aarch64 like the rest of `c_stress`),
and a doc update — extend
`docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` rather than starting a new
one, and add a row to the triage matrix in `docs/runbooks/README.md`.

Then answer question #1 with numbers: run the `-j4` self-host build enough times
to say whether the cargo defect moved. ~15 consecutive greens gets you to roughly
95% confidence that the rate dropped below 20%; fewer than that is not evidence.
