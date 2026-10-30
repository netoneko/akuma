# Supervise goose-glm: epoll + eventfd on Taoxichuan, so that goose-glm runs on it

You are the **supervisor** of the metal track in `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`. goose-glm (`glm-5.3-flash`, low effort) runs on the ryzen **in Akuma**
(`ssh -p 2222 root@192.168.1.159` via `python3 scripts/kvm/goose/ak.py '<cmd>'`; ACP server `192.168.1.159:3284`, herd service `goose-glm`, running now) and writes the kernel. You do not write
the kernel; you brief it, steer it, and judge the evidence. Checkout on the Mac: `/Users/netoneko/github.com/netoneko/akuma`, branch `rust-org-il-experiment`. Never glob the repo root. Read
`scripts/kvm/goose/SUPERVISOR_RUNBOOK.md` and `SUPERVISOR_HANDOVER_2026-10-30.md` first (how to send, steer, the quota rules); then `BRIEF_TEMPLATE.md` once - every brief you send follows it,
including its **Premises** table, **Composition rule** and **Docs** sections. Keep your own context small: `sed -n` slices, no whole-file dumps.

## Where things stand (read these, in slices)
* `docs/archive/TAOXICHUAN_FIXED_POINT.md` - the guest now compiles its own kernel and the result is byte-stable (K1 == K2). That is the baseline you must not break.
* `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`, last entry "goose + GLM on Taoxichuan - blocked: no epoll": the image boots and answers ssh, then `goose --version` dies in 0.16 s with
  `Failed to build Tokio runtime: Os { code: 38 }`; the console shows `[nosys] nr=291` (`epoll_create1`) after a harmless `[nosys] nr=157` (`prctl(PR_SET_NAME)`).
  "We forgot the epoll": cargo, git, curl and rustc got by because none of them is an async-Rust program.
* The side rig that reproduces it (Pop OS side, so only usable while the ryzen is in Pop): `scripts/kvm/taoxichuan_pop_side.sh`, `scripts/kvm/taoxichuan_goose_image.sh`. On Akuma the equivalent is the
  dev cycle in `docs/archive/AKUMA_KVM_DEV_CYCLE.md` (`kbuild -p taoxichuan`, guest kernel only at `/boot/akuma-guest-dev`, herd `vm-dev`). **Find out first** how a guest disk with the 150 MB `goose`
  binary and its config is made on Akuma (premise 6); if there is no recipe, that is stage 0 and it is small.
* What goose needs, measured: `strace -f -c goose --version` on Linux (Pop, this session) made exactly: `epoll_create1 epoll_ctl eventfd2 socketpair poll prctl sched_getaffinity clone futex
  set_tid_address sigaltstack rt_sigaction rt_sigprocmask getrandom clock_gettime` plus file calls. A real `goose run` adds `epoll_pwait`/`epoll_wait`, sockets and TLS.

## The task, one stage at a time (do not let it build ahead)
1. **epoll family** (`epoll_create1` 291, `epoll_create` 213, `epoll_ctl` 233, `epoll_wait` 232, `epoll_pwait` 281; 441 `epoll_pwait2` only if goose asks): level-triggered, `EPOLLET`,
   `EPOLLONESHOT`, `EPOLLIN/OUT/ERR/HUP/RDHUP`, `EPOLL_CTL_ADD/MOD/DEL`, timeouts, and **blocking**: `epoll_wait` must park the calling thread (other threads keep running) and wake on readiness
   of a socket, a pipe end, an eventfd, or the timeout.
2. **eventfd** correctness for mio's waker: `EFD_NONBLOCK|EFD_CLOEXEC` (already), readable edge reported through epoll, `EFD_SEMAPHORE`, a write from another thread wakes a parked `epoll_wait`.
3. `prctl(PR_SET_NAME)` (157) as an accepted no-op that records the name; any other call goose's own census shows as `[nosys]`.
Then the gates: `goose --version` prints `1.52.0`; `goose run -t "reply with the single word pong"` with the GLM config returns an answer over TLS.

## The rules that matter (composition, gating)
* **Pull, do not write.** The decisions exist: `akuma-syscalls-poll` (interest list, `epoll_ctl` errnos, `EPOLLET` armed-state, readiness map; `ctl edge fdset interest pollfd readiness`) and `akuma-net-yarn`
  (the wait loop as a state machine). amd64 wires them through `akuma-syscalls-glue`'s `poll::sys_epoll_*` (`EPOLL_TABLE`, and an `ExecRuntime` `epoll_destroy` hook). Choose and justify, in the RESULT file:
  fold glue in (what amd64 does; count its dependencies and compile time), or pull the two crates and write thin arms over Taoxichuan's own readiness probe (`sock.rs::fd_events` already answers
  `poll(2)`/`select`, including eventfd via `eventfd_can_read`). Per arm: *pulled / extracted / written new (why)*, and `wc -l taoxichuan/src/*.rs` before and after.
* **Read these first, they are this exact bug class:** `docs/archive/AKUMA_AMD64_EPOLL_EVENTFD_FOLD.md`, `AKUMA_AMD64_EPOLLET_REARM_KOT_WEDGE.md` (the `EPOLLET` re-arm wedge: do not reintroduce it),
  `TOKIO_PIPE_EPOLL_HANG.md`, `EPOLL_ONESHOT_AND_NESTED_SPIN.md`, `EPOLL_MULTI_POLLER_PIPE_FLAKE.md`, and `docs/reference/subsystems/syscalls/poll.md` (the seven known divergences from Linux, pinned).
* **Simplest correct first.** A first cut where `epoll_wait` re-checks readiness each lap and yields (`x86_yield`/`netpoll`) is correct and shippable; a waker-registration design is the second slice,
  only if goose's idle CPU makes it matter. Say which one is in the tree.
* Gate amd64-only code with `cfg`, keep amd64-only crates out of `default-members` (this has broken the aarch64 build twice). `scripts/check_unsafe_budget.py` must stay green (budget for `taoxichuan/src` is 55).
  Console output through `safe_print!` only. Allocation on the syscall path is justified in the RESULT file.
* Behaviour is pinned by tests: host tests in the crates; a **differential** run of `userspace/epollprobe` (C, builds with `x86_64-linux-musl-gcc`: `userspace/epollprobe/c/build.sh`, and `scripts/epoll_suite.py --linux`
  runs the same static binary on Linux) - same probe, Linux vs Taoxichuan, outputs compared. A new probe is allowed only if that list says why none fits.
* Do not regress the fixed point or the memory floor: after the work, the in-guest build still finishes and `scripts/kvm/goose/cow_leak_gate.py --cycles 20` still reports `delta=0`.
* Docs in the same commits (`taoxichuan/docs/reference/syscalls.md` rows for 213/232/233/281/291, `gaps.md`, the status table); an incident gets an `archive/` doc; `docs/archive/BUG_FIX_LIST.md` if a bug is fixed.

## Premises (you fill "saw"; the check commands are exact; about 10 tool calls)
| # | premise | check | supervisor saw (2026-10-30) |
|---|---|---|---|
| 1 | Taoxichuan serves no epoll syscall | `grep -c -i epoll taoxichuan/src/main.rs taoxichuan/src/child.rs` | 0 and 0 (sock.rs has 2 mentions, in comments) |
| 2 | `eventfd2` exists, 4 slots, reads answer EAGAIN when empty | `grep -n "eventfd2_arm\|fn eventfd_can_read" taoxichuan/src/main.rs` | lines 2420, 2494 |
| 3 | `poll(2)` readiness for sockets, pipes, eventfd is one function | `grep -n "fn fd_events" taoxichuan/src/sock.rs` | line 128 |
| 4 | `akuma-syscalls-poll` is a dependency of taoxichuan, `akuma-net-yarn` is not | `grep -n "syscalls-poll\|net-yarn" taoxichuan/Cargo.toml` | `akuma-syscalls-poll` only (line 99) |
| 5 | goose-glm answers on Akuma now | `python3 scripts/goose_acp_probe.py` (pong test) | herd shows `goose-glm: running` |
| 6 | a Taoxichuan guest disk with goose exists or can be made on Akuma | find the recipe in the runbook/dev-cycle doc; the binary is `/usr/local/bin/goose` (149,764,512 bytes, static-pie) on the Akuma root | **not verified - stage 0 if absent** |
| 7 | the ryzen is in Akuma, not Pop | `uname -a` over `ak.py` | `Akuma akuma-ryzen 0.0.8 f22e0d5d-release-smp-shared` |
Any WRONG row that changes the plan goes to the user at once.

## The loop
Brief (a file in `scripts/kvm/goose/`, committed and pushed first) -> `goose_acp_prompt.py` (one goose session per stage, never two at once) -> fetch and read its `RESULT.md` and the gate log -> validate what is
cheap and decisive yourself (build for aarch64 compile-only if `cfg` was touched; the regression gates above) -> next brief while goose works. Steer with `steer.py` only for a real mistake. Trust a PASS only when the
RESULT and the log both show it. Report to the user after each stage in five lines: gate, evidence, what changed in `wc -l`, what is open. If goose-glm's output claims something you cannot see in a log, say so.

Commit rule: this repo's `CLAUDE.md` says the user drives commits; commit and push **only** what the user has told you to in this session (briefs and docs are the usual exception they approved); never rewrite history.
