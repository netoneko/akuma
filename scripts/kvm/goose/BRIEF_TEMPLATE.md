# Taoxichuan stage N, session X: <one line: the gate>

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session:
<RESULT file, commit>. **Keep your context small.** Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
<What already works elsewhere / what must NOT be changed / what the last session got wrong. One paragraph.>

## Read first (in order; `sed -n` the named sections, never whole files)
1. `<doc>` - <section> - <what it tells you>
2. ...
**Probes already exist - use one before writing a new one.** `userspace/` holds purpose-built guest-side probes (list and
one-liners: `docs/reference/userspace-layout.md`): `nettest` (libcurl and `std::net` + `poll(2)` client probes, built for a
network hang), `abiprobe`, `epollprobe`/`memprobe`/`futexprobe` (per-syscall cost and behaviour), `ncaprobe`, `ext2probe`.
Name the one that fits this gate, and how to build it (`userspace/build.sh --<name>-only`).
(Docs are in the checkout under `docs/`; the front door is `docs/README.md`, its symptom matrix lists the right doc.)

## Composition rule (mandatory; stages 1-4 had it, stages 6-7 lost it)
`taoxichuan/` is a **composition root**, not a kernel written from scratch. At the start of stage 7 it was 4,450 hand-written lines
(`main.rs` 2,928) next to 101 existing crates and a full amd64 kernel that already implements nearly every syscall you will meet.
Before you write any syscall arm, subsystem or helper of more than about 20 lines:
1. **Look first**: `grep -rn "<name>" crates/ amd64/src/` for the syscall or concept; skim `docs/reference/crate-safety.md` and
   `ls crates/` for the crate that owns it (`akuma-syscalls-glue` and the `akuma-syscalls-*` family, `akuma-fdtable`, `akuma-vfs`,
   `akuma-ext2`, `akuma-net`, `akuma-exec`, `akuma-ring3-amd64`, `akuma-svm-amd64`).
2. **It exists in a crate**: add a path dependency and call it. Do not copy it.
3. **It exists only in `amd64/src/`**: extract it into a crate first (`akuma-<thing>`, named for what it owns; keep `#![forbid(unsafe_code)]`
   where it can, `scripts/check_unsafe_budget.py` gates new `unsafe`), make `amd64/` depend on the crate too, then depend on it from
   `taoxichuan/`. One implementation, two consumers.
4. **It exists nowhere**: write it, in a crate with host tests where the logic is pure, and say why in the RESULT file.
In every RESULT file report, per arm or feature you added: *pulled* / *extracted* / *written new (why)*, and `wc -l taoxichuan/src/*.rs`
at the start and the end. A session that closes a gate and makes `taoxichuan/src` smaller is better than one that closes it larger.

## Gates
- <ID>: <exact command> -> <exact expected output>. Source of the command/address: <doc or passing run>.
- Zero `Fault|PANIC|sig!` in a passing boot; re-run the previous stage's gates once at the end.
- Out of scope: <list>.

## Ops traps
- Guest kernel installs only with an explicit `/boot/akuma-guest-dev` destination; never write `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs; confirm the
  `guest disk:` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot; restore everything at the end.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after every boot cycle that taught you something and after every fix that builds.
WIP commits are fine. Never push `origin`. Gate arch-specific code with `cfg`; keep amd64-only crates out of default-members.

## When to stop
Gate passes in a clean boot, or <N> boots with no new evidence, or the quota error. Count boots, not minutes. Write
`/root/dev-cycle/<dir>/RESULT-<X>.md` (gates, exact failing step with console evidence, what is next) and end the turn.
