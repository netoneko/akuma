#!/bin/sh
# denver/probe-nullstore.sh - the PHASE-2-NULLSTORE gate probe (probe-procfs.sh's
# shape: stamps + console window; a NEW rig: denver-nullstore kernel binary +
# vm-nullstore.spec, D30/D36).
#
#   sh denver/probe-nullstore.sh 2>&1 | tee denver/gates/PHASE-2-NULLSTORE.log
#
# G9 (denver/docs/REQUIREMENTS.md): the merged tree's java -version dies mid-init
# with a user-mode write to the NULL page (rip=0x3005fc41 rsp=0x6fffffffba60
# cr2=0x0 code=0x6, /root/denver/merge-verify/gate-vdso.log). PROCFS closed with
# maps serving but its NAME arm starved the guest sshd (RESULT.md session 8), so
# G9's first job - the rip's owning module - moves console-side: the kernel's
# ring-3 kill path now prints the attribution itself (D33, behind procfs-diag),
# with the dying task's last 40 syscalls + results (D34, behind trace-err).
#
# Pass conditions (D36, in order):
#   1. uname says denver AND NOT the procfs hash f0dcacb6 AND NOT the merged
#      tree 3531cf69 (an old binary booting = staging failed);
#   2. tokioprobe RESULT PASS (platform sanity);
#   3. RWX PASS rw_to_rx=42 (wx-relaxed rides);
#   4. MAPS PASS + AUXV PASS procfs-auxv (the procfs-diag walk alive);
#   5. MMARMS PASS (the taoxichuan blocker-3 contract facts, java-free: mmap
#      failures are MAP_FAILED never 0/NULL; PROT_NONE reserve commits RW and
#      RX; RWX executes under wx-relaxed);
#   6. SYSINFO line recorded (PASS or INFO errno=N - G7 owns that verdict);
#   7. NAME PASS: the java arm's console window carries the fault at the
#      python-checked rip AND a `[Fault] ring3 map at` row covering it AND the
#      host-side arithmetic names the module (ld-musl+0x5fc41 fstatat per the
#      research artifact's Finding 0 - INTERP_BASE 0x30000000, RX 14000..6c59f).
#      The regs/stack/[ring] lines are the fix decision's evidence, recorded.
#   RESULT PASS = 1-6 AND 7. A different rip with a covering row = the rip moved;
#   the gate FAILs honestly (the diagnostic worked, the constant did not).
#
# Machine notes carried: ONE VM at a time (free the slot; stale = EBUSY); wait
# for an ssh round trip, never a log marker; always stop the guest; grep
# console-derived text -a-tolerantly; invoke as `sh <file>` (not #! runnable).
# The java arm WILL stall sshd (session 8's data): its 180 s timeout and the
# console window are the designed path, not a failure of the probe.

W=${DENVER_W:-/root/denver/w}
SPEC=${1:-$W/vm-nullstore.spec}
PORT=${DENVER_PORT:-2247}
WAIT=${DENVER_BOOT_WAIT_TRIES:-30}     # x 5 s
SSH="ssh -y -i $W/key -p $PORT root@127.0.0.1"
VMLOG=$W/vmrun.log
PROCSHASH="f0dcacb6"                   # the proven PROCFS binary - must NOT boot (D31 lesson)
MERGEDHASH="3531cf69"                  # the merged-tree/vdso binary - must NOT boot
[ -n "$PROCSHASH" ] && [ -n "$MERGEDHASH" ] ||
    { echo "RESULT FAIL: rig identity constant empty (PROCSHASH/MERGEDHASH) - the probe would match any uname" >&2; exit 1; }
RIP=3005fc41                           # G9's rip (python-checked: 805698625)
VSYSCALL=ffffffffff600800              # the closed vsyscall death - must NOT recur
INTERP_BASE=$((0x30000000))            # akuma-elf-exec INTERP_BASE (Finding 0)
LD_MUSL_RX_LO=$((0x14000))             # ld-musl RX segment vaddr range (readelf, the rig image)
LD_MUSL_RX_HI=$((0x6c59f))
T0=$(date +%s)

VP=
step() {
    N=$(date +%s); E=$((N - T0)); CL=""
    if [ "$E" -lt 0 ]; then E=0; CL=" -- clock slewed, elapsed clamped (t0=$T0 epoch=$N)"; fi
    echo "== $1 (elapsed=${E}s$CL)"
}
verdict() { E=$(( $(date +%s) - T0 )); [ "$E" -lt 0 ] && E=0
    echo "$1"; stop_vm; echo "== wall clock: seconds=$E (t0=$T0 end=$(date +%s))"; exit "$2"; }
stop_vm() {
    echo stop > /dev/kvm0 2>/dev/null
    [ -n "$VP" ] && kill "$VP" 2>/dev/null
    VP=
}

[ -r "$SPEC" ] || verdict "RESULT FAIL: no spec at $SPEC (stage denver-nullstore + vm-nullstore.spec first, denver/docs/research/PHASE-2-NULLSTORE.md D34)" 1
[ -r "$W/key" ] || verdict "RESULT FAIL: no key at $W/key" 1
[ -r "$VMLOG" ] || verdict "RESULT FAIL: no console log at $VMLOG - the console capture is this gate's evidence" 1

step "free the VM slot (a stale guest answers EBUSY)"
echo stop > /dev/kvm0 2>/dev/null
sleep 3

step "boot: herd vm-run $SPEC"
herd vm-run "$SPEC" > "$VMLOG" 2>&1 &
VP=$!

i=0
while :; do
    out=$($SSH "uname -a" 2>&1)
    case "$out" in *Linux*) break ;; esac
    i=$((i + 1))
    if [ "$i" -gt "$WAIT" ]; then
        echo "-- vm-run log tail:"; tail -c 600 "$VMLOG"
        verdict "RESULT FAIL: guest never answered ssh in $((WAIT * 5)) s" 1
    fi
    case "$(tail -c 300 "$VMLOG")" in
        *"no /dev/kvm0"*) verdict "RESULT FAIL: no /dev/kvm0 - host not booted with kvmctl" 1 ;;
        *"errno 16"*) verdict "RESULT FAIL: VM slot still busy (EBUSY) after stop" 1 ;;
    esac
    sleep 5
done

step "rig identity: uname says denver, NOT $PROCSHASH, NOT $MERGEDHASH"
echo "$out" | grep Linux
case "$out" in
    *denver*) ;;
    *) verdict "RESULT FAIL: uname does not say denver" 1 ;;
esac
case "$out" in
    *"$PROCSHASH"*) verdict "RESULT FAIL: the procfs rig binary booted - no trace-err/attribution in it; staging failed" 1 ;;
esac
case "$out" in
    *"$MERGEDHASH"*) verdict "RESULT FAIL: the vdso rig binary booted - staging failed" 1 ;;
esac

step "platform sanity: tokioprobe"
tok=$(timeout 90 $SSH "/bin/tokioprobe" 2>&1)
echo "$tok"
case "$tok" in *"RESULT PASS"*) ;; *) verdict "RESULT FAIL: tokioprobe did not pass" 1 ;; esac

step "wx-relaxed proof: the rwx arm (rides; this binary carries wx-relaxed AND vsyscall AND procfs-diag AND trace-err)"
rwx=$(timeout 60 $SSH "/bin/jvmprobe rwx" 2>&1)
echo "$rwx"
case "$rwx" in *"RWX PASS rw_to_rx=42"*) ;; *) verdict "RESULT FAIL: rwx arm did not pass" 1 ;; esac

step "procfs-diag alive: the maps arm"
maps=$(timeout 60 $SSH "/bin/jvmprobe maps" 2>&1)
echo "$maps"
case "$maps" in
    *"MAPS PASS lines="*) ;;
    *) verdict "RESULT FAIL: the maps arm did not print MAPS PASS" 1 ;;
esac

step "procfs-diag alive: the auxv arm"
aux=$(timeout 60 $SSH "/bin/jvmprobe auxv" 2>&1)
echo "$aux"
case "$aux" in
    *"AUXV PASS procfs-auxv"*) ;;
    *) verdict "RESULT FAIL: the auxv arm did not print AUXV PASS procfs-auxv" 1 ;;
esac
case "$aux" in
    *"unreadable errno=2"*) verdict "RESULT FAIL: /proc/self/auxv unreadable again - the procfs-diag walk regressed" 1 ;;
esac

step "the contract facts, java-free (D35): mmaparms"
mma=$(timeout 90 $SSH "/bin/jvmprobe mmaparms" 2>&1)
echo "$mma"
case "$mma" in
    *"MMARMS PASS len0"*"MMARMS PASS oversize"*) ;;
    *) verdict "RESULT FAIL: the mmap failure conventions did not hold (taoxichuan suspect 1 lives?)" 1 ;;
esac
case "$mma" in
    *"MMARMS PASS commit-RW"*"MMARMS PASS commit-RX call=42"*) ;;
    *) verdict "RESULT FAIL: the code-cache commit shape did not hold (taoxichuan suspect 2 lives?)" 1 ;;
esac
case "$mma" in
    *"RED"*|*"CRASH"*) verdict "RESULT FAIL: mmaparms printed a RED/CRASH line - read it above" 1 ;;
esac

step "sysinfo recorded (G7's feed; this gate does not judge it)"
sys=$(timeout 60 $SSH "/bin/jvmprobe sysinfo" 2>&1)
echo "$sys"
case "$sys" in
    *"SYSINFO PASS"*|*"SYSINFO INFO"*) echo "  (recorded for the G7 pass)" ;;
    *) verdict "RESULT FAIL: the sysinfo arm printed neither PASS nor INFO" 1 ;;
esac

step "the payoff: the java arm - the CONSOLE now names the death (D33/D34); sshd may starve (session 8's data)"
OFF=$(wc -c < "$VMLOG")                    # console bytes before the arm
timeout 180 $SSH "/bin/jvmprobe name /opt/jdk/bin/java $RIP -version" 2>&1 | head -5
echo "  (arm dispatched; its stdout was expected to stall - the verdict is console-side)"

step "the console window: fault + attribution + the dying task's syscall ring"
sleep 2
win=$(tail -c +$((OFF + 1)) "$VMLOG" 2>/dev/null | grep -a .)
faultline=$(printf '%s\n' "$win" | grep 'ring3 PageFault rip=' | tail -1)
slotline=$(printf '%s\n' "$win" | grep -F 'killed (SIGSEGV)' | tail -1)
echo "-- console window: fault line:"
echo "${faultline:-<none>}"
echo "-- console window: slot line:"
echo "${slotline:-<none>}"

printf '%s\n' "$win" | grep 'ring3 map ' > /tmp/nullstore-maps.txt 2>/dev/null
printf '%s\n' "$win" | grep 'ring3 regs ' > /tmp/nullstore-regs.txt 2>/dev/null
printf '%s\n' "$win" | grep 'ring3 stack ' > /tmp/nullstore-stack.txt 2>/dev/null
printf '%s\n' "$win" | grep '\[ring\]\|\[hang\]\|\[fault-code\]\|\[mm\]' > /tmp/nullstore-ring.txt 2>/dev/null
echo "-- console window: attribution lines (D33):"
cat /tmp/nullstore-maps.txt /tmp/nullstore-regs.txt /tmp/nullstore-stack.txt 2>/dev/null
echo "-- console window: evidence lines (D34):"
head -45 /tmp/nullstore-ring.txt 2>/dev/null

case "$faultline" in
    *"$VSYSCALL"*)
        verdict "RESULT FAIL: the workload fetched the vsyscall page again - the map did not take" 1 ;;
esac

case "$faultline" in
    *"rip=0x00000000$RIP"*) ;;
    *"ring3 PageFault"*)
        echo "-- a fault at a DIFFERENT rip (the constant moved; re-rank owns the data)"
        verdict "RESULT FAIL: the java arm died at an unexpected rip - see the window above" 1 ;;
    *)
        verdict "RESULT FAIL: no fault line captured in the console window" 1 ;;
esac

step "the naming arithmetic (D36 condition 7): range covers the rip, module = ld-musl fstatat"
python3 - "$RIP" <<'PYEOF'
import re, sys
rip = int(sys.argv[1], 16)
INTERP_BASE = 0x30000000
RX_LO, RX_HI = 0x14000, 0x6c59f
at_line = None
for line in open('/tmp/nullstore-maps.txt'):
    m = re.search(r'map at rip=0x([0-9a-f]+) in 0x([0-9a-f]+)-0x([0-9a-f]+) (\S{4})', line)
    if m:
        r_rip, s, e, perms = int(m.group(1), 16), int(m.group(2), 16), int(m.group(3), 16), m.group(4)
        if s <= r_rip < e:
            at_line = (s, e, perms, r_rip)
            break
if not at_line:
    print("NAME MODULE MISSING no covering [Fault] ring3 map at row in the window")
    sys.exit(1)
s, e, perms, r_rip = at_line
assert r_rip == rip, f"the attribution row's rip {r_rip:#x} != the fault line's {rip:#x}"
off = rip - s
mod = "ld-musl" if s == INTERP_BASE else ("PIE image" if s == 0x10000000 else "UNKNOWN-BASE")
in_rx = RX_LO <= off < RX_HI if mod == "ld-musl" else False
print(f"NAME PASS rip=0x{rip:016x} in {s:08x}-{e:08x} perms={perms} off=0x{off:x}")
print(f"NAME MODULE base={s:#x} module={mod} offset=0x{off:x}" +
      (f" -> ld-musl RX segment (0x{RX_LO:x}..0x{RX_HI:x}): fstatat's copy-out (`rep stos`)" if in_rx else " (offset outside the known RX range - read the research artifact)"))
sys.exit(0 if (mod == "ld-musl" and in_rx) else 1)
PYEOF
case "$?" in
    0) ;;
    *) verdict "RESULT FAIL: the attribution row did not confirm Finding 0 - read the artifact and the maps lines" 1 ;;
esac

step "verdict"
echo "MOVEMENT: G9's NULL store is NAMED - kernel-side attribution + the dying task's syscall ring are in the window; the fix pass reads them"
verdict "RESULT PASS" 0
