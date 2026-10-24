# Taoxichuan stage 1: the minimal kernel boots in KVM

You are goose-glm, the metal track of the experiment in
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md` (read it first), running
inside Akuma on the ryzen box. The dev loop you already know is
`docs/archive/AKUMA_KVM_DEV_CYCLE.md`. The minimal kernel you are composing
is named **Taoxichuan** — use that name for the new bin/crate, the branch
(`taoxichuan-s1`), and the result files.

Environment (every shell command): start with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`.
The checkout is `/src/github.com/netoneko/akuma` (`$AKUMA_SRC`), branch
`rust-org-il-experiment`, and it already contains both docs above. Work on a
new branch `taoxichuan-s1` off it.

## Stage 1 deliverable

A minimal Akuma amd64 kernel, **Taoxichuan**, that boots as a `/dev/kvm0`
guest (the `vm-dev` pattern from the dev-cycle doc) and passes this gate:

- reaches init spawn,
- prints its banner on the guest console (`/var/log/herd/vm-dev.log`),
- zero `[Fault]`, `PANIC`, or `[sig!]` lines in the ring.

Stage 1 is only "reaches init spawn" — userspace actually *running* is stage
2, a later prompt. Do not build ahead of the ladder.

## How to compose (the experiment's pinned rule)

Each stage is a new line in the composition root that pulls a crate. Pull as
few crates as possible. Where the logic you need still lives in `amd64/src/`
and is not in a crate yet, extract it into a crate first, then pull it.
Composition and refactoring are one loop. The composition root is your choice
per the experiment doc ("new bin crate (or feature set)"), but the artifact
must be named `taoxichuan`.

Pinned exclusions (load-bearing, from the experiment doc): no pty layer, no
users/multi-user machinery, no herd in the minimal image — init is
kernel-spawned (the extreme-size pattern). Single vCPU. Boot under
Firecracker-style virtio-mmio assumptions where a choice exists (no PCI walk
requirement), ext2 root on virtio-blk.

## Ops rules that have cost time before

- Build with `kbuild -j 4 --features no-tests` (output in `/root/ktarget`;
  never `cargo clean`). Install the candidate guest kernel to
  `/boot/akuma-guest-dev` only — **never overwrite `/boot/akuma-guest`**.
- One `kvm0` VM at a time: `herd stop vm-serve-mc` before `herd start vm-dev`,
  and `herd stop vm-dev; herd disable vm-dev; herd start vm-serve-mc` when you
  are done. `vm-serve-mc` must be left running at the end.
- Never `pkill` by name; kill by pid. Never pipe a long-running command into
  `grep -q`; output to a file, poll the file.
- `git fetch`/`push` over HTTPS from this box is believed working; try
  `git push litter taoxichuan-s1` (remote `litter` =
  github.com/netoneko/akuma-litter). **Never push to `origin`.** If the push
  hangs longer than 2 minutes, kill it and fall back to
  `git bundle create /root/dev-cycle/taoxichuan-s1.bundle taoxichuan-s1` and
  say so in the result file.
- Commit early, commit often on `taoxichuan-s1`. Do not touch the dirty
  submodules (`userspace/rumpkernel/src-netbsd`, `userspace/tcc/tinycc`) or
  the untracked `root/`.

## Reporting (required, even on failure)

1. Append a dated `### Stage 1 — metal track (goose-glm)` section to
   `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` in the checkout:
   what you composed, which crates you pulled or extracted, the gate evidence
   (console log excerpts), and anything the hypervisor did that surprised you.
   Commit it with the rest.
2. Write `/root/dev-cycle/taoxichuan-s1/RESULT.md`: PASS/FAIL against the
   gate, branch + commit, how the kernel was booted, and what you would tell
   the next stage.

Work autonomously until the gate passes or you are genuinely blocked; then
write the result file and end the turn.
