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
#
# Patience depends on what the radio is doing (`state=` in /dev/wifi0): while
# the kernel is still scanning or associating it is retrying every second and
# will rejoin the moment the access point is back, so a router restart or a
# walk through a dead spot must not cost the session. Reset to Pop only after
# RETRY_LOSS of that; a link that is "connected" yet answers nothing, or a
# radio that stopped trying (failed/down), is the wedge the reset is for, and
# keeps the short LOSS (measured 2026-10-08: a router restart rebooted a
# station that was scanning fine, at 120 s).
LOSS=120
NEVER=300
RETRY_LOSS=1200
RETRY_NEVER=900
STEP=10
D=/var/log/ryzen
L=$D/netwatch.log
/bin/busybox mkdir -p $D
/bin/busybox sleep 20
N=$(/bin/busybox cat $D/count 2>/dev/null)
wstate() {
    /bin/busybox cat /dev/wifi0 2>/dev/null | /bin/busybox grep '^state=' | /bin/busybox cut -d= -f2
}
probe() {
    /bin/busybox timeout 8 /bin/busybox nslookup example.com > /dev/null 2>&1
}
say() { echo "boot $N t=$t $*" >> $L; }
t=20
up=0
down=0
say "watching: loss ${LOSS}s (${RETRY_LOSS}s while the radio retries), never ${NEVER}s (${RETRY_NEVER}s)"
while true; do
    if probe; then
        [ $up -eq 0 ] && say "network up"
        [ $down -gt 0 ] && say "network back after ${down}s"
        up=1
        down=0
    else
        [ $down -eq 0 ] && say "probe failed (up=$up)"
        down=$((down + STEP))
        loss=$LOSS
        never=$NEVER
        case "$(wstate)" in
            scanning|associating) loss=$RETRY_LOSS; never=$RETRY_NEVER ;;
        esac
        if { [ $up -eq 1 ] && [ $down -ge $loss ]; } || { [ $up -eq 0 ] && [ $t -ge $never ]; }; then
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
