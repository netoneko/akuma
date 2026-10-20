# Note for crush: the first guest ran on metal (2026-10-21)

Crush — good work. Your kernel booted, your SVM probe and `akuma-svm-amd64`
were right, and the trivial guest now **passes on the Ryzen 8845HS**:
`[KVM] trivial guest: PASS (vmmcalls=2, final rip=...)`. Full boot log:
`docs/archive/AMD64_KVMTEST_METAL_DMESG_2026_10_21.txt` (SVM lines at 73-76).

Four things were wrong between your build and that line. Each was visible only
from an exit code or a breadcrumb, and each is in the `AKUMA_KVM.md` timeline:

1. `F3 0F 01 D9` is `VMGEXIT`; `VMMCALL` is `0F 01 D9` (length 3).
2. The VMCB segment limit is the byte-scaled 32-bit value: `0xFFFFF` is 1 MiB.
3. NPT entries need U=1 (nested walks are user-mode accesses): NPF `0x400`
   with `info1` P=1, ID=1 is the signature.
4. **`#VMEXIT` leaves GIF clear.** Run `stgi` straight after `vmrun`, or the
   host gets no interrupts and the watchdog resets the box ~1 min later.

How it was found, so you can repeat it: rehearse in QEMU/KVM on ryzen first
(`overlays/ryzen/qemu.sh 27`, nested SVM is on), and on metal drop a breadcrumb
byte in CMOS 0x7F (`crumb()` in `kvm.rs`) — it survives the reset; read it from
Pop with `/dev/port` (index 0x70, data 0x71). Entry 27 in
`overlays/ryzen/grub.cfg` is the `kvmtest` boot. Note the box stays on that
kernel until Pop re-arms it; `dmesg` loses the boot to `[rtw]` spam within
minutes, so read it first.

Next rung: per `AKUMA_KVM.md`, enable SVM on the APs, then MSRPM/IOPM. Keep the
spec-first, host-tested, honest-about-what-is-unproven habit.
