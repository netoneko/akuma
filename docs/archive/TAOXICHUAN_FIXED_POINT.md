# Taoxichuan: the self-hosting fixed point (2026-10-30; bare-metal run 2026-10-31)

**Result.** The taoxichuan kernel compiles itself, and what it compiles reproduces:

| generation | built by | md5 | bytes |
|---|---|---|---|
| K0 | the Mac (`nightly-2026-09-19`, `cargo build --release --target x86_64-unknown-none --features no-tests,trace-err`) | `9ef4f0647e56670033b0bff9c15b2774` | 1,458,520 |
| K1 | a guest running **K0**, building the committed tree `6af48c06` inside it (musl-host nightly, `-j4`, 3072 MiB, KVM on the ryzen) | `6facda34cbf9ef3cb318dae4ce09ab04` | 1,458,104 |
| K2 | a guest running **K1**, same commit, same command, same paths (`/tmp/gh`, `/tmp/tgt`) | `6facda34cbf9ef3cb318dae4ce09ab04` | 1,458,104 |

**K1 == K2, byte for byte.** K1 boots (G1 answers ssh as `Linux taoxichuan 6.6.0-akuma ...`) and is a working builder. K0 != K1 is
expected and is not a failure: they are different compilers (the Mac's gnu nightly 09-19 vs the guest's musl nightly 09-29), and the
kernel that *runs* the build is not an input of the output - which is exactly what K1 == K2 shows from the other side (two different
kernels, K0 and K1, ran the two builds and produced the same bytes from the same toolchain).

Features and tree: `no-tests,trace-err`, commit `6af48c06` (CoW flat table, W^X, the process-table fix, the 16 GiB mmap slab, the clock fix,
E2BIG, `close(0)`/`open`). Reproducible because the builds use the same absolute paths and no timestamps reach the image. Wall time
~10 minutes per generation (the guest's clock now agrees with the host's, see the bugs doc section 3).

## Procedure (what was run)
1. `git worktree add --detach <dir> <sha>`; build K0 on the host; copy it to `/root/tx-target/x86_64-unknown-none/release/taoxichuan` **with the exec bit**
   (`cat > K.new && chmod +x K.new && mv K.new K`), then `TX_MEM=3072 /root/tp.sh boot fresh` and require a **new** `guest up (pid N)`.
2. In the guest: `git init; git fetch --depth 1 --filter=blob:none origin <sha>; git sparse-checkout set --no-cone /Cargo.toml /Cargo.lock /build.rs
   /linker.ld /.cargo /rust-toolchain.toml /src /amd64 /taoxichuan /crates; git checkout FETCH_HEAD` (the repo is public over HTTPS; the guest has no
   key), then `cargo build --release --target x86_64-unknown-none -p taoxichuan --features no-tests,trace-err -j4 --target-dir /tmp/tgt`.
3. When `/tmp/tgt/x86_64-unknown-none/release/taoxichuan` exists, `md5sum` it and pull it out (below). Ship it as K1, `boot fresh`, repeat step 2, compare.

## Pitfalls that cost an hour (all recorded in [the bugs doc](TAOXICHUAN_BUGS_FOUND_2026_10_30.md) or fixed)
* **Shipping a kernel without `+x`**: `tp.sh boot` prints "no kernel: run build first", the *previous* guest keeps running, and ssh still answers.
  Two runs and a probe tested a stale kernel. Check for a new pid.
* **The build outlives the ssh session that started it, but not by design**: a long foreground ssh dies (`Timeout, server not responding`, exit 255) under
  load and sshd then spins on the dead socket (`[err] nr=45 -104` flood, ~80% host CPU); the build itself keeps going. Judge completion by the output
  file, **not** by console line count (the per-syscall error trace is capped, so the log "freezes" while work continues - this misfired once as a "wedge").
* **`nohup ... &` inside an ssh command is killed when that session ends** (sshd reaps the process group), and ash's `&` needed the `close(0)`/`open` fix to start at all.
* **A boot program cannot run a build**: `init=/bin/busybox initargs=sh,/tmp/script` gets `can't fork: Operation not permitted` - gap G4, "init is not a Process".
  (`init=/bin/sh` fails earlier: the boot loader does not follow the `/bin/sh` symlink and reads 0 bytes.)
* **The guest sshd's large stdout is lossy**: `base64 <1.4 MB file>` over ssh arrives truncated. Pull binaries in line-range chunks (`sed -n 'a,bp'`, 1,500 lines
  per call) and verify the md5. Binary or base64 *stdin* into the guest is also unreliable (`gzip: invalid magic`).

## What this does and does not establish
It establishes that the toolchain-in-guest can rebuild the kernel that hosts it, and that the result is stable under iteration, on KVM on the ryzen
(Linux host). It does **not** establish: the same on bare metal (the metal validation run is next), reproducibility across different toolchains (K0 != K1),
or that the produced kernel passes the full gate suite beyond booting and building (K1 hosted the K2 build, which is a substantial workload: ~100 rustc/cargo/git
processes, threads, pipes, the network).

## Bare-metal run (2026-10-31)

The same fixed point on the ryzen under **Akuma's own hypervisor** (`/dev/kvm0`, AMD SVM, nested paging), not Linux KVM:

| generation | built by | md5 | bytes | wall time |
|---|---|---|---|---|
| K1 | a guest running K0 (3 GiB, 4 vCPUs on cores 4-7) | `6facda34cbf9ef3cb318dae4ce09ab04` | 1,458,104 | 5m22s |
| K2 | a guest running K1 (same image, same commit `6af48c06`, same paths) | `6facda34cbf9ef3cb318dae4ce09ab04` | 1,458,104 | 5m18s |

K1 here is byte-identical to the K1 of the Linux-KVM run above, so the result is also reproducible across host hypervisors. K1 was pulled out of the guest's disk image with `debugfs dump` (the guest sshd's large output is lossy), shipped as `/boot/akuma-guest-tx2`, and booted as the guest kernel for K2.

What it needed beyond the KVM run:
* **Guest RAM above 1 GiB.** The host kernel capped guest RAM at 1 GiB (one nested-page-table directory); lifted to 4 GiB by glm (`KVM_GUEST_RAM_CAP_LIFTED.md`), with a rebuilt host kernel and a rebuilt `herd` (its `vm_ram_mib` cap was compiled in). herd's PID-1 daemon only reloads its own binary on reboot, so the guest was run with `herd vm-run <spec>` directly.
* **DNS.** The host relay answers DNS only at `10.0.2.3:53`; the image's `/etc/resolv.conf` named public resolvers.
* **A toolchain image built on the box**, not shipped over wifi: staged from the box's own root and populated with `debugfs -f` (`mke2fs -d` fails there on xattrs).
* **One ssh session at a time.** The cooperative sshd serves a single session, and the relay holds only two forwarded connections, so a second ssh (a status poll) is refused or times out while a long command runs. The build was started as one long foreground ssh and observed through the host-side console log.

Not shown: that K1/K2 pass anything beyond booting and building; a second toolchain (K0 != K1 still stands).

## After the cleanup (2026-11-02, B14)

Re-run on `ade75544` (after B7b-B13 of [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): the
crate switch, `akuma-runtime-tokio`, one ELF loader, containers off) under Pop's Linux KVM on the ryzen, with
`scripts/kvm/taoxichuan/fp_pop.py K0 ade75544` (smp=4, 3 GiB; K(n) pulled with `qemu-nbd --read-only` +
`debugfs dump` and checked against the guest's own `md5sum`):

| generation | built by | md5 | bytes | build time |
|---|---|---|---|---|
| K0 | the Mac (`--features no-tests,trace-err`) | `d41a2a1b8f17a859b1423542a7e79c9b` | 2,298,760 | - |
| K1 | a guest running K0 | `6343290ae3a631eba40cce8491e0254e` | 2,291,536 | 476 s |
| K2 | a guest running K1 | `6343290ae3a631eba40cce8491e0254e` | 2,291,536 | 471 s |

**K1 == K2** (also `cmp`-identical on the Mac), with 0 fault, panic or bad-frame lines on either console. The
image is larger than the 10-31 one because it is built with `trace-err` and carries everything B1-B13 pulled in.
The bare-metal repeat under Akuma's hypervisor (`fp_metal.py`) waits for the ryzen to boot into Akuma.

## After B15 (2026-11-02)

Re-run on `961a77be` (the same kernel source as B15 - Taoxichuan moved into `akuma-tokio-kernel`), under
Pop's Linux KVM on the ryzen with `scripts/kvm/taoxichuan/fp_pop.py` (smp=4, 3 GiB; K(n) pulled with
`qemu-nbd --read-only` + `debugfs dump` and checked against the guest's own `md5sum`):

| generation | built by | md5 | bytes | build time |
|---|---|---|---|---|
| K0 | the Mac (`--features no-tests,trace-err`) | `0272f1eb7de0e031e4266e9d89aec181` | - | - |
| K1 | a guest running K0 | `5f568279177b7edf28bbc9445fee4d4c` | 2,304,624 | ~450 s (Mac-driven; its `/tmp/fp.status` is not preserved) |
| K2 | a guest running K1 | `5f568279177b7edf28bbc9445fee4d4c` | 2,304,624 | 452 s (`/tmp/fp.status` pulled out of the overlay: 16:48:32 checked out, 16:56:11 built) |

**K1 == K2** (`cmp`-identical), and each pulled kernel's md5 equals its guest's own `md5sum`. Generation 1's
console had 0 fault-ish lines; generation 2's console has exactly one, `[BKL] cpu 0 stuck: owner cpu 2`
(the BKL watchdog's "stuck" wording, not a Fault/PANIC/BADFRAME).

**The Mac drove only half of generation 2.** It booted the K1 guest at 16:48 and the build finished at
16:56, but the Mac never came back to stop the guest and pull K2. At 18:02 the ryzen's goose handoff
session finished the generation by hand with `fp_pop.py`'s own commands: stopped the guest by its pidfile
(`TX_RUN=/root/tx-fp bash /root/tx-pop.sh stop`), pulled K2 and `/tmp/fp.status` out of the overlay with
`/dev/nbd7` (`qemu-nbd --read-only`, `debugfs dump`, `qemu-nbd -d`), and checked the pulled md5 against the
guest's. Nothing was rebuilt or rebooted; both steps the Mac skipped are the ones done by hand.
