#!/bin/sh
# Run ON the ryzen (Akuma). One trial: a local model drives install.sh to boot a Taoxichuan VM under herd and ssh in.
#   sh akuma_trial.sh NAME MODEL            (goose, native ollama provider at $OLLAMA_HOST)
# Result: /root/demo/trials/NAME/{goose.out,verdict.txt}. The HARNESS judges: live VM + real ssh uname + model ran the ssh step.
NAME=$1; MODEL=$2; HARNESS=${3:-goose}
export HOME=/root TERM=dumb OLLAMA_HOST=${OLLAMA_HOST:-http://192.168.1.203:11434}
T=/root/demo/trials/$NAME; rm -rf "$T"; mkdir -p "$T"
S=/root/demo/scripts/install.sh
for s in $(herd status 2>&1 | sed -n 's/^ *\(akuma-install-[0-9]*\):.*/\1/p'); do herd stop "$s" >/dev/null 2>&1; done
rm -rf /root/demo/run; rm -f /root/demo/up.log; sed -i '/^127.0.0.1:/d' /root/.ssh/known_hosts 2>/dev/null
mkdir -p /tmp/gcfg-$NAME
export XDG_CONFIG_HOME=/tmp/gcfg-$NAME XDG_DATA_HOME=/tmp/gdata-$NAME GOOSE_DISABLE_KEYRING=1 GOOSE_MODE=auto GOOSE_PROVIDER=ollama GOOSE_MODEL=$MODEL GOOSE_TELEMETRY_ENABLED=false
cd /root/demo
t0=$(date +%s)
echo "$t0" > "$T/start_epoch"
if [ "$HARNESS" = infinity ]; then
	INF=/root/demo/inf
	HOME=$INF/home $INF/bin/infinity daemon stop >/dev/null 2>&1; sleep 2; rm -f $INF/home/.infinity/daemon.log
	task="Your task brief is the file /root/demo/install.akuma.txt. Read it completely with run_command (cat), then carry it out. Use run_command for every command. Do not ask the user questions; the user is not there."
	( cd $INF/proj && env HOME=$INF/home OPENAI_API_KEY=none PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin OPENAI_BASE_URL=$OLLAMA_HOST/v1 INFINITY_PROVIDER_MODEL_ID=$MODEL timeout 1800 $INF/bin/infinity -H "$task" > "$T/goose.out" 2>&1 )
	n=0
	while [ $n -lt 360 ]; do   # the daemon keeps working after -H returns: wait for it to go idle
		grep -a -q 'idle with no keep-alive clients' $INF/home/.infinity/daemon.log 2>/dev/null && break
		n=$((n+1)); sleep 5
	done
	cp "$(ls -t $INF/home/.infinity/threads/*.json | grep -v meta | head -1)" "$T/thread.json" 2>/dev/null
	cp $INF/home/.infinity/daemon.log "$T/daemon.log" 2>/dev/null
	echo "harness_rc=0" > "$T/verdict.txt"
else
	timeout 1800 goose run -i /root/demo/install.akuma.txt --name "$NAME" --max-turns 40 > "$T/goose.out" 2>&1
	echo "goose_rc=$?" > "$T/verdict.txt"
fi
date +%s > "$T/end_epoch"
# token stats: goose keeps accumulated_*_tokens in its session db; Infinity's thread JSON carries token_usage
python3 - "$NAME" "$T" "$HARNESS" > "$T/stats.txt" 2>&1 <<'PY'
import sys, glob, sqlite3, json, re
name, T, h = sys.argv[1:4]
if h == "goose":
    for db in glob.glob(f"/tmp/gdata-{name}/**/sessions.db", recursive=True):
        c = sqlite3.connect(db)
        for r in c.execute("select name, accumulated_input_tokens, accumulated_output_tokens, accumulated_total_tokens, input_tokens, output_tokens, total_tokens from sessions"):
            print("goose session", r)
else:
    try:
        raw = open(f"{T}/thread.json", errors="replace").read()
        ins = [int(x) for x in re.findall(r'"input_tokens"\s*:\s*(\d+)', raw)]
        outs = [int(x) for x in re.findall(r'"output_tokens"\s*:\s*(\d+)', raw)]
        tot = re.findall(r'"total_tokens_used"\s*:\s*(\d+)', raw)
        print("infinity turns_with_usage", len(ins), "input_sum", sum(ins), "output_sum", sum(outs), "total_tokens_used", tot[-1:] )
    except Exception as e:
        print("no thread.json usage:", e)
PY
wall=$(( $(date +%s) - t0 ))
vm=no; herd status 2>&1 | grep -q 'akuma-install-[0-9]*: running' && vm=yes
line=$(timeout 90 sh $S ssh --dir /root/demo/run busybox uname -a 2>&1 | grep -a 'Linux taoxichuan' | tail -1)
case "$line" in "Linux taoxichuan"*) ok=yes ;; *) ok=no ;; esac
# did the MODEL run the ssh step? goose prints "command: ..." for each tool call; Infinity's thread has tool_call entries.
ran=no; cmds=0
if [ "$HARNESS" = infinity ]; then
	r=$(python3 - "$T/thread.json" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception:
    print("no 0"); sys.exit()
c = [m[0]["call"]["function"]["arguments"]["arguments"].get("command", "") for m in d["messages"] if m[0].get("kind") == "tool_call"]
print("yes" if any("install.sh ssh" in x for x in c) else "no", len(c))
PY
)
	ran=${r% *}; cmds=${r#* }
else
	cmds=$(tr -d '\000' < "$T/goose.out" | grep -a -c '^ *command:')
	tr -d '\000' < "$T/goose.out" | grep -a '^ *command:' | grep -a -q 'install.sh ssh' && ran=yes
fi
pass=no; [ $vm = yes ] && [ $ok = yes ] && [ $ran = yes ] && pass=yes
{ echo "model=$MODEL harness=$HARNESS vm_alive=$vm ssh_ok=$ok model_ran_ssh=$ran wall_s=$wall tool_calls=$cmds"; echo "ssh_line=$line"; echo "PASS=$pass"; } >> "$T/verdict.txt"
echo FINISHED >> "$T/verdict.txt"
