#!/bin/sh
# Wifi stage W0: a register-level trace of Linux's rtw89 bringing up the
# RTL8852CE: power-on, efuse, firmware download, `fw ready`, one scan. Run as
# root on ryzen. docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md § 5.
#
#   sh w0-trace.sh            # detaches itself (systemd-run), returns at once
#   sh w0-trace.sh --check    # preflight only: changes nothing
#
# From the laptop (hpbox.py's root helper; the run itself survives the drop):
#   python3 -c 'import sys; sys.path.insert(0,"scripts/utils"); import hpbox;
#     print(hpbox.ryzen_root("cat > /root/w0-trace.sh && sh /root/w0-trace.sh",
#                            input=open("overlays/ryzen/w0-trace.sh").read()))'
#
# Rebinding the card takes down ryzen's only network, which takes down the ssh
# session that started this. So the script detaches into its own systemd unit
# (`akuma-w0`), and every exit path, including an error, puts the network back
# (the EXIT trap). Wifi is gone for about a minute.
#
# What changes on the box while it runs, and how each change is undone:
#   - NetworkManager is told to leave the wifi interface alone, through a
#     runtime drop-in under /run (tmpfs, so a reboot clears it too). Without
#     that, NM would associate during the trace, and with WPA2 the trace
#     would hold that session's keys. Undone: the drop-in is removed and NM
#     reloaded.
#   - mmiotrace takes **every CPU but one offline** while it is the current
#     tracer (the kernel's own requirement). The llama services slow to a crawl
#     for that window. Undone: `nop` tracer, CPUs come back.
#   - tracefs buffer_size_kb is raised. Undone: the old value is written back.
#   - the card is unbound and rebound. If the bind fails, the fallback is a
#     module reload, then a NetworkManager restart. The script never reboots.
#
# Output: /var/tmp/akuma-w0/<stamp>/ (root, 0700), see the `files` list at the
# end of the run in `log`. The trace has no SSIDs and no keys (association never
# happens while tracing; frames arrive by DMA, which mmiotrace cannot see), but
# it holds the card's efuse, so the MAC address. Fetch from the laptop:
#   python3 -c 'import sys,subprocess; sys.path.insert(0,"scripts/utils"); import hpbox;
#     sys.stdout.buffer.write(subprocess.run(hpbox.RZ_ROOT + ["tar -C /var/tmp/akuma-w0 -cf - <stamp>"],
#     capture_output=True).stdout)' | tar -C <dir> -xf -
# (not `hpbox.py rzr`: its CLI decodes output as text, which mangles a tar.)
set -u
OUT_ROOT=/var/tmp/akuma-w0
T=/sys/kernel/tracing
NM_DROPIN=/run/NetworkManager/conf.d/90-akuma-w0.conf
TRACE_KB=32768          # per CPU, allocated for every possible CPU (16 here)
PCI_ID=10ec:c852

die() { echo "w0: $*" >&2; exit 1; }

find_dev() {
    for d in /sys/bus/pci/devices/*; do
        [ "$(cat $d/vendor 2>/dev/null):$(cat $d/device 2>/dev/null)" = "0x${PCI_ID%%:*}:0x${PCI_ID##*:}" ] && { echo $d; return; }
    done
}

preflight() {
    [ "$(id -u)" = 0 ] || die "run as root"
    DEV=$(find_dev); [ -n "$DEV" ] || die "no $PCI_ID on the PCI bus"
    BDF=${DEV##*/}
    [ -L $DEV/driver ] || die "$BDF has no driver bound; nothing to trace a rebind of"
    DRV=$(basename "$(readlink $DEV/driver)")
    IF=$(ls $DEV/net 2>/dev/null | head -n1); [ -n "$IF" ] || die "$BDF has no network interface"
    [ -d $T/events ] || mount -t tracefs nodev $T 2>/dev/null
    grep -qw mmiotrace $T/available_tracers 2>/dev/null || die "kernel has no mmiotrace (CONFIG_MMIOTRACE); nothing changed"
    [ "$(cat $T/current_tracer)" = nop ] || die "tracer '$(cat $T/current_tracer)' is in use; nothing changed"
    command -v nmcli >/dev/null || die "no nmcli"
    echo "w0: $BDF driver=$DRV if=$IF tracer ok, cpus=$(nproc)"
}

if [ "${1:-}" = --check ]; then preflight; exit 0; fi
if [ "${1:-}" != --run ]; then
    preflight
    systemctl reset-failed akuma-w0 2>/dev/null
    systemd-run --unit=akuma-w0 --collect --property=Type=exec /bin/sh "$(readlink -f "$0")" --run \
        || die "systemd-run failed"
    echo "w0: running as unit akuma-w0; wifi drops now, back in ~1-2 min."
    echo "w0: then: journalctl -u akuma-w0; ls $OUT_ROOT"
    exit 0
fi

# ---- detached from here on ----
preflight
O=$OUT_ROOT/$(date +%Y%m%d-%H%M%S)
mkdir -p $O && chmod 700 $OUT_ROOT $O
exec >>$O/log 2>&1
log() { echo "$(date +%T.%N | cut -c1-12) $*"; }
mark() { echo "w0: $*" > $T/trace_marker 2>/dev/null; log "mark: $*"; }
# The size to restore. An unexpanded buffer reads back as `7 (expanded: 1408)`,
# which cannot be written back, so take the expanded figure; and keep the first
# value ever seen, so a run that failed to restore cannot make its own size the
# "original" of the next run.
[ -f $OUT_ROOT/buffer_size_kb.orig ] || sed 's/.*expanded: \([0-9]*\).*/\1/' $T/buffer_size_kb > $OUT_ROOT/buffer_size_kb.orig
OLD_KB=$(cat $OUT_ROOT/buffer_size_kb.orig)
PIPE_PID=
CLEANED=

cleanup() {
    [ -n "$CLEANED" ] && return; CLEANED=1
    log "cleanup"
    # Stop recording, let the reader drain, and only then switch tracers:
    # changing the tracer resets the buffer, unread events with it.
    echo 0 > $T/tracing_on 2>/dev/null
    sleep 2
    [ -n "$PIPE_PID" ] && kill $PIPE_PID 2>/dev/null
    echo nop > $T/current_tracer 2>/dev/null
    echo 1 > $T/tracing_on 2>/dev/null
    echo "$OLD_KB" > $T/buffer_size_kb 2>/dev/null
    log "tracer=$(cat $T/current_tracer) buffer_size_kb=$(cat $T/buffer_size_kb) cpus=$(nproc)"
    if [ ! -L $DEV/driver ]; then
        log "card unbound; bind again"
        echo $BDF > /sys/bus/pci/drivers/$DRV/bind 2>/dev/null; sleep 5
    fi
    if [ ! -L $DEV/driver ]; then
        log "bind failed; reload $DRV"
        modprobe -r $DRV; sleep 2; modprobe $DRV; sleep 5
    fi
    rm -f $NM_DROPIN
    nmcli general reload conf
    i=0
    until [ "$(nmcli -t -f STATE general 2>/dev/null)" = connected ] || [ $i -ge 90 ]; do sleep 1; i=$((i+1)); done
    if [ "$(nmcli -t -f STATE general)" != connected ]; then
        log "no connection after 90 s; restart NetworkManager"
        systemctl restart NetworkManager
        i=0
        until [ "$(nmcli -t -f STATE general 2>/dev/null)" = connected ] || [ $i -ge 90 ]; do sleep 1; i=$((i+1)); done
    fi
    log "network: $(nmcli -t -f STATE general) driver=$(basename "$(readlink $DEV/driver 2>/dev/null)" 2>/dev/null)"
    [ -f $O/mmiotrace.txt ] && gzip -9 $O/mmiotrace.txt
    dmesg > $O/dmesg-after.txt
    log "files: $(cd $O && ls | tr '\n' ' ')"
    log "done"
    # Readable by whoever owns this script (netoneko, via hpbox.py's `rz`).
    chown -R "$(stat -c %U "$0")" $OUT_ROOT
}
trap cleanup EXIT
trap 'exit 1' INT TERM HUP

# 1. The quiet part: identity, nothing disturbed yet.
log "start $BDF $DRV $IF"
{ uname -a; cat /proc/cmdline; for m in rtw89_8852ce rtw89_8852c rtw89_pci rtw89_core; do modinfo $m 2>/dev/null | grep -E '^(filename|version|srcversion|vermagic|firmware):'; done; } > $O/kernel.txt
lspci -vvv -nn -s $BDF > $O/lspci.txt 2>&1
lspci -xxxx -s $BDF > $O/pci-config.txt 2>&1
cat $DEV/resource > $O/resource.txt
dmesg > $O/dmesg-before.txt
grep -i rtw89 $O/dmesg-before.txt | grep -iE 'firmware|fw' | tail -n5 > $O/fw-loaded.txt
sha256sum /lib/firmware/rtw89/rtw8852c_fw* > $O/fw-files.txt 2>&1
ls -la /sys/kernel/debug/ieee80211/*/rtw89/ > $O/debugfs-list.txt 2>&1

# 2. NetworkManager lets go of the interface: the wifi drops here.
mkdir -p ${NM_DROPIN%/*}
# By MAC, not name: the rebound card comes back as wlan0 and is renamed a
# moment later, and a name match would not cover that window.
printf '[keyfile]\nunmanaged-devices=mac:%s\n' "$(cat /sys/class/net/$IF/address)" > $NM_DROPIN
nmcli general reload conf
sleep 3
log "nm: $(nmcli -t -f DEVICE,STATE device | grep "^$IF:")"

# 3. Trace on. The reader must be running before the tracer starts.
echo $TRACE_KB > $T/buffer_size_kb || exit 1
cat $T/trace_pipe > $O/mmiotrace.txt &
PIPE_PID=$!
echo mmiotrace > $T/current_tracer || exit 1
log "mmiotrace on, cpus=$(nproc)"

# 4. Unbind, bind: probe (power on, efuse, power off) under the trace.
mark unbind
echo $BDF > /sys/bus/pci/drivers/$DRV/unbind
sleep 2
mark bind
echo $BDF > /sys/bus/pci/drivers/$DRV/bind || { log "bind refused"; exit 1; }
i=0; NEWIF=
while [ $i -lt 20 ]; do NEWIF=$(ls $DEV/net 2>/dev/null | head -n1); [ -n "$NEWIF" ] && break; sleep 1; i=$((i+1)); done
[ -n "$NEWIF" ] || { log "no interface 20 s after bind"; exit 1; }
# udev renames wlan0 -> wlp2s0 just after bind; run 1 (2026-10-06) lost the
# race and `ip link set wlan0 up` found nothing, so the firmware download was
# never traced. Settle, then take the name again.
udevadm settle --timeout=10
sleep 1
NEWIF=$(ls $DEV/net 2>/dev/null | head -n1)
log "interface: $NEWIF"
sleep 1

# 5. Interface up: power on again, MAC init, firmware download, fw ready.
mark up
ip link set dev $NEWIF up || log "link up failed"
sleep 5

# 6. One scan: channel switching, the RX path. Only the count is kept, since
# the results are the neighbours' (and our own) network names.
# Pop ships no `iw` (checked 2026-10-06); without it nothing can ask for a
# scan, since NetworkManager is kept off the card, so the phase is skipped.
if command -v iw >/dev/null; then
    mark scan
    n=$(timeout 20 iw dev $NEWIF scan 2>&1 | grep -c '^BSS')
    log "scan: $n BSS"
    sleep 1
else
    log "scan: skipped, no iw"
fi

# 7. Down, then trace off (cleanup), then NetworkManager takes it back.
mark down
ip link set dev $NEWIF down
sleep 2
mark end
sleep 1
exit 0
