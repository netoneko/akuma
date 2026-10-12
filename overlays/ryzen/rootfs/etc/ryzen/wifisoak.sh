# `wifisoak` (herd service, menu entry 15): `wifi auto` against the kernel's
# LOSSY simulated radio (`wifisim=lossy`: scans that miss networks, joins
# that take seconds or time out, a link that drops and comes back) for five
# minutes, the link's state sampled every 10 s, then reboot. The transcript
# is /var/log/ryzen/wifisoak-N.txt; the verdict is how much of the sample
# column says `connected`, and that `wifi auto` never died.
#
# The passphrase below is the SIMULATOR's — public, fake, in akuma-wifi's
# sim.rs. A real network's never appears in this repo.
D=/var/log/ryzen
/bin/busybox mkdir -p $D
N=$(/bin/busybox cat $D/count 2>/dev/null)
N=$((${N:-0} + 1))
echo $N > $D/count
L=$D/wifisoak-$N.txt
echo "wifisoak boot $N" > $L
/bin/busybox ls -l /dev/wifi0 >> $L 2>&1
echo akuma-sim-passphrase > /tmp/simpass
/bin/wifi add sim-wpa2 akuma-sim-wpa2 < /tmp/simpass >> $L 2>&1
/bin/busybox rm -f /tmp/simpass
/bin/wifi auto >> $D/wifiauto-$N.txt 2>&1 &
AUTO=$!
i=0
while [ $i -lt 30 ]; do
    /bin/busybox sleep 10
    i=$((i + 1))
    echo "## sample $i, auto pid $AUTO $(/bin/busybox kill -0 $AUTO 2>/dev/null && echo alive || echo DEAD)" >> $L
    /bin/wifi status >> $L 2>&1
done
echo "## wifi auto said:" >> $L
/bin/busybox cat $D/wifiauto-$N.txt >> $L
echo "WIFISOAK DONE" >> $L
/bin/busybox dmesg > $D/boot-$N.dmesg
/bin/busybox sync
/bin/busybox reboot -f
