#!/bin/bash
# shanghai_glm_starter.sh - fire goose (GLM) ONCE on the Shanghai brief, a few minutes
# after the machine comes up, and never again. Built for Task 2 of the 2026-11-02
# handoff (scripts/kvm/goose/CLAUDE_HANDOFF_2026-11-02.md).
#
# Installed on Pop as the systemd unit shanghai-glm.service (this directory), enabled
# under multi-user.target; the machine boots Pop next (efibootmgr, 2026-11-02).
#
# goose setup is the one earlier GLM sessions on this box use: /usr/local/bin/goose,
# `. /etc/goose.env`, root-only /root/.config/goose (config.yaml carries the GLM model
# settings, secrets.yaml the z.ai key - never printed or copied). It runs goose with the
# brief as the prompt in a dedicated worktree, so /root/akuma-handoff stays on
# rust-org-il-experiment and the brief's own first step (`git checkout -b shanghai
# origin/rust-org-il-experiment`) happens inside the worktree.
#
# Every prerequisite is checked BEFORE the marker is written: on any failure the starter
# exits 1 with the marker still absent, so the next boot retries. The marker goes down
# immediately before `goose run`, so goose fires exactly once even if it is interrupted.
#
# Re-arm: delete the marker (/root/shanghai-starter/fired). Log:
# /root/shanghai-starter/goose.log.
#
# Test without a reboot (fires a REAL goose on the REAL brief - stop the goose process
# once it has read the brief, then delete the scratch marker and the scratch worktree):
#   SHANGHAI_DELAY=0 SHANGHAI_MARKER=/tmp/fired-test SHANGHAI_LOG=/tmp/goose-test.log \
#     SHANGHAI_WT=/root/shanghai-wt-test /usr/local/sbin/shanghai_glm_starter.sh

set -u

STARTER_DIR=${SHANGHAI_DIR:-/root/shanghai-starter}
MARKER=${SHANGHAI_MARKER:-$STARTER_DIR/fired}
LOG=${SHANGHAI_LOG:-$STARTER_DIR/goose.log}
REPO=${SHANGHAI_REPO:-/root/akuma-handoff}
WT=${SHANGHAI_WT:-/root/shanghai-wt}
DELAY=${SHANGHAI_DELAY:-240}          # seconds after boot before firing (network/services up)
SESSION=shanghai-01
REF=origin/rust-org-il-experiment     # the brief branches shanghai from this

log() { printf '%s %s\n' "$(date '+%F %T')" "$*" >> "$LOG"; }

mkdir -p "$STARTER_DIR"

# One-shot guard, checked first: never fire twice.
if [ -e "$MARKER" ]; then
    log "marker $MARKER present - not firing"
    exit 0
fi

sleep "$DELAY"
log "starter awake; checking prerequisites (marker still absent)"

if ! command -v goose >/dev/null 2>&1; then
    log "FATAL: goose not on PATH - not firing; next boot retries"
    exit 1
fi
if [ ! -e "$REPO/.git" ]; then
    log "FATAL: no repo at $REPO - not firing; next boot retries"
    exit 1
fi

# Work in a dedicated worktree so /root/akuma-handoff stays on rust-org-il-experiment.
# This checkout's remote.origin.fetch may not carry the branch (it shipped main-only),
# so fetch the refspec that creates the tracking ref, then verify it.
if [ ! -e "$WT/.git" ]; then
    git -C "$REPO" fetch origin "+refs/heads/rust-org-il-experiment:refs/remotes/origin/rust-org-il-experiment" >> "$LOG" 2>&1 \
        || log "fetch failed (offline?); using the local tracking ref if it exists"
    if ! git -C "$REPO" rev-parse --verify -q "$REF" >/dev/null; then
        log "FATAL: $REF does not exist in $REPO - not firing; next boot retries"
        exit 1
    fi
    git -C "$REPO" worktree add --detach "$WT" "$REF" >> "$LOG" 2>&1 \
        || { log "FATAL: worktree add $WT failed - not firing; next boot retries"; exit 1; }
fi
[ -e "$WT/.git" ] || { log "FATAL: $WT is not a worktree - not firing"; exit 1; }
log "worktree $WT at $(git -C "$WT" log --oneline -1 | head -1)"

BRIEF="$WT/scripts/kvm/goose/shanghai-01-new-kernel.md"
if [ ! -r "$BRIEF" ]; then
    BRIEF="$STARTER_DIR/shanghai-01-new-kernel.md"
    [ -r "$BRIEF" ] || { log "FATAL: brief not readable in $WT or $STARTER_DIR - not firing"; exit 1; }
fi
log "brief $BRIEF"

# All checks passed: the marker goes down NOW, immediately before goose starts.
date > "$MARKER"
log "marker $MARKER written; goose run starts (session $SESSION)"

. /etc/goose.env
export GOOSE_MODE=auto

cd "$WT"
goose run --with-builtin developer -n "$SESSION" -i "$BRIEF" >> "$LOG" 2>&1
rc=$?
log "goose exited rc=$rc"
exit "$rc"
