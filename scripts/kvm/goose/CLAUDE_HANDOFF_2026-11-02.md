# Handoff (2026-11-02, after B15): record the fixed point, then start GLM on Shanghai unattended

You are running on the ryzen, which is booted into Pop (`192.168.1.126`), as root. The user is away. Do both
tasks below without asking; where a decision is genuinely theirs, pick the conservative option, note it, and keep
going. Follow the repo's `CLAUDE.md`. Never use fork subagents. Commit and push are allowed for this work. For
docs-only commits, `--no-verify` is fine.

**The repo.** `netoneko/akuma`, branch `rust-org-il-experiment` (HEAD `0bbc8980` or later). Use an existing
checkout on Pop if there is one, at the right branch and with a clean `git status`. Otherwise clone it under
`/root`: `git clone --branch rust-org-il-experiment https://github.com/netoneko/akuma` (no
`--recurse-submodules`, since the vendored submodules are about 37 GB and none is needed here). Add the remote
`litter` = `git@github.com:netoneko/akuma-litter.git` if it is missing. Check that you can push before you need
to: `ssh -T git@github.com`.

- If you cannot push anywhere, do not create, copy or ask for credentials. Commit locally, write down the exact
  push commands, and say so in the result file.
- Never commit a submodule pointer.

**Do not mount `/dev/nvme0n1p3`** (the Akuma root partition) read-write. The fixed-point guests run on qcow2
overlays of it.

## Context

B15 (`aafbb973`) moved Taoxichuan's whole kernel into `crates/akuma-tokio-kernel-amd64`. It added
`crates/akuma-tokio-kernel`, whose `entry!` and `Kernel` builder are documented in its README, which is also the
brief for building a new kernel. Taoxichuan is now a 19-line `main.rs`, and `osaka/` is the 10-line sample.
Read `docs/archive/TAOXICHUAN_CLEANUP_PLAN.md` (the B15 rows) and `crates/akuma-tokio-kernel/README.md` first.

The ryzen is running Pop (`192.168.1.126`). Its Akuma
root partition is `/dev/nvme0n1p3`.

## Task 1: record the B15 fixed point

At 16:38 the user's Mac started a two-generation self-hosting fixed point of `961a77be` (the same kernel source as
B15) on this box, using `scripts/kvm/taoxichuan/fp_pop.py`. Read that script first: it is the procedure.

**How the run works.**

- K0 was built on the Mac (md5 `0272f1eb7de0e031e4266e9d89aec181`).
- For n = 1 and 2:
  - `/root/tx-pop.sh boot fresh` boots K(n-1) on a fresh overlay of the Akuma root partition. The run dir is
    `TX_RUN=/root/tx-fp`; the guest's ssh is on port 2226.
  - The guest builds `961a77be` detached and writes `/tmp/fp.status` (ending in K(n)'s md5).
  - The Mac stops the guest and pulls K(n) to `/root/tx-fp/k<n>` with `qemu-nbd --read-only` + `debugfs dump`.
- Each build takes about 8 minutes, so a healthy run ended around 16:56. Pass means `/root/tx-fp/k1` and
  `/root/tx-fp/k2` are byte-identical.

**The run is driven from the Mac, so the user leaving may have killed it halfway.** Work out where it stopped:

- **Both `k1` and `k2` exist, and no guest is running.** Done: md5sum both and compare.
  - "No guest" means `/root/tx-fp/qemu.pid` is absent, or names a dead pid.
- **A guest is still up** (`ssh -p 2226 root@127.0.0.1 'cat /tmp/fp.status; tail -3 /tmp/fp.log'` answers):
  - Wait for its status to carry an md5.
  - Then finish that generation by hand, with the same commands as fp_pop.py: `TX_RUN=/root/tx-fp bash
    /root/tx-pop.sh stop`, then the `qemu-nbd` / `debugfs dump` / `qemu-nbd -d` pull into `/root/tx-fp/k<n>`.
    Use `/dev/nbd7`, and always detach it.
  - Check that the pulled md5 equals the md5 the guest reported.
- **`k1` exists but `k2` does not, and no guest is running.** Run generation 2 yourself, following fp_pop.py:
  - Put `k1` at `/root/tx-fp/target/x86_64-unknown-none/release/taoxichuan`.
  - Boot it, send the same build script, poll, stop, pull `k2`.
- **Neither exists.** Rerun from K0. If `/root/tx-fp/target/x86_64-unknown-none/release/taoxichuan` still has
  K0's md5, it is K0.
  - fp_pop.py can drive the whole run from this box if `ssh root@192.168.1.126 true` works here (it reaches the
    box through `scripts/utils/hpbox.py`'s `RZ_ROOT`).
  - Otherwise do the steps by hand.

Throughout:

- Stop a guest only by its pidfile (`tx-pop.sh stop`). Never `pkill` by pattern, since other VMs may be running.
- Readiness is a completed ssh command, not an open port.

**Record it.**

- In `docs/archive/TAOXICHUAN_FIXED_POINT.md`, add a section "After B15 (2026-11-02)". Use the table shape of
  "After the cleanup": K0, K1, K2 with md5, size and build time, plus the count of fault-ish console lines.
  - The grep is fp_pop.py's: `grep -acE 'Fault|PANIC|BADFRAME|stuck|never ack' /root/tx-fp/console.log`.
  - The console log holds only the last generation.
  - Note which steps you finished by hand.
  - If K1 != K2, say so plainly. Do not soften it, and do not retry until it passes.
- In `docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`, the B15 row ends "Fixed point not rerun yet". Replace that with
  the result.
- Commit, then push: `git push origin rust-org-il-experiment && git push litter rust-org-il-experiment`.

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
