# Taoxichuan stage 8, session K: trim step 2 - the wire structs come from the ABI crates, behaviour byte-identical

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: J (`RESULT-J.md`, head `49d418de`; all of G1 G2 G3 G4 H1 H2 H3a H3b H3c C1 D1 D2 PASS in one boot). **Keep your context small.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4` (fetch `litter` first if behind). Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
This is a **refactor with no behaviour change**. `taoxichuan/src` hand-writes several Linux wire structures byte by byte (`fill_stat64` with `st[24..28]` style offsets, `fill_statfs`, the `getdents64`
record encoder, the `uname` fields, timespec/timeval writers, and a `nr_name` table of syscall names). The crates `akuma-syscalls-linux` (the `repr(C)` structs with layout assertions: `stat`, `dirent`, `time`, `uts`)
and `akuma-syscalls-abi` (one `syscall_table!` row per call, x86_64 numbers) already own those shapes and the full kernels consume them. Step 2 makes `taoxichuan` consume them too, **one family per commit**, so any
single revert is clean. What must NOT change: any byte the guest sees. apk, busybox and the DNS path (M3/M4 next) hammer exactly these structs, so a silent change here costs the next two milestones.

## Read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `docs/archive/TAOXICHUAN_STAGE7_TRIM_PLAN.md` - § 2 (step 2 row) and § 4 (new dependencies) - what the plan expects. Its gate for step 2 names `abi_write_probe`: **that probe is built for aarch64 only
   (`userspace/abiprobe/c/build.sh` uses `aarch64-linux-musl-gcc`) and cannot run in this guest; use the W1 gate below instead.**
2. `docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md` - § 0 (the `akuma-syscalls-linux` / `-abi` rows) and § 5 (trim interleaving).
3. `crates/akuma-syscalls-linux/src/{stat,dirent,uts,time}.rs` - the public API only (`grep -n "pub " <file> | head -40`); `crates/akuma-syscalls-abi/src/lib.rs` head comment and the `Syscall` enum's decode.
4. How `amd64/src/` consumes them: `grep -rn "akuma_syscalls_linux\|akuma_syscalls_abi" amd64/src/ | head -30` - copy the call shape, do not invent one.
5. `scripts/kvm/goose/run_gates.sh` - G3 (the guest-side sha check pattern) and the head comments.
Probes: none needed beyond the gates below.

## Composition rule (mandatory)
`taoxichuan/` is a **composition root**. It is 4,779 newly written lines now (`main.rs` 3,085) and has 16 `akuma-*` path dependencies (`grep -c '^akuma-' taoxichuan/Cargo.toml`; earlier briefs said 18, that was wrong).
This whole session *is* the rule applied: for every hand-written struct writer, **look first** (`grep -rn` in `crates/`), **pull** the crate type, delete the local bytes. Say in RESULT-K per family: *pulled / extracted / cut /
written new (why)* with lines removed. Anything the ABI crates lack (a field, a helper) is a step-3 extraction into the crate with a host test, not a local patch. Do not widen the work into routing or the socket
table. Before adding the two dependencies, diff `amd64/Cargo.toml`'s `akuma-*` list against `taoxichuan/Cargo.toml`'s and say what else unused there could shrink this file (do not act on it; list it).
A session that closes its gates and makes `taoxichuan/src` smaller is better than one that does not.

## Cost rules
Cap every read (`| cut -c1-180 | head -60`, `grep -n -m 20`); never `cat` a source file; read a gate's output file only for a FAIL; repeated sequences go in scripts under `scripts/kvm/goose/`.

## Gates
- **W0 (baseline, once, before any edit)**: boot the CURRENT kernel with `scripts/kvm/goose/boot_dev.sh`, then record the guest's own bytes for the structs you will touch, as three single commands each (`sh` cannot fork:
  no `$(...)`, `;`, `&&`, pipes): `stat -c "%d %i %h %f %u %g %t %T %s %B %b %o" /etc/hostname > /tmp/w1a`; `stat -f -c "%t %S %b %f %a %c %d %l" / > /tmp/w1b`; `ls -l /etc > /tmp/w1c`; `uname -a > /tmp/w1d`; then
  `sha256sum /tmp/w1a /tmp/w1b /tmp/w1c /tmp/w1d > /tmp/w1h` and read the four hashes from the console/guest (follow G3's pattern). Commit the four expected hash prefixes to a file under `scripts/kvm/goose/`.
  If a busybox applet is not in the image or a field is unsupported, pick the nearest applet; do not skip a family silently - say which family has no gate.
- **W1**: add to `run_gates.sh` as `W1a`..`W1d`, judged by guest-side exit code (`grep -c <prefix> /tmp/w1h`-style, as G3), NOT by client capture. All four must give the same hash as W0 after every family commit.
- Zero `Fault|PANIC|sig!` in a passing boot; `run_gates.sh` prints `PASS` for G1 G2 G3 G4 H1 H2 H3a H3b H3c C1 D1 D2 W1a W1b W1c W1d in the same boot with `restarts=0`, at the end of the session on the final kernel.
- Host: `cargo test -p akuma-syscalls-abi -p akuma-syscalls-linux`; `scripts/check_unsafe_budget.py` and clippy must pass at commit (pre-commit runs them; never `--no-verify`).
- Per-family commit order (stop at any family whose bytes differ and say so): (1) stat/fstat/lstat + statfs; (2) getdents64; (3) uname; (4) time structs; (5) `nr_name` via `akuma-syscalls-abi` if it comes out smaller. Run the full
  W1 battery after each family that changed bytes; family 5 changes no bytes so one final run suffices.
- Out of scope: routing, the socket table, the clock, DNS, `apk update`, the loader, anything under `amd64/` or `src/` unless a pure extraction is required (then say so).

## Ops traps
- Guest kernel installs only with an explicit `/boot/akuma-guest-dev` destination; never write `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); herd re-parses only on `disable` then `enable`; truth is `/var/run/herd/vm-dev.vm`;
  confirm the `guest disk:` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot; restore everything at the end (stage4 conf, `vm-dev` disabled, `vm-serve-mc` up, `:8123` stopped).
- The vm-dev log **appends across boots**: attribute evidence per boot boundary. Check the `kbuild` exit code before booting (a failed build boots the previous binary).
- The host->guest ssh capture loses output and the forward degrades near 30 connections per boot: judge by the console (`[SSH] Exit status`), pace 15 s, one exec per connection.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box**: Akuma's shell turns it into an interactive REPL that hangs your tool call. Write the script to a file, run `python3 /path/x.py < /dev/null`.
- Never run goose/crush wrappers on the box.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after W0, after each family that builds and passes W1, and after every boot cycle that taught you something. WIP commits are fine. Never push `origin`.
Gate arch-specific code with `cfg`; keep amd64-only crates out of default-members.

## When to stop
All families done (or the ones that cannot be byte-identical named with evidence) and the full battery PASS in one clean boot, or 4 boots with no new evidence, or the quota error. Count boots, not minutes.
Write `/root/dev-cycle/taoxichuan-s7/RESULT-K.md` (gates; per family pulled / extracted / cut / written new; `wc -l taoxichuan/src/*.rs` start and end; `akuma-*` dependency count (16 now, 18 after this step);
the exact failing step with console evidence if any; what is next) and end the turn.
