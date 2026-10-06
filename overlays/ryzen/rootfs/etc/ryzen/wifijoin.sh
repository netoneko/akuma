# `wifijoin` (herd service, menu entry 10): join a known network (/etc/wifi on
# p3) through ryzen's own radio (`rtw89wifi`) once, keep the kernel log, and
# reboot. Wifi stages W3/W4 (docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md § 5).
#
# Nothing written here names a network: the transcript keeps the tool's exit
# status and only /dev/wifi0's non-identifying keys (never ssid or bssid), and
# the kernel's `[rtw]` lines carry SSIDs as hashes and BSSIDs as their OUI.
# The tool's own output (which prints the network's name) goes to /dev/null.
D=/var/log/ryzen
/bin/busybox mkdir -p $D
N=$(/bin/busybox cat $D/count 2>/dev/null)
N=$((${N:-0} + 1))
echo $N > $D/count
L=$D/wifijoin-$N.txt
keys() {
    /bin/busybox grep -E '^(radio|state|chan|security|error|scans)=' /dev/wifi0 | /bin/busybox tr '\n' ' ' >> $L
    echo >> $L
}
echo "wifijoin boot $N" > $L
/bin/busybox sleep 2
keys
# `connect` with no name: scan, pick the best known network, join it (the tool
# gives the driver 20 s).
/bin/wifi connect > /dev/null 2>&1
echo "wifi connect exit $?" >> $L
keys
# Still joined a while later? A deauthentication shows in the [rtw] lines.
/bin/busybox sleep 20
keys
/bin/busybox dmesg > $D/boot-$N.dmesg
/bin/busybox sync
/bin/busybox reboot -f
