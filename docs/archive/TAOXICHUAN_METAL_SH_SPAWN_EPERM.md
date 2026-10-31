# Taoxichuan on metal: `/bin/sh` never spawned (SPAWN from a pool-thread init = EPERM)

**Stability: B.** 2026-10-31.

## Symptom
On the ryzen (Akuma host, entry 29, Taoxichuan as a `kvm0` guest) every `ssh guest 'sh -c ...'` printed
`[SSH] '/bin/sh' failed to spawn ... retrying with /bin/paws ... both failed ... running built-in paws in-process`
and answered `paws: sh: not a builtin`. Most of `scripts/kvm/goose/run_gates.sh` read FAIL; the few PASSes were the shell fallback.
Reproduced on plain QEMU/KVM with the same disk image, so not the SVM hypervisor, and with the pre-session guest kernel, so not a regression.

## Cause
The vm-dev image (`/boot/akuma-dev-root-apk.img.orig`, copied fresh by `boot_dev.sh`) carried the **cooperative** `sshd` (rev `27924603df`,
148720 bytes, md5 `f4ef9276...`), which spawns with Akuma's private `SPAWN` syscall. Since slice S1 init is a pool thread
(`child::start_init_thread`, [`processes.md`](../../taoxichuan/docs/reference/processes.md) § Init is pid 1 on a pool thread) and `spawn_arm`
refuses any caller that is not the boot task (`nested_active()`): `-EPERM` on the very first exec. Nothing logged it: `trace_err` keeps
a 512-entry per-number cap and `SPAWN` is `AKUMA_PRIVATE_BASE + 301`, off the end of it.

It was not init wiring: init is wired as designed. The image was stale against the design ("sshd spawns the Linux way", `switch-to-crates.md` rule 2).

## Fix
* The image's `/bin/sshd` is now `sshd-fork-linuxabi` (`fork-sessions` + `libakuma/linux-abi`; md5 `2e3f32d3...`, from p3 `/root/tx/`), the old one kept as `/bin/sshd-coop`. Applied to the `.orig` (loop mount on Pop, p3 not in use by Akuma).
* `spawn_arm` now prints `[spawn] refused: the caller is pool slot N - build sshd with libakuma/linux-abi`, and `trace_err` never filters or caps `SPAWN`'s answer.

## Verified
`sh -c "echo hi"` 3/3 on QEMU/KVM with the patched image. On metal (host `e7661570`), Taoxichuan as a 1- and 4-vCPU guest: `run_gates.sh` G1-G4, H1-H3c, F1, C1, D1, D2, A1-A3 all PASS at 4 vCPUs (`smp: 4 cpus online`); M4curl/M4web, W1b-d PASS. Still failing, same on both host kernels where compared: M4page, W1a (gate scripts, not investigated here); M4web flaked once with `curl: not found` after a green `apk add curl`.

## Open: the host wedges under guest load
Twice the box went unreachable during a gate run and the watchdog reset it to Pop (`[bkls>] core=N ... owner=M spins=...` climbing on KVM cores 4-7). It happened on the new host kernel (1 vCPU) **and on the pre-X1 control (`5108b5e5b`, 4 vCPU)**, and not on other runs of either, so it is an intermittent host-side BKL stall with a running guest, not a regression of the X1/idle extractions. Every host boot also logs ~11 early `[BKL] stuck` lines (also on boots from 2026-10-31 00:58, before this work).
