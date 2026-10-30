# Taoxichuan stage 8, session L: M3 - `apk update` from the guest against the frozen LAN mirror

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: K (`RESULT-K.md`, head `694cddd5`; G1 G2 G3 G4 H1 H2 H3a H3b H3c C1 D1 D2 W1a-d PASS in one boot, `restarts=0`). **Keep your context small.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4` (fetch `litter` first if behind). Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
Stage 6 needs the guest to fetch a package index itself. The pieces exist: the guest has outbound TCP (H1 fetches `http://192.168.1.159:8123/` through the relay), a working resolver (D2), a wall clock (C1) and
`apk.static` 3.0.8 (G2/G3). The frozen mirror is on this box at `/srv/alpine-mirror` (`scripts/kvm/goose/mirror.sh start|stop|verify`, port 8123, `MANIFEST.sha256` is the integrity contract) and the Linux reference is
`scripts/kvm/goose/refroot.sh`. This session adds exactly one capability: **`apk update` in the guest, signed, against `http://192.168.1.159:8123/main`, rc 0, with the downloaded index byte-equal to the mirror's.**
apk-tools 3 appends the architecture itself, so the repositories line is `http://192.168.1.159:8123/main` and the wire request is `/main/x86_64/APKINDEX.tar.gz`. Nothing else changes: no `apk add` of curl, no loader, no clock work.

## Reference values (verified on the box 2026-10-26, not guessed)
- `sha256sum /srv/alpine-mirror/main/x86_64/APKINDEX.tar.gz` = `c169fa06972f9d2639ad1ea0effa26bf7223a72101a41c5cf2c12813d885cab5`.
- The reference root (`refroot.sh`, same `apk.static` 3.0.8, signed with the host's alpine keys) caches the index as `/tmp/refroot/var/cache/apk/APKINDEX.<8 hex>.tar.gz` with that **same** sha256; the 8 hex is a hash of the repository
  URL, so the guest's cache file name will differ from the reference's. Judge by content, not name.
- The guest's `/etc/apk/keys` should hold the three official alpine-devel keys (U1 in `TAOXICHUAN_STAGE6_REST_PLAN.md` § 6 says so; confirm by an `ls /etc/apk/keys` exec before relying on it). `--allow-untrusted` is NOT acceptable
  unless the keys are absent, and then only with that stated in RESULT-L.

## Read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md` - § 2 (the LAN mirror, steps 2-4) and § 5 (the M3 row) - what the plan expects.
2. `scripts/kvm/goose/run_gates.sh` - the head comments, H1 (how the host listener and the guest fetch are driven), D1/D2, W1 (the self-contained group pattern).
3. `userspace/ext2probe/src/bin/devroot_ensure.rs` - the host-side image writer that already puts `/etc/resolv.conf` and `/bin/nslookup` on the per-boot COPY; it is where a `/etc/apk/repositories` line belongs.
4. `scripts/kvm/goose/boot_dev.sh` (the `.orig` copy + `devroot-ensure` + kernel install) and `scripts/kvm/goose/mirror.sh` head.
5. `docs/archive/KVM_OUTBOUND_NETWORKING.md` - § Verified: how the relay reaches a LAN address.
Probes: `userspace/nettest` (`std::net` and libcurl clients) if a network symptom needs isolating; busybox `wget` is the proven client (H1).

## Composition rule (mandatory)
`taoxichuan/` is a **composition root**, not a kernel written from scratch. It is 4,744 newly written lines and has 18 `akuma-*` path dependencies (`grep -c '^akuma-' taoxichuan/Cargo.toml`).
Before you write any arm, subsystem or helper of more than about 20 lines: (1) **look first** - `grep -rn "<name>" crates/ amd64/src/`; (2) **it exists in a crate**: add a path dependency and call it;
(3) **only in `amd64/src/`**: extract it into an `akuma-<thing>` crate first and make both kernels depend on it; (3b) **welded to `Process`/`akuma-exec`**: that is a seam to cut (leaf crate or cargo feature), say which and
what it costs; (4) **exists nowhere**: write it in a crate with host tests, and say why in RESULT-L. The image preparation for this milestone should extend `devroot-ensure` (the existing `akuma-ext2` host writer), not
add a hand patcher. Expect most of the work to be image/gate plumbing and a syscall or two that `apk`'s own fetcher needs and `busybox wget` did not. Report per change *pulled / extracted / cut / written new (why)*,
`wc -l taoxichuan/src/*.rs` at start and end, and the dependency count. A session that closes its gate and makes `taoxichuan/src` smaller is better than one that does not.

## Cost rules
Cap every read (`| cut -c1-180 | head -60`, `grep -n -m 20`); never `cat` a source file; read a gate's output file only for a FAIL; repeated sequences go in scripts under `scripts/kvm/goose/`.

## Gates
- **U1 (read-only, first)**: with the `.orig` image untouched, `ls /etc/apk/keys` in a guest exec (one `ls` exec) and note the three key files; and `cat /etc/apk/repositories` (it may be missing or hold a dead URL).
- **A1**: `/etc/apk/repositories` in the guest contains exactly the line `http://192.168.1.159:8123/main`, put there on the per-boot COPY by `boot_dev.sh` through `devroot-ensure` (say how; the `.orig` stays unmodified).
- **A2**: `run_gates.sh` starts the mirror (`mirror.sh start`, which verifies the manifest first and aborts the run on a mismatch) and stops it at the end; ONE command `apk update` over
  `/bin/ssh -p 2224 root@localhost 'apk update'` (no flags: the guest's own repositories and keys) -> console `[SSH] Exit status: 0`, and the mirror's request log shows `GET /main/x86_64/APKINDEX.tar.gz`.
- **A3**: the downloaded bytes: `sha256sum /var/cache/apk/APKINDEX.*.tar.gz > /tmp/h` then `grep -c c169fa06 /tmp/h` (G3's pattern: guest-side exit code, one simple command per exec, `sh` cannot fork - no `$(...)`, `;`, `&&`, pipes)
  -> exit 0. Add A1 A2 A3 to `run_gates.sh` after D2.
- Zero `Fault|PANIC|sig!` in a passing boot; `run_gates.sh` prints `PASS` for G1 G2 G3 G4 H1 H2 H3a H3b H3c C1 D1 D2 W1a W1b W1c W1d A1 A2 A3 in the same boot with `restarts=0` (a forward-degradation restart is a
  known artifact: rerun for a clean boot, and say so).
- Out of scope: `apk add curl`, the loader/`PT_INTERP`, the clock, any change under `amd64/` or `src/` unless step 3 of the composition rule requires an extraction (then say so).

## Ops traps
- Guest kernel installs only with an explicit `/boot/akuma-guest-dev` destination; never write `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this, from the tracked `vm-dev.stage7.conf` with `vm_outbound=true`); herd re-parses only on
  `disable` then `enable`; the truth is `/var/run/herd/vm-dev.vm`; confirm the `guest disk:` line in `/var/log/herd/vm-dev.log`; fresh `.orig` image copy per boot; restore everything at the end (stage4 conf from
  `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4`, `vm-dev` disabled, `vm-serve-mc` up, `:8123` stopped via `mirror.sh stop`).
- The vm-dev log appends across boots: attribute evidence per boot boundary. Check the `kbuild` exit code before booting (a failed build boots the previous binary; a chained command whose grep matches "error" can still exit 0).
- The host->guest ssh capture loses output and the forward degrades near 30 connections per boot: judge by the console (`[SSH] Exit status`), pace 15 s, one exec per connection.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box.** Write the script to a file, run `python3 /path/x.py < /dev/null`. Never run goose/crush wrappers on the box.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after every boot cycle that taught you something and after every fix that builds. WIP commits are fine. Never push `origin`.
Gate arch-specific code with `cfg`; keep amd64-only crates out of default-members; pre-commit clippy and the unsafe-budget check must pass (never `--no-verify`).

## When to stop
A1 A2 A3 pass in a clean boot with the full battery, or 4 boots with no new evidence, or the quota error. Count boots, not minutes. Write `/root/dev-cycle/taoxichuan-s7/RESULT-L.md` (gates; per change
pulled / extracted / cut / written new; `wc -l taoxichuan/src/*.rs` start and end; `akuma-*` dependency count (18 now); the exact failing step with console evidence if any; what is next) and end the turn.
