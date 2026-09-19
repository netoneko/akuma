# Akuma From Scratch — the trashbox dev cycle with no Ubuntu in the middle

**Status:** IN PROGRESS as of 2026-09-19 — the box now holds the repository, the
toolchain and a cargo cache of its own (§9), and the goal has been restated as
the full loop: build, **patch**, reboot, continue (§8). **Machine:** the HP
500-502nj bare metal.
**Prerequisite met today:** the box builds, installs and boots its own kernel,
and generation 3 is byte-identical to generation 2
(`docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md`).

Inspired by Linux From Scratch, and named for the same reason: the value is not
the artifact, it is that **every step is one you performed on the machine
itself**. Today the metal self-hosts the *kernel* but still inherits everything
else from elsewhere. This closes that.

---

## 1. What is still borrowed

The self-host loop proved the kernel. Three things around it are still made on
the Ubuntu side and copied in, and each is a place the box is not yet
independent:

| thing | where it comes from today | why it matters |
|---|---|---|
| ~~**the source tree**~~ | ~~`rsync` from Ubuntu's `/root/akuma`, staged with `--exclude .git`~~ | **CLOSED 2026-09-19 — §9.** The partition carries a full `git clone` at `/src/github.com/netoneko/akuma` |
| ~~**`vendor/`**~~ | ~~`cargo vendor` run on Ubuntu~~ | **CLOSED 2026-09-19 — §9.** Replaced by an ordinary cargo cache at `/root/.cargo`, not a vendor directory |
| **all of userland** | `amd64/mkdisk.sh` on Ubuntu, `rsync`'d onto sdb1 | `/bin/sh`, `/bin/sshd`, `herd`, `box` — the box runs binaries it cannot rebuild. Every one of them **builds** on the box now (§9), but none has been installed from a box-built copy yet |

That last row is the sharp one. **A machine that can rebuild its kernel but not
its shell is not self-hosting; it is a very good cross-compilation target.**

## 2. The goal, concretely

On the metal, inside Akuma, with the network it already has:

```sh
git clone https://github.com/netoneko/akuma /src/github.com/netoneko/akuma
cd /src/github.com/netoneko/akuma && git submodule update --init <the ones needed>
cargo build -p akuma-amd64 --target x86_64-unknown-none --release   # kernel
userspace/build.sh --<member>-only                                   # userland
scripts/install_kernel_amd64.sh && reboot -f
```

**Deliberately not "all of userland".** The target list is `nca`, `box`, `herd`,
`sshd`, and whatever `busybox`/musl they need — the set that makes the box a
place to work and run an agent, not a distribution. Everything else stays a
cross-build until it earns its place.

`/src/github.com/netoneko/akuma` is the path the AArch64 self-host guest already
uses (`docs/runbooks/selfhost-kernel-build.md`), so harnesses that know one know
the other.

## 3. The gaps, in the order they will bite

Each of these is a *test*, not an assumption. None has been run.

1. **`git clone` over HTTPS from inside Akuma.** `git` 2.54.0 is installed on the
   metal and works locally. The clone must fetch this repo and its submodules
   without an Ubuntu round trip — everything below depends on it.

   **Status: DONE 2026-09-19.** `git clone --depth=1` of this repo completes in
   the Firecracker guest in ~21 s — 1617 files checked out, `git status` clean,
   HEAD at the `v0.0.8` tag. A small-repo clone takes 6.4 s and `ls-remote`
   1.2 s.

   It took four passes to get here because one symptom had four causes. The last
   was `execve` not closing `FD_CLOEXEC` descriptors on amd64, which hung `git`
   on the EOF its `start_command` notify pipe was supposed to produce. History
   and evidence: [`AMD64_TRASHCAN_ISSUES.md`](AMD64_TRASHCAN_ISSUES.md) §1.
2. **Submodule size — measured, and it is a non-issue.** The received wisdom is
   "the vendored submodules are ~37 GB, never copy the tree". The *rule* is right
   for the wrong reason, and the number is wrong. Measured 2026-09-18:

   | | |
   |---|---|
   | whole worktree | 23 GB |
   | `target/` (build output) | 4.5 GB |
   | all submodule **working trees** | 3.5 GB — of which `meow` 1.7 G and `nca` 1.2 G are **100% `target/`** |
   | all submodule **objects** (`.git/modules`, what a clone fetches) | **450 MB** |

   Per submodule, clone cost: `llama.cpp` **314 MB**, `rumpkernel` 95 MB, `xbps`
   11 MB, **`nca` 9.0 MB**, `musl` 7.2 MB, `tcc` 5.9 MB, `meow` 7.1 MB, `ubase`
   0.5 MB.

   **`box` and `herd` are not submodules** — both are in-tree under `userspace/`.
   So §2's target list needs `nca` plus `musl`: **~16 MB**, against 57 GB free.
   Even `--init` with no arguments is 450 MB and would fit comfortably; the only
   reason to name them is to skip `llama.cpp` and `rumpkernel`, which are 91% of
   the total and unrelated to this goal.

   **And `--depth=1` removes even that.** The figures above are *full* object
   stores, i.e. mostly history; a shallow clone fetches the tip tree only, which
   is where `llama.cpp`'s 314 MB largely goes:

   ```sh
   git clone --depth=1 --recurse-submodules --shallow-submodules \
       https://github.com/netoneko/akuma /src/github.com/netoneko/akuma
   ```

   So "which submodules can we afford" is not a question this proposal needs to
   answer. Take them all, shallow.

   One consequence to accept knowingly: a shallow clone cannot `git log` far
   back, `git bisect`, or `git describe`. `git rev-parse --short HEAD` **does**
   work, so `AKUMA_GIT_SHA` stops reporting `unknown` either way (§7.5). If the
   box is later meant to do real development rather than builds, `git fetch
   --deepen=<n>` or `--unshallow` buys the history back without re-cloning.

   The reason not to *rsync* the tree stands and is unaffected: 23 GB of working
   tree, mostly `target/`, is a terrible thing to copy over a network. That is an
   argument against `rsync`, not against `git`.
3. **`vendor/` on the box.** `cargo vendor` needs network and writes ~19 MB for
   the kernel graph. Once git works this is easy; it is listed because the
   current rig treats `vendor/` and the `[source.crates-io]` block as **rig
   state, not source**, and that distinction has to survive the move.
4. **A musl toolchain for userland.** The metal has
   `x86_64-alpine-linux-musl` gcc (`/usr/libexec/gcc/…/15.2.0`) and it compiles
   and links static binaries — proved today by building `fbstress` on the box.
   Whether `userspace/build.sh` drives it unmodified for an amd64 target is
   untested; it was written for the AArch64 cross-build.
5. **Stability under a long build.** The gating item, see §5.

## 4. Why this is the right next step and not a detour

- **It removes the last reason to boot Ubuntu.** With the GRUB default set to
  Akuma, Ubuntu is already unreachable remotely; the only things it is still
  needed for are the three rows in §1. Close those and the box is a machine you
  develop *on*, not a target you deploy *to*.
- **It is the substrate for NCA + GLM.** An agent that can edit and rebuild the
  system it runs on needs the repository and the toolchain *on the machine*. A
  model that can only recompile the kernel cannot fix `herd`.
- **The precedent already exists**, and it is worth stating because it makes this
  an increment rather than a leap — see §6.

## 5. The gating risk, stated plainly

**The SMP corruption is not fixed**, and it is the reason to sequence this
carefully. Measured 2026-09-18 on the metal, SMP=4:

| cell | outcome |
|---|---|
| `-j4` | `rc=139` at 30 crates / at 22 crates |
| `-j1` | `rc=139` at 35 crates — **`-j1` is not protective** |
| `-j1` | clean, ×3 (gen-1 compile, gen-2, gen-3) |

Roughly half of ~10-minute builds die. An autonomous agent loop runs for hours,
and no retry logic above the corruption fixes the corruption beneath it. **Treat
that race as the gate on "truly autonomous", not as something to work around.**

**And the gate is wider than that one race.** Before the autonomous experiment —
an agent driving the loop unattended for hours — **two whole areas of this
target need to be severely stabilized, not merely working once**:

- **Networking.** An agent's session is continuous HTTPS: model calls, `git
  fetch`/`push`, and long-lived TLS connections that must survive minutes of
  silence and then resume. On bare metal that traffic goes through
  `crates/akuma-net-nic/src/rtl8169.rs`, a driver **no host test and neither
  fast-lane target ever executes** (both are virtio), on a machine where a
  one-line diagnostic in the poll loop livelocked the box on 2026-09-19. DNS
  alone has already had a four-cause investigation
  ([`AKUMA_AMD64_DNS_CONNECTED_UDP.md`](AKUMA_AMD64_DNS_CONNECTED_UDP.md)). A
  dropped connection is not a tidy failure for an agent: it is a half-written
  patch and a loop that cannot report what it did.
- **Processes and threads.** A build is thousands of `fork`/`execve`/`clone`
  cycles and an agent adds thousands more (every tool call is a process). The
  known-open list here is not short — the SMP user-process corruption above,
  the thread-lifecycle paths
  ([`AKUMA_AMD64_THREAD_LIFECYCLE.md`](AKUMA_AMD64_THREAD_LIFECYCLE.md)), and
  the process table that still **panics** when it fills rather than applying
  backpressure.

Both have the same property that makes them gates rather than chores: they fail
*probabilistically*, hours in, and the failure destroys the evidence. Autonomy
multiplies exactly that. Measure both with something that runs for hours before
handing the machine to an agent that runs for hours.

The probe aimed at it is `userspace/amd64/fbstress/` — concurrent demand faults
on *file-backed* pages shared between processes through `akuma-fpcache`, plus CoW
on those pages. It is the shape `rustc` has and `mtstress` does not, and — worth
noting for the LLM box — **it is also the shape `llama.cpp` has**: many threads
over one large file-backed `mmap`. One probe, both workloads.

Two things make this cheaper than it was this morning: the heap fix took a full
build from 1398 s to **640 s**, and `fbstress` should answer in seconds if the
hypothesis is right. If it comes back clean, that does not exonerate the
file-page path — it means the window is not reachable that way, and the next
candidates are `fork`/`exec` churn concurrent with faults (cargo's actual shape)
and the thread-lifecycle paths.

## 6. Precedent: this has partly happened before, in the guest

Worth recording, because it sets the bar and dates it.

- **2026-08-17** — `nca` (`native-cli-ai`, a Rust AI CLI) driven from inside the
  guest against **Z.ai GLM-4.7 over real HTTPS**
  (`docs/archive/NCA_MISSING_SYSCALLS.md` §6b).
- **2026-08-22** — **the first program written for Akuma *inside Akuma* was
  written by GLM-4.7** (`docs/archive/600_BUGS_ANNIVERSARY.md`, slide 03). The
  same day, a sustained in-guest self-hosted build in
  `/src/github.com/netoneko/akuma-cli` ran many `cargo build`/`cargo check`
  invocations across a long `nca` session.
- **2026-08-25, the same evening the AArch64 `KERNEL_DROPOFF` self-host loop
  landed** (`169e799c`, 20:12), the terminal layer was being patched from the
  same seat:

  | time | commit | |
  |---|---|---|
  | 18:34 | `73d1b099` | tty shenanigans part 1 |
  | 19:29 | `b19ae838` | more tty shenanigans — **reworked the terminal-ioctl gate and dropped the `FIONREAD` arm for `Stdin`/`DevTty`** |
  | 20:12 | `169e799c` | kernel self hosting loop |
  | 23:15 | `cb01b945` | **"glm broke tty lmao"** — replaced the `fd > 2` cutoff with a real `FileDescriptor` match so `/dev/tty`'s own fd stops answering `ENOTTY` |
  | 23:20 | `50539819` | **restored the arm `b19ae838` dropped** — without it `FIONREAD` on stdin/`/dev/tty` always answered 0, so a program polling before a non-blocking read never saw input arrive |
  | 00:22 | `ec0c3ee9` | more fixes for tty |

  That is the "one patch fixed tty, another broke it again" cycle: `b19ae838`
  fixed the gate and silently removed an arm; five minutes of debugging later
  `50539819` put it back, with the regression named in the comment.

  **Caveat on attribution:** every one of these is authored by
  `Kirill Maksimov` (the user drives all commits), and only `cb01b945`'s message
  names the model. Which of the others were model-written is not recorded in
  git, so this table reports the *changes* and their order, not authorship.

The gap between then and this proposal: all of that was a **guest**, on AArch64,
with the source tree and userland supplied from outside. Doing it on the metal,
from a repository the machine cloned itself, is the increment.

## 7. Suggested order

1. `fbstress` on the metal at SMP=4 — before building anything long. If it
   reproduces in seconds, fix that first; everything below gets cheaper.
2. `git clone` over the network from inside Akuma (§3.1). One command, decides
   the whole shape of the rest.
3. Named submodules only; record the list and the disk cost (§3.2).
4. `cargo vendor` on the box; keep `vendor/` and the offline block as rig state.
5. Kernel build from the cloned tree — `AKUMA_GIT_SHA` should stop saying
   `unknown` on its own, which is the cheap proof the repository is real.
6. Userland: `herd`, `box`, `sshd`, then `nca`.
7. Only then NCA + GLM driving the loop.

## 8. The goal, restated: **patch itself, reboot, and continue**

Building its own kernel is done (2026-09-18) and is, on its own, a smaller claim
than it sounds: a machine can compile a byte-identical copy of what it is
already running and still be a cross-compilation target with extra steps. The
proof that matters is the **loop**, not the artifact:

> The box holds the repository. It changes the source, builds the change,
> installs it, reboots into it, and picks the work back up — **with nothing
> outside the machine involved in any of those five steps.**

Written as the cycle it has to close:

```sh
# inside Akuma, on the metal, with no Ubuntu and no laptop in the path
cd /src/github.com/netoneko/akuma
vi <a file>                      # or an agent edits it
kbuild -j 1                      # build the change
kinstall                         # /boot/akuma-amd64, md5-verified
/bin/busybox reboot -f           # up on what it just wrote
git commit -am "…" && git push   # report what it changed
#  … and the next iteration starts here, on the new kernel
```

Four properties make that a real claim rather than a demo, and each is a thing
to check rather than assume:

1. **The change is authored where it is built.** A patch that arrives from the
   laptop proves the compiler works, not the machine. `git status` must be
   clean before the edit and show exactly the edit after it — which is why
   nothing the rig needs (cargo config, target dirs, wrapper scripts) lives in
   the checkout.
2. **The reboot is unattended.** `GRUB_DEFAULT="Akuma/amd64"`, so `reboot -f`
   returns to Akuma. Nothing arms anything.
3. **The work survives the reboot.** The checkout, the toolchain, the cargo
   cache and the build output are all on the partition the kernel boots from,
   so iteration `n+1` starts with everything iteration `n` produced.
4. **A failed iteration is recoverable without the loop.**
   `/boot/akuma-amd64.good` is the only remote-free way back, and it is worth
   exactly as much as the kernel behind it — promote it after a boot that
   passed its self-tests, never at install time.

**The first real exercise of this loop is Intel HDA audio**
([`../runbooks/add-intel-hda-audio.md`](../runbooks/add-intel-hda-audio.md)),
driven by meow + GLM on the box. It was chosen because it is a *new* subsystem
rather than a patch to an existing one — a driver for hardware only this machine
has (`8086:8c20` at `00:1b.0`), which no host test and no QEMU fast lane can
fully stand in for — and because "did it work?" is answerable by a human in one
second, from across the room, without reading a log.

The gate in §5 has not moved, and it is wider than the SMP race: **networking
and the process/thread paths both need to be severely stabilized on amd64
before the autonomous version of this is attempted at all.** Closing the loop
by hand is worth doing now regardless — every iteration is a trial of exactly
those paths, run by someone who can tell a kernel bug from an agent mistake.
Handing the same loop to an agent before then produces neither the driver nor a
usable bug report.

## 9. What is on the partition now (2026-09-19)

Staged from Ubuntu with `sdb1` mounted at `/mnt/ak`. Three of these were
**blockers that had not been found yet** — the box could not have built anything
as it stood that morning.

| | where | note |
|---|---|---|
| the repository | `/src/github.com/netoneko/akuma` | full clone (not shallow), `amd64-cleanup-and-improvements` @ `894ec53b`, `origin` over https |
| submodules | `crates/akuma-fbcon/vendor/spleen`, `userspace/meow`, `userspace/nca/native-cli-ai` | objects had been fetched but **three worktrees were empty** — `git submodule update --init` reported nothing to do because the recorded SHA already matched; `--force` is what checks the files out. Without `spleen` the kernel does not build at all (`akuma-fbcon`'s `build.rs` bakes the BDF) |
| toolchain | `/usr/local/rust` | nightly `1.100.0-nightly (420ed2a0c 2026-09-18)`, musl host, targets `x86_64-unknown-{none,linux-musl}`, **plus `rust-src`** |
| cargo cache | `/root/.cargo` | ~1 GB, an ordinary registry cache (`cache/` + extracted `src/` + the one git dependency), **not** a vendor directory. Primed for the kernel workspace, `userspace/`, `meow` and `nca` |
| cargo config | `/root/.cargo/config.toml` | box-local: `--threads=1` for lld, and the host linker below. Deliberately **not** in the checkout, so the tree stays `git status` clean |
| env + wrappers | `/etc/akuma-dev.env`, `/etc/profile`, `/bin/{kbuild,ubuild,mbuild,kinstall}` | sshd sets no environment for a session, so every wrapper sources the env itself |
| git identity | `/etc/gitconfig` (and `/root/.gitconfig`) | system-wide because a session has no `HOME` unless a wrapper sets one |

**Trap 1 — the toolchain was silently truncated.** `libcore.rmeta` for
`x86_64-unknown-none` was **209 KB where it should be 68 MB**, and `liballoc`
and `libcompiler_builtins` were missing outright; the tree was also littered
with 163-byte `._*` AppleDouble stubs, so it had been copied from the Mac. The
failure this produces names the wrong thing entirely — *"only metadata stub
found for `rlib` dependency `core`"*, on crate 3 of 137. Fixed by installing the
same nightly through `rustup` on Ubuntu and rsyncing it whole. **Check
`ls -la …/x86_64-unknown-none/lib/` after any toolchain copy**: four files, and
`libcore.rmeta` is tens of megabytes.

**Trap 2 — proc macros could not have linked.** `syn`, `quote`,
`thiserror-impl`, `enumn` and `zerocopy-derive` build as musl **dylibs** and
need `-lc` and `-lgcc_s`. There is no `cc` on this root, and cargo does **not**
apply `target.<triple>.rustflags` to host units, so no flag can carry the `-L`
paths.

The first fix was a shell wrapper named `ld.lld` that injected
`-L/usr/lib -L/lib`. It worked on Ubuntu and **failed on the box**, which is
trap 2b and the more interesting half:

> **`execve` on this kernel does not understand `#!`.** rustc execs the linker
> directly and got `Exec format error (os error 8)`; busybox runs the identical
> file because a *shell* retries a non-ELF itself. So `/bin/kbuild` works and
> the same script as a linker cannot. Anything a build reaches by `execve` —
> a linker, a `build.rs` helper, a cargo `runner` — must be a real ELF here.

The fix that holds is to move the libraries onto the search path lld already
has rather than to wrap the linker: musl's `libc.so` (which *is* the loader)
and `libgcc_s.so` copied into
`lib/rustlib/x86_64-unknown-linux-musl/lib` and its `self-contained/`, with
`linker = ".../bin/gcc-ld/ld.lld"` — the distribution's own ELF.

**Trap 3 — cwd, not `--manifest-path`.** Cargo discovers `.cargo/config.toml`
from the *working directory*. Building the kernel from anywhere but the manifest
directory drops `relocation-model=static` / `code-model=kernel` and the link
dies with `relocation R_X86_64_32 cannot be used against symbol '_start'` —
which reads as a code fault and is not one. `kbuild` `cd`s for you.

**And a hazard the staging created:** `hpbox.restage_disk()` is
`rsync -aH --delete` from `root.img` onto this partition, which would have
deleted all of the above *and* `/boot/akuma-amd64`. It now excludes `/src`,
`/root`, `/usr/local` and `/boot`.

Verified from Ubuntu, using the partition's own toolchain and cache (the box's
musl binaries run there through a symlinked `/lib/ld-musl-x86_64.so.1`), all
`--offline`:

| | result |
|---|---|
| `akuma-amd64`, `x86_64-unknown-none`, `-j8` | **3 411 960 B ELF, 52 s** |
| `userspace`: `paws httpd herd hget wall box sshd ssh` | all built |
| `meow` for `x86_64-unknown-none` | **built — 246 KB**; the agent has never had an amd64 binary before, and `amd64/mkdisk.sh` does not stage one yet |

### 9.1 The loop, run end to end on the metal — 2026-09-19

Not a rehearsal from Ubuntu: every step below happened inside Akuma, over ssh,
on the machine itself.

| step | result |
|---|---|
| `git fetch origin` from the box | 3.1 s, under build load |
| branch `why-are-we-here-just-to-suffer` off the pushed head | `13bd1595`, `git status` clean |
| `kbuild -j 1` | **EXIT=0, 95 crates, 12 m 16 s**, 3 412 096 B |
| `kinstall` | md5 `811d0401…` verified on read-back; `/boot/akuma-amd64.prev` written (first real use of that path) |
| `/bin/busybox reboot -f` | back on ssh in **48 s** |
| the kernel it came up on | `uname`: `0.0.8 13bd1595-release-smp-shared` — **the branch's own SHA**, and `/boot/akuma-amd64` is the md5 it just built |
| self-tests | **775 passed, 0 failed** |
| `meow -c '…'` against z.ai | answered, streaming, first token in 5.9 s |
| `.good` promoted | after the boot passed, not at install time |

So: **build → install → reboot → still working, with nothing outside the
machine in the path.** That is §8's claim, demonstrated by hand. What it does
**not** yet show is the same loop driven by the agent rather than by a person,
which is what §5's gates are about.

Two notes for whoever compares binaries next: the self-built kernel is **not**
byte-identical to the one it replaced (3 412 096 vs 3 411 472 B) and should not
be — the box's `--threads=1` rustflag feeds cargo's `-C metadata` hash, so a
fixed-point check has to hold rustflags constant. And `kbuild` at `-j 1` took
12 m 16 s here against the ~23 min the earlier record gives; the heap is 1 GiB
now.

**`git push` from the box does not work, and as of 2026-09-19 that is a
decision rather than a gap.** `origin` is plain https and the machine holds no
credential (no `.git-credentials`, no helper), so the box commits locally and a
human collects the branch. Keeping it that way bounds what an agent on this
machine can reach: it can change this machine and nothing else. The loop in §8
is unaffected — "report what it changed" is satisfied by the git log on the
partition — but anything that assumes the box can publish has to route through
a person until that call is revisited.

## Background

- [`../docs/archive/AKUMA_AMD64_BARE_METAL_SELFHOST.md`](AKUMA_AMD64_BARE_METAL_SELFHOST.md)
  — the kernel half, done today, with the heap finding and the SMP measurements.
- [`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md)
  § "Self-hosting on the metal" — the install mechanism this builds on.
- [`../docs/archive/NCA_MISSING_SYSCALLS.md`](NCA_MISSING_SYSCALLS.md)
  — what `nca` needed from the kernel, and the GLM-4.7-over-HTTPS session.
- [`../docs/archive/600_BUGS_ANNIVERSARY.md`](600_BUGS_ANNIVERSARY.md)
  slide 03 — the 22 Aug 2026 first-program-inside-Akuma milestone.
- [`../docs/archive/TTY_SHENANIGANS.md`](TTY_SHENANIGANS.md)
  — rounds 1-3 of the terminal work above.
