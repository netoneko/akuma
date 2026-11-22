# AArch64 platform socket wait starvation

On 2026-11-22 taoxichuan on QEMU AArch64 with one CPU passed UDP
loopback but timed out on TCP loopback; with two CPUs both passed. A blocking
socket wait pumped the stack and halted, holding the user task in the kernel
instead of scheduling the peer. The shared wait hooks already supported timed
scheduler parks, but the AArch64 `NetMachine` left `can_park` false.

The adapter now enables timed parks for a published, running user task, using
the existing `is_real_task` check. Boot-time DHCP and idle slots retain bounded
halt laps. Each park wakes on its deadline, allowing another network pump; this
change adds no allocations. Taoxichuan, Shanghai and Denver share this adapter.

Verification: `premerge_build.sh --only taoxichuan-aarch64,shanghai-aarch64,denver-aarch64`
passed (including Denver JVM). QEMU TCG booted taoxichuan with a static AArch64
musl `scripts/gate/gateprobe` on an ext2 root and virtio devices. At SMP=1 and
SMP=2, `udp_loopback` and `tcp_loopback` both passed, including the 64 KiB TCP
echo and half-close; TCP took 1.95 s and 1.25 s respectively. Shanghai and
Denver were built but not booted in this check. The machine crate's host test
run has no tests; AArch64 clippy passed with an existing shared-core warning.

Background: [the earlier amd64 incident](TOKIO_KERNELS_LOOPBACK_STARVATION.md)
and [kernel convergence](KERNEL_CONVERGENCE.md).
