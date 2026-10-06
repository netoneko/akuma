# `wifitest` (herd service, menu entry 7): exercise the wifi tool against the
# kernel's simulated radio (`wifisim`) end to end, save the transcript to
# /var/log/ryzen/wifitest-N.txt, and reboot. Every step's exit status is
# recorded; the transcript is the verdict.
#
# The passphrase below is the SIMULATOR's — public, fake, in akuma-wifi's
# sim.rs. A real network's never appears in this repo.
D=/var/log/ryzen
/bin/busybox mkdir -p $D
N=$(/bin/busybox cat $D/count 2>/dev/null)
N=$((${N:-0} + 1))
echo $N > $D/count
L=$D/wifitest-$N.txt
step() {
    echo "## $*" >> $L
    "$@" >> $L 2>&1
    echo "## exit $?" >> $L
}
echo "wifitest boot $N" > $L
step /bin/busybox ls -l /dev/wifi0
step /bin/wifi status
step /bin/wifi scan
step /bin/wifi add-open sim-open akuma-sim-open
echo akuma-sim-passphrase > /tmp/simpass
step /bin/wifi add sim-wpa2 akuma-sim-wpa2 < /tmp/simpass
echo not-the-passphrase > /tmp/badpass
step /bin/wifi add sim-wrong akuma-sim-wpa2 < /tmp/badpass
/bin/busybox rm -f /tmp/simpass /tmp/badpass
step /bin/busybox ls -l /etc/wifi
step /bin/wifi list
step /bin/wifi connect sim-wrong
step /bin/wifi connect sim-wpa2
step /bin/wifi status
step /bin/busybox cat /dev/wifi0
step /bin/wifi disconnect
step /bin/wifi connect
step /bin/wifi forget sim-wrong
step /bin/wifi list
echo "WIFITEST DONE" >> $L
/bin/busybox dmesg > $D/boot-$N.dmesg
/bin/busybox sync
/bin/busybox reboot -f
