# Taoxichuan stage 1 — evidence logs (2026-10-24)

Raw console logs behind the stage-1 entries in
[`../MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`](../MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md).

| file | who / what |
|---|---|
| `metal-track-amd64-kvm-taoxichuan-gate.log` | goose-glm, Taoxichuan (amd64) as a `kvm0` guest on the ryzen: banner, ext2 root, `init spawn`, `stage 1 gate reached`, 0 faults. The harness's last `FAIL (banner=yes, init=no)` line is herd's generic marker check, not the gate |
| `metal-track-amd64-kvm-main-kernel-after-refactor.log` | Sonnet, the *main* amd64 kernel at `43edbffc` (after goose's boot-kit extraction) as a `kvm0` guest: `AKUMA-GUEST-UP` ... `DONE` |
| `metal-reboot-test-report.txt` | Sonnet, `dev_cycle.py reboot-test`: the same main kernel on bare metal via GRUB, Pop -> Akuma, `PASS` |
| `host-track-aarch64-firecracker-boot.log` | Sonnet, Taoxichuan (aarch64, `taoxichuan` feature + `platform-firecracker`) on Firecracker in Lima: ext2 mounted, `/bin/sshd` spawned, 0 faults |

The banner text and the line "tao xi chuan, the kiln-fire market" are goose's
invention (the first version, `43edbffc`, used the characters 陶 熙 川 and "the
kiln-fire stage"); the experiment's brief gives the name only. The host console
relay turned the non-ASCII bytes into `?`, which is why `107eb25c` made it ASCII.
