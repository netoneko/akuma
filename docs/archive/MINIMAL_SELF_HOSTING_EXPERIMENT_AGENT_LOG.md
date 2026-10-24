# Minimal self-hosting kernel "Taoxichuan" — agent log

**Status: in progress. Started 2026-10-24.**

Running log for the two-agent execution of
[`MINIMAL_SELF_HOSTING_EXPERIMENT.md`](MINIMAL_SELF_HOSTING_EXPERIMENT.md).
The minimal kernel being composed is named **Taoxichuan**.

**Oversight change (2026-10-24, afternoon).** The host track and the babysitting
of goose-glm were first run by Kimi (Crush, `k3-256k`). Kimi ran out of tokens
because of a bad subscription setting on the owner's side — not a model failure.
Oversight moved to **Claude Sonnet 5 (Claude Code)**, which takes over the host
track and supervises goose. The metal track is unchanged: still goose + GLM on
the ryzen. The tracks below still say "Kimi/Crush" for what was done before the
handover; entries after it are marked *(Sonnet)*. Note for the open-weights
claim: the host track is no longer open-weights from this point; the metal
track still is.

Working rules carried over from the owner's last Crush session (the "wisdom"):

1. Every stage is a **separate goose prompt** (new ACP session); help it when
   stuck, but let it finish with KVM verification.
2. The new kernel gets a **new source tree** — a named thing in the repo, not
   only a filename on a binary. Each track records which shape it chose and why.
3. Sync through **GitHub** (host pushes `origin`, ryzen pushes `litter`), not
   bundles; reconcile the two tracks' changes into one kernel.
4. Say honestly **who built what** (host vs goose) in this log.
5. Document the process here as it goes; hypervisor disagreements are findings.
6. **Cost is tokens and time worked, not dollars**: goose-glm is on the lowest-tier
   GLM subscription. After every goose session, record wall time, LLM calls, input /
   output / cache-read tokens and the largest context in this log
   (`scripts/kvm/goose_box_stats.py`, run on the box).
7. Agents' shells need `HOME=/root` (ssh does not set it; git and logs depend on it).

Tracks:

- **Host track (Kimi/Crush on the Mac)**: builds the same kernel, verifies
  under Firecracker in the Lima VM `fc`. Commits on `rust-org-il-experiment`,
  pushes to `origin`.
- **Metal track (goose-glm on the ryzen, ACP server on :3284)**: builds and
  tests under Akuma's own KVM (`/dev/kvm0`, herd `vm-dev` pattern). Commits on
  its own branches, pushes to the `litter` remote only. Each stage is a
  separate goose prompt (new ACP session per stage).

A stage counts when BOTH tracks pass its gate. Hypervisor disagreements are
findings, recorded here, not averaged away.

## Environment baseline (verified 2026-10-24)

- Box checkout `/src/github.com/netoneko/akuma` on `rust-org-il-experiment`
  @ f8dd945f, experiment doc present. Dirty submodules
  (`userspace/rumpkernel/src-netbsd`, `userspace/tcc/tinycc`) and untracked
  `root/`, `rustc-ice-*.txt` — pre-existing, left alone.
- ACP probe from Mac: `pong` / `end_turn` against
  `http://192.168.1.159:3284/acp`; `/health` 200.
- Lima `fc` VM on the Mac: Running (vz, aarch64, 4 CPU, 8 GiB).

## Stage log

### Stage 1 — boot in KVM (gate: init spawn, banner, zero [Fault]/PANIC/[sig!])

- 2026-10-24 host track: brief prepared, sent to goose as ACP session
  (see below). Firecracker/Lima host-track setup in progress.

- 2026-10-24 *(Sonnet)*: handover. State found: goose-glm is 25+ min into the
  stage-1 turn, reading only (surveying `amd64/src` vs the crate tree), box on
  `taoxichuan-s1` @ 96b3585c, no commits, no `RESULT.md`. Kimi's host-track
  claims (aarch64 stage 1 PASS under Firecracker, an `init-script` stage-2 prep
  in `akuma-config` / `akuma-kernel-glue`, `taoxichuan-fc.bin`) are **not in the
  tree** (uncommitted, lost with the session): treated as unverified and to be
  redone, not as a pass.

- 2026-10-24 *(Sonnet)*: first cost reading of the stage-1 session (`20261024_9`, at
  09:30 UTC, still running): 52 min wall, 96 LLM calls, 8,391,298 input tokens of
  which 8,103,936 (96.6%) cache reads, 46,859 output, largest context 148,749. The
  cost is re-reading a growing history, not generation.

(Entries append below this line, newest stage last.)

### Stage 1 — host track (Sonnet), aarch64 / Firecracker in Lima `fc` — PASS

Built and run by Sonnet on the Mac, not by goose and not by Kimi (Kimi's earlier
claim was re-done, not trusted).

- **Composition root, now named in the tree:** Cargo feature `taoxichuan`
  (`no-tests`, `extreme`, `userspace-sshd`) + `platform-firecracker`, built by
  `scripts/build_taoxichuan_fc.sh`. Zero new Rust source: stage 1 is a feature
  selection over the existing crate tree. The image is **byte-identical**
  (`cmp`) to the one from Kimi's ad-hoc `--features no-tests,extreme,userspace-sshd,
  platform-firecracker` recipe, 662,792 bytes flat (729,136 ELF).
- **Boot:** `FC_KERNEL=taoxichuan-fc.bin overlays/devbox-firecracker/run.sh --no-net`,
  1 vCPU, 1 GiB, the existing 2 GiB ext2 `disk.img` on virtio-blk.
- **Gate evidence** (`/tmp/akuma-fc-boot.log` in the guest, 134 lines):
  `[FS] Ext2 filesystem mounted at /` -> `[Main] Starting userspace sshd (no
  supervisor)...` -> `[Main] sshd started (tid=8)`; kernel-spawned, no herd.
  `grep -c "Fault|PANIC|sig!"` = 0 (the only `fault` text is the `[TLB]
  stale_write_faults=0` counter). Banner: `Akuma Kernel starting...`.
- **Control:** with `--no-disk` the same image reaches only `Block device not
  found`/`Continuing without filesystem` and never spawns init — so "reaches init
  spawn" needs the disk; a diskless pass would have meant nothing.
- **Observations, not failures:** the banner says Akuma, not Taoxichuan (a
  kernel-visible name is still to do); `[unregister] pid=1 tid=8 has NO map owner`
  follows the sshd start, and `[EXCC] el0=29` shows sshd running at EL0 for a
  moment; with `--no-net` it has nothing to serve, which is stage 4's business.
- Metal track (goose, amd64, Akuma KVM) has not reported yet; the stage does not
  count until it does.

### Stage 1 — metal track (goose-glm), amd64 / Akuma KVM — reached the gate, not signed off

Built by goose-glm (GLM 5.3) on the ryzen box; supervised by Sonnet. Branch
`taoxichuan-s1`, commit `43edbffc`, pushed to `litter` by Sonnet (goose had not
pushed before it stopped).

- **What it composed:** a real new source tree. `taoxichuan/` is the composition
  root (own `main.rs`, `linker.ld`, `build.rs`; pulls 13 crates; PVH boot, ext2 root
  on polled virtio-blk, single vCPU, no scheduler/BKL/userspace). Two crates were
  extracted first, as the brief asks: `akuma-boot-amd64` (`boot.s`, `phys.rs`, the
  paging walker, the memory policy; `amd64/` now re-exports them) and
  `akuma-comconsole` (the 16550 console). Image: 162 KB. The `amd64/` kernel was
  refactored onto the shared crates (about 1,000 lines moved) and still *builds*;
  **it has not been booted since**: open item before merging.
- **Gate evidence** (`/root/dev-cycle/taoxichuan-s1/vm-dev-pass.log` on the box,
  09:55 UTC): banner box; `ext2 root mounted on /dev/vda`; `Taoxichuan: init spawn
  -- init=/bin/busybox argv=[sh,/etc/guest.sh]`; `init: 1131168 bytes on the ext2
  root, ELF magic ok`; `Taoxichuan: stage 1 gate reached`; 0 `fault|panic|sig!`
  lines. The guest then halts with interrupts off, as a stage-1 kernel with no
  userspace should.
- **Disagreement, recorded not averaged:** the generic harness verdict printed `[KVM]
  akuma-vm guest: FAIL (banner=yes, init=no, net=no)`. That check looks for the
  full Akuma init/net markers, which a stage-1 kernel never prints; the stage-1 gate
  is the kernel's own `init spawn` line. Both facts stand; the stage is signed off
  only when the harness is given a stage-1 expectation or the gate is re-run by
  hand.
- **Stopped by quota, not by failure:** at 09:56 UTC z.ai answered `Usage limit
  reached for 5 hour. Your limit will reset at 2026-10-24 21:17:13` and the turn
  ended. No `RESULT.md` was written, and `taoxichuan/src/main.rs` has an
  uncommitted edit. The 5-hour window on the lowest GLM tier is a real constraint of
  this track: plan stages to fit it.
- **Cost (from `sessions.db`, session `20261024_9`):** 77.2 min wall, 142 LLM calls,
  16,057,961 input tokens (15,736,512 cache reads, 98.0%), 68,932 output tokens,
  largest context 181,081.

