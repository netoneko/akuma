# Run by herd's `autoreboot` service (sh is exec'd explicitly: execve here does
# not understand `#!`). Gives the boot time to settle, says so on every output
# the machine has, and resets. The firmware then boots Pop: the systemd-boot
# one-shot that brought us here is already consumed.
#
# Pop coming back ~DELAY s after the reboot is the signal that Akuma reached
# userspace on this hardware. Once there is a log sink, dump dmesg to it here.
DELAY=90
echo "[ryzen] autoreboot: up; rebooting to Pop in ${DELAY}s"
/bin/busybox sleep $DELAY
echo "[ryzen] autoreboot: rebooting now"
/bin/busybox sync
/bin/busybox reboot -f
