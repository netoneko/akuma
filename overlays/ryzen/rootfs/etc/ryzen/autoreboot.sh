# Run by herd's `autoreboot` service (sh is exec'd explicitly: execve here does
# not understand `#!`). Saves the kernel log to the root filesystem twice — a
# snapshot early, the full log just before the reset — and resets. The firmware
# then boots Pop: the systemd-boot one-shot that brought us here is consumed.
#
# On an NVMe root (`root=/dev/nvme0n1p3`) the logs survive and Pop reads them
# from /var/log/ryzen on that partition (overlays/ryzen/README.md).
#
# When the NVMe root did NOT come up the logs go to the RAM root and are lost,
# so the delay itself carries the answer: how far NVMe bring-up got. Pop reads
# it back as the time spent outside Linux (`journalctl --list-boots`: previous
# boot's end to this boot's start), which is ~35 s of firmware + boot + DELAY.
# Stages, furthest first, 40 s apart so boot-time variance cannot blur them:
#
#   ext2 mounted on /dev/nvme0n1p3   -> 140 s  (logs are on p3: read them)
#   nvme: p3 = ...  (GPT read, mount refused) -> 100 s
#   nvme: ns1 ...   (Identify worked)         ->  60 s
#   anything else   (no controller / takeover failed) -> 20 s
D=/var/log/ryzen
/bin/busybox mkdir -p $D
N=$(/bin/busybox cat $D/count 2>/dev/null)
N=$((${N:-0} + 1))
echo $N > $D/count
/bin/busybox sleep 10
/bin/busybox dmesg > $D/boot-$N.early
if /bin/busybox grep -q "ext2 mounted on /dev/nvme0n1p" $D/boot-$N.early; then
    DELAY=140
elif /bin/busybox grep -q "nvme: p[0-9]* = LBA" $D/boot-$N.early; then
    DELAY=100
elif /bin/busybox grep -q "nvme: ns1 " $D/boot-$N.early; then
    DELAY=60
else
    DELAY=20
fi
echo "[ryzen] autoreboot: boot $N, nvme stage delay ${DELAY}s; rebooting to Pop then"
/bin/busybox sync
/bin/busybox sleep $((DELAY - 10))
/bin/busybox dmesg > $D/boot-$N.dmesg
/bin/busybox sync
/bin/busybox reboot -f
