# `netwatch` (herd service, menu entries 12-14): reboot to Pop when the network
# has been lost for LOSS seconds, after saving the kernel log. A box that sits
# in a dark corner of wifi is no use to anyone, and Pop's wifi still works, so
# the way out is a reset (`reboot -f`: the systemd-boot one-shot that brought
# Akuma up is consumed, so this lands in Pop, from where `arm.sh` goes again).
#
# "Network" is a DNS lookup answered (`nslookup example.com`, the resolver
# DHCP gave): it needs the association, the keys, an uplink and a downlink, and
# is exactly what ssh needs. Watching starts once it has worked, or after
# NEVER seconds without ever working (a boot that never got on the air).
LOSS=120
NEVER=300
STEP=10
D=/var/log/ryzen
L=$D/netwatch.log
/bin/busybox mkdir -p $D
/bin/busybox sleep 20
N=$(/bin/busybox cat $D/count 2>/dev/null)
probe() {
    /bin/busybox timeout 8 /bin/busybox nslookup example.com > /dev/null 2>&1
}
say() { echo "boot $N t=$t $*" >> $L; }
t=20
up=0
down=0
say "watching: loss ${LOSS}s, never ${NEVER}s"
while true; do
    if probe; then
        [ $up -eq 0 ] && say "network up"
        [ $down -gt 0 ] && say "network back after ${down}s"
        up=1
        down=0
    else
        [ $down -eq 0 ] && say "probe failed (up=$up)"
        down=$((down + STEP))
        if { [ $up -eq 1 ] && [ $down -ge $LOSS ]; } || { [ $up -eq 0 ] && [ $t -ge $NEVER ]; }; then
            say "network lost for ${down}s: saving the log and rebooting to Pop"
            /bin/busybox dmesg > $D/netwatch-$N.dmesg
            /bin/busybox cat /dev/wifi0 2>/dev/null | /bin/busybox grep -E '^(radio|state|chan|signal|security|error)=' | /bin/busybox tr '\n' ' ' >> $L
            echo >> $L
            /bin/busybox sync
            /bin/busybox reboot -f
        fi
    fi
    /bin/busybox sleep $STEP
    t=$((t + STEP))
done
