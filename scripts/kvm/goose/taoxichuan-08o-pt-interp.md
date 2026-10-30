# Taoxichuan stage 8, session O: M5 - run a dynamically linked PIE: `/usr/bin/curl --version` (PT_INTERP, auxv, the dynamic linker)

You are goose-glm, the metal track of `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`, on the ryzen box. Previous session: N (`RESULT-N.md`, the process-model plan) and M before it (`RESULT-M.md`, head `9d39529a`; `apk add curl` works, B1-B4 PASS, the installed files match the Linux reference). **Keep your context small.**
Checkout `/src/github.com/netoneko/akuma`, branch `taoxichuan-s4` (fetch `litter` first if behind). Every shell command starts with
`export TERM=dumb HOME=/root GIT_PAGER=cat; . /etc/akuma-dev.env`. Box log times are UTC.

## The key fact
`apk add curl` puts a dynamically linked PIE at `/usr/bin/curl` (`PT_INTERP=/lib/ld-musl-x86_64.so.1`; `NEEDED`: `libcurl.so.4`, `libz.so.1`, `libc.musl-x86_64.so.1`; the closure's libraries are in `/usr/lib`, the loader is the image's own, sha `095b047c`). Nothing in the guest has ever executed it. `taoxichuan`'s loader (`load_task`, `taoxichuan/src/main.rs` near line 2723)
loads static and static-PIE images only: it writes `AT_BASE = 0` with the comment "no interpreter" (near line 2813). This session adds exactly one capability: **the guest runs a dynamically linked PIE through its dynamic linker, and `/usr/bin/curl --version` prints the same first line as the Linux reference.**
The full amd64 kernel already does this (`amd64/src/loader.rs`, 710 lines: `LoadedImage` with an `interp`, `interp_base`, `build_stack`, `compute_stack_top`; its header comment records that a one-NUL-byte `PT_INTERP` is a static-PIE and that the interpreter base moved to avoid a collision). The ldso's own syscall needs
(`mmap` with `MAP_FIXED` - `taoxichuan/src/main.rs` near line 1606 handles the flag -, `mprotect`, `futex`, `prlimit64`, `sigaltstack`, `set_robust_list`, `set_tid_address`) are said to be served; **verify, do not assume**. Nothing else changes: no network use of curl, no TLS, no fork.

## First, one open crash (read-only first, then decide)
`RESULT-M.md` section 4: `apk info -L curl` killed the guest with a ring-3 NULL write (`cr2=0x0`, `rip=0x1001718f`, right after a 5.4 MB kernel-heap allocation line); a ring-3 fault parks the whole guest by design at this stage. It may be the same mmap/stack/auxv path this session touches. Reproduce it once on the first boot (a single `apk info -L curl` exec after `apk add curl`),
read the fault's `rip` against the binary it ran (`apk.static`), and write down in RESULT-O.md whether it is related to the loader work. Do not chase it past one boot and about 15 tool calls; if unrelated, record it as open.

## Reference values (verified on the box 2026-10-26 by running the reference root's curl through the amd64 kernel's loader)
- First line of `curl --version` (exactly 160 bytes with the newline, sha256 prefix `0409ecf5079f0ce0`):
  `curl 8.23.0 (x86_64-alpine-linux-musl) libcurl/8.23.0 OpenSSL/3.5.9 zlib/1.3.2 brotli/1.2.1 zstd/1.5.7 c-ares/1.34.9 libidn2/2.3.8 libpsl/0.21.5 nghttp2/1.70.0`
  The whole output is 4 lines; exit status 0.
- `/usr/bin/curl` sha `9eb8284f`, `/lib/ld-musl-x86_64.so.1` `095b047c`, `libcurl.so.4.8.0` `1ee39403`.

## Read first (in order; `sed -n` windows of 60 lines or less, never whole files)
1. `docs/archive/TAOXICHUAN_STAGE6_REST_PLAN.md` - section 4 (dynamic linking) and the M5 row of section 5; `docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` - the M5 row and U3-U5 only.
2. `amd64/src/loader.rs` - the header comment (lines 1-90) and the signatures (`grep -n "^pub fn\|^pub struct\|^fn " amd64/src/loader.rs`); then only the interp and auxv sections. `crates/akuma-elf/src/lib.rs` (the PT_INTERP note near line 123 and its public API).
3. `taoxichuan/src/main.rs` - `load_task` and the stack/auxv build around it (near lines 2723-2830), and the MMAP arm's `MAP_FIXED` branch (near line 1606).
4. `docs/archive/AKUMA_AMD64_EXECVE_INSTALL_IMAGE.md` and `docs/archive/RELR_*`/`INTERP_BASE` notes if `grep -il "interp_base" docs/archive | head` finds them (the loader's interpreter-base history). Headers and conclusions only.
Probes: `userspace/abiprobe`, `userspace/forktest/c_stress` have mmap and exec probes; a tiny dynamic hello would be the cleanest first test if you can build one (`userspace/build.sh`); curl itself is the real gate.

## Composition rule (mandatory)
`taoxichuan/` is a **composition root**. It is 4,916 newly written lines with 19 `akuma-*` path dependencies (`grep -c '^akuma-' taoxichuan/Cargo.toml`). The interpreter/auxv decision logic exists in `amd64/src/loader.rs` and welded to `UserAddressSpace`: that is composition step 3/3b - **extract the pure decision core into a leaf crate**
(extend `akuma-elf` or add `akuma-elf-exec`; `#![forbid(unsafe_code)]`, host tests, zero or minimal dependencies), make `amd64/` depend on it too (its behaviour must not change: run its host tests, and say how you know), then call it from `taoxichuan`'s `load_task`. Candidates for the core: reading and validating `PT_INTERP` (including the one-NUL static-PIE case), choosing the interpreter base,
computing the auxv set (`AT_PHDR`, `AT_PHENT`, `AT_PHNUM`, `AT_BASE`, `AT_ENTRY`, `AT_PAGESZ`, `AT_RANDOM`, ...), the entry point to jump to. The mapping itself stays in each kernel. **Before writing code, put the survey in RESULT-O.md** (what is pure, what is welded, the extraction's size and the amd64 risk); if the extraction is larger than about 300 lines of amd64 churn, stop after the survey and report.
Report per change *pulled / extracted / cut / written new (why)*, `wc -l taoxichuan/src/*.rs` start and end, and the dependency count. A session that closes its gate and makes `taoxichuan/src` smaller is better than one that does not.

## Cost rules
Cap every read (`| cut -c1-180 | head -60`, `grep -n -m 20`); never `cat` a source file; read a gate's output file only for a FAIL; repeated sequences go in scripts under `scripts/kvm/goose/`. Expect about 100-150 tool calls; if you pass 150 without a `curl --version` line on the console, write what you have and stop.

## Gates
- **E1**: in one boot group (the image is fresh every boot, so first `apk add curl`, idempotent, B1's command): ONE command `curl --version > /tmp/cv`, console `[SSH] Exit status: 0`.
- **E2**: `grep -cx 'curl 8.23.0 (x86_64-alpine-linux-musl) libcurl/8.23.0 OpenSSL/3.5.9 zlib/1.3.2 brotli/1.2.1 zstd/1.5.7 c-ares/1.34.9 libidn2/2.3.8 libpsl/0.21.5 nghttp2/1.70.0' /tmp/cv` -> exit 0 (G3's pattern: guest-side exit codes; one simple command per exec, `sh` cannot fork: no `$(...)`, `;`, `&&`, pipes).
- **E3 (regression)**: the main battery (G1 G2 G3 G4 H1 H2 H3a-c C1 D1 D2 A1 A2 A3 W1a-d) still PASSes on the final binary, `restarts=0`, and B1-B4 still PASS: static and static-PIE binaries (busybox, `apk.static`) must run as before.
- Zero `Fault|PANIC|sig!` in a passing boot. Put E1/E2 in the B group boot (the forward ceiling is near; do not add them to the main battery).
- Out of scope: running curl against the network, TLS, fork/clone, the `[statfs]` debug print (leave it), the relay RST finding, anything under `src/`. Changes under `amd64/` only for the extraction in the composition rule, and then say so.

## Ops traps
- Guest kernel installs only with an explicit `/boot/akuma-guest-dev` destination; never write `/boot/akuma-amd64*`.
- Stop `vm-serve-mc` before `vm-dev`; `boot_dev.sh` handles the confs, the disable-during-rewrite order and the 1 GiB image (expect `guest disk: ..., 1024 MiB` on every boot; anything else is a regression to report with the console line); herd re-parses only on `disable` then `enable`; the vm-dev log appends across boots and herd rotates it mid-boot: attribute evidence per boot boundary.
  Restore at the end, **in this order**: `herd stop vm-dev`, stage4 conf from `/root/dev-cycle/taoxichuan-s6/vm-dev.conf.stage4` into BOTH confs, `herd disable vm-dev`, `herd start vm-serve-mc`, `mirror.sh stop`.
- Check the `kbuild` exit code before booting (a failed build boots the previous binary; a chained command whose grep matches "error" can still exit 0). The host->guest ssh capture loses output: judge by the console (`[SSH] Exit status`), pace 15 s, one exec per connection. If the box stops answering for minutes, stop and say so in RESULT-O.md; it reset once today under a long build.
- **Never run `python3 -`, `python3 <<EOF` or any python fed from stdin on this box.** Never run goose/crush wrappers on the box.

## Commit rule (you have no clock)
Commit and `git push litter taoxichuan-s4` after every boot cycle that taught you something and after every fix that builds; WIP commits are fine. Never push `origin`. Gate arch-specific code with `cfg`; keep amd64-only crates out of default-members; pre-commit clippy and the unsafe-budget check must pass (never `--no-verify`).

## When to stop
E1 and E2 pass in a clean boot with E3 green, or 4 boots with no new evidence, or the quota error. Count boots, not minutes. Write `/root/dev-cycle/taoxichuan-s7/RESULT-O.md` (gates; the survey and per-change accounting; the `apk info -L` finding; `wc -l` start and end; dependency count (19 now); the exact failing step with console evidence if any; what is next) and end the turn.
