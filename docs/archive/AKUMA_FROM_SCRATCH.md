# Akuma From Scratch — the trashbox dev cycle with no Ubuntu in the middle

**Status:** IN PROGRESS as of 2026-09-18 — the clone step is being run by hand now. **Machine:** the HP 500-502nj bare metal.
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
| **the source tree** | `rsync` from Ubuntu's `/root/akuma`, staged with `--exclude .git` | the box has no repository, so it cannot fetch, diff, branch or report what it built — and `AKUMA_GIT_SHA` falls back to `unknown` |
| **`vendor/`** | `cargo vendor` run on Ubuntu | the box cannot resolve a dependency change without leaving Akuma |
| **all of userland** | `amd64/mkdisk.sh` on Ubuntu, `rsync`'d onto sdb1 | `/bin/sh`, `/bin/sshd`, `herd`, `box` — the box runs binaries it cannot rebuild |

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
   metal and works locally.

   **First attempt, 2026-09-18: hung — no DNS.** `git clone --depth=1
   https://github.com/netoneko/akuma.git` sat with three processes
   (`git clone`, `git remote-https`, `git-remote-https`) at **0:00 CPU across
   12 s**. That signature is worth knowing because it is *blocked on I/O*, not
   the SMP wedge — which also shows 0:00 but with `cargo`/`rustc` and after real
   work. A hung resolver looks exactly like a stuck clone.

   The bare-metal root needs the same two things the `box` rootfs needed for DNS
   and HTTPS, and they are separate failures:

   | missing | symptom |
   |---|---|
   | `/etc/resolv.conf` | clone **hangs** at 0:00 CPU, no error |
   | CA bundle (`ca-certificates.crt`) | clone **fails with a TLS error** — git verifies github's certificate in its own stack |

   Stage both on sdb1 before concluding anything about git's TLS support.

   > **ROOT-CAUSED and FIXED the same day, and it was not TLS — it was the
   > kernel.** `/etc/resolv.conf` was present and correct the whole time, and
   > `nslookup github.com` resolved fine. `curl` and `git` use **c-ares**, which
   > `connect()`s its UDP socket where musl's resolver does not, and three
   > syscalls on that connected path were wrong: `send()` answered `EBADF`
   > (a null `sendto` destination fell into the TCP path) and
   > `getsockname`/`getpeername` answered `ENOSYS`. Full account:
   > [`AKUMA_AMD64_DNS_CONNECTED_UDP.md`](AKUMA_AMD64_DNS_CONNECTED_UDP.md).
   >
   > So this step's real lesson is the diagnostic, not the fallback list:
   > **`nslookup` working proves nothing about whether `git` can resolve.**
   > The CA-bundle row above is still untested — it simply never got reached.
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
