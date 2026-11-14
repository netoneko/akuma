#!/bin/sh
# denver/bookkeep.sh - the loop's bookkeeping: read-only checks by default, targeted
# repairs and digests behind flags. Run as `sh denver/bookkeep.sh` (busybox ash; no
# bashisms). Sessions and the driver call this instead of cat-ing LOOP_LOG.md (39k) or
# REQUIREMENTS.md (18k) into context: quota = context size x calls.
#
#   --fix            rewrite the newest `## `/`## Gate` LOOP_LOG heading into the canonical
#                    `### <GATE> - <date>, session <n>` form (the driver counts only `### `
#                    headings; session 5 once lost its entry to a `## ` one)
#   --digest         (re)write denver/docs/STATE.md, at most 60 lines: current gate, last
#                    verdict, ranked open requirements, last three INTERVENTION lines,
#                    rig/kernel stamp, file map. Generated - never hand-edit it.
#   --cost [--append]
#                    print the newest `denver-*` goose session's spend line from
#                    sessions.db (aggregates only - never a token or a key); --append adds
#                    the line to denver/docs/COST_LEDGER.md
#
# Checks (one MISSING:/FIXED:/OK: line each; exit 1 if anything is missing):
#   1. the newest gate entry in LOOP_LOG.md has a `### ` heading
#   2. LOOP_LOG.md ends with exactly one `NEXT GATE: <name>` line (one in the newest entry,
#      and it is the last non-empty line)
#   3. denver/gates/<last gate>.log exists, is git-tracked (the root `*.log` ignore trap)
#      and carries a RESULT PASS / RESULT FAIL line
#   4. RESULT.md has an entry for that gate; INTERVENTION lines without a date are listed
#   5. how many commits denver is behind litter/main (after `git fetch litter main`);
#      warns above 50
set -u
cd "$(dirname "$0")/.."   # repo root

LOOP=denver/docs/LOOP_LOG.md
RESULT=denver/RESULT.md
REQ=denver/docs/REQUIREMENTS.md
GATES=denver/gates
STATE=denver/docs/STATE.md
LEDGER=denver/docs/COST_LEDGER.md

FIX=0; DIGEST=0; COST=0; APPEND=0
for a in "$@"; do
    case "$a" in
        --fix) FIX=1 ;;
        --digest) DIGEST=1 ;;
        --cost) COST=1 ;;
        --append) APPEND=1 ;;
        *) printf 'bookkeep: unknown flag %s\n' "$a" >&2; exit 2 ;;
    esac
done

missing=0

# helpers ------------------------------------------------------------

count() { # pattern file -> match count (0 if file absent / no match)
    n=$(grep -c "$1" "$2" 2>/dev/null) || n=0
    printf '%s' "${n:-0}"
}

newest_heading_line() { # -> "NN:## text" of the last line starting with '#'
    grep -n '^#' "$LOOP" 2>/dev/null | tail -1
}

# gate names of the `### ` entries, newest first
gate_names_newest_first() {
    grep '^### ' "$LOOP" 2>/dev/null | sed 's/^### //' | cut -d' ' -f1 \
        | awk '{a[NR]=$0} END{for(i=NR;i>=1;i--) print a[i]}'
}

last_gate() { # -> the newest `### ` entry that actually has a gate log
    gate_names_newest_first | while read -r cand; do
        [ -s "$GATES/$cand.log" ] && { printf '%s\n' "$cand"; exit 0; }
    done | head -1
}

fix_headings() { # repair the newest `## ` heading into the canonical `### ` form
    FIXED_N=0
    h=$(newest_heading_line)
    case "$h" in
        *':## '*) ;;
        *) return 0 ;;
    esac
    ln=$(printf '%s\n' "$h" | cut -d: -f1)
    text=$(sed -n "${ln}p" "$LOOP" | sed 's/^## *//')
    case "$text" in
        Gate*|gate*|PHASE-*) ;;          # gate-entry-shaped headings only: never touch
                                         # a body's own `## ` sub-heading
        *) return 0 ;;
    esac
    gate=$(printf '%s\n' "$text" | cut -d' ' -f1)
    if [ "$gate" = "Gate" ] || [ "$gate" = "gate" ]; then
        gate=$(printf '%s\n' "$text" | cut -d' ' -f2)
    fi
    date=$(printf '%s\n' "$text" | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}' | head -1)
    [ -n "$date" ] || date=$(date +%F)
    sess=$(( $(count '^### ' "$LOOP") + 1 ))
    sed -i "${ln}s|^## .*|### ${gate} - ${date}, session ${sess}|" "$LOOP" && FIXED_N=1
}

digest() { # regenerate denver/docs/STATE.md (<= 60 lines, never hand-edited)
    tmp=$(mktemp /tmp/denver-state.XXXXXX 2>/dev/null) || tmp=/tmp/denver-state.$$
    ng=$(sed -n 's/^NEXT GATE: *//p' "$LOOP" 2>/dev/null | tail -1)
    lg=$(last_gate)
    verd=
    [ -n "$lg" ] && verd=$(awk '/RESULT PASS|RESULT FAIL/{v=$0} END{print v}' "$GATES/$lg.log" 2>/dev/null)
    # ids from the newest 'New order:' sentence only (a re-rank paragraph can mention
    # other G-ids earlier in the line), deduped in order. Single awk process: multi-stage
    # pipelines as direct commands inside a { } > file group have raced on this box's
    # busybox ash (stages ran out of order and truncated output - seen 2026-11-14).
    ids=$(awk '/New order/{l=$0} END{i=index(l,"New order"); if(i==0)exit; s=substr(l,i+9)
        while(match(s,/G[0-9]+/)){t=substr(s,RSTART,RLENGTH); if(!(t in seen)){seen[t]=1; printf "%s ", t} s=substr(s,RSTART+RLENGTH)}}' "$REQ" 2>/dev/null)
    [ -n "$ids" ] || ids=$(grep -oE '^\| G[0-9]+' "$REQ" 2>/dev/null | sed 's/[^G0-9]//g' | awk '!seen[$0]++')
    {
        echo "# denver state digest (generated by: sh denver/bookkeep.sh --digest; NEVER hand-edit)"
        echo "generated: $(date -u '+%F %T UTC')  head: $(git rev-parse --short HEAD 2>/dev/null)  branch: $(git rev-parse --abbrev-ref HEAD 2>/dev/null)"
        echo "current gate (next to run): ${ng:-UNKNOWN}"
        echo "last gate verdict: ${lg:-none}: ${verd:-no verdict line}"
        echo ""
        echo "## ranked open requirements (REQUIREMENTS.md, newest 'New order' first)"
        for id in $ids; do
            line=$(grep -m1 "^| $id |" "$REQ" 2>/dev/null \
                | awk -F'|' '{s=$3; gsub(/~~/,"",s); gsub(/^ +| +$/,"",s); print s}' \
                | cut -c1-130)
            case "$line" in *suspects*) continue ;; esac   # suspects are not open gaps
            echo "  $id - ${line:-?}"
        done
        echo ""
        echo "## last three INTERVENTION lines (RESULT.md)"
        # captured then printed: direct external output inside a { } > file group loses
        # its stdout on this box (busybox ash / Akuma kernel - grep and awk both seen
        # dropped or truncated here, 2026-11-14); command substitution is reliable.
        iv=$(awk '/INTERVENTION/{k[++c]=NR; a[NR]=$0} END{s=c-2; if(s<1)s=1; for(i=s;i<=c;i++){m=a[k[i]]; if(length(m)>160)m=substr(m,1,160); print "  "m}}' "$RESULT" 2>/dev/null)
        printf '%s\n' "$iv"
        echo ""
        echo "## rig / kernel stamp"
        newest=$(ls -t "$GATES"/*.log 2>/dev/null | head -1)
        uline=; nname=
        if [ -n "$newest" ]; then
            nname=$(basename "$newest")
            uline=$(awk '/0\.1\.0-.*-denver/{u=$0} END{print u}' "$newest" 2>/dev/null)
        fi
        echo "  guest uname (newest gate log by mtime: ${nname:-none}): ${uline:-not captured}"
        echo "  rig dir: /root/denver/w ($(ls /root/denver/w 2>/dev/null | tr '\n' ' ' | cut -c1-200))"
        echo ""
        echo "## file map"
        echo "  probes: $(ls denver/probe-*.sh denver/probes/ 2>/dev/null | tr '\n' ' ')"
        echo "  gate logs: $(ls "$GATES"/*.log 2>/dev/null | tr '\n' ' ')"
        echo "  research artifacts: $(ls denver/docs/research/*.md 2>/dev/null | tr '\n' ' ')"
        echo "  rigs: /root/denver/w (kernel binaries, specs, key, console vmrun.log)"
    } > "$tmp"
    head -n 60 "$tmp" > "$STATE" 2>/dev/null || cat "$tmp" > "$STATE"
    rm -f "$tmp"
    printf 'OK: wrote %s (%s lines)\n' "$STATE" "$(wc -l < "$STATE")"
}

cost() { # newest denver-* goose session -> one aggregate line (never a token or a key)
    rc=0
    python3 - "$APPEND" "$LEDGER" <<'PYEOF' || rc=1
import sqlite3, sys, json, datetime, os
append = sys.argv[1] == '1'
ledger = sys.argv[2]
db = sqlite3.connect('file:/root/.local/share/goose/sessions/sessions.db?mode=ro', uri=True)
cur = db.cursor()
row = cur.execute(
    "SELECT name,input_tokens,accumulated_input_tokens,accumulated_output_tokens,"
    "accumulated_cache_read_tokens,created_at,updated_at FROM sessions "
    "WHERE name LIKE 'denver-%' AND input_tokens IS NOT NULL "
    "ORDER BY created_at DESC, updated_at DESC LIMIT 1").fetchone()
if row is None:
    print('MISSING: no denver-* goose session in sessions.db')
    sys.exit(1)
name, fin, ain, aout, acr, ca, ua = row

def num(v): return v if isinstance(v, (int, float)) else 0
def ts(v):
    try: return datetime.datetime.strptime(str(v)[:19], '%Y-%m-%d %H:%M:%S')
    except Exception: return None

tools = 0
for (cj,) in cur.execute('SELECT content_json FROM messages WHERE session_id IN '
                         '(SELECT id FROM sessions WHERE name=?)', (name,)):
    try:
        parts = json.loads(cj)
        if isinstance(parts, dict): parts = [parts]
        for p in parts or []:
            if isinstance(p, dict) and p.get('type') == 'toolRequest':
                tools += 1
    except Exception:
        pass
t0, t1 = ts(ca), ts(ua)
wall = round((t1 - t0).total_seconds() / 60) if t0 and t1 else 0
ain = num(ain)
line = ('session %s wall=%d in=%.1fM out=%dk final_ctx=%dk cache_read=%d%% tools=%d'
        % (name, wall, ain / 1e6, round(num(aout) / 1e3), round(num(fin) / 1e3),
           round(100 * num(acr) / ain) if ain else 0, tools))
print(line)
if append:
    d = os.path.dirname(ledger)
    if d and not os.path.isdir(d):
        os.makedirs(d, exist_ok=True)
    if not os.path.exists(ledger):
        with open(ledger, 'w') as f:
            f.write('# denver cost ledger - one line per goose session\n'
                    '# generated by `sh denver/bookkeep.sh --cost --append`; aggregates only\n')
    with open(ledger, 'a') as f:
        f.write(line + '\n')
    print('OK: appended to %s' % ledger)
PYEOF
    [ "$rc" -ne 0 ] && missing=1
}

# flag actions run first, so the checks below see the fixed/generated state ----------
[ "$COST" = 1 ] && cost
[ "$DIGEST" = 1 ] && digest

# check 1: the newest gate entry has a `### ` heading --------------------------------
if [ ! -r "$LOOP" ]; then
    printf 'MISSING: %s not readable\n' "$LOOP"; missing=1
else
    h=$(newest_heading_line)
    if [ "$FIX" = 1 ] && [ -n "$h" ]; then fix_headings; h=$(newest_heading_line); fi
    case "$h" in
        *':### '*)
            if [ "$FIX" = 1 ] && [ "${FIXED_N:-0}" = 1 ]; then
                printf 'FIXED: rewrote the ## heading to the ### <GATE> - <date>, session <n> form\n'
            else
                printf 'OK: newest gate entry has a ### heading\n'
            fi ;;
        *':## '*)
            printf 'MISSING: newest gate entry is under a ## heading (the driver counts only ###; --fix repairs gate-shaped ones)\n'
            missing=1 ;;
        *)
            printf 'MISSING: %s has no gate entries at all\n' "$LOOP"; missing=1
            ;;
    esac
fi

# check 2: ends with exactly one NEXT GATE line --------------------------------------
if [ -r "$LOOP" ]; then
    last=$(tail -n 8 "$LOOP" 2>/dev/null | grep -v '^[[:space:]]*$' | tail -1)
    lasth=$(grep -n '^### ' "$LOOP" 2>/dev/null | tail -1 | cut -d: -f1)
    after=$( { [ -n "$lasth" ] && tail -n +$((lasth + 1)) "$LOOP" || cat "$LOOP"; } \
        | grep -c '^NEXT GATE:' ) || after=0
    case "$last" in
        "NEXT GATE: "?*) ng_ok=1 ;;
        *) ng_ok=0 ;;
    esac
    if [ "$ng_ok" = 1 ] && [ "${after:-0}" = 1 ]; then
        printf 'OK: ends with exactly one NEXT GATE line (%s)\n' \
            "$(sed -n 's/^NEXT GATE: *//p' "$LOOP" | tail -1)"
    else
        printf 'MISSING: LOOP_LOG must end with exactly one NEXT GATE line (last-line-ok=%s, in-newest-entry=%s)\n' \
            "$ng_ok" "${after:-0}"
        missing=1
    fi
fi

# check 3: the last gate's log exists, is tracked, carries a verdict -----------------
LG=$(last_gate)
if [ -z "$LG" ]; then
    printf 'MISSING: no ### entry in %s has a gate log under %s\n' "$LOOP" "$GATES"
    missing=1
else
    if [ ! -s "$GATES/$LG.log" ]; then
        printf 'MISSING: %s/%s.log does not exist\n' "$GATES" "$LG"; missing=1
    else
        ok3=1
        if git ls-files -- "$GATES/$LG.log" | grep -q .; then
            :   # tracked - the root *.log ignore trap otherwise eats the evidence
        else
            printf 'MISSING: %s/%s.log is not tracked by git (the *.log ignore trap; git add -f it)\n' "$GATES" "$LG"
            missing=1; ok3=0
        fi
        if grep -q 'RESULT PASS\|RESULT FAIL' "$GATES/$LG.log"; then
            :   # the marker convention the driver greps
        else
            printf 'MISSING: %s/%s.log carries no RESULT PASS / RESULT FAIL line\n' "$GATES" "$LG"
            missing=1; ok3=0
        fi
        [ "$ok3" = 1 ] && printf 'OK: gate log %s/%s.log exists, tracked, verdict present\n' "$GATES" "$LG"
    fi
fi

# check 4: RESULT.md has an entry for that gate; list undated INTERVENTION lines -----
if [ -r "$RESULT" ] && [ -n "${LG:-}" ]; then
    if grep -q "^#.*${LG}" "$RESULT"; then
        printf 'OK: RESULT.md has an entry for %s\n' "$LG"
    else
        printf 'MISSING: RESULT.md has no entry for %s\n' "$LG"
        missing=1
    fi
    grep -n 'INTERVENTION' "$RESULT" 2>/dev/null | \
    while IFS=: read -r ln rest; do
        case "$rest" in
            *[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]*) ;;
            *) printf 'note: INTERVENTION without a date, line %s: %s\n' "$ln" \
                "$(printf '%s\n' "$rest" | cut -c1-90)" ;;
        esac
    done
fi

# check 5: drift behind litter/main (warn above 50) ----------------------------------
if git fetch litter main >/dev/null 2>&1; then
    behind=$(git rev-list --count denver..litter/main 2>/dev/null) || behind=
    if [ -n "$behind" ]; then
        printf 'OK: branch denver is %s commit(s) behind litter/main\n' "$behind"
        if [ "$behind" -gt 50 ]; then
            printf 'note: >50 commits behind litter/main - consider a merge\n'
        fi
    else
        printf 'MISSING: no litter/main ref to compare against\n'
        missing=1
    fi
else
    printf 'MISSING: git fetch litter main failed (offline? no litter remote?)\n'
    missing=1
fi

if [ "$missing" -ne 0 ]; then exit 1; fi
exit 0
