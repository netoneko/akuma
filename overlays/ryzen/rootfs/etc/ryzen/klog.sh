# `klog` (herd service, menu entry 12): the kernel log to p3 every 5 s, for a
# box that stays up (no autoreboot to save it). Written to a temporary file and
# renamed, so a hang mid-write leaves the previous complete copy.
D=/var/log/ryzen
/bin/busybox mkdir -p $D
N=$(/bin/busybox cat $D/count 2>/dev/null)
N=$((${N:-0} + 1))
echo $N > $D/count
while true; do
    /bin/busybox dmesg > $D/klog-$N.tmp
    /bin/busybox mv $D/klog-$N.tmp $D/klog-$N.dmesg
    /bin/busybox sync
    /bin/busybox sleep 5
done
