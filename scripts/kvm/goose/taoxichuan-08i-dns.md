# Taoxichuan stage 8, session I: M2 - DNS from inside the guest

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: `RESULT-H.md` in `/root/dev-cycle/taoxichuan-s7/`, head `cbb6f4be`
(all of G1 G2 G3 G4 H1 H2 H3a H3b H3c C1 PASS in one boot via `scripts/kvm/goose/run_gates.sh`; the wall clock works from `epoch=`). **Keep your context small.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4` (fetch `litter` first if behind). Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
Stage 6 needs the guest to resolve names itself before `apk update`/`apk add` (the mirror is by IP, but musl's resolver and curl are not). The host gateway's outbound relay answers DNS as a
datagram proxy from `10.0.2.3:53` when `vm_outbound = true`, and has counters for it in the status line of `/var/log/herd/vm-dev.log`. The guest must send and receive the UDP datagrams itself, using
the networking it already has. This session adds exactly one capability: **a name lookup from the guest, by the configured resolver, that works.** Nothing else changes.

## Read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md` - § 1 (DNS row) and § 5 (M2 row) - what the plan expects.
2. `docs/archive/KVM_OUTBOUND_NETWORKING.md` - the DNS paragraphs and § Verified - how the relay answers and how it was verified on the metal.
3. `scripts/kvm/akuma_guest_rootfs.sh` - where a guest image gets `/etc/resolv.conf` today.
4. `scripts/kvm/goose/boot_dev.sh` and `scripts/kvm/goose/run_gates.sh` (head comments only) - how the dev guest is booted and judged (console first).
5. `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md` - § Process and § Session protocol.
(Docs are in the checkout under `docs/`; the front door is `docs/README.md`.)
**Probes already exist - use one before writing a new one.** `userspace/nettest` has `std::net` and libcurl client probes; `abiprobe` checks syscall behaviour (`userspace/build.sh --<name>-only`).
busybox `nslookup` is in the guest image.

## Composition rule (mandatory)
`taoxichuan/` is a **composition root**, not a kernel written from scratch. It is 4,625 newly written lines (`main.rs` 3,077) next to 101 existing crates and a full amd64 kernel.
Before you write any arm, subsystem or helper of more than about 20 lines:
1. **Look first**: `grep -rn "<name>" crates/ amd64/src/` for the concept; `ls crates/`; plan § 0 lists the candidate crates (`akuma-net` carries the smoltcp UDP sockets).
2. **It exists in a crate**: add a path dependency and call it. Do not copy it.
3. **It exists only in `amd64/src/`**: extract it into a crate first (`akuma-<thing>`, `#![forbid(unsafe_code)]` where it can; `scripts/check_unsafe_budget.py` gates new `unsafe`), make `amd64/`
   depend on it too, then depend on it from `taoxichuan/`.
3b. **It exists but is welded to `Process`/`akuma-exec`**: that is a seam to cut (pure leaf crate, as `akuma-sntp` was cut from `akuma-syscalls-time`) or a cargo feature. Say which and what it costs.
    Before writing: diff `amd64/Cargo.toml`'s `akuma-*` dependencies against `taoxichuan/Cargo.toml`'s (18) and say which unused one covers this milestone, if any.
4. **It exists nowhere**: write it, in a crate with host tests where the logic is pure, and say why in the RESULT file.
In the RESULT file report per change *pulled / extracted / cut / written new (why)*, `wc -l taoxichuan/src/*.rs`
at the start and end, and the `akuma-*` dependency count of `taoxichuan/Cargo.toml` (18 now). A session that closes its gates and makes `taoxichuan/src` smaller is better than one that does not.

## Cost rules
Cap every read (`| cut -c1-180 | head -60`, `grep -n -m 20`); never `cat` a source file; read a gate's output file only for a FAIL; put repeated sequences in scripts under `scripts/kvm/goose/`
(instrumentation lives in the repo, not in `/tmp`).

## Gates
- D1: the guest has a resolver configuration that points at `10.0.2.3`, put there by the image/boot scripts (the `.orig` image stays unmodified; say how you did it).
- D2: ONE command over `/bin/ssh -p 2224 root@localhost 'nslookup alpinelinux.org'` (no server argument, so the guest's own configuration is used) -> console `[SSH] Exit status: 0`, and the relay's
  `dns_answered` counter in `/var/log/herd/vm-dev.log` is higher after the command than before. Add it to `run_gates.sh` as `D2`, judged on the console like the others, with the counter check as a NOTE line.
- Zero `Fault|PANIC|sig!` in a passing boot; `run_gates.sh` prints `PASS` for G1 G2 G3 G4 H1 H2 H3a H3b H3c C1 D2 in the same boot with `restarts=0`.
- Out of scope: `apk update`, the mirror, the clock, the loader, any change under `amd64/` or `src/` unless the composition rule's step 3 requires an extraction (then say so).

## Ops traps
- Guest kernel installs only with an explicit `/boot/akuma-guest-dev` destination; never write `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); herd re-parses only on `disable` then `enable`; the truth is
  `/var/run/herd/vm-dev.vm`; confirm the `guest disk:` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot; restore everything at the end (stage4 conf, `vm-dev` disabled,
  `vm-serve-mc` up, `:8123` stopped).
- The host->guest ssh capture loses output and the forward degrades near 30 connections per boot: judge by the console (`[SSH] Exit status`), pace 15 s, one exec per connection.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box**: Akuma's shell turns it into an interactive REPL that hangs your tool call. Write the script to a file and run
  `python3 /path/x.py < /dev/null`.
- Never run goose/crush wrappers on the box.
- Check the `kbuild` exit code before booting: a failed build boots the previous binary (session H, boot 2). Gate commands run as ONE simple command (`sh` cannot fork in the guest: no `$(...)`, `;`, `&&`, pipes).

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after M0 is done, after every boot cycle that taught you something, and after every fix that builds. WIP commits are fine. Never push `origin`.
Gate arch-specific code with `cfg`; keep amd64-only crates out of default-members; pre-commit clippy and the unsafe-budget check must pass (never `--no-verify`).

## When to stop
D1 and D2 pass in a clean boot with the full battery, or 4 boots with no new evidence, or the quota error. Count boots, not minutes. Write `/root/dev-cycle/taoxichuan-s7/RESULT-I.md` (gates; per change
pulled / extracted / cut / written new; `wc -l taoxichuan/src/*.rs` start and end; `akuma-*` dependency count of `taoxichuan/Cargo.toml` (18 now); the exact failing step with console evidence if any;
what is next) and end the turn.
