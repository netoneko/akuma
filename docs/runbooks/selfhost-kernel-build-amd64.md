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

## Background

- [`../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`](../archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md) — every fix and measurement behind this page.
- [`../archive/AKUMA_SELF_HOSTING_AMD64.md`](../archive/AKUMA_SELF_HOSTING_AMD64.md) — the bring-up order that reached the first in-guest build.
- [`stage-rust-toolchain-amd64.md`](stage-rust-toolchain-amd64.md) — putting the toolchain in the image in the first place.
- [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) — the same box, booted as Akuma instead of Ubuntu.
- [`../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md`](../archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md)
  — **the same loop without the hypervisor.** Read it before assuming this page's
  numbers transfer: the metal builds at `-j1` because `-j4` still SIGSEGVs there
  (this guest has been green at `-j4` since §14), the link needs
  `--threads=1` that this guest has never needed, and installing a kernel is a
  `cp` onto Akuma's own ext2 root rather than anything image-shaped.
