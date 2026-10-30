# Supervisor prompt - Taoxichuan, 2026-10-31 (paste as the first message of a new session)

You are continuing the Taoxichuan experiment on branch `rust-org-il-experiment` in /Users/netoneko/github.com/netoneko/akuma
(work from the `taoxichuan/` subdirectory; never glob or list the repo root - it has 1000+ files).

## Permissions (the user's, 2026-10-31)

* **You may commit and push to GitHub: `git push origin rust-org-il-experiment`.** Commit when a unit of work is green and say
  what it is; run the pre-commit hook (clippy `-D warnings` over every extracted crate + host tests) and fix what it finds -
  **never `--no-verify`**, never `git reset`/`rebase`/force-push/rewrite history. Merge, do not rebase.
* **goose-glm** may commit and push to the `litter` remote (`git push litter taoxichuan-s4`); **you never push `litter`**, glm never
  pushes `origin`. GitHub is the channel between you (see "Talking to glm" below).
* You may reboot the ryzen (Akuma -> Pop -> Akuma), stop/start herd services and VMs on it, install kernels to
  `/boot/akuma-amd64.verify` and `/boot/akuma-guest-*`, and edit `/etc/herd/`. Never write `/boot/akuma-amd64` itself (that is the
  fallback), never `pkill -f qemu-system-aarch64` (the user's own VMs run on the Mac), never kill by `pkill -f PATTERN` on the box
  (the pattern matches your own shell command and kills your session - kill by pid).
* No sub-agents/forks (`CLAUDE.md`). Talk to the box only through Python (`python3 -I scripts/kvm/goose/ak.py '<cmd>'`,
  `AKUMA_PORT=2224` for the guest; `scripts/utils/hpbox.py rz|rzr` for Pop).

## Where things stand (read these, in this order)

1. `docs/archive/TAOXICHUAN_FIXED_POINT.md` (+ its "Bare-metal run" section) and the last entries of
   `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`: **K1 == K2 on bare metal**, md5 `6facda34cbf9ef3cb318dae4ce09ab04`,
   commit `6af48c06`, under Akuma's own hypervisor (3 GiB, 4 vCPUs, ~5.5 min a build). Done; do not redo it.
2. `docs/archive/TAOXICHUAN_CRATE_EXTRACTION_PLAN.md` - glm's plan (ten ranked extractions, per-file tables, gates). **Your job A.**
3. `scripts/kvm/goose/SUPERVISOR_RUNBOOK.md` - how glm is supervised (brief as a file, one clean goose server, probe, poll every ~5
   min, steer with `steer.py`). `scripts/kvm/goose/BRIEF_TEMPLATE.md` - every brief follows it. **Your job B.**
4. `taoxichuan/docs/reference/switch-to-crates.md` (S1-S7 slices) and `taoxichuan/docs/README.md`.

What changed this week that you must know:
* `SLOTS` and `TASKS` are `BorrowCell`s (`with_slots`, `with_task`, `with_task_slot`, `with_tasks`, `with_fds`, `task_root`): a
  closure must not call anything that reaches the same record (`user_ok`, `copy_*_user`, `add_region`, `with_fds` itself) - read what
  you need out, close the borrow, then act. A nested borrow is a runtime panic naming the bug.
* **Init is pid 1 on a real pool thread** (`child::start_init_thread`, called from `spawn()`), the shape of amd64's `run_init`: the
  boot thread is adopted as pool slot 0 and parks; init is tid >= 1 with a ring row, a task record `tid+1` and a registered
  `Process`. `clone(SIGCHLD)` now goes to `fork_arm_real`. This lets **sshd run with `fork-sessions` + `libakuma/linux-abi`** (one
  forked process per ssh session, preemptible). It is always on: the old cooperative sshd (SPAWN) no longer works on this kernel,
  so test images must carry the forking sshd: `cargo build --release --target x86_64-unknown-none --offline -p sshd --features
  libakuma/linux-abi` in `userspace/` (on the box: `/root/tx/sshd-fork-linuxabi`).
* Known leftover: `[SSHD] bridge_process: couldn't open stdin for pid N` (a private-syscall remnant; output still works).
* **The user wants a real init next: `herd` as pid 1** built with `linux-abi` and a `taoxi` feature that gates off everything this kernel
  does not support. Not started.
* The host kernel now allows guest RAM up to 4 GiB (glm's cap lift); the metal kernel was built ON the ryzen
  (`kbuild -j 4 --features no-tests`, ~2 min) and installed as `/boot/akuma-amd64.verify` (GRUB entry 30 boots it: `arm.sh 29` from Pop;
  the previous one is `/boot/akuma-amd64.verify.pre-ramcap`). `herd` userspace had the old 1 GiB cap compiled in: a guest is started
  with `/root/tx/herd-new vm-run /root/tx/vm-tx.spec` (spec format: see that file).

## Job A - execute glm's extraction plan yourself, safest first

Order: X2 clock/rng -> X1 futex -> X3 user access -> then reassess (X5/X6/X8 next; X7 file-description model last and only with the
user's go-ahead). Per extraction: new/extended crate with host tests, `taoxichuan/src` calls it, **`amd64/` depends on it too and
still builds** (`cargo check --release --target x86_64-unknown-none -p akuma-amd64 --features no-tests`; the aarch64 `cargo check`
must stay green - gate arch-specific code with `cfg`, keep amd64-only crates out of `default-members`), the unsafe budget does not grow
(`scripts/check_unsafe_budget.py`), docs updated (`taoxichuan/docs/reference/`, the plan's status). Verify on TCG with the local rig
(below) and, once a batch is green, on metal (build on the ryzen, install `.verify`, reboot into entry 30, boot a guest, run the
fixed point's K1 build as the heavy test - it is the best regression test there is: ~100 processes, threads, pipes, the network).
Commit per extraction.

## Job B - babysit glm wiring eventfd/epoll

The brief exists: `scripts/kvm/goose/epoll-babysit.md` (epoll family 213/232/233/281/291, eventfd semantics tokio's waker needs,
`prctl(PR_SET_NAME)` no-op; gates: an `epollprobe` differential vs Linux, `goose --version` prints 1.52.0, a GLM `goose run` completes
over TLS). Re-read it against today's tree before sending - premises have been wrong in about one brief in two - and note the
changes above (BorrowCell accessors, init as a process, the forking sshd in the image). Send it as a file with a 3-line prompt, probe
goose first, one clean goose server (runbook steps 1-4). Poll every ~5 minutes; a silent stretch > 7 min with no running process =
restart and resend; at 90 calls without a commit, nudge. glm works in `/src/github.com/netoneko/akuma` on the box, branch
`taoxichuan-s4`; **its first action each session is `git fetch origin rust-org-il-experiment && git merge origin/rust-org-il-experiment`**
so it works on top of your staged commits.

### Talking to glm via GitHub

* You -> glm: push to `origin rust-org-il-experiment`; glm merges it (above). Briefs and steering notes live in
  `scripts/kvm/goose/` (committed, so they are the record). Mid-turn corrections: `scripts/kvm/goose/steer.py SESSION_ID FILE`.
* glm -> you: it pushes `litter taoxichuan-s4`; you `git fetch litter taoxichuan-s4` and merge into your branch when it reports (its
  `RESULT-*.md` is in `/root/dev-cycle/taoxichuan-s8/` on the box). Merge conflicts are expected in `taoxichuan/src/{main,sock}.rs`
  (your extractions move the code its eventfd/epoll arms sit next to): resolve toward keeping both, rebuild, run the battery.
* Coordination file: `scripts/kvm/goose/COORDINATION.md` - a short list of "who is touching which file right now". Update it when
  you start an extraction; ask glm to update it when it starts an arm. `gh` is on the box and on the Mac if an issue/PR thread helps.

## Local TCG rig (Mac) and the box

* Build a trace kernel: `cd taoxichuan && cargo build --release --target x86_64-unknown-none --features no-tests,trace-err
  --target-dir $SCRATCH/tgt-new`. Boot rig and battery live in the previous session's scratchpad
  (`/private/tmp/claude-502/-Users-netoneko-github-com-netoneko-akuma/92196a57-f57d-4d69-8b74-b0b562261a61/scratchpad`:
  `runqemu_new.sh`, `battery.sh`, `battery_fork.sh`, `sshq.py`, `img/fork.img` = the image with the forking sshd); the battery is
  quick execs, F1, PK1, fs, `apk add git`, `git --version` and must stay green.
* Metal: kernel built with `kbuild -j 4 --features no-tests` on the box, installed to `/boot/akuma-amd64.verify` (`chmod +x`,
  `cat > new && chmod +x new && mv new K` - a non-executable kernel is silently ignored and the old guest keeps running; check for a NEW
  pid), then from Pop `sh overlays/ryzen/arm.sh 29`. Wait for Akuma with an ssh round trip (`ak.py 'uname -a'`), never a log grep.

## Traps this session cost time on (each is real)

* The guest sshd serves ONE session at a time and the host relay holds two forwarded connections: a second ssh (a status poll) is
  refused/times out while a long command runs. Start long guest jobs as ONE foreground ssh from a local background process and watch
  the host console (`/root/tx/vm-tx.log`) or wait for it to end. A background job started inside a session dies with the session.
  (The forking sshd should lift this - verify on metal.)
* `ak.py --stdin` is text-only and the guest sshd drops binary/large stdin and stdout: pass scripts as a heredoc inside the command
  string; extract big files from a stopped guest with `debugfs -R "dump ..."` on its image, ship binaries to the box with a binary-safe
  `ssh ... 'cat > f'` Python wrapper. Do not push bulk data over wifi - build on the box.
* `mke2fs -d` fails on Akuma (xattrs: ENOSYS); populate images with `debugfs -w -f cmds`. The relay's DNS is `10.0.2.3:53`.
* herd (PID 1) reloads its own binary only on reboot; `herd enable/disable` re-parses confs. `zsh`: never `echo ====` (it is a command).
* `docs/` conventions: a bug fix gets an `archive/` doc; `reference/` gets the invariant. Console output uses `safe_print!`.

## First steps

1. `git status`, `git log --oneline -5`, read the files above. Check the box (`ak.py 'uname -a; herd status'`): if it is on Pop, arm entry 30.
2. If the init-as-process work is not yet committed (`git log --oneline -3 -- taoxichuan/src/child.rs`), finish it first: build the fork
   sshd image, run `battery_fork.sh`, commit, push, then **validate on metal** (build the metal guest image with the fork sshd, boot it,
   confirm two simultaneous ssh sessions and a `cargo build` still finishes - that is the sshd fix's real test).
3. Start Job A's first extraction; send Job B's brief to glm; report to the user every ~5 minutes while glm works.
