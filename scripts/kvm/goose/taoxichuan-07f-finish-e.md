# Taoxichuan stage 7, session F: finish session E (G4, the leak question, ops restore, the plan for the rest of stage 6)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Session E (`taoxichuan-s4` @ `a3834ee1` plus whatever it left uncommitted in
the checkout) wrote and used `boot_dev.sh` and `run_gates.sh` in `scripts/kvm/goose/`; a network error ended its turn before it finished. **This session picks up exactly there.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`. Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`.
**Keep your context small, stay under about 80 tool calls.**

## Where session E stopped (facts; read `/root/dev-cycle/taoxichuan-s7/gates-E.txt` first)
The committed head `0ecb1611` ran the whole battery in one chained call, `restarts=0`:
`PASS G1, G2, G3, H1, H2, H3a, H3b, H3c` and `FAIL G4 (see g4.console)` (`ls /etc/ssl1.1/certs` rc 0). `RESULT-D.md` recorded G4 passing on an earlier boot, before steps 1b/1c/1d.
`vm-dev` may still be running with `vm-serve-mc` halted. `git status` may show `run_gates.sh` uncommitted.

## Cost rules
Cap every read (`| cut -c1-180 | head -60`, `grep -n -m 20`, `sed -n` windows of 60 lines or less, never `cat` a source file); put repeated sequences in the scripts; read a script's
output file only for a FAIL.

## Your tasks, in order
1. `git status`: commit `run_gates.sh` and any script fixes, `git push litter taoxichuan-s4`.
2. **G4**: read `g4.console` (capped) and decide whether the kernel or the script's judge is wrong. If the kernel, fix only what the evidence names, in as few new lines as the composition
   rule allows, rebuild (`kbuild -j 4 -p taoxichuan --features no-tests`), `boot_dev.sh`, `run_gates.sh`, report the new table. At most two boots; then record what you know.
3. **The leak question, with the evidence the runs already produced**: `RESULT-D.md` says the host forward "leaks at about 30 connections per boot" and calls it host-side. Does the guest close
   its session sockets when an exec ends? Look at the relay counters and slots in `/var/log/herd/vm-dev.log`, `docs/reference/subsystems/kvm.md` § relay, `docs/archive/KVM_RELAY_SYN_STALL.md`
   and the guest's close/shutdown arms. Report what the evidence shows, with the line numbers.
4. **Restore ops**: stop `vm-dev` and the `:8123` listener, restore both confs to `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4`, `herd disable vm-dev`, `herd start vm-serve-mc`.
5. **Job 3** (below), then `RESULT-E.md`.

## Job 3: the plan for the rest of stage 6, and what comes with it (a plan document, no kernel code)
Table stage 6 (`apk add curl`) is about half done: offline `apk add` passes and outbound TCP passes (H1). Still needed: DNS, a wall clock, `apk update` from a **LAN HTTP mirror**
(no internet, no TLS), `apk add curl` (and `/etc/apk/world`), and `curl --version` running. Write `/root/dev-cycle/taoxichuan-s7/PLAN-stage6-rest.md` (copy to
`docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md`, commit, push `litter`). Read first, selectively: `docs/archive/TAOXICHUAN_STAGE6_PLAN.md` (§3 network, §4 clock, M4-M6),
`docs/archive/TAOXICHUAN_STAGE7_TRIM_PLAN.md` (steps 2-4: where the trim fits), `docs/archive/KVM_OUTBOUND_NETWORKING.md` (the host's DNS forwarder at `10.0.2.3`),
`docs/reference/subsystems/syscalls/time.md` and `crates/akuma-syscalls-time` (the SNTP client and what is callable), `docs/archive/APK_MISSING_SYSCALLS.md`, and `userspace/nettest`
(`nettest-std` is a ready client). The plan must contain: (1) the syscall/feature gap per milestone, diffed against what `taoxichuan/src` serves today, each row with the existing crate or
`amd64/src` function that already does it and a verdict (depend / extract / write new); (2) how the LAN mirror is built and served reproducibly (which packages, signed or `--allow-untrusted`,
how its checksums are compared with a Linux reference); (3) the clock: cheapest correct source; (4) dynamic linking: is `curl` static or does it need `PT_INTERP`/`ld-musl`, and what the
loader has; (5) milestones with a mini-gate command each, boots and token estimates, session boundaries, the cheapest first milestone, and where trim steps 2-3 should be interleaved;
(6) at most five unknowns with their read-only settle. Do not describe unverified fixes as facts.


## Addition from the user (applies to Job 3): the leaf-crate survey comes first
`diff` the `akuma-*` dependencies of `amd64/Cargo.toml` against `taoxichuan/Cargo.toml` (taoxichuan has 17; amd64 about 70). For every milestone of the rest of stage 6, name which of the crates
amd64 uses and Taoxichuan does not covers it (candidates: `akuma-sntp` and `akuma-cmos-rtc` for the clock, `akuma-syscalls-poll` for select/ppoll, `akuma-syscalls-mem`, `akuma-syscalls-net`,
`akuma-syscalls-{linux,abi}` wire types, `akuma-pipe`, `akuma-exec-core`, `akuma-port`, `akuma-tty`). The last plan checked 11 crates; check them all, cheaply (`grep -n "^//!" crates/<name>/src/lib.rs | head -8`
gives each crate's purpose). Where the useful logic is welded to `akuma-exec`/`Process`, say whether a seam (a pure leaf crate, as `akuma-sntp` was cut out of `akuma-syscalls-time`) or a cargo
feature (making `akuma-exec` optional) would free it, and roughly what that costs. The expectation is that the minimal kernel is assembled mostly from the existing crates, cut as needed.

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

## Ops traps
Guest kernel installs only to `/boot/akuma-guest-dev`; never write `/boot/akuma-amd64*`; the host kernel is not rebuilt. `herd` re-parses a conf only on `disable` then `enable`, and a config
reload can create a second guest mid-run (your own finding): the truth is `/var/run/herd/vm-dev.vm`. kvm0 is exclusive.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box**: Akuma's shell turns it into an interactive REPL that waits forever and hangs your tool call (three of
  the supervisor's own did, and a session of yours stalled right after one). Write the script to a file (the `write` tool) and run `python3 /path/x.py < /dev/null`.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after each script fix, each boot that taught you something, each fix that builds, and after the plan. Never `origin`. `taoxichuan` stays amd64-only and
out of root `default-members`; pre-commit clippy and the unsafe-budget check must pass.

## When to stop
Tasks 1-5 done, or about 80 tool calls, or the quota error. `RESULT-E.md` must have: the gate table, per change *pulled / extracted / cut / written new*, `wc -l taoxichuan/src/*.rs` (4,608 before)
and the `akuma-*` dependency count (17 before), the leak answer and the plan's path. End the turn.
