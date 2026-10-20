# Note for crush: the kernel is in, keep going (2026-10-20)

Crush — good work. Your last session ended with the reboot, and it worked:
the ryzen box (192.168.1.159:2222, DHCP moved it off .126) came back up on the
kernel you built and installed (`/boot/akuma-amd64`, md5 `eb33a116…`, installed
21:06, `uname` says `9e714bd6-release-smp-shared`). Branch `kvm-part-0`
(`3ad797e6`) has `amd64/src/kvm.rs`, `akuma-svm-amd64` and your
`docs/archive/AKUMA_KVM.md` timeline. Your read of the install script, the
`.prev` fallbacks and the unsafe-budget bump was all correct.

## What is true right now

- The new kernel is **running**. The SVM probe runs every boot.
- `kvmtest` is **not** on the kernel line. You were right that grub.cfg is out
  of reach from inside Akuma (it is on the ESP / Pop's ext4; this kernel has
  neither vfat nor ext4). Nobody has edited it. Getting `kvmtest` in needs
  either `e` on the `Akuma/amd64` entry at the GRUB menu (10 s timeout; append
  ` kvmtest`, Ctrl-X) or an edit from Pop. Both need the user at the machine.
- `dmesg` cannot show the boot probe now: the ring holds only ~511 lines and
  `[rtw]` register-diff spam has wrapped it. The dump is
  `docs/archive/AMD64_KVMTEST_BOOT_DMESG_2026_10_20.txt` — it contains no
  `svm`/`kvm` line, and that is the ring wrapping, **not** evidence the probe
  failed. Don't read it as a result either way.

## Next, when the user resumes you

1. Ask for the one-time GRUB edit above (or a Pop-side edit) to add `kvmtest`.
2. After that boot, read `dmesg` **immediately**, before rtw fills the ring.
3. Record the outcome in the `AKUMA_KVM.md` timeline row that says "reboot
   pending". Only call the trivial guest working with the exit reason in hand.

Keep up the good work: spec-first, host-tested, and honest about what is
unproven. That is why this got as far as it did.
