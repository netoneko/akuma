QUESTION: Reboot the bare-metal box into the Akuma kernel with `kvmctl kvmmem=4096` on its
command line (the boot that creates /dev/kvm0), then rerun this gate:
`ESCALATE_ANSWERED=1 sh denver/autoresearch.sh 1 PHASE-0-SCAFFOLD`.

CONTEXT: The PHASE-0-SCAFFOLD gate is done except for its boot probe. The scaffold exists and
builds green (akuma-jvm-kernel front crate + denver kernel; the platform stack merged from the
proven shanghai branch — see denver/docs/research/PHASE-0-SCAFFOLD.md, decision D1; capture:
denver/gates/PHASE-0-SCAFFOLD.log). The probe rig is fully prepared under /root/denver/w/
(denver kernel binary, fsck-clean ext2 image with busybox/sshd/uname/tokioprobe, herd spec,
port 2247): after the reboot the next session only runs `herd vm-run /root/denver/w/vm.spec`,
ssh-probes `uname -a` + tokioprobe, and records.

Why it stops here: `herd vm-run` opens /dev/kvm0 and got ENOENT — the running host kernel
(`Akuma akuma-ryzen 0.0.9 886dabf-release-smp-shared`) was not booted with `kvmctl` (semantics
per docs/reference/subsystems/kvm.md line 192; arming at boot per amd64/src/main.rs). Firecracker
(/root/fc/firecracker) needs the same device; no QEMU exists on the box (checked /usr/local, /opt
incl. the gentoo root, /root). The agent runs ON this box — rebooting it itself would kill the
session and is a machine-policy decision, not a plan decision. Evidence trail: three hypotheses
(H1 herd vm-run, H2 firecracker, H3 qemu-anywhere) recorded in the gate log.

Note for whoever reboots: the shanghai run's working boot armed KVM (`kvmctl kvmmem=4096`,
kvm.md entry 30, 2026-10-22). Nothing else about the rig changed; /root/denver/w/ survives the
reboot (it is on the root disk).
