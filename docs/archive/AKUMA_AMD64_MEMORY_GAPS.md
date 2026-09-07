# amd64 memory: what the probes say, measured five ways

**Date:** 2026-09-07
**Scope:** the ten `scripts/mem_suite.py` probes run five ways — on **real Linux
(both architectures)**, on **Akuma/aarch64 from `main`**, on **Akuma/aarch64 from
this branch**, and on **Akuma/amd64** with the region table of
`docs/archive/AKUMA_AMD64_MMAP_REGIONS.md` (B1/B2) in place. Plus a coverage
audit of every probe in `userspace/forktest/c_stress/`.
**Status:** measurement complete; six amd64 gaps identified and attributed. None
is a memory-mapping defect.

> **CLOSED, later the same day (2026-09-07).** Four of the six gaps below — and
> the fifth, file-backed `mmap` — were fixed in the pass this measurement
> ordered: `docs/archive/AKUMA_AMD64_MEMORY_CLOSEOUT.md`. **amd64 is 8/10 on both
> rigs**, and the two that remain are `mprotectlb` and `eager_mprotect_probe`,
> which need signal delivery (trunk A2) and were never on this list's to-do.
>
> The diagnosis below is left as written because it is still correct and it is
> what the fixes were built from. What changed is the **amd64 column of the
> table**, which now carries a "closed by" cell, and the "What to do next"
> section, which is now a record of what was done.
>
> Two findings from doing the work contradict the analysis here, and both are
> marked inline: `pread64` alone does **not** unblock `mmapsum` (§1), and item 3
> was not the missing files it looked like (§4).

---

## Why this was measured at all

The amd64 kernel failed six of ten memory probes right after B1/B2 landed, and
"six of ten" reads as a broken subsystem. It is not, and the way to establish
that is not to argue from the code — it is to run **the same static binaries**
on kernels that are known good and see which column the failure lives in.

That is what these probes are for. Nearly every one carries a
`docker run --platform … alpine /<probe>` line in its own header stating what a
correct kernel prints, so the Linux answer is not a new claim to be trusted; it
is the calibration the probe was written against. `mem_suite.py` grew a
`--docker` transport (2026-09-07) so that answer and the Akuma answer go through
**one** `verdict()` — a difference in the table below is then a difference in the
kernel and not in how the two were scored.

## The table

Identical binaries per architecture. Linux is `alpine` under Docker; aarch64 is a
`devbox-smoltcp` kernel built from `main` (b7c89d47) at `SMP=4` against a
copy-on-write clone of `devbox.img`; amd64 is the branch kernel at `SMP=1`.

The **amd64 (now)** column was added 2026-09-07 when the gaps were closed; the
`amd64` column beside it is the measurement this document was written from and is
left as it was taken.

| probe | Linux x86_64 | Linux arm64 | Akuma aarch64 (`main`) | Akuma aarch64 (this branch) | Akuma amd64 | what amd64 is missing | Akuma amd64 (now) |
|---|---|---|---|---|---|---|---|
| `mmap_stress` | PASS | PASS | PASS | PASS | **PASS** | — | **PASS** |
| `madvshared` | PASS | PASS | PASS | PASS | **PASS** | — | **PASS** — and no longer by skipping; see below |
| `shmanon` | PASS | PASS | PASS | PASS | **PASS** | — (was failing; fixed in B1) | **PASS** |
| `cowstale` | PASS | PASS | PASS | PASS | **PASS** | — | **PASS** |
| `mmapsum` | PASS | PASS | PASS | PASS | FAIL | `pread64` (x86_64 syscall 17) not dispatched | **PASS** — needed `pread64` **and** file-backed `mmap` |
| `mmap_file` | PASS | PASS | PASS | PASS | FAIL | file-backed `mmap` is `ENOSYS` **by design** — no page cache | **PASS** — `MAP_PRIVATE` served; `MAP_SHARED` writable still `ENOSYS` |
| `mremapmove` | PASS | PASS | PASS | PASS | FAIL | `mremap` not implemented | **PASS** |
| `smapsdirty` | PASS | PASS | PASS (3 DIVERGE) | PASS (3 DIVERGE) | FAIL | no `/proc/self/smaps`; `MADV_FREE` not implemented | **PASS (2 DIVERGE)** — one fewer than aarch64 |
| `mprotectlb` | PASS | PASS | PASS | PASS | FAIL | no signal delivery — the probe needs a `SIGSEGV` **handler** | FAIL — **A2**, unchanged and out of scope |
| `eager_mprotect_probe` | PASS | PASS | PASS | PASS | FAIL | a killed child exits `128+SIGSEGV`; no *signalled* wait status | FAIL — **A2**, unchanged and out of scope |
| | **10/10** | **10/10** | **10/10, 3 DIVERGE** | **10/10, 3 DIVERGE** | **4/10** | | **8/10, 2 DIVERGE** |

`madvshared`'s cell is the one worth reading twice. It passed in both columns, but
for opposite reasons: before, `madvise` was `ENOSYS` and all three of its
sub-tests reported `madvise unsupported, skipped`; now `MADV_DONTNEED` is real and
all three exercise it, including the two that check a `fork` peer's page survives.
A probe that passes by not running is the silent-pass trap in the one place the
suite's own `verdict()` cannot see it — the probe scored itself.

The branch column was measured separately (2026-09-07, `66237aff`, its own
`devbox-smoltcp` build against a copy-on-write clone of `devbox.img`) rather than
assumed equal to `main`'s. That mattered: the branch is 129 commits of amd64 work
that also moved five shared crates out of the aarch64 kernel, and the same
branch's aarch64 boot suite **does** carry a regression elsewhere
(`test_spawn_ext_passes_env`, `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` issue 4).
It is cell-for-cell identical to `main`, so the memory family specifically is
unaffected by that work — which is a measurement, not an inference from the diff.

Three readings follow immediately, and they are the point of doing it this way:

- **Linux passes 10/10 on both architectures**, so no failure is a probe bug or a
  toolchain artifact. The x86_64 binaries are freshly built by the same command
  that builds the aarch64 ones.
- **Akuma/aarch64 passes 10/10 too**, on `main` and on this branch alike. So the
  failures are not "Akuma's memory model"; they are this *target's* missing
  syscalls. The amd64 kernel is younger, not wrong.
- **The two aarch64 columns are identical**, so the amd64 port's five crate
  extractions did not disturb the memory family on the architecture they came
  from.

## The six, attributed

Each was diagnosed rather than inferred from the failure text.

### 1. `mmapsum` — `pread64` is not dispatched

`mmapsum` reads one file four ways (`read`, `pread`, `mmap`, `mmap` after
`madvise`) and compares digests. The `pread` arm returns `ENOSYS`, so it aborts
before comparing anything: `mmapsum: pread failed at 0`.

Verified it is the syscall and not the file: `wc -c /tmp/mem_suite_data` on the
same guest returns the right 262144, so `open`/`read`/`fstat` are all fine. There
is no arm for x86_64 syscall **17** in `amd64/src/usermode.rs`.

Cheap to fix, and worth it — `pread` is how every archive reader and every
`rustc` metadata load reaches into a file.

> **Corrected 2026-09-07 (same day).** "Unblocks `mmapsum`" is wrong, and the
> next-steps list below repeats the error. `mmapsum` hashes the file **four**
> ways and only the first is `pread`; the other three are `mmap`. Adding
> `pread64` moved its failure from `mmapsum: pread failed at 0` to
> `mmapsum: mmap failed` and left the verdict FAIL. It took item 1 **and** item 5
> to turn the probe green. The fix was still worth doing first — it is one arm
> and it unblocks real software — but the probe was not the thing it unblocked.
> Read a probe's source before claiming which change turns it green.

### 2. `mmap_file` — file-backed `mmap` is refused on purpose

`mmap(262144) failed`. This is the one **deliberate** refusal left in
`amd64/src/mm.rs`: serving a file-backed mapping as anonymous memory would look
like a working call and hand the caller a file full of zeros. It needs a page
cache, which is the last item of the B trunk.

### 3. `mremapmove` — `mremap` is not implemented

`mremap(grow, MAYMOVE): Function not implemented`. Not part of B1/B2. Now that
regions exist, `akuma_syscalls_mem::mmap`'s move-vs-expand decision is available
to build against, so this is smaller than it was.

### 4. `smapsdirty` — no `smaps`, no `MADV_FREE`

Four sub-probes; two `DIVERGE` and two `FAIL`:

```
smaps-present         DIVERGE  fopen(/proc/self/smaps) errno=2
proc-self-files       DIVERGE  5 missing: maps status stat statm cmdline
madv-free-accepted    FAIL     ret=-1 errno=38 (Function not implemented)
redis-arm64-cow-check FAIL     res=0 (madvise failed (not EINVAL)) -> redis EXITS
```

Note what the aarch64 column does here: it reports **3 DIVERGE and 0 FAIL**,
because there `MADV_FREE` returns `EINVAL` — and the probe's fourth sub-test
exists precisely because redis treats `EINVAL` as "unsupported, skip" and
`ENOSYS` as "broken, exit". So the amd64 gap is not merely "one more missing
syscall": **returning the wrong errno for an unimplemented call changes what
userspace does.** `EINVAL` here would move this probe from FAIL to DIVERGE and
let redis start.

`akuma-procfs` already holds the `/proc/<pid>/{stat,status,cmdline}` formats and
is already an amd64 dependency, so three of those five files are closer than they
look.

> **Corrected 2026-09-07 (same day).** Closer than they look, and for a different
> reason than this says: those three files were **already being served**. `open`
> and `stat` both answered for them; `access(2)` did not, because `sys_access`
> went straight to the disk and never consulted the `/proc` synthesis at all. So
> `proc-self-files` — which probes with `access` — reported `stat`, `status` and
> `cmdline` missing on a target that had all three. The bug was three callers and
> two implementations of "what exists under `/proc`", in a function whose own
> header says one implementation serves `open` and `stat` "so the two can never
> disagree".
>
> Also: **item 3 was never what made this probe fail.** Sub-probes 1 and 2 score
> `DIVERGE`, which is green. Item 2 alone (`MADV_FREE` → `EINVAL`) took
> `smapsdirty` from FAIL to PASS. Item 3 was worth doing on its own merits and
> took it from 3 DIVERGE to 2.

### 5 and 6. `mprotectlb` and `eager_mprotect_probe` — **not** `mprotect` bugs

Both look like memory failures and neither is. They are the two probes that need
**signals**, which this target does not have.

- `mprotectlb` installs a `SIGSEGV` handler and `siglongjmp`s out of it to test
  whether a downgraded page still faults. With no handler it simply dies: exit
  139, no output, scored `SILENT`.
- `eager_mprotect_probe` forks a child, has it write to an `mprotect`ed page, and
  asserts `WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV`. Here a killed
  process exits with **code** `128 + SIGSEGV` rather than reporting a signalled
  status, so that can never be true. It prints only `RESULT: FAIL` because the
  child's own diagnostic is lost to `_exit`, which does not flush stdio.

**`mprotect` itself works.** Verified directly on the guest rather than inferred:

```
mmap=0x100000000
wrote ok
mprotect(PROT_READ)=0
read back=a
about to write (should die)      → rc 139
```

Before B1 that write **succeeded**, because `mprotect` was `return 0`. So the
139 both probes trip over is the feature working.

Both belong to trunk **A2** (signals), not to B.

---

## Coverage: probes nothing runs

Measured while wiring `mem_suite`'s set into the trim-the-fat gate. Of 36 probe
sources in `userspace/forktest/c_stress/`:

| runner | before 2026-09-07 | after |
|---|---|---|
| `verify_trim.py` `EXERCISES` | 17 | **35** |
| `mem_suite.py` | 10 | 10 — all also in `EXERCISES` now |
| `epoll_suite.py` | 1 (`epollops`) | 1 |
| `futex_suite.py` | 4 | 4 |
| **nothing at all** | **17** | **0** (1 excluded with a reason: `dynchild`) |

**All of them are now in the gate** (2026-09-07). `EXERCISES` went 17 → 35: the
three memory ones first, then the remaining sixteen. Every one was run on a
booted VM before being added and its `healthy` string copied from that run's
**actual output**, which is the rule the existing entries were added under.
`dynchild` is the single exclusion, and for a reason rather than by omission: it
is `dynspawn`'s spawnee, not a standalone probe.

What the seventeen were built for, since that is what the gate now protects:

- **The `-j4` self-host jam** — the largest cluster, and the incidents least
  likely to be re-found by hand: `abortsig`, `segvchild`, `segvgroup`,
  `tlsdirty`, `pipewake`, `threadmax`, `clonearg`, `spawnalias`, `tidflags`.
- **The busybox hash miscompute** — a chain of eliminations, several of which are
  now the only record of what was ruled out: `computecheck`, `md5probe`,
  `readback`, `neonstate`.
- **Dynamic linking**: `dynspawn` (+ `dynchild` as its spawnee).
- **The shared file-page cache**: `fpcpoison` — cross-process integrity, and its
  own header says `mmapsum` does *not* cover it.
- **A forktest control experiment**: `pattern2_parent`.

Final state, measured on `disk.img` at `SMP=4` through `verify_trim`'s own
`EXERCISES`/`ssh` code rather than a lookalike: **18 pass, 1 known-fail, 0
unexpected.**

### `tidflags` is a KNOWN_FAIL, and it found something

Seven of its eight cases pass — all three of `clone(2)`'s tid flags behave as
Linux documents. The eighth does not:

```
pthread churn survives   FAIL — round 31: only 3/8 threads created
```

Deterministic in *where* (round 31 both runs) and not in *how badly* (1/8 then
3/8), which makes it a capacity wall rather than a race. `threadmax` corroborates
it from the other side and passes only because it retries:

```
[B] spawn refused at iter 249: rc=11 (EAGAIN) — retrying after 100ms
[B] retry succeeded — COOLDOWN WALL (transient), slots existed but were still cooling
```

Both describe one thing: a thread slot is not reusable immediately after a join.
Recorded with what its flipping would mean, as `KNOWN_FAIL_EXERCISES` requires.

### Two traps found by wiring these up, both worth more than the wiring

**A marker that can pass vacuously.** `dynspawn`'s summary line is
`=== DYNSPAWN DONE — 0 divergence(s) ===`, and that was the obvious marker. It is
also true when **nothing ran**: the probe spawns `/tmp/dynchild` 800 times, and
with the spawnee absent every `posix_spawn` fails, no child runs, and nothing can
diverge. Measured exactly that: `children reaching main : 0` beside
`posix_spawn/wait errors: 800`, under a green marker. The marker is now
`children reaching main : 800` — a count that cannot be reached by doing nothing
— and `dynchild` is staged (**dynamically** linked, which is the whole point of
it; a `-static` build would exit 42 while exercising none of the loader).

Only a flake exposed it: the probe passed vacuously twice, and then a run where
the launch itself failed reported `UNEXPECTED`, which is what prompted reading
the output instead of the verdict.

**Two probes on `disk.img` were stale binaries.** `smapsdirty` reported
`3 failure(s)` where the current source reports `0 failure(s), 3 documented
divergence(s)` — its DIVERGE classification and the explanatory suffixes were
simply absent, because the staged build predated them. `cowstale` — an exercise
**already in the gate** — differed from a fresh build too. `userspace/build.sh`
compiles and copies these, so a probe only refreshes when someone runs it; the
gate had been scoring an old binary. Both rebuilt and re-staged.

A third of the same class: `pattern2_parent` was built and copied inside
`build.sh`'s `WITH_FORKTEST` block despite being pure C with no Go in it, so it
had never actually reached `disk.img` — `mmap_stress` from the same two-line
block was there and it was not, meaning the block had run once and never again
after that line was added. Moved to the unconditional block.

---

## What was done next — all five, 2026-09-07

The list below was the plan; every item was taken in this order and verified
before the next was started. Full account, including what each measurement
actually showed: `docs/archive/AKUMA_AMD64_MEMORY_CLOSEOUT.md`.

| # | item | done | what it actually moved |
|---|---|---|---|
| 1 | **`pread64`** — one dispatch arm | yes | *not* `mmapsum` on its own (see the correction in §1); real software, and the `read` reference arm of `mmapsum` |
| 2 | **`MADV_FREE` → `EINVAL`** | yes, **and more** | `smapsdirty` FAIL → PASS. `MADV_DONTNEED` was implemented too, which turned `madvshared` from three skipped sub-tests into three real ones |
| 3 | **`/proc/<pid>/` on amd64** | yes | `smapsdirty` 3 DIVERGE → 2. Found the `access`-vs-`open` bug in §4's correction; added `maps` and `statm` |
| 4 | **`mremap`** | yes | `mremapmove` FAIL → PASS, all three phases including the sparse one |
| 5 | **File-backed `mmap`** | **partly, on purpose** | `mmap_file` and `mmapsum` FAIL → PASS. `MAP_PRIVATE` is served from the file's bytes; **writable `MAP_SHARED` is still `ENOSYS`** — that is the half that genuinely needs a page cache |

Item 5 is the one to read carefully before assuming the B trunk is finished. What
landed is not a page cache: every mapping gets its **own copy** of every page, so
two processes mapping one file hold two sets of frames, and a file mapping is
always eager. That is a cost rather than a semantic difference for `MAP_PRIVATE`,
which is the state the AArch64 kernel was in until `src/file_page_cache.rs`
landed. Writable `MAP_SHARED` is refused because for *that* the sharing is the
semantics.

Signals (`mprotectlb`, `eager_mprotect_probe`) are A2, were not on this list, and
are still failing. **amd64 is 8/10, which is the ceiling without them.**

## How to reproduce any row of the table

```bash
# Linux calibration, either architecture
python3 scripts/mem_suite.py --docker --arch x86_64
python3 scripts/mem_suite.py --docker --arch aarch64

# Akuma/aarch64 from main, against a COW clone so devbox.img is untouched
cp -c devbox.img /tmp/devbox-main.img
(cd /path/to/main-worktree && sh scripts/build_devbox_smoltcp.sh &&
 DEVBOX_DISK=/tmp/devbox-main.img SMP=4 sh overlays/devbox/run-smoltcp.sh)
python3 scripts/vm_ready.py 2222
python3 scripts/mem_suite.py --port 2222 --arch aarch64

# Akuma/amd64, over ssh
SMP=1 SSH_PORT=2244 INIT=/bin/sshd sh amd64/run.sh &
python3 scripts/mem_suite.py --port 2244 --arch x86_64 \
    -i target/x86_64-unknown-none/release/amd64-ssh-test-key

# Akuma/amd64 on QEMU *and* the box's Firecracker, in parallel, over the console
python3 scripts/utils/amd64_mem_trials.py
```

`DEVBOX_SSH_PORT` is **not** honoured by `run-smoltcp.sh` — it forwards 2222
regardless, which is worth knowing before you spend time on a refused
connection to the port you asked for.

## Background

- `docs/archive/AKUMA_AMD64_MMAP_REGIONS.md` — B1/B2, the work these probes were
  run against.
- `docs/archive/AKUMA_SELF_HOSTING_AMD64.md` — the unlock tree; A2 is signals.
- `docs/runbooks/verify-trim-fat-change.md` — the gate the three probes joined,
  and the rule they were added under.
- `docs/runbooks/amd64-bare-metal-loop.md` — running the probes on both amd64
  rigs, and the three guest gaps that bite any harness.
- `docs/archive/LONG_ROAD_TO_REDIS.md` — why `smapsdirty` checks the errno rather
  than the feature.
