# Taoxichuan stage 2: userspace works — a static musl hello runs, exit 0

You are goose-glm, the metal track of the experiment in
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Stage 1 is done and
signed off on both tracks (your `taoxichuan-s1` branch, head `d2781860`, is merged into
`rust-org-il-experiment`; the aarch64 host track re-passed the gate on the merged tree).
This prompt is stage 2 only — a hello binary in ring 3. busybox is stage 3, a later
prompt; do not build ahead.

Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout:
`/src/github.com/netoneko/akuma` ($AKUMA_SRC).

## 1. Take the supervisor's state first

`git fetch origin rust-org-il-experiment`, then branch `taoxichuan-s2` off
`taoxichuan-s1` and `git merge --no-edit origin/rust-org-il-experiment` into it (expect
no conflicts; it adds only agent-log sections and supervisor tooling). Keep the stage-1
hygiene rules from here on: any new `asm!`/`global_asm!`/port I/O behind
`#[cfg(target_arch = "x86_64")]`, and `cargo clippy -p <crate> --target
x86_64-unknown-none -- -D warnings` green for every crate you touch.

## 2. Stage 2 deliverable

The pinned gate (experiment doc, stage 2): **a static musl hello runs, exit 0, output
matches the host-built binary.** Concretely, as a `/dev/kvm0` guest (the `vm-dev`
pattern):

- Build a static musl `hello` on the box (amd64), run it on the host, save its exact
  output bytes as the reference.
- Stage that same binary onto the guest ext2 root (the same image stage 1 booted;
  `/bin/busybox` already lives there — add `/bin/hello` the same way).
- Taoxichuan loads it, enters it in ring 3, it prints its line and exits 0.
- Console evidence: the hello's output line, a kernel line reporting the captured exit
  code (`0`), and zero `[Fault]`/`PANIC`/`[sig!]` lines in the ring. Compare the
  guest's hello output against the host reference byte-for-byte (state the result in
  RESULT.md).

Your stage-1 RESULT.md already sketched the work; it is the plan, quoted:

1. **Ring-3 plumbing:** user code/data GDT descriptors (DPL 3), a TSS with an RSP0
   kernel stack, `U/S` on user page-table entries, IDT stack switch on privilege
   change.
2. **A loader:** walk the already-read ELF's `PT_LOAD` segments (the ext2 +
   virtio-blk read path works), map at link addresses, build a stack with
   argc/argv/envp/auxv — musl static wants `AT_PHDR/AT_PHENT/AT_PHNUM/AT_ENTRY/
   AT_PAGESZ` at minimum — enter via `sysretq`. Keep the identity map until entry
   works without it.
3. **A syscall path:** `EFER.SCE` + `LSTAR`/`STAR`/`SFMASK`; a minimal table:
   `write(1, buf, len)` into the existing `console::puts` path, `exit(code)` records
   the code and tears the task down, a `brk` stub over a bump region as insurance for
   musl's early setup.
4. **Exit as the evidence:** the kernel prints the captured exit code; ring-3 faults
   must print `[Fault]` and kill the task, not triple-fault into guest silence.

Keep the pinned exclusions: single vCPU, no pty layer, no users machinery, no
scheduler (one task that only exits does not need one), no herd in the minimal image —
init stays kernel-spawned. Per the experiment's composition rule: pull as few crates
as possible; where the logic you need still lives in `amd64/src/` (or in `taoxichuan/`
as inline code that the main kernel also has), extract into a crate first, then pull.
Do not regress the stage-1 gate: the banner, the spawn line, and the zero-fault grep
must all still hold.

## 3. Ops rules (unchanged, they have cost time before)

- `kbuild -j 4 -p taoxichuan --features no-tests`; install to `/boot/akuma-guest-dev`
  only, never `/boot/akuma-guest`. Never `cargo clean`.
- One `kvm0` VM at a time: `herd stop vm-serve-mc` before `herd enable vm-dev; herd
  start vm-dev`; when done, `herd stop vm-dev; herd disable vm-dev; herd start
  vm-serve-mc`. `vm-serve-mc` must be running at the end.
- Never `pkill` by name; kill by pid. Never pipe a long-running command into
  `grep -q`; output to a file, poll the file.
- The harness's `[KVM] akuma-vm guest: FAIL (banner=yes, init=no, ...)` line is herd's
  generic marker check and appears on clean passes; the stage gate is the kernel's own
  evidence lines plus the zero-fault grep.
- Also boot the main amd64 kernel once as a guest (`kbuild -j 4 --features no-tests`,
  same dance; `AKUMA-GUEST-UP` → `AKUMA-GUEST-DONE`) before you finish, to prove your
  changes did not break it.
- `git push litter taoxichuan-s2` after every commit (over 2 minutes: kill it and
  `git bundle create /root/dev-cycle/taoxichuan-s2.bundle taoxichuan-s2`, and say so).
  **Never push to `origin`.** Do not touch the dirty submodules
  (`userspace/rumpkernel/src-netbsd`, `userspace/tcc/tinycc`) or untracked `root/`.

## 4. Paperwork (commit early, commit often — after each numbered step, not only at the end)

1. Save the gate console log to `/root/dev-cycle/taoxichuan-s2/vm-dev-pass.log` (and
   the main-kernel boot log beside it).
2. Append a dated `### Stage 2 — metal track (goose-glm)` section to
   `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`: what you composed,
   which crates you pulled or extracted, the gate evidence (console excerpts), the
   byte-compare result, anything surprising. Do not rewrite other sections.
3. Write `/root/dev-cycle/taoxichuan-s2/RESULT.md`: PASS/FAIL against the gate,
   branch + commit, how it was booted, and what you would tell the stage-3 prompt
   (busybox `ash` running a script against the ext2 root).

You are on `glm-5.3-flash` at low effort: read files in slices, no whole-file dumps;
the 5-hour quota is the budget, and stage 1 showed a finish-the-paperwork follow-up
prompt costs a tenth of a fresh stage. Work autonomously until the gate passes or you
are genuinely blocked; then write the result file, commit, push, and end the turn.
