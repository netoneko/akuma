#!/bin/sh
# Wifi stage W0: a register-level trace of Linux's rtw89 bringing up the
# RTL8852CE: power-on, efuse, firmware download, `fw ready`, one scan. Run as
# root on ryzen. docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md § 5.
#
#   sh w0-trace.sh            # detaches itself (systemd-run), returns at once
#   sh w0-trace.sh --check    # preflight only: changes nothing
#   CHAN=1 sh w0-trace.sh     # channel switching: monitor mode, `iw set channel`
#                             # over CHANS (default 1..13), a mark per channel
#   JOIN=1 sh w0-trace.sh     # stage W3: instead of scan + down, let
#                             # NetworkManager join its network while tracing
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
    systemd-run --unit=akuma-w0 --collect --property=Type=exec \
        --setenv=JOIN="${JOIN:-0}" --setenv=CHAN="${CHAN:-0}" --setenv=CHANS="${CHANS:-1 2 3 4 5 6 7 8 9 10 11 12 13}" --setenv=H2C="${H2C:-1}" /bin/sh "$(readlink -f "$0")" --run \
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
mark() {
    echo "w0: $*" > $T/trace_marker 2>/dev/null
    [ -d "$T/instances/akuma" ] && echo "w0: $*" > $T/instances/akuma/trace_marker 2>/dev/null
    log "mark: $*"
}
# The size to restore. An unexpanded buffer reads back as `7 (expanded: 1408)`,
# which cannot be written back, so take the expanded figure; and keep the first
# value ever seen, so a run that failed to restore cannot make its own size the
# "original" of the next run.
[ -f $OUT_ROOT/buffer_size_kb.orig ] || sed 's/.*expanded: \([0-9]*\).*/\1/' $T/buffer_size_kb > $OUT_ROOT/buffer_size_kb.orig
OLD_KB=$(cat $OUT_ROOT/buffer_size_kb.orig)
PIPE_PID=
CLEANED=

# Before anything leaves this box: zero the body of every security-CAM H2C
# (CAT_MAC, class SEC_CAM: the only command that carries a key — the pairwise
# and group keys of a JOIN run), and every occurrence of the joined network's
# name (probe-request templates in scan-offload commands). The name is read
# from NetworkManager here and never written anywhere. A run whose redaction
# fails deletes its H2C dump rather than keep it.
redact() {
    # Every saved wifi profile's SSID, hex, one per argument: not just the
    # active one (2026-10-06's JOIN run looked up the active connection, got
    # nothing, and left the name in 4 commands; redacted by hand afterwards).
    NAMES=""
    for c in $(nmcli -t -f UUID,TYPE connection show | grep ':802-11-wireless$' | cut -d: -f1); do
        h=$(nmcli -t -f 802-11-wireless.ssid connection show "$c" 2>/dev/null | cut -d: -f2- | tr -d '\n' | od -An -tx1 | tr -d ' \n')
        [ -n "$h" ] && NAMES="$NAMES $h"
    done
    log "redact: $(echo $NAMES | wc -w) saved network names"
    if [ "${JOIN:-0}" = 1 ] && [ -z "$NAMES" ]; then rm -f "$1"; log "no names to redact on a JOIN run: h2c dump deleted"; return 1; fi
    python3 - "$1" $NAMES <<'PY' || { rm -f "$1"; log "redaction failed: h2c dump deleted"; return 1; }
import re, sys
path, ssids = sys.argv[1], [bytes.fromhex(h) for h in sys.argv[2:] if len(h) >= 4]
out, keys, names = [], 0, 0
for line in open(path, errors="replace"):
    m = re.search(r"(h2c|c2h|txh|txm|rxm): ", line)
    if not m:
        out.append(line); continue
    words = []
    for arr in re.findall(r"d\d=\{([^}]*)\}", line):
        words += [int(w, 16) for w in arr.split(",")]
    data = bytearray(b"".join(w.to_bytes(8, "little") for w in words))
    if m.group(1) == "h2c" and len(data) >= 8:
        h0 = int.from_bytes(data[:4], "little")
        if h0 & 3 == 1 and (h0 >> 2) & 0x3f == 0xa:
            data[8:] = bytes(len(data) - 8); keys += 1
    for ssid in ssids:
        i = data.find(ssid)
        while i >= 0:
            data[i:i + len(ssid)] = bytes(len(ssid)); names += 1
            i = data.find(ssid, i + 1)
    it = iter(range(0, len(data), 8))
    def sub(mm):
        n = len(mm.group(2).split(","))
        ws = [hex(int.from_bytes(data[next(it):][:8], "little")) for _ in range(n)]
        return mm.group(1) + "{" + ",".join(ws) + "}"
    out.append(re.sub(r"(d\d=)\{([^}]*)\}", sub, line))
open(path, "w").writelines(out)
print(f"redacted {keys} sec-cam commands, {names} name occurrences")
PY
}

cleanup() {
    [ -n "$CLEANED" ] && return; CLEANED=1
    log "cleanup"
    # Stop recording, let the reader drain, and only then switch tracers:
    # changing the tracer resets the buffer, unread events with it.
    echo 0 > $T/tracing_on 2>/dev/null
    sleep 2
    [ -n "$PIPE_PID" ] && { kill $PIPE_PID 2>/dev/null; wait $PIPE_PID 2>/dev/null; }
    # The tracer cannot change while its trace_pipe is open (EBUSY), and a
    # killed `cat` may not have closed it yet: 2026-10-06 run 3 left
    # mmiotrace on, and 15 CPUs offline, that way. Retry until it takes.
    i=0
    until echo nop > $T/current_tracer 2>/dev/null || [ $i -ge 20 ]; do sleep 1; i=$((i+1)); done
    echo 0 > $I/events/akuma/enable 2>/dev/null
    sleep 1
    [ -n "$H2C_PID" ] && kill $H2C_PID 2>/dev/null
    rmdir $I 2>/dev/null
    echo "-:akuma/h2c" >> $T/dynamic_events 2>/dev/null
    echo "-:akuma/c2h" >> $T/dynamic_events 2>/dev/null
    echo "-:akuma/txd" >> $T/dynamic_events 2>/dev/null
    echo "-:akuma/txh" >> $T/dynamic_events 2>/dev/null
    echo "-:akuma/txm" >> $T/dynamic_events 2>/dev/null
    echo "-:akuma/rxm" >> $T/dynamic_events 2>/dev/null
    for w in 8 16 32; do
        for e in r$w r${w}v w$w; do echo "-:akuma/$e" >> $T/dynamic_events 2>/dev/null; done
    done
    [ -f $O/h2c.txt ] && redact $O/h2c.txt && gzip -9 $O/h2c.txt
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
# Stage W2: the firmware commands (H2C) and events (C2H) travel by DMA, where
# mmiotrace cannot see them. Two fprobe events dump their bytes into a trace
# instance of their own (`instances/akuma` -> h2c.txt; the mmiotrace tracer's
# own pipe drops foreign events, found 2026-10-06), timestamped on the same
# clock as the register accesses. H2C: the first 2 KiB (an x64[64] array is
# the largest one argument may be); C2H: 512 bytes. Without JOIN=1 nothing
# here is a key: NetworkManager is kept off the card, so no association
# happens and no security CAM entry is ever written. With it, see `redact`.
I=$T/instances/akuma
H2C_PID=
if [ "${H2C:-1}" = 1 ]; then
    A=""; for i in 0 1 2 3; do A="$A d$i=+$((i*512))(skb->data):x64[64]"; done
    mkdir -p $I \
        && echo 32768 > $I/buffer_size_kb \
        && echo "f:akuma/h2c rtw89_h2c_tx len=skb->len$A" >> $T/dynamic_events \
        && echo "f:akuma/c2h rtw89_fw_c2h_irqsafe len=c2h->len d0=+0(c2h->data):x64[64]" >> $T/dynamic_events \
        && echo "f:akuma/txd rtw89_core_fill_txdesc_v1 d0=+0(desc_info):x64[8]" >> $T/dynamic_events \
        && echo "f:akuma/txh rtw89_core_tx_write len=skb->len d0=+0(skb->data):x64[3]" >> $T/dynamic_events \
        && echo "f:akuma/txm rtw89_core_tx_write len=skb->len fc=+0(skb->data):u8 d0=+0(skb->data):x64[40]" >> $T/dynamic_events \
        && echo "f:akuma/rxm rtw89_core_rx len=skb->len fc=+0(skb->data):u8 d0=+0(skb->data):x64[64]" >> $T/dynamic_events \
        && echo "fc == 0 || fc == 32 || fc == 64 || fc == 176 || fc == 208" > $I/events/akuma/txm/filter \
        && echo "fc == 16 || fc == 48 || fc == 176 || fc == 208 || fc == 160 || fc == 192" > $I/events/akuma/rxm/filter \
        && { cat $I/trace_pipe > $O/h2c.txt & H2C_PID=$!; } \
        && echo 1 > $I/events/akuma/enable && log "h2c/c2h probes on" || log "h2c/c2h probes FAILED"
fi
# MMIO=fprobe (the default with JOIN=1): no mmiotrace. Register accesses are
# fprobe events on the PCI layer's six accessors (`rtw89_pci_ops_{read,write}
# {8,16,32}`; every rtw89 MMIO goes through one, by function pointer, so none
# is inlined), a read's value from a second event on its return. All CPUs stay
# online and accesses cost ~1 µs instead of a page fault each: under mmiotrace
# every hardware scan timed out (`rtw89_hw_scan_offload failed ret -110`), so
# NetworkManager never found the network and the JOIN run of 2026-10-06
# recorded four failed scans and no association.
if [ "${MMIO:-$([ "${JOIN:-0}" = 1 ] || [ "${CHAN:-0}" = 1 ] && echo fprobe || echo mmiotrace)}" = fprobe ]; then
    ok=1
    for w in 8 16 32; do
        echo "f:akuma/r$w rtw89_pci_ops_read$w a=addr" >> $T/dynamic_events || ok=0
        echo "f:akuma/r${w}v rtw89_pci_ops_read$w%return v=\$retval" >> $T/dynamic_events || ok=0
        echo "f:akuma/w$w rtw89_pci_ops_write$w a=addr v=data" >> $T/dynamic_events || ok=0
    done
    echo 1 > $I/events/akuma/enable
    [ $ok = 1 ] && log "mmio fprobes on, cpus=$(nproc)" || { log "mmio fprobes FAILED"; exit 1; }
else
    cat $T/trace_pipe > $O/mmiotrace.txt &
    PIPE_PID=$!
    echo mmiotrace > $T/current_tracer || exit 1
    log "mmiotrace on, cpus=$(nproc)"
fi

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
# CHAN=1 (channel switching): the interface comes up in monitor mode, where
# `iw set channel` makes mac80211 call rtw89's own set_channel with no
# association, scan offload or keys involved. Stage "chan" below walks
# $CHANS (default 1..13) and marks each, so the recording splits per channel.
if [ "${CHAN:-0}" = 1 ]; then
    command -v iw >/dev/null || { log "CHAN=1 needs iw"; exit 1; }
    iw dev $NEWIF set type monitor || { log "monitor mode refused"; exit 1; }
fi
mark up
ip link set dev $NEWIF up || log "link up failed"
sleep 5

# 6C. CHAN=1: walk the channels. Nothing here is a network name or a key.
if [ "${CHAN:-0}" = 1 ]; then
    for c in ${CHANS:-1 2 3 4 5 6 7 8 9 10 11 12 13}; do
        mark "chan $c"
        iw dev $NEWIF set channel $c || log "set channel $c failed"
        sleep 1
    done
    mark end
    sleep 1
    exit 0
fi

# 6J. JOIN=1 (stage W3): give the card back to NetworkManager while still
# tracing, and record it associating with the remembered network: scan,
# authentication, association, the 4-way handshake, DHCP. Keys reach the card
# only as security-CAM H2Cs, which `redact` zeroes; EAPOL frames travel by
# DMA, and the TX-header probe captures only the first 24 bytes of each frame.
# Whole frames are captured only for management subtypes, by event filter
# (`txm`: association/probe requests, authentication, action; `rxm`: their
# responses, never beacons): no EAPOL frame, which an offline guess at the
# passphrase would need, is ever captured whole.
if [ "${JOIN:-0}" = 1 ]; then
    mark join
    rm -f $NM_DROPIN
    nmcli general reload conf
    i=0
    # The card's own state: NetworkManager's general state reads
    # "connected (local only)" off other interfaces long before wifi is up.
    until nmcli -t -f DEVICE,STATE device | grep -q "^$NEWIF:connected$" || [ $i -ge 60 ]; do sleep 1; i=$((i+1)); done
    log "join: $(nmcli -t -f DEVICE,STATE device | grep "^$NEWIF:") after ${i}s"
    sleep 5
    mark end
    sleep 1
    exit 0
fi

# 6. One scan: channel switching, the RX path. Only the count is kept, since
# the results are the neighbours' (and our own) network names.
# Pop shipped no `iw` (2026-10-06; installed for W2 the same day); without it nothing can ask for a
# scan, since NetworkManager is kept off the card, so the phase is skipped.
if command -v iw >/dev/null; then
    mark scan
    out=$(timeout 20 iw dev $NEWIF scan 2>&1)
    n=$(printf '%s\n' "$out" | grep -c '^BSS')
    # An error is iw's one unindented non-BSS line; no network names in it.
    err=$(printf '%s\n' "$out" | grep -v '^BSS' | grep -v '^[[:space:]]' | head -n1 | cut -c1-80)
    log "scan: $n BSS${err:+ ($err)}"
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
