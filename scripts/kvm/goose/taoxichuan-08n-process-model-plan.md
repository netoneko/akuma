# Taoxichuan stage 8, session N: PLAN - the process model (fork / clone / execve / wait4 / pipes) for stages 8-11 (planning only, no boots, no kernel code)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: M (`RESULT-M.md`, apk add curl). **Keep your context small. This session boots nothing and changes no kernel code**: its output is one plan document, like session G's `TAOXICHUAN_STAGE6_REST_PLAN.md`.
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4` (fetch `litter` first if behind). Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
`taoxichuan/` has no `fork`/`clone`. It has one process shape: `SPAWN`, a run-to-completion nested child (`taoxichuan/src/child.rs`, about 700 lines) whose parent resumes when the child exits. That is enough for `apk`, busybox and single commands; it is why every gate is one simple command per ssh exec
(`sh -c 'a; b'` cannot run `a`). The rest of the ladder (`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, stage table) needs more: stage 7 (curl HTTPS) is one process; **stage 8 (`git clone`) forks and execs helpers (`git-remote-https`, `index-pack`), connects them with pipes and waits on them; stages 9-11 (cargo, rustc) add threads
and many children.** The full amd64 kernel already does all of this: fork with copy-on-write, a real process per thread, execve, wait4, pipes. That code is welded to `Process` in `akuma-exec`. Per the composition rule, the answer is to **cut that seam and reuse it**, not to write a second fork in `taoxichuan/`. This session finds out how, and in what order.

## What to produce
`docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md`, committed and pushed to `litter taoxichuan-s4`, with these sections (keep it under 250 lines):
1. **Needs per stage**: for each stage 7-12 of the table, the exact process-model calls it needs (name the syscalls and the real programs: what does `git clone` over HTTPS spawn, what does `cargo build` spawn; read musl's `posix_spawn`/`system`/`popen` and busybox ash's fork use only as far as needed to name the calls). Mark which are already served.
2. **Inventory of the amd64 path**: for `fork`/`vfork`/`clone` (thread and process), `execve`, `wait4`/`waitid`, `exit`/`exit_group`, `pipe2`, `dup`, `kill`/signals the children need: where each lives (`amd64/src/{thread,exec_runtime,usermode}.rs`, `crates/akuma-exec/src/process/{spawn,lifecycle,children,table}.rs`, and so on), which crate owns each piece
   (`akuma-exec`, `akuma-exec-core`, `akuma-user-space`, `akuma-mmap`, `akuma-cow`, `akuma-fdtable`, `akuma-pipe`/`akuma-pipes`, `akuma-elf`, `akuma-ring3-amd64`), and what each is **welded to**. A table: piece | where | crate | welded to | line count.
3. **Dependency diff**: `amd64/Cargo.toml`'s `akuma-*` list against `taoxichuan/Cargo.toml`'s; which of the unused ones cover each piece. Do not guess: a path dependency that compiles for `x86_64-unknown-none` without `akuma-exec`'s heavy dependencies is a fact you can check (`cargo tree -p <crate> --target x86_64-unknown-none -e normal | head -40`, host side or on the box, capped).
4. **Seam options per piece**: leaf crate already exists / cut a leaf crate out (as `akuma-exec-core` and `akuma-sntp` were) / cargo feature / `taoxichuan` keeps its own tiny version and why. For each choice say what it costs in lines and in risk; recommend one. The honest alternative to weigh: making `taoxichuan` depend on `akuma-exec` whole (it is then close to the amd64 kernel; say how close and what is lost).
5. **Milestones**: an ordered list, each with a mini-gate (exact command over `/bin/ssh -p 2224 root@localhost '<cmd>'`, one simple command per exec unless the milestone is the one that lifts that limit), boots and tokens as guesses, and the first milestone being the cheapest one that lifts the biggest limit (probably `sh -c 'a; b'` working: `fork`+`execve`+`wait4`). Include where `PT_INTERP` (M5 of the stage-6 plan) fits, since it is also an exec path.
6. **Unknowns that could kill the plan**: each with a read-only settle (a grep, a `cargo tree`, a doc read). Examples to check, not to assume: is CoW fork usable without `akuma-exec`'s page-table walker; what `execve` needs from the loader that `taoxichuan` already has; how threads are scheduled on this single-core kernel.
7. **What this changes in the ladder**: say plainly whether stages 8-12 are reachable by this route and what the per-stage cost looks like.

## Read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md` - the stage table and § Process; `docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md` (the template for your output) - § 0 and § 5; `docs/archive/TAOXICHUAN_STAGE7_TRIM_PLAN.md` - § 3 "H3" (why the one-line `;` gate cannot pass, option H3-f).
2. `docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md`, `AKUMA_EXEC_AUDIT.md`, `CLONE_AUDIT.md`, `AKUMA_AMD64_EXECVE_INSTALL_IMAGE.md`, `AMD64_FORK_VS_THREADS_SEGV.md` (headers and conclusions only: `sed -n 1,50p`). Use `docs/README.md`'s subsystem index and `grep -il "fork_process\|akuma-exec-core" docs/archive | head` for anything else; the amd64 fork/clone/execve folds are described in the archive docs named `AKUMA_AMD64_*`.
3. `docs/reference/subsystems/fdtable.md` and `docs/reference/crate-safety.md` (the crate map).
4. Code only as far as the table needs: `grep -n "fn sys_fork\|fn fork_process\|fn sys_clone\|fn sys_execve\|fn sys_wait4" amd64/src/*.rs crates/akuma-exec/src/process/*.rs | head -30`, then `sed -n` windows around each. `taoxichuan/src/child.rs` for what `SPAWN` does today.

## Composition rule (mandatory, and the whole point)
The plan must not propose writing fork/clone/execve/wait4 machinery in `taoxichuan/` unless section 4 shows that every existing implementation is welded beyond a reasonable cut and says why. Prefer: pull a crate; else extract a leaf crate from `amd64/`/`akuma-exec` and make both kernels depend on it; else cut a seam. State for each piece which of *pulled / extracted / cut / written new* the plan chooses, and the expected effect on `wc -l taoxichuan/src/*.rs` (4,856 now; 19 `akuma-*` dependencies).

## Cost rules
Cap every read (`| cut -c1-180 | head -60`, `grep -n -m 20`); never `cat` a source file; do not read whole docs; keep your own notes in the plan file as you go (write it early, refine it) so a lost context loses nothing. Expect about 60-100 tool calls and under 3M input tokens; if you pass 150 calls, write what you have and stop.

## Gates
- The plan file exists with all seven sections, every file path and command in it checked (a path that does not exist, or a `cargo tree` that was not run, is marked "unchecked").
- Zero boots, zero kernel or crate source changes (the only diff is the plan file and, if you add them, probe scripts clearly named). `git status` clean otherwise.

## Ops traps
- Do not start, stop or restart any VM or herd service; do not touch `/boot`; the box may be used for other work. This session only reads the tree.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box.** Never run goose/crush wrappers on the box.

## Commit rule (you have no clock)
Commit the plan file and `git push litter taoxichuan-s4` when sections 1-4 exist, and again at the end. Never push `origin`.

## When to stop
All seven sections written and checked, or 150 tool calls, or the quota error. Then write `/root/dev-cycle/taoxichuan-s7/RESULT-N.md` (the plan's conclusion in 20 lines: the recommended route, the first milestone and its gate, the biggest unknown) and end the turn.
