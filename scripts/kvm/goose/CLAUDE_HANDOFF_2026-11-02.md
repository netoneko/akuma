# Handoff (2026-11-02, after B15): record the fixed point, then start GLM on Shanghai unattended

You are working alone in `/Users/netoneko/github.com/netoneko/akuma`, branch `rust-org-il-experiment` (HEAD
`38eb26c6` or later). The user is away. Do both tasks below without asking; where a decision is genuinely
theirs, pick the conservative option, note it, and keep going. Follow `CLAUDE.md` and the memory index. Never
use fork subagents. Commit and push are allowed for this work: run `.git/hooks/pre-commit` separately when you
change code, then commit with `--no-verify`. Docs-only commits may skip the hook.

## Context

B15 (`aafbb973`) moved Taoxichuan's whole kernel into `crates/akuma-tokio-kernel-amd64`. It added
`crates/akuma-tokio-kernel`, whose `entry!` and `Kernel` builder are documented in its README, which is also the
brief for building a new kernel. Taoxichuan is now a 19-line `main.rs`, and `osaka/` is the 10-line sample.
Read `docs/archive/TAOXICHUAN_CLEANUP_PLAN.md` (the B15 rows) and `crates/akuma-tokio-kernel/README.md` first.

The ryzen is running Pop (`root@192.168.1.126`, reached as `scripts/utils/hpbox.py`'s `RZ_ROOT`). Its Akuma
root partition is `/dev/nvme0n1p3`.

## Task 1: record the B15 fixed point

A two-generation fixed point of `961a77be` (the same kernel source as B15) was started from the previous session
at 16:38. It runs under Pop's KVM through `scripts/kvm/taoxichuan/fp_pop.py`, with K0 built on the Mac. Let
`S=/private/tmp/claude-502/-Users-netoneko-github-com-netoneko-akuma/56d3949a-0cba-4330-8d3d-383d80ffc90a/scratchpad`.

- **Read the result.** The log is `$S/fp15/fp_pop.log`. It ends with `FIXED POINT: K1 == K2` (or `NOT A FIXED
  POINT`) and then `rc=N`. Each generation's build takes about 8 minutes, so the run finished around 16:56.
- **If the log stopped short** (no `rc=` line, and nothing new for 20 minutes), the previous session's shell
  died. Stop the guest by its pidfile only: `ssh root@192.168.1.126 'TX_RUN=/root/tx-fp bash /root/tx-pop.sh stop'`.
  Then rerun the check:
  `python3 -I scripts/kvm/taoxichuan/fp_pop.py $S/fp15/k0 961a77be > $S/fp15/fp_pop.log 2>&1`.
  If `$S/fp15/k0` is gone, rebuild it with
  `cargo build -p taoxichuan --target x86_64-unknown-none --release --features no-tests,trace-err`.
- **Record it** in `docs/archive/TAOXICHUAN_FIXED_POINT.md`: add a section "After B15 (2026-11-02)". Use the same
  table shape as "After the cleanup": K0, K1, K2 with md5, bytes and build time from the log, plus the console
  fault count. If K1 != K2, say so plainly; do not soften it.
- **Update the B15 row** in `docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`: its last sentence, "Fixed point not rerun
  yet", becomes the result.
- **Check the hook.** `$S/hook_b15b.log` ends with `hook rc=N`. If N is not 0, fix what it names, rerun the hook,
  and commit the fix.
- **Commit and push** to both remotes:
  `git push origin rust-org-il-experiment && git push litter rust-org-il-experiment`.

## Task 2: GLM starts on Shanghai by itself after the next reboot

GLM (goose + z.ai's GLM) should pick up `scripts/kvm/goose/shanghai-01-new-kernel.md` with nobody at the keyboard.
That brief tells it to build a new kernel, Shanghai, from the akuma-tokio-kernel README: a skyline banner, six
gates from build to goose `PONG`, and a commit and `git push litter shanghai` after every gate.

**What to build.** A one-shot starter. Some time after the machine comes up (a delay of a few minutes, so the
network and the services are up), it sends goose that brief once and then never fires again:

- a marker file written before it starts goose, checked first;
- its output in a log file;
- `goose run` in the right working directory, with the brief as the prompt.

Use the setup earlier GLM sessions used rather than inventing one. Read these first:

- `scripts/kvm/goose/SUPERVISOR_RUNBOOK.md`, `COORDINATION.md`, `goose_glm.sh`
- `SUPERVISOR_PROMPT_2026-10-31.md` and `scripts/kvm/goose/BRIEF_TEMPLATE.md`
- the `ryzen_*` and `amd64_bare_metal_dev_environment` memories

They tell you where goose has run: the dev guest at `192.168.1.159:2224` under Akuma's hypervisor, or the box
itself. They also tell you where its config and z.ai key live and how a session is started.

**Questions to settle before writing anything,** each answered from the box rather than guessed:

1. Which OS does the ryzen boot next? Check the boot entries from Pop.
2. Is that the system goose will run on? Wire the starter into that system's startup: a herd service conf on
   Akuma (`/etc/herd/...` on the Akuma root), or a systemd unit on Pop.
3. Does that system have the repo, the Rust toolchain, `e2fsprogs`, and a way to boot a guest? Shanghai's gates
   need a build, `debugfs`, and a VM: QEMU or KVM on Pop, `herd vm-run` on Akuma.
4. Can it `git push litter`? It needs a key or token with write access to `netoneko/akuma-litter`. If it cannot,
   do not create or copy credentials. Write that down for the user as the one thing they must do, and still
   install the starter.

**Rules:**

- The z.ai key stays where it is, in root-only files on the box. Never copy it to the Mac or into the repo.
- Do not write to `/dev/nvme0n1p3` with `debugfs -w` while any guest runs on an overlay of it. Task 1's guest
  does, so finish Task 1 first.
- Never kill a VM or QEMU by pattern; kill only by the pidfile.
- Never rsync the whole tree; copy single files.

**Test it** without a reboot: run the starter by hand with the delay set to 0 and a scratch marker path. Check
that goose starts and reads the brief, then stop that goose session and remove the scratch marker. Leave the
real marker absent, so the starter fires after the reboot.

**Commit** the starter script and its service file under `scripts/kvm/goose/`, with a short README section
saying how it is installed, how to re-arm it (delete the marker) and where its log is. Push to both remotes.

## When done

Write `scripts/kvm/goose/HANDOFF_2026-11-02_RESULT.md` and commit and push it. It should cover:

- Task 1: K1, K2 and pass or fail.
- Task 2: where the starter is installed, which OS it fires on, its delay, the marker and log paths, and the
  result of the manual test.
- Anything the user must do before rebooting, such as a missing push credential for `litter`.

Your final message is the same summary in five lines.
