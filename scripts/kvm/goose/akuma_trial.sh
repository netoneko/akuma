#!/bin/sh
# Run ON the ryzen (Akuma). One trial: a local model drives install.sh to boot a Taoxichuan VM under herd and ssh in.
#   sh akuma_trial.sh NAME MODEL            (goose, native ollama provider at $OLLAMA_HOST)
# Result: /root/demo/trials/NAME/{goose.out,verdict.txt}. The HARNESS judges: live VM + real ssh uname + model ran the ssh step.
NAME=$1; MODEL=$2
export HOME=/root TERM=dumb OLLAMA_HOST=${OLLAMA_HOST:-http://192.168.1.203:11434}
T=/root/demo/trials/$NAME; rm -rf "$T"; mkdir -p "$T"
S=/root/demo/scripts/install.sh
for s in $(herd status 2>&1 | sed -n 's/^ *\(akuma-install-[0-9]*\):.*/\1/p'); do herd stop "$s" >/dev/null 2>&1; done
rm -rf /root/demo/run; rm -f /root/demo/up.log; sed -i '/^127.0.0.1:/d' /root/.ssh/known_hosts 2>/dev/null
mkdir -p /tmp/gcfg-$NAME
export XDG_CONFIG_HOME=/tmp/gcfg-$NAME GOOSE_DISABLE_KEYRING=1 GOOSE_MODE=auto GOOSE_PROVIDER=ollama GOOSE_MODEL=$MODEL GOOSE_TELEMETRY_ENABLED=false
cd /root/demo
t0=$(date +%s)
timeout 1800 goose run -i /root/demo/install.akuma.txt --no-session --max-turns 40 > "$T/goose.out" 2>&1
echo "goose_rc=$?" > "$T/verdict.txt"
wall=$(( $(date +%s) - t0 ))
vm=no; herd status 2>&1 | grep -q 'akuma-install-[0-9]*: running' && vm=yes
line=$(timeout 60 sh $S ssh --dir /root/demo/run busybox uname -a 2>&1 | tail -1)
case "$line" in "Linux taoxichuan"*) ok=yes ;; *) ok=no ;; esac
ran=no; tr -d '\000' < "$T/goose.out" | grep -a -q 'install.sh ssh' && ran=yes
pass=no; [ $vm = yes ] && [ $ok = yes ] && [ $ran = yes ] && pass=yes
{ echo "model=$MODEL vm_alive=$vm ssh_ok=$ok model_ran_ssh=$ran wall_s=$wall"; echo "ssh_line=$line"; echo "PASS=$pass"; } >> "$T/verdict.txt"
echo FINISHED >> "$T/verdict.txt"
