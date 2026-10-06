# `wifistay` (herd service, menu entry 11): join a known network (/etc/wifi on
# p3) through ryzen's own radio (`rtw89wifi`) and stay up for STAY seconds so
# sshd (the other service the entry runs) can be reached over wifi; then keep
# the kernel log and reboot to Pop. The log is saved every 30 s, so a wedge
# still leaves the last half minute on p3.
#
# Like `wifijoin`, nothing written here names a network: only /dev/wifi0's
# non-identifying keys, the tool's exit status, and the address DHCP gave.
STAY=600
D=/var/log/ryzen
/bin/busybox mkdir -p $D
N=$(/bin/busybox cat $D/count 2>/dev/null)
N=$((${N:-0} + 1))
echo $N > $D/count
L=$D/wifistay-$N.txt
keys() {
    /bin/busybox grep -E '^(radio|state|chan|signal|security|error)=' /dev/wifi0 | /bin/busybox tr '\n' ' ' >> $L
    echo >> $L
}
echo "wifistay boot $N" > $L
# No radio (the QEMU rehearsal): nothing to stay up for.
if [ ! -e /dev/wifi0 ]; then
    echo "no /dev/wifi0" >> $L
    /bin/busybox dmesg > $D/boot-$N.dmesg
    /bin/busybox sync
    /bin/busybox reboot -f
fi
/bin/busybox sleep 2
/bin/wifi connect > /dev/null 2>&1
echo "wifi connect exit $?" >> $L
i=0
while [ $i -lt $STAY ]; do
    /bin/busybox sleep 30
    i=$((i + 30))
    echo "t=$i" >> $L
    keys
    /bin/busybox ifconfig 2>/dev/null | /bin/busybox grep -E 'inet addr' >> $L
    /bin/busybox dmesg > $D/boot-$N.dmesg
    /bin/busybox sync
done
/bin/busybox dmesg > $D/boot-$N.dmesg
/bin/busybox sync
/bin/busybox reboot -f
