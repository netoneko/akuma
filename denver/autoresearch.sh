#!/bin/sh
# denver/autoresearch.sh - the Denver autoresearch session driver.
#
# One invocation is one (or N) autoresearch session(s), each exactly what
# denver/docs/runbooks/AUTORESEARCH.md calls a session: orient, one gate, goose runs the
# denver-01 recipe brief, record, leave the NEXT GATE line. The script is the loop's shell;
# the runbook is the loop.
#
#   sh denver/autoresearch.sh [sessions] [gate]
#
#   sessions   how many sessions to run back to back (default 1)
#   gate       force this gate instead of the last `NEXT GATE:` line of LOOP_LOG.md
#
# Division of labor (deliberate): the script decides only what the loop needs to continue -
# which gate is next, that goose exists, that the bookkeeping appeared after a session. It
# never decides whether a gate passed: a gate passes when its probe line appears in
# denver/gates/<gate>.log, and the script only greps for the marker convention `RESULT PASS`
# there. Judging gates from self-report is how the Qwen experiment's fairness was lost; not
# here (see docs/archive/INSTALL_SCRIPT_QWEN_EXPERIMENT.md). The script also does the
# committing and the pushing; the agent's job ends at the record.
#
# Commit policy (token spend): after each green session the script commits and pushes to
# litter itself - docs and records (`denver/docs/**`, `RESULT.md`, `ESCALATE.md`, this
# script, the recipe) with `--no-verify`; code only after targeted checks, `cargo test -p`
# for every crate the session touched (repo-wide hooks would check everything; the loop
# pays per token, so checks are per-package), committed with `--no-verify` too. Checks red
# = the code stays uncommitted and the loop stops.
#
# Stops on:
#   - the escape hatch: `denver/ESCALATE.md` present (the agent asked for a human or a
#     stronger model). Answer the file and rerun with ESCALATE_ANSWERED=1; the script
#     archives it to denver/docs/archive/escalations/ and continues. ESCALATE_MODEL=<m>
#     (and ESCALATE_PROVIDER=<p> if needed) redo the same gate with a stronger model.
#   - bookkeeping missing after a session (no gate log, no LOOP_LOG entry)
#   - a targeted check failing
#   - the `INTERVENTION:` count in RESULT.md rising - autonomy is earned per phase (runbook),
#     so an intervention hands the loop back to a human
#   - goose exiting nonzero twice in a row (once is retried; the model gets one bad night)
#   - the session budget
#
# A z.ai quota exhaustion is a wait, not a failure: if goose dies and the log tail matches the
# quota patterns (429, quota, rate limit, ...), the driver sleeps QUOTA_WAIT_SECS and retries the
# same session - up to QUOTA_MAX_RETRIES times - without burning the session budget or the failure
# count. This is what makes batch mode survivable overnight.
#
# Env:
#   DENVER_LOG            session log (default /tmp/denver-autoresearch.log)
#   GOOSE_BIN             (default goose)
#   SESSION_TIMEOUT_SECS  kill the session's goose after this long (default 14400)
#   DENVER_WT             run in this dedicated worktree instead of the current checkout
#   QUOTA_WAIT_SECS       sleep before retrying after a quota death (default 600)
#   QUOTA_MAX_RETRIES     quota waits before giving up and stopping (default 12)
#   ESCALATE_ANSWERED     1 = the escalation in ESCALATE.md is answered; archive and continue
#   ESCALATE_MODEL        model override for goose (goose reads GOOSE_MODEL/GOOSE_PROVIDER)
#   ESCALATE_PROVIDER     provider override, if the model needs one
#   DRY_RUN               1 = print the plan, touch nothing
#
# House rules honored: branch denver, push to litter only; never kill a VM by pattern (the
# script kills only the goose process it started); unproven code lands only as `denver(wip)`
# commits (--no-verify, must build, every ~15 minutes) - the gate commit is the driver's and
# follows evidence.
set -eu
cd "$(dirname "$0")/.."   # repo root, like shanghai/mw_gate.sh

LOG=${DENVER_LOG:-/tmp/denver-autoresearch.log}
GOOSE=${GOOSE_BIN:-goose}
TMO=${SESSION_TIMEOUT_SECS:-14400}
QW=${QUOTA_WAIT_SECS:-600}
QMR=${QUOTA_MAX_RETRIES:-12}
N=${1:-1}
FORCE_GATE=${2:-}
RUNBOOK=denver/docs/runbooks/AUTORESEARCH.md
RECIPE=scripts/kvm/goose/denver-01-autoresearch.md
LOOP=denver/docs/LOOP_LOG.md
RESULT=denver/RESULT.md
REQ=denver/docs/REQUIREMENTS.md
GATES=denver/gates
RESEARCH=denver/docs/research
LOCK=/tmp/denver-autoresearch.lock
W=/tmp/denver-autoresearch

log() { printf '%s %s\n' "$(date '+%F %T')" "$*" >> "$LOG"; }
die() { log "FATAL: $*"; printf 'FATAL: %s\n' "$*" >&2; rm -rf "$LOCK"; exit 1; }
count() { # pattern file -> match count, 0 if file absent or no match
    n=$(grep -c "$1" "$2" 2>/dev/null) || n=0
    printf '%s' "${n:-0}"
}

mkdir "$LOCK" 2>/dev/null || { printf 'FATAL: another denver-autoresearch run holds %s\n' "$LOCK" >&2; exit 1; }
trap 'rm -rf "$LOCK"' EXIT INT TERM
mkdir -p "$W" "$GATES" "$RESEARCH" denver/docs/lessons

# Preflight: every check before the first session, like the shanghai starter.
command -v "$GOOSE" >/dev/null 2>&1 || die "$GOOSE not on PATH"
[ -r "$RUNBOOK" ] || die "$RUNBOOK missing - the procedure is the runbook"
[ -r "$RECIPE" ] || die "$RECIPE missing - the session brief"
if [ -n "${DENVER_WT:-}" ]; then
    [ -e "$DENVER_WT/.git" ] || die "DENVER_WT=$DENVER_WT is not a checkout/worktree"
    cd "$DENVER_WT"
fi
git rev-parse --verify -q denver >/dev/null || die "no branch denver"
git checkout -q denver
log "=== denver autoresearch: $N session(s) on $(git log --oneline -1 | head -1) ==="

next_gate() {
    if [ -n "$FORCE_GATE" ]; then printf '%s' "$FORCE_GATE"; return; fi
    if [ -r "$LOOP" ]; then
        g=$(sed -n 's/^NEXT GATE: *//p' "$LOOP" | tail -1)
        if [ -n "$g" ]; then printf '%s' "$g"; return; fi
    fi
    printf 'PHASE-0-SCAFFOLD'   # no state yet: the runbook's step 2 falls back to the phase table
}

session() { # n gate
    n=$1; g=$2
    # Gate names are one token with dashes (NEXT GATE: java-hello-world); anything a
    # filesystem dislikes becomes a dash here.
    slug=$(printf '%s' "$g" | tr ' /:' '---')
    sess="denver-$slug"
    glog="$GATES/$slug.log"
    instr="$W/instr-$n.txt"
    {
        cat "$RECIPE"
        printf '\n## This session\n\n'
        printf 'Gate: %s (taken from LOOP_LOG.md NEXT GATE; session %s of this script run)\n' "$g" "$n"
        printf 'Capture the probe output to %s - it passes when the RESULT PASS line appears there.\n' "$glog"
        printf 'Write the research artifact first (rule R1): %s/<pass>.md\n' "$RESEARCH"
        printf 'Interventions you needed go into %s as `INTERVENTION: ...` lines.\n' "$RESULT"
        printf 'Decisions need precedents (rule R9): search docs/archive, docs/reference and the other kernels (shanghai/, taoxichuan/, the akuma-tokio-kernel* crates) before deciding, and cite what you found in the research artifact.\n'
        printf 'Beyond the stall protocol? Write denver/ESCALATE.md (QUESTION: / CONTEXT:), leave NEXT GATE pointing at this same gate, and stop - the driver stops the loop for a human or a stronger model.\n'
        printf 'The driver commits and pushes to litter after the bookkeeping check: docs with --no-verify, code after targeted cargo test -p on the crates you touched. Your job ends at the record.\n'
    } > "$instr"
    log "session $n: gate [$g] goose session [$sess] brief + header in $instr"
    if [ "${DRY_RUN:-0}" = 1 ]; then printf 'DRY: would run: %s run --with-builtin developer -n %s -i %s\n' "$GOOSE" "$sess" "$instr"; return 0; fi

    loop_before=$(count '^### ' "$LOOP")
    interv_before=$(count 'INTERVENTION:' "$RESULT")
    if [ -r /etc/goose.env ]; then . /etc/goose.env; fi
# Pin the GLM endpoint: config.yaml may carry kimi defaults (api.kimi.com), and a launching
# shell without the GLM env sends glm-5.3-flash to kimi's host - the kimi auth error the
# first real run died of. The trio below is what the infra itself runs on; any of these may
# still be overridden from the environment.
GOOSE_PROVIDER=${GOOSE_PROVIDER:-openai}
GOOSE_MODEL=${GOOSE_MODEL:-glm-5.3-flash}
OPENAI_HOST=${OPENAI_HOST:-https://api.z.ai}
OPENAI_BASE_PATH=${OPENAI_BASE_PATH:-api/coding/paas/v4/chat/completions}
export GOOSE_PROVIDER GOOSE_MODEL OPENAI_HOST OPENAI_BASE_PATH
log "model: $GOOSE_MODEL via $GOOSE_PROVIDER at $OPENAI_HOST"
    GOOSE_MODE=auto "$GOOSE" run --with-builtin developer -n "$sess" -i "$instr" >> "$LOG" 2>&1 &
    pid=$!
    # Watchdog: kills only the goose process this script started, never a VM by pattern.
    ( sleep "$TMO"; kill "$pid" 2>/dev/null ) &
    watchdog=$!
    rc=0
    wait "$pid" || rc=$?
    kill "$watchdog" 2>/dev/null
    if [ "$rc" -ne 0 ]; then
        # A quota death is a wait, not a failure: read the log tail, sleep, retry the same
        # session. Only a real failure reaches the retry/fail counters.
        if tail -50 "$LOG" | grep -Eiq '429|quota|rate.?limit|too many requests|insufficient|billing|exceeded'; then
            log "session $n: z.ai quota exhausted (rc=$rc) - waiting ${QW}s, then retrying this session"
            sleep "$QW"
            return 8
        fi
        log "session $n: goose exited rc=$rc (125+ = timeout kill)"
        return 9
    fi

    # A mid-session escalation: the agent asked for help while working. Commit what is in the
    # tree so litter shows the question, then stop. Archiving happens at the loop check when
    # the answer comes back with ESCALATE_ANSWERED=1.
    if [ -s denver/ESCALATE.md ]; then
        commit_and_push "$g" || log "escalation commit incomplete; the file is still in the tree"
        log "session $n: ESCALATE.md filed mid-session - loop stopped. Answer it, then: ESCALATE_ANSWERED=1 sh denver/autoresearch.sh 1 [$g]"
        return 5
    fi

    # A redo with a stronger model is the other half of the hatch (goose reads
    # GOOSE_MODEL/GOOSE_PROVIDER from the environment).
    if [ -n "${ESCALATE_MODEL:-}" ]; then
        export GOOSE_MODEL="$ESCALATE_MODEL"
        if [ -n "${ESCALATE_PROVIDER:-}" ]; then export GOOSE_PROVIDER="$ESCALATE_PROVIDER"; fi
        log "session $n: model override GOOSE_MODEL=$ESCALATE_MODEL"
    fi

    # Judge the bookkeeping, never the gate's truth: the pieces the loop needs to continue.
    loop_after=$(count '^### ' "$LOOP")
    interv_after=$(count 'INTERVENTION:' "$RESULT")
    if [ ! -s "$glog" ]; then
        log "session $n: no captured output at $glog - bookkeeping incomplete"
        return 2
    fi
    if ! grep -q 'RESULT PASS' "$glog"; then
        log "session $n: $glog has no RESULT PASS - gate not green, loop continues per runbook step 6"
        return 1
    fi
    if [ "$loop_after" -le "$loop_before" ]; then
        log "session $n: LOOP_LOG.md gained no ### entry - record step skipped"
        return 2
    fi
    if [ "$interv_after" -gt "$interv_before" ]; then
        log "session $n: RESULT.md INTERVENTION count rose ($interv_before -> $interv_after)"
        return 3   # autonomy handed back; runbook says a human reviews at the phase line
    fi
    ng=$(next_gate)
    if [ "${DRY_RUN:-0}" != 1 ]; then commit_and_push "$g" || return 4; fi
    log "session $n: gate [$g] green; bookkeeping complete; next gate from LOOP_LOG: [$ng]"
    return 0
}

commit_and_push() { # gate -> 0 ok, 4 checks red or commit failed. Docs and records take
                   # --no-verify; code pays targeted `cargo test -p` per touched crate first
                   # (repo-wide hooks would check everything; the loop pays per token).
    g=$1
    # Root manifests are part of the record when a gate adds workspace members or
    # locks: without them the committed tree cannot build its own kernel (a
    # members-line gate commit that is not committed is a broken tree for the
    # next session - the unforgivable exit).
    git add -A denver akuma-jvm-kernel crates Cargo.toml Cargo.lock scripts/kvm/goose/denver-01-autoresearch.md 2>/dev/null || true
    # Gate logs are the evidence and must ride with the record: the root .gitignore's
    # `*.log` would silently leave denver/gates/<gate>.log out of the commit - the
    # driver would grep the on-disk file while litter lost the proof (found the hard
    # way, session denver-01; see AUTORESEARCH_WORK_LOG.md).
    git add -f denver/gates 2>/dev/null || true
    staged=$(git diff --cached --name-only)
    [ -n "$staged" ] || { log "commit: nothing staged"; return 0; }
    docs=$(printf '%s\n' "$staged" | grep -E '^(denver/(docs/|RESULT\.md|ESCALATE\.md|autoresearch\.sh|README|banner)|scripts/kvm/goose/denver-01)' || true)
    code=$(printf '%s\n' "$staged" | grep -vE '^(denver/(docs/|RESULT\.md|ESCALATE\.md|autoresearch\.sh|README|banner)|scripts/kvm/goose/denver-01)' || true)
    if [ -n "$code" ]; then
        pkgs=""
        for f in $code; do
            d=$(dirname "$f")
            # The workspace root's own manifest (and the lock) are not "a crate
            # touched": testing the whole root kernel for a members line would pay
            # a root-package host test every session. Member crates under it are
            # still tested through their own paths.
            if [ "$d" = "." ]; then continue; fi
            while [ "$d" != "." ] && [ ! -f "$d/Cargo.toml" ]; do d=$(dirname "$d"); done
            if [ -f "$d/Cargo.toml" ]; then
                name=$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$d/Cargo.toml" | head -1)
                if [ -n "$name" ]; then pkgs="$pkgs $name"; fi
            fi
        done
        pkgs=$(printf '%s\n' $pkgs | sort -u)
        for p in $pkgs; do
            log "check: cargo test -p $p (touched by this session)"
            cargo test -p "$p" >> "$LOG" 2>&1 || { log "commit: cargo test -p $p FAILED - code stays uncommitted"; return 4; }
        done
        git commit --no-verify -m "denver: gate $g - code; targeted checks:$pkgs" -- $code >> "$LOG" 2>&1 || { log "commit (code) failed"; return 4; }
        log "committed code for gate [$g]"
    fi
    if [ -n "$docs" ]; then
        git commit --no-verify -m "denver: gate $g - docs, records and runbook" >> "$LOG" 2>&1 || { log "commit (docs) failed"; return 4; }
        log "committed docs for gate [$g]"
    fi
    if git remote get-url litter >/dev/null 2>&1; then
        git push litter denver >> "$LOG" 2>&1 || log "PUSH FAILED to litter - push by hand before the next session"
    else
        log "no litter remote in this checkout - commits stay local"
    fi
    return 0
}

i=1; fails=0; quota_retries=0
while [ "$i" -le "$N" ]; do
    g=$(next_gate)
    [ -n "$g" ] || die "no NEXT GATE line in $LOOP and no forced gate - orient by hand"
    # The escape hatch is checked before anything fires: an open escalation from a previous
    # run stops the loop here; an answered one is archived and the redo proceeds.
    if [ -s denver/ESCALATE.md ]; then
        if [ "${ESCALATE_ANSWERED:-0}" = 1 ]; then
            mkdir -p denver/docs/archive/escalations
            eslug=$(printf '%s' "$g" | tr ' /:' '---')
            mv denver/ESCALATE.md "denver/docs/archive/escalations/$(date +%F)-$eslug.md"
            log "escalation answered - archived to denver/docs/archive/escalations/"
        else
            log "FATAL: escape hatch open (denver/ESCALATE.md) - answer it, then: ESCALATE_ANSWERED=1 sh denver/autoresearch.sh 1 [$g]"
            printf 'FATAL: escape hatch open - answer denver/ESCALATE.md, rerun with ESCALATE_ANSWERED=1\n' >&2
            rm -rf "$LOCK"; exit 5
        fi
    fi
    rc=0; session "$i" "$g" || rc=$?
    case $rc in
        0) fails=0; i=$((i+1)) ;;
        1) fails=0; i=$((i+1)); log "gate not green: continuing - the loop expects failures (runbook step 6)" ;;
        2) die "bookkeeping incomplete after session $i - fix by hand, see $RUNBOOK step 7" ;;
        3) die "intervention recorded in $RESULT - loop stops for human review (autonomy is per-phase)" ;;
        4) die "targeted checks red after session $i - code left uncommitted; see $LOG" ;;
        5) die "escape hatch open - answer denver/ESCALATE.md, rerun with ESCALATE_ANSWERED=1" ;;
        9) fails=$((fails+1))
           if [ "$fails" -ge 2 ]; then die "goose failed twice in a row - stopping"; fi
           log "retrying session $i after one goose failure"; i=$((i+1)) ;;
        8) quota_retries=$((quota_retries+1))
           if [ "$quota_retries" -gt "$QMR" ]; then die "quota still exhausted after $quota_retries wait(s) - stopping for a human"; fi
           log "quota wait done ($quota_retries/$QMR) - retrying session $i, same gate, budget untouched" ;;
        *) die "unexpected rc=$rc" ;;
    esac
done

log "=== done: $((i-1)) session(s); final NEXT GATE: [$(next_gate)] ==="
printf 'done; log %s; next gate: %s\n' "$LOG" "$(next_gate)"
