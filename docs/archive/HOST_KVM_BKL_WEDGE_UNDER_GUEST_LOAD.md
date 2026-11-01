# Host Akuma wedges (BKL stall, watchdog reset) while a `kvm0` guest runs the gate battery

**Stability: C - open, not root-caused.** Recorded 2026-10-31 so the drill-down starts from the evidence, not from memory.

## Symptom
On the ryzen (bare metal, Akuma host `smp=8`, GRUB menu index 29 = "KVM herd", `kvmctl kvmmem=4096 kvmcores=4-7`) with Taoxichuan running as a
`kvm0` guest (herd service `vm-dev`) and `scripts/kvm/goose/run_gates.sh` driving it over ssh, the **host** stops making progress. After
60 s the AMD FCH watchdog (`wdt`, armed at boot, `reset on expiry`) resets the machine and it returns to Pop!_OS. Wifi/ssh to the host
die first (`ssh: connect ... Operation timed out`); there is no panic and no `[Fault]` - the host console is just a growing
```
[bkls>] core=2 ticket=6656574 serving=6656573 owner=6 spins=1048576
[bkls>] core=4 ticket=2222013 serving=2222012 owner=3 spins=2097152
```
(`owner` is the core holding the big kernel lock, `core` the one spinning for it; `spins` doubles each report). In the 4-vCPU control run the
stuck waiters were cores 4 and 6 behind owners 3 and 5, i.e. **KVM cores waiting on a lock held by another core and vice versa**.

It is the host, not the guest: the guest was healthy (gates G1-G4... passing) right up to the host stopping. "Guest wedge" is the wrong name.

## What was run, and what wedged
All runs: guest kernel = Taoxichuan built from this branch (`akuma-guest-dev`), image `/boot/akuma-dev-root-ssh.img` (a fresh copy of
`.orig` per `boot_dev.sh`), driven by `scripts/kvm/taoxichuan/metal_gates.sh KERNEL VCPUS`.

| run (2026-10-31) | host kernel | guest vCPUs | outcome |
|---|---|---|---|
| mg-fix1 | `e7661570` (X1 futex body + idle marking) | 1 | **wedged** after G4 (klog-248: `owner=6 spins=...`, then wdt reset) |
| mg-ctl2 | `5108b5e5b` (control, before X1) | 1 | completed through M4web, host alive |
| mg-new2 | `e7661570` | 1 | completed through W1d, host alive |
| mg-new4 | `e7661570` | 4 | completed through W1d, host alive |
| mg-ctl4 | `5108b5e5b` (control) | 4 | **wedged** (klog-252: `core=4/6 ... owner=3/5`) |
| mg-x3-4 | `e7661570` | 4 | completed through W1d, host alive |

So: **2 wedges in 6 runs, one on each host kernel**, and the control kernel (which predates the futex-body and idle-marking extractions) wedges
too. It is therefore **not** a regression from those changes, and it is intermittent. It needs the guest to be doing real work: before the
`/bin/sh` fix ([`TAOXICHUAN_METAL_SH_SPAWN_EPERM.md`](TAOXICHUAN_METAL_SH_SPAWN_EPERM.md)) the guest could not spawn anything and the
same gates never wedged the host - which may only mean the path was unexercised.

## Related, older, and probably the same family
* Every host boot, including ones before this work (klog-239, 00:58, and klog-240), logs about 11 early `[BKL] stuck: owner=1 waiter=2 tag=511 serving=0` lines and 8 `[bkls>]` reports during
  bring-up; the system recovers from those. They are the same reporter, so the stall *mechanism* is plausibly the same, just short at boot.
* Boots 242, 244, 246 ended by an operator `reboot -f` but show 108, 302 and 521 `[bkls>]` lines: long stalls under guest load that recovered.
* `docs/archive/` has the family history: [`BKL_FINE_GRAINED_LOCKING_PLAN`](BKL_FINE_GRAINED_LOCKING_PLAN.md), the Firecracker ticket-lock livelock
  (`project_bkl_ticket_livelock_firecracker`: NETWORK <-> SOCKET_TABLE inversion) and the amd64 `-j4` wedge notes. None is shown to apply here yet.

## What is known about the lock owner
Nothing beyond the number. The reporter prints the owning core and a `tag`, and the early boot lines carry `tag=511`; the wedge reports in
klog-248 / klog-252 carry no tag at all. Not yet answered: what the owner core is executing, whether it is inside `VMRUN` (a guest vCPU
run while holding the host BKL would stall everyone for a guest timeslice), inside the relay/gateway path (`[KVM] relay`, `gw_*`), or the
TLB-shootdown spin-assist. `[KVM] cpu: task(core N) guest=...% busy: c0..c7` lines show the cores busy at ~6% each right before the stop,
so it is a lock-ordering/livelock shape, not CPU saturation.

## Hypotheses to separate (not tested)
1. **A vCPU thread holds the host BKL across (part of) a `VMRUN`**, and another core's `kvm0` ioctl/relay path or a TLB shootdown waits
   for it; the guest then needs the waiter (a gateway frame, an IPI ack) to progress -> livelock. Look at `amd64/src/kvm/linux/machine.rs`
   around the run loop for any BKL-held region that includes the exit handlers' blocking calls.
2. **Shootdown/IPI ack** waiting on a core that is in guest mode (SVM) and does not take the host IPI until the next exit.
3. **The gateway/relay thread** (`vm_outbound`) and the net poll contending on `NETWORK`/`SOCKET_TABLE` with a guest exit - the shape of
   the Firecracker livelock.
4. Load-dependent only: `apk add` / `curl` phases of the battery (G3, A2, M4) are the network-heavy ones.

## How to reproduce / drill down
1. Ryzen in Pop. Stage the host kernel built ON the ryzen (`CARGO_TARGET_DIR=/root/am-target cargo build -p akuma-amd64 --release --target x86_64-unknown-none --features no-tests`) to p3 `/boot/akuma-amd64.verify` and the Taoxichuan kernel to `/boot/akuma-guest-dev`; make sure the image's `/bin/sshd` is the `linux-abi` build.
2. `sh /root/am-wt/overlays/ryzen/arm.sh 29` (index 29, **not** 30 - see `overlays/ryzen/README.md`), reach Akuma with `overlays/ryzen/wifi-ssh.py --key ~/.ssh/id_ed25519`.
3. `sh /root/metal_gates.sh /boot/akuma-guest-dev 4` (copy of `scripts/kvm/taoxichuan/metal_gates.sh`), detached; poll `/root/mg-*.out` for `PASS/FAIL`.
4. After a reset, the evidence is on p3: `/var/log/ryzen/klog-N.all` and `klog-N.dmesg` (`grep -a 'bkls>\|BKL\] stuck'`), `herd-N.log`.
5. To catch the owner: the next run should enable the BKL reporter's tag (or add the owner's `rip`/stack to the `[bkls>]` line), and run with `kvmcores=` fewer than the vCPUs to see if the stall is core-count dependent. A cheap bisect: run the 4-vCPU battery ~5 times per kernel (`5108b5e5b`, `e7661570`) and count resets, since 2/6 is too few runs to rank them.

## Verification status
Evidence is the six runs above plus the klog tails; no fix attempted, no A/B beyond the host-kernel control. Nothing in `reference/` changed.

## Collateral damage: each watchdog reset can corrupt p3 (found 2026-11-01)
Akuma mounts p3 (the ext2 root, no journal) read-write; a watchdog reset mid-write leaves it dirty. After the two resets above
`e2fsck -fn /dev/nvme0n1p3` (Pop, unmounted, read-only check) reports **HTREE directory errors and corrupted directory blocks under
`/root/.cargo/registry/src/...`**. The Pop KVM rig uses p3 as the base of its guest disk, and from then on **every** in-guest `-j4`
build stalled in `rust-lld` with `[BKL] cpu N stuck: owner cpu M` climbing (123-180 reports, 25-50 minutes, no `[Fault]`) - for the
current kernel, for `e7661570`, for `8cd638d8` (before the futex-body extraction) and for `kb-fix` (`5108b5e5`), which had built the same tree in
13m11s the day before. A stall that follows the disk, not the kernel, is the disk. Not repaired yet (`e2fsck -fp` was refused by the
environment's safety check and needs the owner's go-ahead); until it is, in-guest builds on the p3 root are not a valid regression oracle.
Lesson for the drill-down: run the metal gates with p3 mounted **read-only** from the host side where possible, and fsck p3 after any watchdog reset.

## Resolved: the in-guest `-j4` stall was the disk, confirmed without writing p3 (2026-11-01)
p3 itself needs no repair for the rig, because every guest boots it through a qcow2 overlay. Repair the *overlay*:
`qemu-img create -f qcow2 -b /dev/nvme0n1p3 -F raw ov.qcow2 && qemu-nbd -c /dev/nbd0 ov.qcow2 && e2fsck -fy /dev/nbd0 && qemu-nbd -d /dev/nbd0`,
then copy it to `$TX_RUN/overlay.qcow2` and run `TX_FRESH=keep python3 scripts/kvm/taoxichuan/build_run.py KERNEL 4`
(`TX_FRESH` defaults to `fresh`, which recreates the overlay and so discards the repair).
e2fsck on the overlay fixed 2 invalid HTREE roots, 4 corrupted directories (one holding a garbage entry in a cargo target dir) and ~300 unattached inodes.
Result with the X4 kernel (`target/.../taoxichuan`, 4 vCPUs): `-j4` build **10m29s, artifact md5 `38852d54...` (identical to the kb-fix artifact)**, 12 `[BKL] stuck`
lines (vs 123-180 on the corrupted disk), no `[Fault]`. So there is no CoW/X1-X4 regression; the host watchdog resets are the only open item here.
The real p3 is still unrepaired and still carries the corruption; the owner decides whether to `e2fsck` it.

## Correction (2026-11-01, later): the stall is NOT explained by the disk
The "Resolved" section above rested on one passing run. After p3 itself was repaired (`e2fsck -fp` stopped at a corrupt directory, `-fy` finished it; clean on re-check) and
fresh overlays were used, the same pre-X5 control kernel (1591736 bytes, in-guest tree pinned to one commit) gave: ECHILD (`No child process`, os error 10) from cargo's
spawn at ~200 s; a spin at `execve rust-lld` (4 vCPUs at ~394% CPU, console frozen, `[BKL] stuck` 84+ and climbing, sshd unreachable); then two passes
(4 vCPUs 13m45s, 1 vCPU 8m22s, artifact md5 `38852d54...` both). The X5 kernel gave one rustc `#GP` (ring 3, `rip=0x300469d8`, slot 4) and one `rust-lld` spin.
So: a nondeterministic fork/exec/BKL fault exists on the pre-X5 kernel too, at 4 vCPUs (1 vCPU not yet seen to fail), independent of the fsck. Disk corruption may have
made it more likely; it is not the whole story. `build_run.py` now takes `BR_N` (parallel rig slots) and opens a QEMU monitor at `/root/mon<N>` so a wedged guest's
registers can be read with `info registers -a`.

## Where the guest wedge sits (2026-11-01, QEMU monitor on two wedged 4-vCPU guests, X5 kernel)
Both X5 runs spun at `execve rust-lld` (console frozen ~40 min, 186 `[BKL] stuck`, ~394% host CPU, no `[Fault]`). `info registers -a` over several samples:
three cores sit in `smp::bkl_enter+0x9c` (the ticket spin); the fourth, the owner (`BKL_OWNER`, read from guest memory), moves between `Interface::poll`
(`seq_to_transmit`, `dispatch`), `switch_to`, `can_run` and `memcpy`. `BKL_SERVING` advanced **8 tickets in ~43 s** with a single hold of ~30 s: the lock is
being handed over, so this is starvation/convoy, **not a deadlock**. The owner's stack is `sock::poll_arm -> net::sleep_until -> netpoll()/x86_yield`:
`poll`/`ppoll` waits are 1 ms laps of `sleep_until`, and every lap runs a full smoltcp `poll` (loopback included) **with the BKL held**, so several pollers
(sshd's serve loop, cargo/rustc waiting on pipes) keep the lock busy in network polling while the cores that want it for `execve`/`mmap` spin.
Not yet answered: why it is intermittent (the same pre-X5 kernel passed 3 of 6 builds), and which loopback TCP socket makes `seq_to_transmit`/`dispatch`
dominate each lap. Candidate fixes, none tried: run `netpoll()` outside the BKL when the lap is only waiting (`bkl_unlocked` around the yield/`wait_lap`, take the lock
only for the lap itself); poll_arm sleeping on a deadline with the lock released; bounding the lap's drain budget. Measure with `BR_N` pairs and `info registers -a`
(`build_run.py` opens `/root/mon<N>`); watch `BKL_SERVING` from the monitor (`x/1wx` on the symbol) for the hand-over rate.
