# Taoxichuan stage 12, session X: epoll + eventfd, so that `goose --version` and then `goose run` work on Taoxichuan

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: the RAM-cap lift (`RESULT-X11.md`).
**Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`. **First action: `git fetch origin rust-org-il-experiment && git merge --no-edit origin/rust-org-il-experiment`** (the supervisor has staged
its work there; the box checkout was fast-forwarded to `eadfee8c` before this brief; re-merge at the start of every session). Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
The user wants `goose run` on Taoxichuan today. It dies at its first syscall it cannot get: `[nosys] nr=291` (`epoll_create1`) -> `Failed to build Tokio runtime: Os { code: 38 }` (and a harmless `nr=157` `prctl(PR_SET_NAME)` first).
Taoxichuan serves **no epoll call at all**; cargo, git, curl and rustc never needed one. The decisions already exist as crates (`akuma-syscalls-poll`, `akuma-net-yarn`) and amd64 wires them (`akuma-syscalls-glue::poll::sys_epoll_*`). You write the thin arms over them, nothing more.
Everything else is in place and must not be broken: init is pid 1 on a real pool thread, `fork`/`clone`/`execve`/threads/pipes/sockets/eventfd2 work, the guest builds its own kernel (fixed point K1 == K2).

## The dev cycle (use it; it is one command)
`sh /root/tx/gdev.sh` rebuilds the guest kernel from your checkout (`kbuild -p taoxichuan`), installs it as `/boot/akuma-guest-dev` (+x), boots it under Akuma's hypervisor (3 GiB, 4 vCPUs) on a FRESH copy of the goose image
`/root/tx/rs.goose.orig`, runs `/etc/goose-test.sh` as init's child (`goose --version`; it prints `GOOSE-TEST-START` ... `GOOSE-TEST-RC=<n>`) and prints the console. Takes ~3 min. Console log: `/root/tx/vm-dev.log` (`grep -a`; drop `[KVM]` lines: `grep -a -v "^\[KVM\]\|^$"`).
Other entry: `sh /root/tx/gdev.sh /bin/busybox sh,/etc/goose-run.sh` runs the real thing (`goose run` with the GLM config and key already in the image) and prints `GOOSE-RUN-RC=<n>`. The init command line cannot carry spaces: put anything with spaces in a script file in the image
(add it with `debugfs -w` to `rs.goose.orig`, see `/root/tx/patchg*.cmds`). The baseline today is exactly the failure above.
**You own kvm0 while this brief runs** (one VM at a time; the supervisor will not boot guests meanwhile). gdev.sh kills the previous `vm-run` by pid; never `pkill -f` (it kills your own shell); never write `/boot/akuma-amd64*`.

## Read first (in order; `sed -n` slices)
1. `docs/archive/AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md`, `AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md` (the `EPOLLET` re-arm wedge: do not reintroduce it), `TOKIO_PIPE_EPOLL_HANG.md`, `EPOLL_ONESHOT_AND_NESTED_SPIN.md`, `EPOLL_MULTI_POLLER_PIPE_FLAKE.md`.
2. `docs/reference/subsystems/syscalls/poll.md` (seven known divergences from Linux, pinned) and the `akuma-syscalls-poll` / `akuma-net-yarn` crate docs (`crates/akuma-syscalls-poll/src/lib.rs` head, `crates/akuma-net-yarn/src/lib.rs` head).
3. `taoxichuan/src/sock.rs` (`fd_events`, `poll_arm`, `select_arm` - the readiness probe you reuse) and `main.rs` (`eventfd2_arm`, `eventfd_can_read`, the dispatcher's syscall table). `taoxichuan/docs/reference/{syscalls,processes,pipes}.md`.
4. `docs/archive/TAOXICHUAN_CRATE_EXTRACTION_PLAN.md` § main.rs/sock.rs rows: the supervisor is moving code OUT of `main.rs` (user access, futex, clock) in parallel - **add your arms next to the existing ones, do not move or reformat existing code**, and see `scripts/kvm/goose/COORDINATION.md`.

## Premises (re-run each check; one line per row in RESULT-X.md: HOLDS / WRONG (what you saw) / UNCHECKED; about 10 calls)
| # | premise | check | supervisor saw |
|---|---|---|---|
| 1 | no epoll syscall is served | `grep -c -i "epoll" taoxichuan/src/main.rs taoxichuan/src/child.rs` | 0 and 0 |
| 2 | eventfd2 exists (4 slots), `eventfd_can_read` exists | `grep -n "eventfd2_arm\|fn eventfd_can_read" taoxichuan/src/main.rs` | both present |
| 3 | readiness for sockets/pipes/eventfd is one function | `grep -n "fn fd_events" taoxichuan/src/sock.rs` | present |
| 4 | `akuma-syscalls-poll` is a dependency of taoxichuan, `akuma-net-yarn` is not | `grep -n "syscalls-poll\|net-yarn" taoxichuan/Cargo.toml` | poll only |
| 5 | the baseline fails at nr=291 | `sh /root/tx/gdev.sh 2>&1 \| grep -a "nosys\|Tokio\|GOOSE-TEST"` | `nr=157`, `nr=291`, `Failed to build Tokio runtime` |
| 6 | the image has goose and its test scripts | `debugfs -R "ls -l /etc" /root/tx/rs.goose.orig \| grep goose` | goose-test.sh, goose-run.sh |
A WRONG row that changes the plan is reported in RESULT as soon as you commit.

## Symptom first
`grep -n -i "epoll\|Tokio\|nr=291" docs/README.md docs/runbooks/*.md` and read the matching rows before any source read; write the rows (or "no row") in RESULT-X.md.

## Probe discovery
`userspace/epollprobe/` (C, per-syscall behaviour: `userspace/epollprobe/c/build.sh`, `x86_64-linux-musl-gcc`), `scripts/epoll_suite.py --linux` (runs the same static binary on Linux), `scripts/benchmarks/epoll_ab_run.sh`. Use the differential: same static probe, Linux vs Taoxichuan, outputs compared. A new probe only if
this list says why none fits; it goes in `userspace/`, never `/tmp`. To run a probe in the guest: add the binary to `rs.goose.orig` with `debugfs -w`, boot it with `gdev.sh /path/to/probe args`.

## Guest facts (verified)
Init is pid 1 on a pool thread; `fork`/`clone` work for it. Real `fork_process` children, threads (`clone_thread`), pipes, AF_INET sockets, eventfd2 (4 slots), `poll`/`select` work. No procfs beyond a few files; `ps` prints a header. One ssh session = one forked process (the sshd in the image forks per session). DNS is `10.0.2.3` (the host relay).
4 vCPUs are offered to the guest but the kernel runs on one. busybox 1.35 `sh` runs scripts.

## Composition rule (mandatory)
`taoxichuan/` is a composition root. For epoll: **pull** `akuma-syscalls-poll` (interest list, `epoll_ctl` errnos, `EPOLLET` armed-state, readiness map) and `akuma-net-yarn` (the wait loop as a state machine) and write thin arms over `sock.rs::fd_events`; OR fold in `akuma-syscalls-glue`'s `poll::sys_epoll_*` as amd64 does -
choose and justify in RESULT (count the dependencies and the compile time). Do not hand-write an epoll table or a wait loop. Per arm report *pulled / extracted / written new (why)* and `wc -l taoxichuan/src/*.rs` before/after.
**Simplest correct first:** `epoll_wait` re-checks readiness each lap and parks/yields (`akuma_threading::schedule_blocking` with the earliest deadline, as `wait4_real` and `poll_arm` do); a waker design is a second slice only if goose's idle CPU makes it matter. `epoll_wait` must park the calling THREAD (other threads of the process keep running), wake on readiness of a socket, a pipe end, an eventfd, or the timeout. State which is in the tree.
Allocation on the syscall path: justify each in RESULT (prefer fixed arrays). Console output through `safe_print!`. Gate amd64-only code with `cfg`; keep amd64-only crates out of `default-members`; `scripts/check_unsafe_budget.py` stays green.

## Docs (mandatory, same commits)
`taoxichuan/docs/reference/syscalls.md` rows for 213/232/233/281/291 (+ 441 if served) and 157, `gaps.md` (what is still missing, with the probe), the status table; an incident gets a `docs/archive/` doc; `docs/archive/BUG_FIX_LIST.md` per its runbook if you fix a bug.

## Gates
- E1: `sh /root/tx/gdev.sh` -> console shows `GOOSE-TEST-START`, goose's version line (`1.52.0`) and `GOOSE-TEST-RC=0`; zero `[nosys]` for nr 291/232/233/281/213/157; zero `Fault|PANIC|sig!`.
- E2: the `epollprobe` differential, Linux vs Taoxichuan, equal except the divergences `poll.md` pins (list any new one).
- E3: `sh /root/tx/gdev.sh /bin/busybox sh,/etc/goose-run.sh` -> `GOOSE-RUN-START`, goose connects to `api.z.ai` over TLS, runs `uname -a` through its developer tool and prints the result, `GOOSE-RUN-RC=0`. If it fails past epoll, name the next `[nosys]`/`[err]` line and fix what is yours to fix (one syscall at a time); report what is not.
- E4: no regression: `cargo test` for the crates you touched, `cargo check --release --target x86_64-unknown-none -p akuma-amd64 --features no-tests`, the aarch64 `cargo check`, and `scripts/kvm/goose/cow_leak_gate.py --cycles 20` still `delta=0` (if it needs a VM, run it after E3).
- Out of scope: moving existing code, `amd64/` edits, SMP, the init program.

## Ops traps
* kvm0 is yours; `gdev.sh` restarts the guest each run (~3 min). Do not leave a guest running when you finish (kill the `vm-run` pid).
* **Never run `python3 -`, `python3 <<EOF` or python from stdin on this box** (REPL hang): write a file, run `python3 /path/x.py < /dev/null`.
* Do not edit `/etc/herd/`. Do not touch `/root/tx/rs.goose.orig` except to ADD files with `debugfs -w` (keep a `.bak`).
* The guest console interleaves threads' trace lines; `grep -a`, and match on the markers above.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` at the first green host test, after every arm that builds, and after every boot cycle that taught you something. WIP commits are fine. Never push `origin`. Before each unit of work: merge `origin/rust-org-il-experiment`.

## Budget
About 150 tool calls for this session (a cycle is ~3 min); the supervisor nudges at 90 calls without a commit. A 5-hour z.ai window is ~34M tokens; stay well under 10M. Past budget with E1 open: write RESULT and stop.

## When to stop
E1-E3 pass in a clean boot, or 150 calls, or the quota error. Write `/root/dev-cycle/taoxichuan-s8/RESULT-X12.md` (premise lines, the pulled/extracted/new table, which wait design is in the tree, gates with console evidence, the first failing line if E3 fails, what is open) and end the turn.
