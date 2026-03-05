# Akuma Development Timeline Visualization

A proposal for generating an interactive timeline of the Akuma project's evolution — mapping goals, bugs, and capability milestones from the git history and docs corpus.

## Motivation

Akuma has grown from a bare-metal Rust kernel to a system that runs containers, a JS engine, an AI assistant, a POSIX shell, git, curl, and now bun — all in ~65 days. That story is recorded in the git log and 103 docs files, but it's invisible unless you know where to look. A timeline visualization would make the arc legible: what was planned, what broke, what shipped, and in what order.

This is useful for:
- Demo and talk preparation (the "From Zero to Claude Code" narrative)
- Onboarding: showing a new contributor how the system grew
- Retrospective: identifying where most time was spent (spoiler: networking)

## Data Sources

### 1. Git log — file creation dates

```bash
git log --diff-filter=A --follow --format="%ad %f" --date=short -- docs/
```

`--diff-filter=A` gives only the commit where each file was **first added**. This is the most meaningful timestamp: it marks when a goal was set, a bug was first discovered, or a capability was declared done.

### 2. Commit message sentiment

The commit messages themselves carry signal beyond dates:

| Pattern | Meaning |
|---|---|
| `"incredible"`, `"hell yeah"`, `"works"` | Capability milestone reached |
| `"oof"`, `"god damn"`, `"holy shit"`, `"hmm"` | Active bug fight |
| `"plan"`, `"strategy"`, `"proposal"`, `"docs"` | Goal / design phase |
| `"fix"`, `"investigation"`, `"analysis"` | Bug resolution |

### 3. Doc filename classification

The docs filenames map cleanly to three categories:

**Goals / Plans**
- `*_PLAN.md` — `CONTAINERS_STAGE_1_PLAN.md`, `PROPER_EXECVE_PLAN.md`, etc.
- `STRATEGY_*.md` — `STRATEGY_A_IMMEDIATE_TUNING.md`, `STRATEGY_B_SMOLTCP_MIGRATION.md`, etc.
- `refactor_plan.md`, `ON_DEMAND_ELF_LOADER.md`, proposals in general

**Bugs / Investigations**
- `*BUG*.md`, `*CORRUPTION*.md`, `*DEADLOCK*.md`
- `*INVESTIGATION*.md`, `*ANALYSIS*.md`
- `*_FIX*.md`, `*ISSUES*.md`, `ERRORS_TO_CHECK.md`

**Capabilities**
- Everything else: `SSH.md`, `HERD.md`, `QJS.md`, `MEOW.md`, `DOOM.md`, `PROCFS.md`, `SCRATCH.md`, `USERSPACE_NETWORKING_SUCCESS.md`, `TLS_INFRASTRUCTURE.md`, etc.

## Output Format

### Option A — Mermaid Timeline (simplest, renders in GitHub/Obsidian)

```
timeline
    title Akuma OS Development
    section Dec 2025
        Kernel foundation : SSH server, basic boot
    section Jan 2026 (early)
        Heap corruption : bug : FAR_0x5, stack corruption
        Threading plan : goal : fixed 32-thread pool
    section Jan 2026 (mid)
        Context switch fixed : capability
        Processes switch correctly : capability
    section Jan 2026 (late)
        Containers (herd) : capability
        Userspace networking : capability
        QuickJS : capability
    section Feb 2026 (early)
        smoltcp migration : goal
        TCP stream corruption : bug
        smoltcp working : capability
        paws shell : capability
    section Feb 2026 (mid)
        Linux syscall alignment : goal
        execve / fork : capability
        dash shell : capability
    section Feb 2026 (late)
        curl, apk, git : capability
        Socket exhaustion : bug
        Dynamic linker : capability
    section Mar 2026
        bun -h works : capability
        mmap / munmap crashes : bug
        Block cache : capability
        Node.js libuv plan : goal
```

### Option B — Mermaid Gantt (three swim lanes)

Three parallel rows — Goals, Bugs, Capabilities — with colored bars over a shared date axis. Better for showing overlap: e.g. that the smoltcp bug fight ran in parallel with planning the paws shell.

```
gantt
    dateFormat YYYY-MM-DD
    axisFormat %b %d

    section Goals
    Threading model          :done, 2026-01-03, 2026-01-16
    smoltcp migration plan   :done, 2026-02-07, 2026-02-14
    execve plan              :done, 2026-02-22, 2026-02-23
    Node.js libuv plan       :active, 2026-03-04, 2026-03-10

    section Bugs
    Heap corruption          :crit, done, 2026-01-02, 2026-01-05
    Context switch bugs      :crit, done, 2026-01-03, 2026-01-18
    SSH threading bug        :crit, done, 2026-01-15, 2026-01-22
    TCP stream corruption    :crit, done, 2026-02-07, 2026-02-14
    OOM / allocator          :crit, done, 2026-01-29, 2026-02-14
    Socket exhaustion        :crit, done, 2026-02-26, 2026-02-26
    Device MMIO conflict     :crit, done, 2026-03-03, 2026-03-04
    bun mmap crashes         :crit, active, 2026-03-04, 2026-03-06

    section Capabilities
    SSH server               :done, 2025-12-30, 2026-01-01
    Multithreading           :done, 2026-01-18, 2026-01-19
    Containers (herd)        :done, 2026-01-24, 2026-01-25
    Userspace networking     :done, 2026-01-24, 2026-01-25
    QuickJS                  :done, 2026-01-26, 2026-01-28
    meow + TLS               :done, 2026-01-30, 2026-01-31
    scratch (git clone)      :done, 2026-01-31, 2026-02-01
    smoltcp TCP working      :done, 2026-02-14, 2026-02-15
    paws shell               :done, 2026-02-13, 2026-02-14
    DOOM                     :done, 2026-02-20, 2026-02-21
    dash shell               :done, 2026-02-25, 2026-02-26
    curl                     :done, 2026-02-27, 2026-02-28
    dynamic linker           :done, 2026-02-27, 2026-02-28
    bun -h                   :done, 2026-03-01, 2026-03-02
    block cache              :done, 2026-03-04, 2026-03-05
```

### Option C — HTML/JS Interactive (most impressive for demos)

Use [vis-timeline](https://visjs.github.io/vis-timeline/docs/timeline/) or [Observable Plot](https://observablehq.com/plot/). Parse the git log with a small Python script, output JSON, render in a browser with hover tooltips showing the full commit message or doc excerpt.

The script pipeline would be:

```
git log → Python classifier → events.json → HTML + vis-timeline
```

Color coding: green = capability, red = bug, blue = goal/plan.

## Implementation Plan

### Script: `scripts/generate_timeline.py`

```python
import subprocess, json, re
from pathlib import Path

BUG_PATTERNS = re.compile(
    r'(BUG|CORRUPTION|FIX|INVESTIGATION|ANALYSIS|DEADLOCK|CRASH|ISSUES|ERRORS)',
    re.IGNORECASE
)
GOAL_PATTERNS = re.compile(r'(PLAN|STRATEGY|PROPOSAL)', re.IGNORECASE)

def classify(filename):
    if GOAL_PATTERNS.search(filename):
        return 'goal'
    if BUG_PATTERNS.search(filename):
        return 'bug'
    return 'capability'

result = subprocess.run(
    ['git', 'log', '--diff-filter=A', '--follow',
     '--format=%ad\t%f', '--date=short', '--', 'docs/'],
    capture_output=True, text=True
)

events = []
for line in result.stdout.strip().splitlines():
    date, slug = line.split('\t', 1)
    filename = slug.replace('-', '_') + '.md'
    events.append({
        'date': date,
        'file': filename,
        'category': classify(filename),
    })

print(json.dumps(sorted(events, key=lambda e: e['date']), indent=2))
```

Run with:

```bash
python3 scripts/generate_timeline.py > timeline_events.json
```

Then feed `timeline_events.json` to whatever renderer you choose (Mermaid template filler, vis-timeline HTML, or a Jupyter notebook with matplotlib).

## Recurring Arc

The data will visibly show a repeated 3-part pattern across every major subsystem:

```
[goal doc] → [1–3 days of bug docs] → [capability doc / "incredible" commit]
```

Examples:
- `STRATEGY_B_SMOLTCP_MIGRATION.md` → `TCP_SEQUENCE_UNDERFLOW_PANIC.md`, `TCPSTREAM_CORRUPTION_FIX.md` → `SMOLTCP_MIGRATION_SUMMARY.md`
- `PROPER_EXECVE_PLAN.md` → `FORK_MMAP_AND_WAIT_STATUS_FIX.md` → dash works
- `BUN_MISSING_SYSCALLS.md` → `BUN_MEMORY_STUDY.md`, crash logs → bun milestone

This arc is the story of Akuma and would make a compelling slide or demo narrative.

## Recommended Next Step

Start with the Mermaid Gantt (Option B) — it requires no tooling beyond a markdown file, renders inline in GitHub, and can be refined incrementally as new milestones land. Once the gantt is stable it can be promoted to an interactive HTML version for talks and demos.
