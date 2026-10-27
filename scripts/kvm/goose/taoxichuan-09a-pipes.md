# Taoxichuan stage 8 (git clone), M3 session pa: `pipe2` and shell pipelines, gates P1-P5

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: `RESULT-Y.md`,
commit `e61f9ee9` on origin (the box's `ee1e1874` plus `260377ff` carry the same code). **Keep your context small.** Checkout
`/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
`fork` works (M2): `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` exits 0. It is a **blocking** fork: the child runs to completion inside
the `fork` syscall, then the parent resumes (`taoxichuan/src/child.rs`, `fork_arm`). That is throwaway scaffolding (plan M4 replaces it with
`akuma-exec`), so **do not make it smarter**. This session adds only what a *small* pipeline needs on top of it: a pipe whose writer finishes
before its reader starts. With `a | b`, ash forks `a` first; `a` writes (at most the 64 KiB cap) and exits, then ash forks `b`, which reads
and sees EOF. Anything bigger or concurrent (git's helper pump, `yes | head`) is M4's scheduler and is **out of scope**.
The pipe table and buffer already exist, host-tested: `crates/akuma-pipes` (end refcounts, waiters, `create/write/read/close_write/close_read`)
over `crates/akuma-pipe` (the 64 KiB FIFO). amd64 consumes them through `amd64/src/fd.rs` `sys_pipe2` and `amd64/src/pipe.rs`. **Pull them; do
not write a FIFO.** Both `cargo check` clean for `x86_64-unknown-none`.

## Read first (in order; `sed -n` the named sections, never whole files)
1. `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` - § 4 rows "pipes" and "fd dispatch", § 5 M3 - what was decided and why.
2. `crates/akuma-pipes/src/lib.rs` lines 1-70 and the `pub fn` list - "wakes are returned, never performed"; you ignore the wakes (nothing blocks).
3. `amd64/src/fd.rs:1073-1130` (`sys_pipe2`) and `amd64/src/pipe.rs` - the other consumer; copy its *shape*, not its fd-table plumbing.
4. `taoxichuan/src/child.rs` `fork_arm` (the "fd table" paragraph, ~line 750) - the child's table is `akuma-fdtable`'s `fork_copy`; its comment says
   **"no kernel-side references to take"** because descriptions are indices. A pipe end is a *counted* kernel resource: `fork_copy` returns the
   payloads in `forked.names` that "need exactly one kernel-side reference taken" - that is where `PipeTable::clone_ref` belongs.
5. `taoxichuan/src/main.rs` `ring3::nr::CLOSE` arm (~2111): sockets and eventfds release their kernel side there; a pipe end must too, or EOF never arrives.
**Probes already exist - use one before writing a new one**: `userspace/ncaprobe` (`epoll` arm = raw `pipe`+`posix_spawn`), `userspace/forktest/c_stress/`
(`pipewake.c` is threaded - not for this session; `spawnprobe.c`, `grandfork.c`), `userspace/forkprobe` (libakuma, may build for aarch64 only).
Docs front door: `docs/README.md`.

## Premises (re-run each check before your first edit; one line per row in RESULT-pa.md: HOLDS / WRONG (what you saw) / UNCHECKED)
| # | premise | read-only check | supervisor saw |
|---|---|---|---|
| 1 | taoxichuan serves no `pipe2` yet | `grep -c "nr::PIPE2" taoxichuan/src/*.rs` | 0 in all four files (the const exists: `crates/akuma-ring3-amd64/src/lib.rs:955`) |
| 2 | the pipe crates build for the kernel target | `cargo check -p akuma-pipe -p akuma-pipes --target x86_64-unknown-none` | Finished, no errors |
| 3 | `taoxichuan/Cargo.toml` lists neither crate | `grep -n "akuma-pipe" taoxichuan/Cargo.toml` | no match: neither is a dependency |
| 4 | `OpenFile` has no pipe variant; 6 `ChildOut` match sites show where variants are handled | `grep -n "OpenFile::ChildOut" taoxichuan/src/*.rs` | 6 sites |
| 5 | `fork_copy` hands back the payloads needing a reference | `sed -n 370,400p crates/akuma-fdtable/src/lib.rs` (the `Forked` type) | `Forked { names, cloexec, nonblock }`; `names` = payloads needing one reference each (`akuma-fdtable/src/lib.rs:485`) |
| 6 | ash runs each member of `a \| b` in a forked child, even for a builtin `echo` | read busybox 1.35 `ash.c` `evalpipe` (or: boot, `echo hi \| cat`, count `[fork]` lines in the console) | UNCHECKED - I believe yes (plan § 1) |
| 7 | fds 0/1/2 are not in the fd table; a `dup2(pipe_w, 1)` must make fd 1 table-backed | `sed -n 2595,2650p taoxichuan/src/main.rs` (DUP2/DUP3) and the F1 note in the agent log, last section | F1's `> /tmp/f` redirect works through this path; pipe payloads untested |
| 8 | a fork child's exit closes its fds (so the writer's end drops and the reader sees EOF) | `grep -n "close_all\|fork_copy\|child_exit" taoxichuan/src/child.rs` | only `child_exit` (child.rs:602) exists and `fork_arm` calls `fork_copy` once (756); whether exit closes the child's table is UNCHECKED - the likely bug |
If a WRONG row changes the plan, write it to RESULT-pa.md and commit at once.

## Symptom first (when a gate fails, before any source read or new probe)
`grep -n -i "<the error text you saw>" docs/README.md docs/runbooks/*.md`; write the matching rows (or "no row") in RESULT-pa.md. Relevant archive docs
already on disk: `docs/archive/TOKIO_PIPE_EPOLL_HANG.md`, `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH3C.md` (pipe2/dup on amd64); more: `grep -il pipe2 docs/archive/*.md`.

## Guest facts (verified; do not rediscover)
Fork exists only inside the spawned shell (depth 1): a fork inside a fork child returns EPERM. So `sh -c 'a | b | c'` works only if ash forks every member
from the shell itself (it does: the shell forks, children do not). One core. No procfs. The host forward degrades near 30 ssh connections per boot; the
suite `scripts/kvm/goose/run_pipes.sh` uses 5 execs. busybox 1.35 has no `stat`; `/etc/hostname` is not in the image. Static and static-PIE binaries run.
`PACE=15` s after every exec is deliberate; do not shorten it in this session.

## Composition rule (mandatory)
`taoxichuan/` is a composition root. For every arm or helper over ~20 lines: look first (`grep -rn <name> crates/ amd64/src/`), pull a crate by path
dependency if it exists, extract it from `amd64/src/` into a crate if it lives only there, write new only if it exists nowhere. Expected here:
`akuma-pipes` + `akuma-pipe` **pulled**; the `pipe2` arm, a `Pipe(PipeId, is_write)` variant and its read/write/close/dup/fork hooks **written new** (taoxichuan's own glue,
~100 lines; say so). Report per arm: pulled / extracted / written new (why), and `wc -l taoxichuan/src/*.rs` at start and end (start: 5,833).

## Gates (the suite is `scripts/kvm/goose/run_pipes.sh`; read its header, do not extend `run_gates.sh`, which is closed)
- P1 `sh -c 'echo hi | cat'` -> exit 0, client bytes `hi\n`.
- P2 `sh -c 'echo hi | cat | wc -c'` -> exit 0, `3\n`.
- P3 `sh -c 'echo a | wc -c; echo b | wc -c'` -> exit 0, `2\n2\n`.
- P4 `sh -c 'echo hi | cat > /tmp/p; cat /tmp/p'` -> exit 0, `hi\n`.
- P5 `sh -c 'echo x | true; echo done'` -> exit 0, `done\n`.
- Zero `Fault|PANIC|sig!`. Run the suite on the box with `sh scripts/kvm/goose/run_pipes.sh KERNEL` after `boot_dev.sh` (see `taoxichuan-08q-curl-https.md` for the boot recipe).
  Read client captures with `od -c`, never `strings`. After a wedge the guest takes no more execs: one gate per boot.
- Spot check once at the end (closed stages): F1 `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` -> exit 0, `x\n2 /tmp/f\n`.
- Out of scope: more than 64 KiB through a pipe, concurrent writer/reader, SIGPIPE delivery, `poll` on a pipe, `pipe` in a fork child's child, the scheduler.

## Ops traps
- Guest kernel installs only with the explicit destination `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`.
- `vm-serve-mc` is running and kvm0 is exclusive: stop it before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs; confirm the `guest disk:` line in
  `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot; **restore everything last** (vm-dev disabled, `vm-serve-mc` running).
- Never run `python3 -`, a heredoc python, or any python from stdin on this box; write a file and run `python3 /path/x.py < /dev/null`. `gcc` only after `. /etc/akuma-dev.env`.
- Debug prints carry the tag `[tmp]` in their text and are listed under "Remove before final" in RESULT-pa.md.

## Commit rule (you have no clock)
Commit before every build, and `git push litter taoxichuan-s4` **only if the state is green** (host tests pass, kernel builds; never a `[tmp]` print, never an amend
of a pushed commit). Never push `origin`. WIP commits are fine locally. Write RESULT-pa.md after every boot and every commit.

## Budget
About 40 tool calls and 3 boots. At 90 calls without a commit the supervisor nudges you. Past the budget without the gate: write what you have and stop.

## When to stop
P1-P5 pass in one clean boot, or 3 boots with no new evidence, or a quota error. Write `/root/dev-cycle/taoxichuan-s7/RESULT-pa.md` (premise results, probe hit list, gate
lines with console evidence, the exact failing step, pulled/extracted/written-new, what is next) and end the turn.
