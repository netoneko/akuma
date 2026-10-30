# Taoxichuan stage 8, session H: M0 (LAN mirror + Linux reference, host only) and M1 (wall clock from an `epoch=` cmdline anchor)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: `RESULT-E.md` / `RESULT-G` in
`/root/dev-cycle/taoxichuan-s7/`, head `fa8a1c53` (all of G1 G2 G3 G4 H1 H2 H3a H3b H3c PASS in one boot via `scripts/kvm/goose/run_gates.sh`). **Keep your context small.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4`.
Every shell command starts with `export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
Stage 6 (`apk add curl` in the guest) is half done: offline `apk add` and outbound TCP (H1) work. The guest will never get internet or TLS; it gets bytes from a frozen LAN HTTP
mirror. This session does the two cheapest milestones of `docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md`: **M0** touches no kernel and boots nothing; **M1** is a small change in one place
plus one boot. Do M0 first, commit, then M1. Everything in `run_gates.sh` that passed must still pass.

## Read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md` - § 0 (survey table), § 1 (gap table), § 2 (mirror), § 3 (clock), § 6 (unknowns U1-U3) - this is the plan you are executing.
2. `docs/archive/TAOXICHUAN_STAGE6_PLAN.md` - § 7 - the host reference recipe for the mirror.
3. `docs/reference/subsystems/syscalls/time.md` - the UTC offset store and what is callable from `akuma-timer` / `akuma-syscalls-time` / `akuma-sntp`.
4. `scripts/kvm/goose/boot_dev.sh` and `scripts/kvm/goose/run_gates.sh` (head comments only) - how a dev guest is booted and how gates are judged (console first).
5. `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md` - § Process and § Session protocol.
(Docs are in the checkout under `docs/`; the front door is `docs/README.md`.)
**Probes already exist - use one before writing a new one.** `userspace/` holds purpose-built guest-side probes (list: `docs/reference/userspace-layout.md`): `abiprobe` checks syscall
behaviour; build with `userspace/build.sh --abiprobe-only`. For the clock gate busybox `date` is enough.

## Composition rule (mandatory)
`taoxichuan/` is a **composition root**, not a kernel written from scratch. It is 4,581 newly written lines (`main.rs` 3,033) next to 101 existing crates and a full amd64 kernel.
Before you write any arm, subsystem or helper of more than about 20 lines:
1. **Look first**: `grep -rn "<name>" crates/ amd64/src/` for the concept; `ls crates/`; the survey table in the plan § 0 already names the crates for this session (`akuma-timer`, `akuma-sntp`,
   `akuma-syscalls-time`, `akuma-cmos-rtc`).
2. **It exists in a crate**: add a path dependency and call it. Do not copy it.
3. **It exists only in `amd64/src/`**: extract it into a crate first (`akuma-<thing>`, `#![forbid(unsafe_code)]` where it can; `scripts/check_unsafe_budget.py` gates new `unsafe`), make `amd64/`
   depend on it too, then depend on it from `taoxichuan/`.
3b. **It exists but is welded to `Process`/`akuma-exec`**: that is a seam to cut (pure leaf crate, as `akuma-sntp` was cut from `akuma-syscalls-time`) or a cargo feature. Say which and what it costs.
    Before M1: diff `amd64/Cargo.toml`'s `akuma-*` dependencies against `taoxichuan/Cargo.toml`'s (18) and say which unused one covers the clock.
4. **It exists nowhere**: write it, in a crate with host tests where the logic is pure, and say why in the RESULT file.
The plan expects M1 to be about 30 newly written lines (cmdline anchor + offset store + the two time arms). If a crate already holds the offset store or the cmdline-token parse and can be
called from `taoxichuan/` without `akuma-exec`, depend on it instead and say so. In the RESULT file report per change *pulled / extracted / cut / written new (why)*, `wc -l taoxichuan/src/*.rs`
at the start and end, and the `akuma-*` dependency count of `taoxichuan/Cargo.toml` (18 now). A session that closes its gates and makes `taoxichuan/src` smaller is better than one that does not.

## Cost rules
Cap every read (`| cut -c1-180 | head -60`, `grep -n -m 20`); never `cat` a source file; read a gate's output file only for a FAIL; put repeated sequences in scripts under `scripts/kvm/goose/`
(instrumentation lives in the repo, not in `/tmp`).

## Part 1 - M0: the LAN mirror and the Linux reference (host only, no boot)
Execute plan § 2 steps 1-4 on the box, which is x86_64 and has internet today:
- Resolve the closure for `curl` with the host's apk in a throwaway root (settle U2: exact dependency set, and whether any package ships `.post-install` / trigger scripts - `tar -tzf` the apks).
  `musl` must be in the closure. Settle U3: is `curl` dynamically linked (`file` on the extracted binary; its `PT_INTERP`).
- Freeze into `/srv/alpine-mirror/main/x86_64` (`apk fetch -R -o ...`), with `APKINDEX.tar.gz`; write `/srv/alpine-mirror/MANIFEST.sha256`; copy the apks also to
  `/root/dev-cycle/taoxichuan-s8/apks/`.
- Write `scripts/kvm/goose/mirror.sh` that (re)verifies `MANIFEST.sha256` and serves the mirror on `192.168.1.159:8123` (LAN only), and `scripts/kvm/goose/refroot.sh` that builds the Linux
  reference root with the same `apk.static` the guest image carries (`--root /tmp/refroot --initdb --repositories-file http://127.0.0.1:8123/main/x86_64 add curl`) and prints: sha256 of
  `APKINDEX.tar.gz`, the `/etc/apk/world` line-set, `apk info -L curl`, sha256 of `/usr/bin/curl` and of every shared library it needs.
- Settle U1 read-only: are `/etc/apk/keys` present in `akuma-dev-root-apk.img.orig`, and which `apk.static --version` is in it. State whether the guest repositories file can use signed
  indexes or needs `--allow-untrusted` (the MANIFEST is then the integrity contract).
- Leave the mirror NOT running and out of herd's enabled set when you finish.

**Gate M0:** `scripts/kvm/goose/mirror.sh verify` -> exit 0 and the line `MANIFEST OK <N> files`; `scripts/kvm/goose/refroot.sh` -> exit 0 and prints the five values above for `curl`.
Write the answers to U1, U2, U3 into `RESULT-H.md`. Commit and `git push litter taoxichuan-s4`.

## Part 2 - M1: the wall clock
Today `CLOCK_GETTIME` and `GETTIMEOFDAY` in `taoxichuan/src/main.rs` return boot-relative seconds for every clock id. Required behaviour: the kernel learns the host's Unix time at boot from an
`epoch=<seconds>` token on the guest command line (cmdline helpers exist: look for `cmdline_token`), and `CLOCK_REALTIME` plus `gettimeofday` return that anchor plus elapsed uptime.
`CLOCK_MONOTONIC` (and the other ids) must keep their current boot-relative meaning. No `epoch=` token means today's behaviour. Nothing else changes.
Boot side: `boot_dev.sh` must inject `epoch=$(date +%s)` into the `vm_cmdline` of BOTH conf copies it writes (the tracked template stays without a value), so every boot gets a fresh anchor.
Add a clock gate to `run_gates.sh`, judged on the console exit status like the others: **C1**: `sh -c 'd=$(date +%s); [ "$d" -ge E ] && [ "$d" -le $((E+900)) ]'` where `E` is the epoch that
`boot_dev.sh` injected (have `boot_dev.sh` write it to `$OUT/epoch`; `run_gates.sh` substitutes it). Exit 0 required.

**Gates:**
- C1 as above -> `PASS C1`.
- `run_gates.sh` -> `PASS` for G1 G2 G3 G4 H1 H2 H3a H3b H3c in the same boot, `restarts=0`.
- Zero `Fault|PANIC|sig!` on the console of a passing boot.
- Out of scope: SNTP, the CMOS RTC, DNS, `apk update`, any loader work, any change under `amd64/` or `src/` unless the composition rule's step 3 requires an extraction (then say so).

## Ops traps
- Guest kernel installs only with an explicit `/boot/akuma-guest-dev` destination; never write `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); herd re-parses only on `disable` then `enable`; the truth is
  `/var/run/herd/vm-dev.vm`; confirm the `guest disk:` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot; restore everything at the end (stage4 conf, `vm-dev` disabled,
  `vm-serve-mc` up, `:8123` stopped).
- The host->guest ssh capture loses output and the forward degrades near 30 connections per boot: judge by the console (`[SSH] Exit status`), pace 15 s, one exec per connection.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box**: Akuma's shell turns it into an interactive REPL that hangs your tool call. Write the script to a file and run
  `python3 /path/x.py < /dev/null`.
- Never run goose/crush wrappers on the box.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after M0 is done, after every boot cycle that taught you something, and after every fix that builds. WIP commits are fine. Never push `origin`.
Gate arch-specific code with `cfg`; keep amd64-only crates out of default-members; pre-commit clippy and the unsafe-budget check must pass (never `--no-verify`).

## When to stop
M0 and M1 gates pass in a clean boot, or 4 boots with no new evidence, or the quota error. Count boots, not minutes. Write `/root/dev-cycle/taoxichuan-s7/RESULT-H.md` (gates; U1/U2/U3 answers;
per change pulled / extracted / cut / written new; `wc -l taoxichuan/src/*.rs` start and end; `akuma-*` dependency count; the exact failing step with console evidence if any; what is next) and end the turn.
