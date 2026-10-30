# Taoxichuan stage 9, session Q: `curl` fetches an HTTPS URL in the guest, body byte-equal to the host's

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: O2 (`RESULT-O2.md`, head `6bde98eb`: `curl --version` prints the reference line, main battery 19/19, B1-B4 PASS). **Keep your context small.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4` (fetch `litter` first if behind). Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
`curl` installs (M4) and starts (M5): a dynamically linked PIE runs through `ld-musl` in the guest. This session adds exactly one capability: **`curl` completes an HTTPS request to the real internet from the guest, and the body it saves is byte-equal to the body the same `curl` saves on the Linux reference.** The guest reaches the internet through the host (`vm_outbound`), names resolve through the relay resolver, and the CA file is `/etc/ssl/certs/ca-certificates.crt`. Nothing else is in scope; the next rung after this one is `git`, and it is not this session.

## Run this first (it is the premise table, executable)
`sh /root/dev-cycle/taoxichuan-s7/premises-Q.sh < /dev/null` - prints one `HOLDS n` or `WRONG n (what it saw)` line per premise (8 of them; the supervisor ran it and all eight held). Paste its output into `RESULT-Q.md` as the first section. A `WRONG` row that changes the plan is reported to the user as soon as you commit. Rows 5-7 only prove the named script or file exists, not that the guest passes it; treat them as UNCHECKED for behaviour until a boot shows it.

## Reference value (measured on the box 2026-10-26 with the refroot `curl`, the same binary the guest installs)
URL (a pinned commit of a public repo; the object never changes):
`https://raw.githubusercontent.com/netoneko/akuma-playground/f70d39f17a7edb1c7ef7245c93cbc8fa59ec12ab/hello.c`
Body: 77 bytes, sha256 `c563b63a9149349aee48b15b5eee4966a8a46a65888967b02aa0952f6c4b916a`. A fetch from the supervisor's Mac gave the same hash.

## Read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `RESULT-O2.md` (on the box, `/root/dev-cycle/taoxichuan-s7/`) - sections 5 and 6 only.
2. `scripts/kvm/goose/e_group.sh` (whole, 35 lines) and `scripts/kvm/goose/run_gates.sh` head comments - the verdict rule: the console's `Exit status: 0` is the only truth; the ssh client's capture lies.
3. `docs/reference/subsystems/kvm.md` - the "Outbound networking" section only.
**Probes already exist - use one before writing a new one**: `userspace/nettest` (libcurl and `std::net` + `poll(2)` client probes, built for a network hang), `userspace/abiprobe`, `userspace/epollprobe`. A new probe is allowed only when you can say why none of the existing ones fits, and it goes into `userspace/` or `scripts/kvm/goose/`, never `/tmp`. When a gate fails, find the existing probe that isolates the failing syscall before reading kernel code. List what you looked at, and what it printed, in `RESULT-Q.md`.

## Guest facts (verified; do not rediscover)
No `fork`/`clone`: `sh` runs one simple command per ssh exec (no `;`, `&&`, `$(...)`, pipes). No procfs: `ps`/`df` print only their header. busybox 1.35 has no `stat`; it has `sha256sum` (with `-c`) and `grep`. Static, static-PIE and dynamic PIE binaries run. One core. The host forward degrades near 30 ssh connections per boot (keep a group to 20 execs or fewer). The root image is 1 GiB; herd rotates `vm-dev.log` mid-boot. The wall clock is set from SNTP at boot. `curl` is not in the image until the B group installs it from the mirror (`mirror.sh start` first, or the guest spins on a refused dial).

## Composition rule (mandatory)
`taoxichuan/` is a composition root, not a kernel written from scratch (about 5,100 lines of its own next to 100+ crates and a full amd64 kernel that serves nearly every syscall you will meet). Before you write any syscall arm, subsystem or helper of more than about 20 lines:
1. **Look first**: `grep -rn "<name>" crates/ amd64/src/` for the syscall or concept; `ls crates/` for the crate that owns it.
2. **It exists in a crate**: add a path dependency and call it. Do not copy it.
3. **It exists only in `amd64/src/`**: extract it into a crate first (`forbid(unsafe_code)` where it can), make `amd64/` depend on the crate too, then depend on it from `taoxichuan/`.
3b. **It exists but is welded to `Process`/`akuma-exec`**: cut a seam, do not rewrite.
4. **It exists nowhere**: write it, in a crate with host tests when the logic is pure, and say why in `RESULT-Q.md`.
Report per arm or feature: *pulled* / *extracted* / *written new (why)*, and `wc -l taoxichuan/src/*.rs` at the start and end.

## Gates (one exec per connection; verdicts from the console)
Start the mirror first (`scripts/kvm/goose/mirror.sh start`), boot, run the main battery and the B group (installs curl), then, on the same boot, with `e_group.sh`:
- Q1: `e_group.sh q1='curl -sS -o /tmp/h https://raw.githubusercontent.com/netoneko/akuma-playground/f70d39f17a7edb1c7ef7245c93cbc8fa59ec12ab/hello.c'` -> console `Exit status: 0`.
- Q2: `e_group.sh q2a='sh -c "sha256sum /tmp/h > /tmp/h.out"' q2="grep -cx 'c563b63a9149349aee48b15b5eee4966a8a46a65888967b02aa0952f6c4b916a  /tmp/h' /tmp/h.out"` -> console `Exit status: 0` for both (the same `grep -cx` shape E2 used; two spaces between hash and name is busybox's output format, supervisor-seen on busybox 1.37 on the box, UNCHECKED on the guest's 1.35 - if Q2 fails, first print `/tmp/h.out` with `cat` to see the actual line).
- Regression: main battery and B1-B4 unchanged (`restarts=0`), E1/E2 still pass, and zero `Fault|PANIC|sig!` in a passing boot.
- Out of scope: `git`, `fork`/`clone`, multi-command `sh -c`, redirects or proxies, `apk info -L curl` (its crash is a separate open item), any change to the host kernel.

## Ops traps
- Guest kernel installs only with an explicit `/boot/akuma-guest-dev` destination; never write `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs; confirm the `guest disk: ..., 1024 MiB` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot. Restore at the end in order: `herd stop vm-dev`, stage4 conf into BOTH dirs, `herd disable vm-dev`, `herd start vm-serve-mc`, `mirror.sh stop`.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box** (Akuma's shell turns it into an interactive REPL that hangs your call). Write the script to a file and run `python3 /path/x.py < /dev/null`.
- Never run goose/crush wrappers on the box. Do not touch other people's services.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after every boot cycle that taught you something and after every fix that builds. WIP commits are fine. Never push `origin`. **Before the final commit run `cargo test -p <crate> --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)` for every crate you touched.** Anything you add only to see (a debug `safe_print!`, a probe line) carries the tag `[tmp]` and is listed under "Remove before final" in `RESULT-Q.md`; remove them before the last boot.

## Budget
About 90 tool calls and at most 5 boots. At 90 calls without a commit the supervisor will nudge you. A 5-hour z.ai window is about 34M tokens; stay well under 10M. If you are past the budget without the gate passing, write what you have and stop: a fresh context with a short continuation prompt is cheaper than a long one.

## When to stop
Q1 and Q2 pass on a clean boot with the regression checks above, or 5 boots with no new evidence, or the quota error. Count boots, not minutes. Write `/root/dev-cycle/taoxichuan-s7/RESULT-Q.md` (the premises output, the probe hit list, the gates with console lines, the exact failing step if any, what is next) and end the turn.
