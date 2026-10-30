# Taoxichuan stage 8, session M: M4 - `apk add curl` in the guest from the frozen LAN mirror (the files land; nothing is executed)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: L (`RESULT-L.md`, head `7e3b422f`; G1 G2 G3 G4 H1 H2 H3a H3b H3c C1 D1 D2 A1 A2 A3 W1a-d PASS in one boot, `restarts=0`). **Keep your context small.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4` (fetch `litter` first if behind). Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
`apk update` works (M3). This session adds exactly one capability: **`apk add curl` over the network, rc 0, with the 14-package closure (about 12.6 MB installed) unpacked into the guest root and byte-equal to the Linux reference.**
Nothing is *run*: `curl` is a dynamically linked PIE (`PT_INTERP=/lib/ld-musl-x86_64.so.1`) and the loader work is M5. `apk.static` and busybox are static, so unpacking a new `/lib/ld-musl-x86_64.so.1` over the image's own does not disturb them; if apk replaces it, note it, do not
try to execute `curl`. Scripts: U2 settled that no package in the closure has `.post-install` or a trigger script, so there is no fork/exec nesting to build; if one appears in the console, stop and say so.
Three things changed under you since session L, read them before the first boot:
1. **The guest root image is now 1 GiB** (was 16 MiB; the 12.6 MB closure would not have fit in 8 MiB free). Grown on the box by `scripts/kvm/goose/grow_dev_image.sh`: the old image is `/boot/akuma-dev-root-apk.img.orig.16M`, the new `.orig` is 262,144 blocks, `e2fsck` clean, and a full battery already passed on it.
2. **`statfs` is still a lie.** `taoxichuan/src/main.rs` `fill_statfs` answers fixed numbers for a 16 MiB filesystem (4,096 blocks, 2,053 free). apk checks free space; with those numbers it may refuse the install. The answer must come from the real superblock (look first: `akuma_vfs_glue::stats_for_path`, `akuma-ext2`), and `W1b` (`df /`) will change **on purpose**:
   re-record the `df` hash in `scripts/kvm/goose/w1_expected_hashes.txt` in the same commit, state in the commit message that this one hash changed deliberately, and show that `W1a`, `W1c`, `W1d` are unchanged.
3. **`boot_dev.sh` carries a supervisor fix you must commit as-is, first.** The host once printed the 1 GiB disk as `856 MiB` (and `10 MiB` on another boot) because herd started an enabled-but-stopped `vm-dev` ~20 s after the stop, while the 1 GiB copy and the `devroot-ensure` rewrite were still writing the image. The fix (origin `9202d190`) disables `vm-dev` for the whole rewrite and writes the confs, enables and starts only after it; two boots on it printed `1024 MiB` (`docs/archive/KVM_GUEST_DISK_856_MIB_RACE.md`). The box checkout has that file as an uncommitted change: `git add scripts/kvm/goose/boot_dev.sh` and commit it alone before any other work, and expect `guest disk: ..., 1024 MiB` on every boot; anything else is a regression to report with the console line.

## Reference values (verified on the box 2026-10-26 against `/tmp/refroot`, built by `refroot.sh`)
- Closure: 14 packages (`brotli-libs c-ares ca-certificates-bundle curl libcrypto3 libcurl libidn2 libpsl libssl3 libunistring musl nghttp2-libs zlib zstd-libs`); `/etc/apk/world` gains the line `curl`.
- `apk info -L curl` = `usr/bin/curl`, `usr/bin/wcurl`.
- sha256 prefixes (8 hex): `/usr/bin/curl` `9eb8284f`; `/usr/lib/libcurl.so.4.8.0` `1ee39403`; `/lib/ld-musl-x86_64.so.1` `095b047c`; `/usr/lib/libssl.so.3` `ed8d3040`; `/usr/lib/libcrypto.so.3` `bb2d86be`; `/usr/lib/libz.so.1.3.2` `ecc8b9df`.
- The guest image already has other packages installed, so compare **files**, not `apk info` totals.

## Read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md` - § 5 (the M4 row) and § 6 (U2).
2. `RESULT-L.md` (on the box, `/root/dev-cycle/taoxichuan-s7/`) - § 4 and § 5 (the hang, the `select` arm, the two tolerated unhandled syscalls: `Ftruncate` (77) and nr 40, which is `sendfile`).
3. `scripts/kvm/goose/run_gates.sh` - the A group and W1 group (the self-contained pattern), and the head comments.
4. `docs/archive/KVM_GUEST_DISK_856_MIB_RACE.md` - the fixed start-order race behind item 3.
Probes: `userspace/abiprobe` and `userspace/ext2probe` if a syscall's bytes need checking; `userspace/nettest` for a network symptom.

## Composition rule (mandatory)
`taoxichuan/` is a **composition root**, not a kernel written from scratch. It is 4,856 newly written lines with 19 `akuma-*` path dependencies (`grep -c '^akuma-' taoxichuan/Cargo.toml`).
Before you write any arm, subsystem or helper of more than about 20 lines: (1) **look first** - `grep -rn "<name>" crates/ amd64/src/`; (2) **it exists in a crate**: add a path dependency and call it;
(3) **only in `amd64/src/`**: extract it into an `akuma-<thing>` crate first, make both kernels depend on it; (3b) **welded to `Process`/`akuma-exec`**: cut the seam (leaf crate or feature) and say what it costs;
(4) **exists nowhere**: write it in a crate with host tests and say why in RESULT-M. Expected new surface: the console will name each `[sc] unhandled` call the unpacker hits (`ftruncate`, `sendfile`, perhaps `fchmod`/`utimensat`/`symlink`/`fsync`/`rename`); for each, find the existing implementation first
(`akuma-syscalls-glue`, `akuma-ext2`'s truncate and write paths, `akuma-vfs-glue`). Report per change *pulled / extracted / cut / written new (why)*, `wc -l taoxichuan/src/*.rs` at start and end, and the dependency count.
A session that closes its gates and makes `taoxichuan/src` smaller is better than one that does not.

## Cost rules
Cap every read (`| cut -c1-180 | head -60`, `grep -n -m 20`); never `cat` a source file; read a gate's output file only for a FAIL; repeated sequences go in scripts under `scripts/kvm/goose/`.

## Gates
- **statfs**: after the change, `df /` in the guest shows the real 1 GiB geometry (total blocks 262,144 x 4 KiB); `W1b`'s new hash recorded as above.
- **B1**: ONE command `apk add curl` over `/bin/ssh -p 2224 root@localhost 'apk add curl'` (no flags: the guest's own repositories and keys; the mirror is up from the start of the battery) -> console `[SSH] Exit status: 0`, and the mirror log shows GETs for the closure's apks.
- **B2**: `grep -cx curl /etc/apk/world` -> exit 0.
- **B3** (G3's pattern, guest-side exit codes, one simple command per exec - `sh` cannot fork: no `$(...)`, `;`, `&&`, pipes): `sha256sum /usr/bin/curl /usr/lib/libcurl.so.4.8.0 /usr/lib/libssl.so.3 /usr/lib/libcrypto.so.3 /usr/lib/libz.so.1.3.2 /lib/ld-musl-x86_64.so.1 > /tmp/h`, then one `grep -c <prefix> /tmp/h` per reference value above.
- **B4**: `apk verify curl` -> exit 0 (apk's own integrity check of what it unpacked).
- **Forward ceiling**: the battery is already near the ~30 ssh connections per boot at which the host forward degrades (it died early in about a third of the batteries in sessions K and L). Put B1-B4 in a second boot group (the prerequisites it needs re-asserted, the W1 self-contained pattern), or fix the ceiling at its source if that is cheap; do not just accept `restarts=1`.
- Zero `Fault|PANIC|sig!` in a passing boot; both boots' groups PASS with `restarts=0`.
- Out of scope: running `curl`, the loader/`PT_INTERP`, the relay's missing RST for a refused dial (an open finding in `RESULT-L.md` - keep the mirror up and it never triggers), anything under `amd64/` or `src/` unless composition step 3 requires an extraction (then say so).

## Ops traps
- Guest kernel installs only with an explicit `/boot/akuma-guest-dev` destination; never write `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev`; edit BOTH `/etc/herd/available/` and `/etc/herd/enabled/` confs (`boot_dev.sh` does this); herd re-parses only on `disable` then `enable`; the truth is `/var/run/herd/vm-dev.vm`; the vm-dev log appends across boots and herd rotates it mid-boot (`boot_dev.sh` handles both files): attribute evidence per boot boundary.
  Restore at the end, **in this order**: `herd stop vm-dev`, stage4 conf from `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4` into BOTH confs, `herd disable vm-dev`, `herd start vm-serve-mc`, `mirror.sh stop`.
- Check the `kbuild` exit code before booting (a failed build boots the previous binary; a chained command whose grep matches "error" can still exit 0). Fresh `.orig` copy per boot; each boot copies 1 GiB (about 45 s with the boot).
- The host->guest ssh capture loses output: judge by the console (`[SSH] Exit status`), pace 15 s, one exec per connection.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box.** Write the script to a file, run `python3 /path/x.py < /dev/null`. Never run goose/crush wrappers on the box.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after every boot cycle that taught you something and after every fix that builds. WIP commits are fine. Never push `origin`.
Gate arch-specific code with `cfg`; keep amd64-only crates out of default-members; pre-commit clippy and the unsafe-budget check must pass (never `--no-verify`).

## When to stop
B1-B4 pass in clean boots with the full battery, or 4 boots with no new evidence, or the quota error. Count boots, not minutes. Write `/root/dev-cycle/taoxichuan-s7/RESULT-M.md` (gates; per change
pulled / extracted / cut / written new; `wc -l taoxichuan/src/*.rs` start and end; `akuma-*` dependency count (19 now); the exact failing step with console evidence if any; what is next) and end the turn.
