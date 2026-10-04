# Self-host on amd64: build the kernel inside Akuma/amd64

The AArch64 procedure is [`selfhost-kernel-build.md`](selfhost-kernel-build.md)
and none of it applies here — different machine, different hypervisor, different
rootfs. This is the amd64 one: the kernel compiles itself in a **Firecracker
guest on the HP box**, and the whole loop is driven from a laptop.

The investigation behind every number here is
[`../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`](../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md)
§10-§18; the bring-up that got there is
[`../archive/AKUMA_SELF_HOSTING_AMD64.md`](../archive/AKUMA_SELF_HOSTING_AMD64.md).

## What the pieces are

| thing | where | note |
|---|---|---|
| the box | `192.168.1.123`, **Ubuntu personality**, port 22 | `scripts/utils/hpbox.py`; port 2222 is Akuma on the metal, a *different* system |
| the deployment checkout | `/root/akuma` on Ubuntu | never authored on — `hpbox.deploy()` resets it to your commit and applies your diff |
| the guest | Firecracker, `10.0.2.15:2222`, 4 vCPU / 6144 MB | config `/root/akuma-fc.json`, boot script `/root/akuma-fc-run.sh`, console log `/root/akuma-fc.log` |
| the guest's rootfs | `/root/akuma-fc-rust.img` (4 GB ext2) | holds `/src/akuma` (the sources it compiles), `vendor/`, and the nightly toolchain at `/usr/local/rust` |
| the guest's ssh key | `/root/akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key` | on the **box**, not the laptop |

## The one command

```bash
scripts/benchmarks/amd64_fc_build_matrix.py --crate akuma-amd64 --clean-all --cells 4x4
```

`--clean-all` is what makes this the self-host gate rather than a one-crate
repro: it cleans `target/` and rebuilds all 137 crates. The harness boots the
guest per cell, times from the host (the guest's own clock is under test), and
reports `PASS` / `ERROR` / `WEDGE` / `NOBOOT` — never a slow pass.

## The loop, step by step

```bash
# 1. put your tree on the box (commits *and* uncommitted work)
python3 -c "import sys; sys.path.insert(0,'scripts/utils'); import hpbox; print(hpbox.deploy())"

# 2. build the kernel there (this is the kernel the guest RUNS)
python3 -c "import sys; sys.path.insert(0,'scripts/utils'); import hpbox; print(hpbox.build())"

# 3. sync your sources into the image (this is what the guest COMPILES) — see below
# 4. run the gate
scripts/benchmarks/amd64_fc_build_matrix.py --crate akuma-amd64 --clean-all --cells 4x4
```

Mid-iteration, with a fix still uncommitted, `hpbox.send_files([...],
touch=False)` sends named files without touching every crate's mtime — a bare
`send_files` costs a full rebuild on the box.

### Step 3 in full: the image is a build input and it drifts

The guest compiles `/src/akuma` **inside the rootfs image**, which is a copy.
A stale copy does not announce itself as staleness — it reports as a compile
error in code that is fine (measured: `error[E0531]: cannot find …
PIDFD_SEND_SIGNAL in module nr`, for a constant that had been in the tree for a
day). **If an in-guest build fails with a missing symbol, suspect the image
before the code.**

```sh
# on the box, guest down
for p in $(pgrep -x firecracker); do kill -9 $p; done
mount -o loop /root/akuma-fc-rust.img /mnt/fcrust
for d in crates amd64 src; do
  rsync -a --delete --exclude vendor --exclude target \
        /root/akuma/$d/ /mnt/fcrust/src/akuma/$d/
done
cp /root/akuma/{Cargo.toml,build.rs,clippy.toml} /mnt/fcrust/src/akuma/
sync && umount /mnt/fcrust
```

**Do not carry `Cargo.lock` across, and do not `cargo vendor` to make it fit.**
The image's lock and its `vendor/` are one pair and the lock is the only file in
that sync whose partner is rig state; the checkout's lock names different
versions of syn/quote/proc-macro2 and will not resolve offline against the
image's vendor directory.

## How long it takes

Clean 137-crate `cargo build -p akuma-amd64 --target x86_64-unknown-none
--release --offline`, in the guest at `vcpu_count=4`, host-timed, 2026-09-18:

| jobs | wall | vs `-j1` |
|---|---|---|
| `-j1` | 226.5 s | — |
| `-j2` | 233.6 s | +3 % — inside the noise |
| `-j4` | ~178 s (174.6 / 178 / 174 / 182.6 / 189.6) | −21 % |
| `-j8` | **~164 s** (163.0 / 165.0) — the fastest cell | **−28 %** |

So **budget ~3 minutes** for a clean kernel build and use `-j8`. Two things to
know about that table:

- **Eight jobs on four cores buy 1.39x, and a second job returns nothing
  measurable** (`-j1`/`-j2` are one run each and their 3 % gap is inside the
  ±4.5 % spread of the five `-j4` runs). The build is dominated by something
  that does not parallelise — the BKL and the filesystem; per-job speed has had
  five rounds of work, so scaling is the open item now, not latency.
- `-j8` needs `amd64::pipe::MAX_PIPES` ≥ ~128 (it is 256). At the old 64 it
  failed in 8 s with `error: could not exec the linker \`cc\`` — which is
  `ENFILE` from `std`'s spawn, not a toolchain fault (§18). If you ever see
  that error, read `[PIPES]` in the console before believing the toolchain.

For scale: the same build was **473 s at `-j1` on 94 crates** on 2026-09-13, so
this is −67 % per crate against the first working self-host, and −76 % at `-j8`
(§19).

## Verify

Three levels, cheapest first.

```sh
# 1. the build itself
#    PASS + rc=0 from the harness, and an ELF where cargo says it put one
$SSH "ls -la /src/akuma/target/x86_64-unknown-none/release/akuma-amd64"

# 2. the kernel it produced actually boots — copy it out (no pty! verify md5),
#    point a Firecracker config at it, and read the boot suite
$SSH "cat /src/akuma/target/x86_64-unknown-none/release/akuma-amd64" > /root/akuma-selfbuilt-fc
$SSH "md5sum /src/akuma/target/…/akuma-amd64"; md5sum /root/akuma-selfbuilt-fc   # must match
#    then boot it with its own config/log and expect:  NNN passed, 0 failed

# 3. the fixed point — build the kernel again INSIDE the self-built kernel
#    and compare md5 with the binary you just booted. Identical is the answer.
```

Level 3 is the one that distinguishes "the build runs" from "the build is
right": a kernel that produces a subtly wrong binary compiles just as happily.
Measured 2026-09-18: generation 2 is byte-identical to generation 1
(`22c696a9…`, 3 389 240 B), and generation 1 boots 768/0 at 4 vCPU.

## Traps

- **`hpbox.py ub '<cmd>'` caps ssh at 300 s.** A build is longer than that, so
  run it detached (`setsid nohup … > /root/x.log 2>&1 &`) and poll the log, or
  call `hpbox.ubuntu(cmd, timeout=…)` from Python.
- **`pkill -f <name>` on the box matches your own ssh command line** and kills
  the shell running it. Use `pgrep -f 'nam[e]'`.
- **The guest's non-interactive shell inherits no `PATH` at all.** Every in-guest
  command needs the `export HOME=… CARGO_HOME=… PATH=/usr/local/rust/bin:…`
  preamble the harness carries as `GUEST_ENV`; without it `cargo` is "not found"
  and no `^error` grep will tell you.
- **`amd64-fc-run.sh` truncates `/root/akuma-fc.log` on every boot**, and the
  harness boots per cell — so grepping the log after a multi-cell run reads only
  the last cell.
- **`2>&1` inside the guest's shell answers `Bad file descriptor`.** Redirect on
  the host side of the ssh instead.
- **Never `grep` the boot log for a readiness marker** — poll with a completed
  ssh command, the rule in `CLAUDE.md` § "Waiting for a VM".

## Watch these in the console

| line | means |
|---|---|
| `[PIPES] live= high= refused= cap=` | pipe pressure; `refused` moving is the `ENFILE` class (§18) |
| `[MM] fault race #N` | two cores demand-faulted one page; one per build is normal (§14) |
| `[FILL-SHORT]` | a file fill came up short and the page kept zeros — never expected |
| `[ISIG-MISS]` | a `^C` arrived and raised no signal (§16) |
| `[BKL] stuck`, `[SWITCH BADFRAME]`, `[TRAMP-BAIL]` | none of these should appear during a build |

## Second rig: ryzen (Firecracker on the Ryzen 7 8845HS) — 2026-09-29

The first self-host build on the Ryzen's Firecracker, and the first time this
gate ran anywhere but the HP box. Everything is built **on ryzen itself**: the
source is a `git clone --depth 1 --branch even-more-cats` of the public repo,
the toolchain comes from rustup on ryzen, and the image is made there by
`amd64/mkdisk.sh` — nothing large crosses the wifi. Scripts and the recipe:
[`scripts/benchmarks/ryzen_fc/`](../../scripts/benchmarks/ryzen_fc/README.md)
(`stage1` → `stage2` → `stage3`, then `jrun`/`batch`).

| | |
|---|---|
| guest | 4 vCPU / 4096 MiB, `init=/bin/herd`, own tap `tapsh` (host `10.0.2.2/24`, guest `10.0.2.15`), disk `/home/netoneko/akuma-selfhost/selfhost.img` (4 GiB, 1.3 GiB used) |
| kernel it runs | built natively on ryzen from `0e331f5`; `uname` = `0e331f5-release-smp-shared`, md5 `88822d83346e…` |
| toolchain in the guest | `nightly-x86_64-unknown-linux-musl`, `rustc 1.101.0-nightly (d080e7dff 2026-09-27)` |
| **the live guest** | a different VM (`akuma-vm.json`, `tap0`, 2 vCPU / 2 GiB, runs kot). **Never touched** — `fcrun.sh` matches only `selfhost-vm.json` |

**Result: 21 fresh-boot `-j 4` runs, 21 PASS, rc=0** (kernel `0e331f5`, 4 vCPU):

| set | runs | guest RAM | clean | cargo `Finished` | `Compiling` lines |
|---|---|---|---|---|---|
| A | 1–11 | 4096 MiB | `kbuild -c` (run 1 on an empty `target/`) | 1m 26s – 1m 28s | 95 (run 1), 79 (2–11) |
| B | 101–110 | 6144 MiB | **whole `/root/ktarget` wiped** (`CLEAN_ALL=1`) | ~1m 31s (host-timed 100–101 s) | 95 in all ten |

(Host-timed figures are the driver's 10 s poll step.) Across all 21: **0**
`[Fault]` lines, **0** `SIGSEGV`/`signal: 11`, **0** `[TLB] stuck`. Set B is the
whole-graph gate; the six extra seconds against set A are the 16 host units set A
kept.

What every run also printed, so the next reader does not re-investigate it:

| console line | per run | reading |
|---|---|---|
| `[BKL] stuck: owner=N waiter=M tag=11 …` | **exactly 27, in all 11 runs** | `tag=11` is x86_64 `munmap` (the raw number — same holder as photo 6227 in [`AMD64_THREE_CRASHES`](../archive/AMD64_THREE_CRASHES.md)), so a shootdown that is slow under contention but *completes*. **The constant 27 is not explained**: the reporter folds by episode (`STUCK_REPORTED`, `akuma-bkl/src/sync.rs`), it is not a hard cap I could find, and a timing-dependent count landing on one value eleven times deserves a look |
| `[MM] fault race #1` | 1 | §14's demand-fault race, one per build, as documented |
| `[TRAMP-MISMATCH]` | 6–9 | the stale-`thread_id` rows of `AKUMA_AMD64_NO_SLOT_RECYCLER.md`; not a defect |

**What this does and does not show.**

* It is a **Firecracker guest on KVM, not bare metal.** It says nothing about the
  HP box's failing `-j4` (`AKUMA_AMD64_BARE_METAL_SELFHOST.md` §3). It does bear on
  that section's explanation: the metal was said to lose more because a 4-core
  box shared with Ubuntu leaves the guest's vCPUs unable to run at the same
  instant. Ryzen has 16 hardware threads and was otherwise idle, so all four
  vCPUs *can* run simultaneously — and 11/11 still pass. That weakens the
  parallelism explanation a little; it does not remove it (different CPU, virtio-blk
  rather than USB, 4 GiB of guest RAM against 16, and the 512 MiB kernel heap).
* **Passes bound the failure rate, they do not remove it.** Zero of 21 puts the
  95 % upper bound near 13 % per build (zero of the 10 whole-graph runs alone:
  near 26 %) — the same trap §3 of that document records.
* **`kbuild -c` cleans only `target/x86_64-unknown-none`.** Run 1 (empty
  `target/`) printed 95 `Compiling` lines; runs 2–11 printed **79**, because 16
  host-side units (proc macros, build scripts) survive under `target/release`.
  95 `Compiling` lines *is* the whole graph — cargo's progress bar counts **137
  units**, which includes build-script runs, and reading that 137 as a crate
  count was this section's first mistake (corrected 2026-09-29). Runs 101+ set
  `CLEAN_ALL=1`, which wipes the whole `/root/ktarget` first
  (`amd64_fc_build_matrix.py --clean-all` semantics) and also prints 95.
* Not done: a fixed-point compare (the guest's kernel was built by a newer musl
  nightly than the gnu one that built the running kernel, so the md5s would
  differ for a reason that is not a bug), `-j8`, and a 2 GiB guest matching the
  live one. Set B ran at 6 GiB only because `llama-server` (7.3 GB) had been
  stopped; that RAM is the one variable that differs from the first batch.

### Re-run on `main` (`e6d8657`), 2026-09-30: 10 of 10, and the metal

Same rig, refreshed rather than rebuilt: the clone moved to `main`, the kernel was
rebuilt natively on ryzen (md5 `9791d5a02e2e`), and `crates/ amd64/ src/` plus the
root manifests were `rsync`ed into `selfhost.img` (the image's `Cargo.lock` and
`.cargo/config.toml` left alone; the only lock change was the new path crate
`akuma-hda`, so the vendor directory still resolved). Guest 4 vCPU / 4 GiB,
`llama-server` left running, `CLEAN_ALL=1`, `batch.sh 201 210 4`.

**10 of 10 PASS, 90 s each, 96 `Compiling` lines, 0 `[Fault]`, 0 SIGSEGV, 0
`[TLB] stuck`**, 27 `[BKL] stuck` and 1 `[MM] fault race` in every run, 5–11
`[TRAMP-MISMATCH]` — all as before. With the 21 above that is 31 of 31 on this
rig (95 % upper bound ≈ 9 %). The matching bare-metal batch, and why it is *not*
a parity result, is in
[`amd64-j4-build-crash-hunt.md`](amd64-j4-build-crash-hunt.md) § 1a.

Traps this rig cost a run each:

- **`kill -9` of the previous Firecracker does not release the tap at once.**
  Relaunching immediately fails with `Open tap device failed … Resource busy`
  and a guest that never boots. `fcrun.sh` now waits for the process to be gone
  and sleeps 3 s.
- **`pkill -f 'jrun.sh 1'` in an ssh command that itself contains `./jrun.sh 1`
  kills the ssh shell** (exit 255, nothing done). Use the `jrun.s[h]` trick or a
  separate call.
- **ryzen's wifi drops** (its kernel log shows `wlp2s0: Connection to AP lost`
  repeatedly from 00:22; ~4 min unreachable, uptime unbroken, no OOM). Detached
  runs survive it; anything tied to the ssh session does not.
- **Memory is shared with `llama-server`** (7.3 GB resident on this host,
  `llama-ryzen-linux-amd64.service`, the kot LLM backend). With it running, 4 GiB
  is the ceiling for this rig; with it stopped (`systemctl stop`, done for set B)
  6 GiB as on the HP box fits.
- **The disk is at 96 %** (8 GB free): the image is 1.3 GiB used, the toolchain
  946 MB in `~netoneko/.rustup`.

## Background

- [`../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`](../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md) — every fix and measurement behind this page.
- [`../archive/AKUMA_SELF_HOSTING_AMD64.md`](../archive/AKUMA_SELF_HOSTING_AMD64.md) — the bring-up order that reached the first in-guest build.
- [`stage-rust-toolchain-amd64.md`](stage-rust-toolchain-amd64.md) — putting the toolchain in the image in the first place.
- [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) — the same box, booted as Akuma instead of Ubuntu.
- [`../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md`](../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md)
  — **the same loop without the hypervisor.** Read it before assuming this page's
  numbers transfer: the metal built at `-j1` because `-j4` SIGSEGVed there
  (**2026-09-30: no longer the case** — `-j4` completed 7 of 10 runs,
  slower than `-j1`, three lost to an `execve` `EIO`; see
  [`amd64-j4-build-crash-hunt.md`](amd64-j4-build-crash-hunt.md) § 1a;
  this guest has been green at `-j4` since §14), the link needs
  `--threads=1` that this guest has never needed, and installing a kernel is a
  `cp` onto Akuma's own ext2 root rather than anything image-shaped.
