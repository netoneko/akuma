# Taoxichuan stage 10, session X: a crate-extraction plan, and the reference docs brought up to date

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: the epoll brief
(`scripts/kvm/goose/epoll-babysit.md`) is a separate, later task; this one is **reading and writing only**. **Keep your context small.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4` (first `git fetch origin rust-org-il-experiment` and merge it: the docs you must fix describe code that
landed there on 2026-10-30). Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
`taoxichuan/src` is 10,350 lines (`main.rs` 5,374, `child.rs` 2,341, `sock.rs` 1,029, `exec_twin.rs` 627) beside ~47 `akuma-*` path dependencies, and the full amd64 kernel in `amd64/src/`
implements most of the same things a second time. Every line that lives in only one of the two kernels is a candidate for a shared crate; every line that is neither
shared nor a crate is the thing the composition rule exists to prevent. The user wants **a plan**, not an extraction: you write one document and update docs, and you change **no kernel code**.
**The metal VM (`kvm0`) is busy with the supervisor's fixed-point run for the next hours. Do not boot a guest, do not touch `vm-dev` / `vm-serve-mc` or anything under `/etc/herd/`, and do not
install anything to `/boot`.** Host tests (`cargo test -p <crate> --target $HOST`) and `cargo check` are fine.

## Read first (in order; `sed -n` the named sections, never whole files)
1. `taoxichuan/docs/README.md`, then `taoxichuan/docs/reference/switch-to-crates.md` - the existing plan, if any; your document extends it, it does not repeat it.
2. `docs/reference/crate-safety.md` and `ls crates/` - which crates exist and which forbid `unsafe`. `CLAUDE.md` § Layout is the long form of the extraction rules (leaf crates, `forbid(unsafe_code)`, `amd64` not `x86`).
3. `docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md` and `docs/archive/TAOXICHUAN_FIXED_POINT.md` - what broke last, and where in `taoxichuan/src`.
4. `diff <(grep '^akuma-' amd64/Cargo.toml | cut -d' ' -f1 | sort) <(grep '^akuma-' taoxichuan/Cargo.toml | cut -d' ' -f1 | sort)` - the crates amd64 uses and Taoxichuan does not.
5. The code, by file, with `grep -n '^pub fn\|^fn\|^static\|^struct\|^enum\|^impl' taoxichuan/src/<file>.rs` first and `sed -n` windows after.

## What to write: `docs/archive/TAOXICHUAN_CRATE_EXTRACTION_PLAN.md`
One table per kernel file (`main.rs`, `child.rs`, `sock.rs`, `exec_twin.rs`, `net.rs`, `pipes.rs`, `futex.rs`), a row per cluster of functions (name the functions and line ranges):
| cluster | lines | what it decides | already in a crate? (path) | duplicated in `amd64/src/`? (path) | proposed crate name | pure? (host-testable) | `unsafe` it would take / shed | order |
Then, in this order:
1. **Ranked list** of the ten extractions with the best value (lines removed from `taoxichuan/src` + lines deduplicated against `amd64/src/`, divided by the risk), each with the seam stated in one sentence
   the way `CLAUDE.md` states them (what the crate owns, what it must NOT depend on, which side keeps the locks and the `unsafe`).
2. **Seams that are in the wrong place** today (a decision welded to a lock, a `static mut` that is really a per-slot table). `main.rs`'s `task()` / `task_slots()` and `child.rs`'s `slots()` hand out long-lived `&mut` to `static mut`s; say which extraction would remove them, and what a `BorrowCell`-closure version costs.
3. **Gates** per extraction: which test or probe proves behaviour is unchanged (`taoxichuan/docs/runbooks/`, `scripts/kvm/goose/run_gates.sh`, `scripts/kvm/goose/cow_leak_gate.py`, the `userspace/*probe*` set), and the A/B you would run.
4. **Unsafe-site count** now (`grep -c 'unsafe' taoxichuan/src/*.rs`) and the count after each extraction.
Premises (examine before writing): the numbers above are supervisor figures from 2026-10-30; re-run `wc -l taoxichuan/src/*.rs`, the dependency diff, and `ls crates/ | wc -l`, and put one line each (HOLDS / WRONG / UNCHECKED) at the top of the plan.

## Docs (mandatory): bring `taoxichuan/docs/` up to date
It has not been updated since the 2026-10-30 fixes landed. Read each of `taoxichuan/docs/reference/*.md` against the current code and fix what is stale; **current behaviour and invariants only**, with the crate that owns each decision, no incident stories (those go in `docs/archive/`, which already has `TAOXICHUAN_BUGS_FOUND_2026_10_30.md`). At minimum:
- `memory.md`: the 16 GiB mmap slab, the flat CoW share table, W^X, the thread-exit process-table fix.
- `processes.md` / `scheduling.md`: `close(0)` then `open`, `setsid`, `execve` argument limits (E2BIG), the guest clock.
- `syscalls.md`: a row for every arm added since it was last edited (`grep -n 'nr::\|=> ' taoxichuan/src/main.rs` against its table), and the still-missing list (epoll 213/232/233/281/291, `prctl` 157, statx 326, sendfile 40, linkat 265, tkill 200, getrusage 98).
- `gaps.md`: G4 (a boot program cannot fork), the guest sshd's lossy large output, the `-j8` crash, the epoll gap, each with the probe or command that shows it.
- `README.md` status table; redraw any figure in `diagrams/` whose behaviour changed (mermaid, one figure per file).

## Composition rule
This session writes no kernel code, so rules 1-4 apply to the **plan**: every row must say *pulled / extract / write new (why)* and name the existing crate if there is one. Do not propose a crate that already exists under another name.

## Gates
- G1: `test -s docs/archive/TAOXICHUAN_CRATE_EXTRACTION_PLAN.md` and it contains one table per file listed above plus the four numbered sections.
- G2: `git diff --stat origin/rust-org-il-experiment -- taoxichuan/src crates amd64` prints nothing (no code changed).
- G3: `git diff --stat -- taoxichuan/docs` shows `memory.md`, `processes.md`, `syscalls.md`, `gaps.md` and `README.md` changed, and every file path or crate name they mention exists (`ls`).
- Out of scope: any code, any boot, the epoll work, `amd64/` docs.

## Ops traps
- Never write `/boot/akuma-amd64*`; this session writes no `/boot` file at all. Do not touch `/etc/herd/`. Do not stop or restart `goose-glm`.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box** (it becomes a REPL and hangs your tool call). Write the script to a file and run `python3 /path/x.py < /dev/null`.
- Never glob or `ls` the repo root (1000+ files); name a subdirectory.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after the plan's first complete table and after every doc you finish. WIP commits are fine. Never push `origin`.

## Budget
About 80 tool calls. Reading costs context: use `grep -n` indexes and 60-line `sed -n` windows. At 40 calls without a commit the supervisor will nudge you. If you are past the budget with the plan unfinished, commit what you have, list the unread files in RESULT, and stop.

## When to stop
G1-G3 hold, or 80 calls, or the quota error. Write `/root/dev-cycle/taoxichuan-s8/RESULT-X10.md` (premise lines, the ten ranked extractions in one line each, which docs changed, what you could not verify) and end the turn.
