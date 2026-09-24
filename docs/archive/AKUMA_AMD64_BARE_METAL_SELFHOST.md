# amd64 bare metal: the box builds its own kernel, installs it, and boots it

**Date:** 2026-09-18. **Machine:** the HP 500-502nj ("the trashcan"), 4 cores,
16 321 MiB usable, root on the USB disk (`sdb1` to Ubuntu, `/dev/sda1` to Akuma).

The Firecracker guest reached a byte-identical fixed point earlier the same day
(`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §17). This is the same bar attempted on
the metal, where there is a real bootloader and no hypervisor. What changed to
make it possible, what it measured, and what is still in the way.

---

## 1. Installing a kernel: what each architecture gets to delete

The blocking question was not the build. It was that "install a kernel" on amd64
meant arranging GRUB from Ubuntu, so every generation needed the other operating
system — which is precisely what "self-hosting" is supposed to exclude.

**AArch64 deleted the bootloader.** QEMU's `-kernel` *is* the bootloader, so
`KERNEL_DROPOFF` exposes that exact file as a second virtio-blk drive and the
guest `dd`s onto it (`RAW_BLOCK_DEVICE_FD.md`). Everything that follows is
downstream of there being no filesystem in the path: the image must be
*flattened* (`mkbin.sh`), the write is a raw block write, and virtio-blk reports a
**fixed capacity** taken from the file's size when QEMU opened it — so a kernel
that grew hits `ENOSPC` partway through a `dd` onto the live file and corrupts it.

**amd64 can delete the install step instead.** GRUB is a real bootloader, it
already does `insmod ext2`, and `sdb1` — the partition GRUB can read — is the
same filesystem Akuma mounts as its own root. So point the menu entry at a kernel
*on that partition* and installing becomes an ordinary file write:

```sh
cp target/x86_64-unknown-none/release/akuma-amd64 /boot/akuma-amd64
```

No ESP write, no grub tooling from the guest, no raw block device, no flatten
(multiboot2 parses the ELF), and no capacity ceiling — 57 GB free against
AArch64's fixed-size drop-off drive. **This kernel has no vfat at all**, so the
ESP and Ubuntu's ext4 are both unreachable from Akuma; the design's whole point is
that it never needs them.

The GRUB config becomes **immutable rig state**: the *path* is fixed once, and
only the file's *contents* change thereafter. Editing GRUB from inside Akuma is a
problem this deletes rather than defers.

### The two traps in doing it

- **`search --file` must match exactly one filesystem.** The new path is
  `/boot/akuma-amd64` on sdb1, deliberately *not* the old
  `/boot/akuma/akuma-amd64` on sda2. Two filesystems answering the same `search`
  is the 2026-09-05 ambiguity that cost an hour.
- **`update-grub` executes every *executable* file in `/etc/grub.d/`.** Backing
  up `45_akuma` with `cp` preserves mode 755, so the backup is sourced as a live
  GRUB script — which produced **two menu entries both titled `Akuma/amd64`**,
  re-creating the exact ambiguity the one-entry rule exists to prevent. Keep
  backups **outside** `/etc/grub.d/`.

### Why there are now two entries, after years of "exactly one"

The one-entry rule came from `grub-reboot` one-shots resolving to the wrong
entry. The default is now set **by title** (`GRUB_DEFAULT="Akuma/amd64"`), so that
mechanism is gone, and a second entry — `Akuma/amd64 (known good)`, booting
`/boot/akuma-amd64.good` — is worth having precisely *because* a self-built kernel
now overwrites the default one. `GRUB_TIMEOUT=10` with `GRUB_TIMEOUT_STYLE=menu`
keeps Ubuntu one keypress away.

Setting the default (rather than arming a one-shot per cycle) is what collapses
the loop to Akuma-only: build → `cp` → `reboot -f` → up on what you just built.
That is the same shape `KERNEL_DROPOFF` gives AArch64, reached without a raw block
device.

---

## 2. Storage: the 6.2x is not the device

`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §20 D records the metal as 6.2x the
Firecracker guest per crate and attributes it to storage — "the metal reads a USB
disk with nothing in front of it". Measured directly, from Ubuntu, `O_DIRECT`:

| | sdb1 (USB, Akuma's root) | sda2 (internal, holds the FC image) | ratio |
|---|---|---|---|
| sequential 1 MiB | 128–138 MB/s | 194 MB/s | **1.4–1.5x** |
| random 4 KiB | 33.9 MB/s | 77.1 MB/s | **2.3x** |

**The device accounts for 1.4–2.3x, not 6.2x.** The rest is *cache residency*:
the guest's root is a file on sda2, so Ubuntu's page cache serves the toolchain
and vendored sources at RAM speed, while the metal has only Akuma's own ext2 block
cache in front of the disk. That is a tunable, not a device limit — see §5.

Two side findings: the drive is on the **5000M** bus (correct socket, per the
runbook's rule), and both disks report `rotational=1` while sustaining 77 MB/s on
4 KiB random, which no spinning disk does. `hpbox.ramdisk`'s docstring justifies
itself on that flag.

---

## 3. SMP on the metal: the compile race, and a link failure that was not what it looked like

§11 saw two failures at SMP=4 and suspected they were one bug. This section was
written arguing they separate; **both of its separating arguments were then
disproved by measurement on the same day**, and the inline corrections below are
the substance rather than errata:

- the compile crash is **not** `-j4`-specific (`-j1` crashes too), and
- the link crash is **not** about LLD's threading (a bigger kernel heap fixed it
  with threading left on).

Read §11's original instinct as the better one. What survives here is the
evidence, which is worth having either way.

### The compile phase fails with concurrent *processes*

`kbuild -c -j 4`, SMP=4, clean 137-crate graph:

| run | outcome | wall | crates reached |
|---|---|---|---|
| 1 | `rc=139` (SIGSEGV) | 377 s | 30 |
| 2 | `rc=139` (SIGSEGV) | 291 s | 22 |

Same outcome, different position — a **race**, and cheap: ~5 minutes to
reproduce where §11's equivalent cost a 23-minute build. §14's fix took the
Firecracker guest from "dies in 2.0 s at 2 vCPU × 2 jobs" to a green `-j4` fixed
point; **the metal is still not green**, on identical source.

**Do not read that as "the guest is immune."** Count what the guest's evidence
actually is: §19 and §17 record roughly **ten** clean full builds (`-j1` ×1,
`-j2` ×1, `-j4` ×5, `-j8` ×2, plus §17's generation 2) and **zero** failures.
Ten clean runs bound the guest's per-build failure rate at something under ~10%
— they cannot establish zero. The metal's rate is **3 failures in 4 builds**.

So the defensible statement is *one bug whose window is far easier to hit on the
metal*, not a metal-only defect. The mechanism that would explain a gap this
size: Firecracker's 4 vCPUs are host **threads** on a 4-core box that is also
running Ubuntu, so they are often not executing at the same instant, while the
metal's 4 cores genuinely are — every time. A race with a tight window is hit far
more often under real parallelism, which is also why §11 met it on the metal
first and why §14's fix looked complete from inside the guest.

The experiment that would settle it is repeated `amd64_fc_build_matrix.py` runs
looking for a rare guest failure. Note it needs the **Ubuntu** personality, which
is behind physical access to the GRUB menu now (see §6.1).

**There is no `[Fault]` line, and that is expected, not evidence of health.**
`amd64/src/idt.rs:990` hands a ring-3 fault to `deliver_fault_signal` *before*
reaching `user_fault`, and Rust's std installs a `SIGSEGV` handler for
stack-overflow detection — so a segfaulting `cargo`/`rustc` is killed silently by
design. Reading "no `[Fault]` lines" as "the kernel killed nothing" sends you
hunting the wrong thing.

### The link phase fails with LLD's internal thread pool

`kbuild -c -j 1`, SMP=4: **all 95 crates compiled, 23 m 18 s, clean** — then
`error: linking with 'rust-lld' failed: signal: 11 (SIGSEGV)`.

So that run compiled fine across four cores with one job, and died in a *single
multi-threaded process*.

> **Corrected the same day, by the gen-2 attempt.** This section originally read
> "the `-j4` crash and the LLD crash are at least independently triggerable",
> inferred from that one clean `-j1` compile. **`-j1` crashes too** — the second
> `-j1` build died at 35 crates with `rc=139`, the same signature as `-j4`. The
> clean 95-crate run was luck, not a property of `-j1`.
>
> | cell | outcome | crates reached |
> |---|---|---|
> | `-j4` | `rc=139` | 30, 22 |
> | `-j1` | `rc=139` | 35 |
> | `-j1` | survived | 95 (gen-1) |
>
> So the variable is **rate, not kind**, and §11's original guess — one bug about
> multi-threaded user processes at SMP>1 — is better supported than the split
> was. `rustc` is itself ~18 threads, so `-j1` already exercises it; `-j4` only
> supplies more. Treat the LLD crash as very likely the *same* defect reached by
> a cheaper, deterministic route, not a second one.
>
> The generalisable trap: a single clean run of a *probabilistic* failure is not
> evidence that a variable is protective. It took one counter-example to delete
> the conclusion.

#### `--threads=1` is NOT stale — measured, not assumed

The rig carries `-C link-arg=--threads=1` in sdb1's `.cargo/config.toml`, added
in §11. The prior was that §14 had made it unnecessary (the guest has never
carried it and links fine at SMP=4). **It has not.**

Because the link *failed*, the 174 `.rcgu.o` files still existed — rustc deletes
them only after a *successful* link, which is exactly why §11 could not run this
experiment. LLD prints its own argv in the crash dump, so the same 30 KB command
line was replayed with one flag changed:

```
rust-lld -flavor gnu <174 objects> -T/root/akuma/amd64/linker.ld …              → SIGSEGV
rust-lld -flavor gnu <174 objects> -T/root/akuma/amd64/linker.ld … --threads=1  → rc=0
```

Same objects, same command, same machine, one variable, ~20 seconds. This is a
controlled A/B, not a retry that happened to work.

> **Corrected the same day, by §5.** The A/B above is sound and reproducible;
> the *conclusion drawn from it* — "LLD's thread pool is what this kernel cannot
> survive at SMP>1" — was too strong. With the kernel heap raised from 512 MiB
> to 1 GiB (§5), the **same build linked cleanly with default multi-threaded
> LLD**, rustflags unchanged and `--threads=1` still absent.
>
> So the defensible statement is that **LLD's crash is memory-pressure
> sensitive**, not that its threading is intrinsically fatal here. `rust-lld` is
> 158 MB and this target's `execve` holds a whole binary in the kernel heap
> (§6.4), so at 512 MiB — with the block cache taking 128 MB of it — the linker
> was running against the edge; `--threads=1` presumably narrowed a window
> rather than removing a defect. `-C link-arg=--threads=1` is therefore **not
> needed on a 1 GiB-heap kernel**, and should not be re-added without
> re-measuring.
>
> The generalisable trap, and it is the second time in this document: a clean
> controlled A/B tells you the flag *changed the outcome*. It does not tell you
> *why*, and the mechanism you assume is the part that later measurement
> overturns.

**Do not "fix" this by putting `--threads=1` into rustflags before comparing
generations.** rustflags feed cargo's `-C metadata` hash, which feeds symbol
hashes, so that change alters the output bytes — and a gen-1/gen-2 comparison
across it fails for a reason that is not a bug.

---

## 4. The result — the fixed point, on the metal

**Generation 3 is byte-identical to generation 2**, built inside it, on the
machine, with no hypervisor and no Ubuntu in the cycle:

```
cmp  /root/ktarget/.../out/akuma_amd64  /boot/akuma-amd64   →  IDENTICAL
     3 397 936 bytes,  md5 a40756211f4bd9c63f34d7be1ac679c7,  both
built by: Akuma akuma 0.0.8 unknown-release-smp-shared x86_64
```

| generation | md5 | built by | wall |
|---|---|---|---|
| gen-1 | `6dd8ba18…` | the old 512 MiB-heap kernel | 1398 s, link failed (hand-relinked) |
| bootstrap | `bb0add8a…` | laptop cross-build, carries §5's heap fix | — |
| **gen-2** | `a4075621…` | the bootstrap, on the metal | **640 s** |
| **gen-3** | **`a4075621…`** | **gen-2, on the metal** | **632 s** |

**Two checks that stop this being a false positive**, because comparing a file
to itself is the easy way to fake this result:

- `cmp`, not just `md5sum` — byte-for-byte.
- `kbuild -c` **wipes `/root/ktarget`** before building, so gen-2's artifact
  cannot have survived at the path gen-3 was read from. The 632 s from-scratch
  time for 95 crates corroborates it; an incremental no-op build takes seconds.

The harness reported `LINK rc=1` on that run and it is **not** a failure: the
build succeeded outright (`DONE rc=0 crates=95`), so there was no `rust-lld`
crash dump to replay and the script's unconditional relink step found an empty
argument file. Worth knowing before reading the logs.

### The earlier result, kept because it is the one that proved the loop

Gen-1, built on the metal at SMP=4 and installed from inside Akuma:

| | bytes | md5 | `uname` version |
|---|---|---|---|
| Ubuntu-built (previous) | 3 365 168 | `9f1d0612…` | `33577243-release-smp-shared` |
| **gen-1, metal-built** | **3 400 944** | **`6dd8ba18…`** | **`unknown-release-smp-shared`** |

Booted: **771 passed, 0 failed**, `fs: ext2 mounted on /dev/sda1`, four cores.
The previous kernel scores **767** — the self-built one runs four *more* tests,
because sdb1's source is newer than the binary it replaced. That is the new tree
showing up in behaviour rather than only in a version string.

### `unknown` is the right answer, not a defect

`crates/akuma-syscalls-glue/build.rs` derives `AKUMA_GIT_SHA` from
`git rev-parse --short HEAD` and falls back to `"unknown"`. sdb1's tree is staged
with `--exclude .git` (§11's recipe), so a metal-built kernel reports `unknown`.
`git` itself **is** on the metal (2.54.0); the sha is absent because the repo is,
not because the tool is.

A *live* sha would be worse than none: it changes between generations whenever
HEAD moves, so byte-identity breaks **by design** and the fixed-point test
becomes a false negative. The identity that matters is the md5.

Provenance is restored without that hazard by `AKUMA_SHA_OVERRIDE`, added
2026-09-18 to `crates/akuma-syscalls-glue/build.rs` — it wins over `git`, falls
back to it, and falls back to `unknown`:

```sh
AKUMA_SHA_OVERRIDE=9e97d726 kbuild -c -j 1     # uname reports the source commit
```

**It must be constant for a given source tree.** A generation counter, a
timestamp or anything else that moves between two builds of the same sources
reintroduces exactly the false negative above. Stamp the commit the sources came
from, and nothing else.

---

## 5. The heap was the bottleneck, and it was worth 3.3x

`HEAP_SIZE` was a hard-coded **512 MiB on a 16 321 MiB machine**. The ext2 block
cache is `min(RAM/8, FSCACHE_CEILING_MB, HEAP_SIZE/4)`, so the heap quarter was
binding at **128 MB** where the policy wanted 384 MB — on precisely the workload
`amd64/src/fs.rs`'s own comment names: *"`rustc` reading rlibs has a working set
in the hundreds of megabytes."*

Sized from RAM instead (`heap_size_for`: 1 GiB at ≥ 8 GiB usable, 512 MiB
otherwise, as a **request** that falls back if no region below `PHYSMAP_LIMIT`
can hold it), the cache reaches 256 MB. Same source, same cell (`-j1`, SMP=4),
same machine — only the heap differs:

| elapsed | 512 MiB heap / 128 MB cache | 1 GiB heap / 256 MB cache |
|---|---|---|
| 188 s | 15 crates | **35** |
| 339 s | 25 crates | **82** |
| 490 s | 33 crates | **95** |
| total | **1398 s, and the link then failed** | **640 s, clean, linked** |

**2.2x on the completed build, ~3.3x at the crossover** — and the build that had
never once linked, linked. Two conclusions follow, and both correct earlier
sections:

- §2 said the metal/guest gap was cache residency rather than the device. This
  is the confirmation: doubling the cache moved the build more than the entire
  measured device gap (1.4–2.3x) could have accounted for.
- §3's `--threads=1` finding needed the correction now inline there.

The `HEAP_SIZE/4` rule itself is **right and should stay**: `execve` loads whole
binaries into the heap, `rust-lld` is 158 MB, and a cache free to take 384 MB of
a 512 MB heap is how you get `[ALLOC FAIL] heap_total=512MB heap_used=510MB`. The
bug was the fixed 512 MiB, not the fraction.

**This is the lever to reach for first on any new machine**: the heap is the only
term in that `min` that does not scale with RAM, so on a big box it is the one
that binds, silently, on the only workload that notices.

## 6. Open, in the order worth attacking

1. **The compile-phase crash at SMP>1 on the metal.** Not `-j4`-specific (see
   §3's correction): `-j1` dies too, just less often, so this is not merely a
   speed problem — **a 23-minute build is a coin flip**, which is what stopped
   the gen-2 fixed point rather than anything about the loop's mechanics. It is
   also the only thing between this loop and a ~3x speedup: the guest gets 137
   crates in ~178 s at `-j4`, the metal takes ~23 min at `-j1`, and §2 shows the
   device explains under 2.3x of that. Repro at `-j4` is ~5 minutes.

   **`nosmp` is the deterministic build (§11) and is *not reachable remotely***:
   it needs a GRUB cmdline edit, and with the kernel and menu as they now stand
   Akuma cannot write ext4 or vfat. Anyone planning to fall back to it needs
   physical access to the machine, or should change the cmdline *before* leaving
   Ubuntu.
2. **LLD's thread pool at SMP>1.** Deterministic, replayable from a build log,
   one multi-threaded process — the cheapest harness in the tree for whatever
   §3's first item is.
3. **`HEAP_SIZE` was a hard-coded 512 MiB on a 16 GiB machine.** The ext2 block
   cache is `min(RAM/8, FSCACHE_CEILING_MB, HEAP_SIZE/4)`, and the heap quarter
   was binding at **128 MB** where the policy wanted 384 MB — on the workload
   `amd64/src/fs.rs`'s own comment names ("`rustc` reading rlibs has a working set
   in the hundreds of megabytes"). Now sized from RAM: 1 GiB at ≥ 8 GiB usable,
   512 MiB otherwise, as a *request* that falls back if no reachable region below
   `PHYSMAP_LIMIT` (4 GiB) can hold it. This is the lever for §2's cache-residency
   gap.
4. **`proposals/AMD64_FD_WHOLE_FILE_HEAP.md`** is the reason the heap is under
   pressure at all: `fd.rs` keeps every open file's entire contents in a heap
   `Vec`, *on top of* the same `akuma-ext2` block cache AArch64 uses alone. So the
   file is held twice — once as bounded, evicting blocks, once as an unbounded
   per-fd copy that is the file's authoritative in-memory image (hence the
   write-back at `close`). Heap use scales with the sum of all open files rather
   than with a cap. When that lands, §5's rule should be deleted, not retuned.

   > **Wrong as written — corrected the same day.** `fd.rs` stopped caching file
   > contents in **C2 slice 5**; its module header now reads "Contents are **not**
   > cached any more", descriptors carry an empty buffer, and reads/writes stream
   > in `MAX_IO` chunks at a heap cost of one 64 KiB chunk. What survives is the
   > **`execve`** half: `usermode.rs`'s `read_image` reads the whole image, so
   > exec'ing `rust-lld` costs 158 MB in one allocation, and *that* is what sets
   > the floor §5's rule protects.
   >
   > The paragraph above was written from the proposal, which still says
   > `**Status:** open` and is stale. **Fix that status before anyone plans work
   > against it** — a stale "open" wastes a session exactly as a stale "FIXED"
   > does, and it produced this error within an hour of the doc being written.
5. **Interactive ssh input arrives one event late** — see the runbook's
   "Known-broken" table. Three candidate mechanisms and the discriminators are
   recorded there; test the contention one first, because it is the only one that
   makes it a non-issue.
6. **herd (PID 1) eats all physical memory after a chatty service dies —
   root-caused, reproduced and FIXED 2026-09-24 (kernel and herd, each
   sufficient alone; see "Fixed" at the end of this item).** On the metal, ~20 min
   after a boot on `67a9a4b5` with the herd from `ca859980`, `free -h` read:

   ```
                 total        used        free      shared  buff/cache   available
   Mem:          15.9G       15.4G        1.4M           0      516.8M      161.1M
   ```

   By then every exec segfaulted (the page fault behind it had no frame to
   use), sshd fell back to builtin-only paws, and kot (the akuma-miot mesh
   node, a herd service) was dead and never restarted. `/proc/<pid>/status`
   says `VmRSS: 0` for every process, so nothing in userspace could name the
   culprit; `[PSTATS]` could: PID 1's `pgfault` rose **2,182,670 pages** in
   120 s while `pmm` free fell **2,186,942** — the same number — at ~900 k
   syscalls/s.

   **The mechanism, three pieces, each harmless alone:**

   - `sys_wait4` (`akuma-syscalls-glue/src/proc.rs`) returns a pid as reaped,
     but `reap_child_channel` (`akuma-exec/src/process/children.rs`) **keeps
     the registry entry while the child's stdout still holds unread data**. So
     the next `wait4(-1)` finds the same exited child and returns it again,
     indefinitely. A kernel bug: `wait4` must never hand back one pid twice.
   - herd's orphan sweep (`5a2b6cc7`, `while let Some(status) = wait_any()`)
     loops until `wait4` has nothing left — which, given the above, is never.
     The pid is a still-`Running` service, so every pass does
     `exited.push((name.clone(), …))`: an unbounded `Vec`, ~80 B per pass, one
     new page per ~50 passes — the measured ratio. It never reaches its
     `sleep_ms`, so nothing else herd does (restarts, reloads, log drains)
     happens again.
   - The unread data is guaranteed by herd itself: it drains at most 1 KiB per
     service per pass, and a pass only happens when **every** service has
     written something — `poll_all_stdout`'s `read_fd` on a `ChildStdout` fd
     **blocks** (no `O_NONBLOCK`; the glue arm parks until data or exit). With
     a quiet sshd beside it, herd advanced about once per ssh session, and
     kot's backlog was large when it died.

   **Reproduced** under Firecracker on the box's Ubuntu (4 vCPU, 2 GiB, the
   metal's own kernel `37bd1301…` and `/bin/herd` `1a07c71d…`, images in
   `/root/leakrepro/`): one service that prints and exits
   (`busybox seq 1 5000`, ~24 KB). 25 s after it started, herd had made
   16.8 M `wait4` calls, taken 390,667 page faults, and left `pmm=0` free;
   `Service burst exited` never printed. Controls in the same directory:
   with only a silent service, herd simply parks in `read` forever (`[SLOT]`
   `sc=0 park=akuma-syscalls-glue/src/fs.rs:665`), on this kernel, on the
   Sep-21 `.good` kernel and with the Sep-19 herd alike — that half is old.

   **Reading PSTATS on amd64:** `syscall_name` labels numbers from the
   *aarch64* table and a syscall is counted under both its x86_64 and its
   canonical number, so one call shows up as a pair with equal counts: herd's
   `getdents64` + `wait4` pair is x86_64 `wait4` (61) alone, and
   `unlinkat` + `nanosleep` is `nanosleep` (35).

   **Fixed**, both halves, each verified alone and together under the same
   Firecracker rig (`/root/leakrepro/matrix.sh` on the box's Ubuntu, runs
   D–H):

   - (a) **kernel** — `reap_child_channel` keeps a still-buffered entry for
     its `ChildStdout` reader but rewrites its parent to `REAPED_PARENT`
     (`Pid::MAX`), so no `wait*` can find it again. Host test
     `reaping_a_buffered_child_keeps_its_output_but_not_the_child`. Covers
     both reapers: glue's `wait4` and amd64's private `sys_waitpid` (#303,
     through `reap_exec_process`).
   - (b) **herd** — service stdout is `O_NONBLOCK` (`nonblocking_stdout`) and
     `poll_all_stdout` drains to `EAGAIN` (≤ 64 × 4 KiB per service per pass);
     the orphan sweep ends on a repeated pid, records a service once, and
     stops after 256 reaps a tick.

   | run | kernel | herd | pmm free, 29 s → 89 s | herd `pgfault` | reloads |
   |---|---|---|---|---|---|
   | D | old | old | 391,448 → **0** | 390,667 | 1 |
   | E | fixed | fixed | 391,447 → 390,909 | 17 | 5 |
   | F | fixed | old | 391,447 → 390,399 | 18 | 5 |
   | G | old | fixed | 391,448 → 390,910 | 17 | 5 |
   | H (silent svc + orphans) | fixed | fixed | 390,942 → 374,509 | 0 | 5 (was 0) |

   Installed on the metal's `sdb1` the same day (`/boot/akuma-amd64` md5
   `e3a301b8…`, `/bin/herd` `6a85a1d0…`; the pre-fix files are `.prev`), **not
   yet booted there**. `uname` still says `8b4decf2`: the fix was uncommitted
   when built, so identify it by md5.

   **Still open, found by the same rig:**

   - **Orphans of a dead *thread* are never reparented** (run H: ~64 MB/min
     gone with herd's own `pgfault` at 0, and herd never once prints
     `reaped`). On Akuma every thread has its own pid, and a child spawned from
     a worker thread records *that* pid as its parent — the metal's leftover
     zombie was `sh` with `PPid: 23`, a kot tokio worker. amd64's exit path
     (`usermode.rs`, `reparent_children_to(dying, 1)`) moves only `dying`'s
     own children; glue's `exit_group` path walks every member of the group.
     A slow leak, not a fast one, but unbounded.
   - **herd revives `restart = false` services anyway.** Observed in the same
     rig: a service exiting 7 was started again 72 times in 110 s on the old
     herd. With herd no longer parked in `read` it is much worse — a
     `restart = false` one-shot (`busybox seq`, exit 0) ran **~700 times in
     85 s** in runs E–G. Not a leak (memory flat), but a real misbehaviour.
     Likely cause, not yet confirmed: the exit lands in `Stopped`, and
     `start_stopped_services` starts every `Stopped` service.

---

## Background

- [`AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`](AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md)
  — §11 the first bare-metal build, §14 the demand-fault race, §17 the guest's
  fixed point, §19/§20 the scaling tables this corrects.
- [`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md)
  — the rig, its rules, and the install procedure.
- [`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md)
  § "Swap the running kernel in place" — the AArch64 mechanism this is measured
  against.
- [`../../proposals/AMD64_FD_WHOLE_FILE_HEAP.md`](../../proposals/AMD64_FD_WHOLE_FILE_HEAP.md)
  — the whole-file heap cache.
