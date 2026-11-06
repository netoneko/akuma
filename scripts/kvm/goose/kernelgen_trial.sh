#!/bin/sh
# Run ON Pop as netoneko. One trial: a model on the Mac (ollama) writes a new kernel from the akuma-tokio-kernel README.
#   sh kernelgen_trial.sh LABEL MODEL CITY [BUDGET_SECONDS]
# Needs $KG/base.tar (git archive of the tree under test). Everything lands in $KG/results/LABEL/.
# The HARNESS judges afterwards (judge step is separate); nothing model-facing lives here except the brief.
LABEL=$1; MODEL=$2; CITY=$3; BUDGET=${4:-5400}
KG=/home/netoneko/kernelgen; WT=$KG/wt; R=$KG/results/$LABEL
LOWER=$(echo "$CITY" | tr 'A-Z' 'a-z')
export PATH=/home/netoneko/.cargo/bin:/home/netoneko/bin:/usr/local/bin:$PATH
export CARGO_TARGET_DIR=$KG/target
rm -rf "$WT" "$R"; mkdir -p "$WT" "$R"
tar -C "$WT" -xf "$KG/base.tar" || exit 1
( cd "$WT" && git init -q && git add -A >/dev/null 2>&1 && git -c user.name=kg -c user.email=kg@x commit -qm base ) || exit 1
sed "s/{CITY}/$CITY/g; s/{LOWER}/$LOWER/g" "$WT/scripts/kvm/goose/kernelgen-brief.md" > "$KG/BRIEF-$LABEL.md"
cp "$KG/BRIEF-$LABEL.md" "$R/brief.md"
export XDG_CONFIG_HOME=$R/cfg XDG_DATA_HOME=$R/data GOOSE_DISABLE_KEYRING=1 GOOSE_MODE=auto GOOSE_TELEMETRY_ENABLED=false
export GOOSE_PROVIDER=ollama OLLAMA_HOST=${OLLAMA_HOST:-http://192.168.1.203:11434} GOOSE_MODEL=$MODEL
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME"
date +%s > "$R/start_epoch"
cd "$WT"
timeout "$BUDGET" goose run --with-builtin developer -i "$KG/BRIEF-$LABEL.md" --name "kg-$LABEL" --max-turns 120 > "$R/goose.out" 2>&1
echo "goose_rc=$?" > "$R/verdict.txt"
date +%s > "$R/end_epoch"
( cd "$WT" && git log --oneline > "$R/commits.txt"; git status --short > "$R/status.txt"; git diff --stat HEAD~0 > /dev/null 2>&1 )
[ -d "$WT/$LOWER" ] && tar -C "$WT" -cf "$R/$LOWER.tar" "$LOWER" Cargo.toml 2>/dev/null
echo done > "$R/DONE"
