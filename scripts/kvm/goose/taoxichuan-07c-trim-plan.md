# Taoxichuan stage 7, session C: PLAN the trim - what can be rewired from existing crates and still hit H1 (planning only, no kernel code)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Stages 6-7 grew
`taoxichuan/src/` by about 1,200 hand-written lines (3,268 at the end of stage 4, 4,466 now; `main.rs` 2,928) while `amd64/` and
`src/` were not touched and no crate was created. The experiment's composition rule says the opposite: `taoxichuan/` is a
composition root over the existing crates. **Your only deliverable this session is a plan document.** A separate session will execute
it, so make it precise. Aim for a small context: read selectively, `grep` before you `sed -n`, never read whole large files.

**Do not** edit any Rust, run `kbuild`, boot a VM, run `herd`, or touch `/boot` and `/root/.config/goose`. Reading, `grep`, `wc` and
host-side `cargo test` of a crate are fine. Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Checkout: `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`
(`b7646dd9`). Work in `/root/dev-cycle/taoxichuan-s7/`.

## The composition rule (the lens for the whole plan)
Before writing any syscall arm, subsystem or helper over ~20 lines: (1) look for it in `crates/` (101 crates: `akuma-syscalls-glue`
and the `akuma-syscalls-*` family, `akuma-fdtable`, `akuma-vfs`, `akuma-ext2`, `akuma-net`, `akuma-exec`, `akuma-ring3-amd64`,
`akuma-svm-amd64`, ...) and in `amd64/src/` (the full amd64 kernel: `usermode.rs` is the syscall dispatch, `fd.rs` the fd layer,
`fs.rs` the filesystem arms, `kvm/` the hypervisor); (2) if it is in a crate, depend on it and call it; (3) if it is only in
`amd64/src/`, extract it into a crate first (`akuma-<thing>`, named for what it owns; `#![forbid(unsafe_code)]` where it can;
`scripts/check_unsafe_budget.py` gates new `unsafe`) so both kernels consume ONE implementation; (4) only if it exists nowhere, write it,
in a crate with host tests. Naming and crate rules: `CLAUDE.md` § Layout / Crate naming.

## Read first (about 15 minutes; the named parts only)
1. `CLAUDE.md` (the layout paragraphs on `akuma-fdtable`, `akuma-syscalls-*`, `akuma-vfs`, `akuma-mmap`: each says what it owns and what
   must never be re-implemented elsewhere) and `docs/reference/crate-safety.md`.
2. `docs/reference/subsystems/syscalls.md` and `docs/reference/subsystems/syscalls/{fs,net,poll,mem}.md`; `docs/reference/subsystems/fdtable.md`;
   `docs/reference/subsystems/vfs.md`.
3. `docs/archive/AKUMA_AMD64_STREAMLINING.md` and the `AKUMA_AMD64_4B_FOLD_BATCH*.md` docs: how the amd64 kernel was already folded onto
   the shared glue (the same fold is what `taoxichuan/` needs).
4. `docs/archive/TAOXICHUAN_STAGE6_PLAN.md` (the previous plan, for the format) and your own `RESULT-A.md`.
5. The code: `taoxichuan/src/{main,child,sock,net}.rs` (`grep -n "^fn \|^pub fn \|^enum \|^struct \|^impl "` first), `taoxichuan/Cargo.toml`
   (what is already a dependency), and `amd64/src/{usermode,fd,fs}.rs` for the same functions.

## What the plan must contain (write it to `/root/dev-cycle/taoxichuan-s7/PLAN-trim.md`, copy to `docs/archive/TAOXICHUAN_STAGE7_TRIM_PLAN.md`)
1. **Inventory table**: every syscall arm and subsystem in `taoxichuan/src/*.rs` (group them: fd table, open/close/dup, read/write/
   writev/lseek, stat family, dirs, sockets/connect/poll, exec/spawn/argv, mmap/brk, time, misc). Per row: lines it occupies in
   `taoxichuan/src`, the existing implementation that does the same job (crate + function, or `amd64/src/<file>:<fn>`), and a verdict:
   **replace by dependency** / **extract then depend** / **keep (say why it exists nowhere)**. Give the total lines the plan would
   remove and the lines it would add. Be honest where the existing implementation is welded to `akuma-exec` or `Process` and cannot
   be called as is.
2. **The H1-preserving core**: the smallest set of rewires that keeps every green gate green. The gates to preserve are stage 6's G1-G3
   (`sh -c "echo hi"`; `apk.static --version`; offline `apk add` of `/pkgs/ca-certificates-bundle-20261020-r0.apk` with
   `--force-non-repository`; hash `5961c0a3...`), `ls /etc/ssl1.1/certs`, and **H1**: the guest's `busybox wget -qO-
   http://192.168.1.159:8123/` prints `hello-from-host` while the host listener logs the GET. Name the order of steps so that after each
   step the whole gate set can be re-run in one boot, and a failed step is revertible alone.
3. **H2 and H3 plans** (they are open, so plan their fix inside the same rewiring, not as extra hand-written arms):
   - H2: `dd if=/etc/hostname of=/dev/null` exits 0 (opening `/dev/null` was EACCES; an `OpenFile::Null` arm exists in `b7646dd9`, ungated).
     Which existing crate/device layer owns `/dev/null` and the other standard devices, and does the trim make the hand-written arm unnecessary?
   - H3: `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` prints `x` then 2. Today `/tmp/f` is created but stays empty. Read the
     create/write path against the ext2 write path the full kernel uses (`akuma-ext2`, `amd64/src/fs.rs`, `akuma-vfs`) and say what the correct
     path is, whether `taoxichuan/` calls it, and the exact change (by rewiring, not a new arm if at all avoidable). You may reason about it from the
     code; do not boot.
   - Name the existing userspace probes that verify each (`userspace/ext2probe/c`, `userspace/abiprobe/c`, `userspace/forktest/c_stress/`; the
     one-liners are in `docs/reference/userspace-layout.md`) and how to run them in the guest.
4. **Portability and gates for the executing session**: which new crates need `#[cfg(target_arch = "x86_64")]`, that none of them may enter
   the root `default-members` unless they build for aarch64, that `scripts/check_unsafe_budget.py` and `cargo clippy` (pre-commit) must pass,
   and the host tests each extraction needs.
5. **Sessions, boots and tokens**: split the plan into execution sessions (a fresh context each; a session should end after one coherent
   group of rewires), estimate boots per step and the risk of each step (low/medium/high, one line why), and the cheapest first step.
   Context size drives cost: ~100k tokens per call average so far, fresh contexts ~30k.
6. **Unknowns that could kill the plan**, at most five, each with the (read-only) experiment that settles it.

## Rules
Commit the two plan files and `git push litter taoxichuan-s4` (after you have written them; never `origin`). End the turn when the plan is
pushed. No RESULT file is needed beyond the plan. Keep it concrete: a table per section beats prose; if a number is a guess, say so.
Do not describe fixes you have not verified in the code as facts.
