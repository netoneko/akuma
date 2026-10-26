# Taoxichuan stage 6, the rest — PLAN (session G, 2026-10-26)

Stage 6 (`apk add curl` in the guest) is half done: offline `apk add` passes (G2 `apk.static
--version`, G3 offline install + hash), outbound TCP passes (H1: guest wget over the outbound
relay to `192.168.1.159:8123` → `hello-from-host`). Still needed: **DNS, a wall clock,
`apk update` from a LAN HTTP mirror, `apk add curl` + `/etc/apk/world`, `curl --version`**.
This plan covers exactly that; it supersedes the milestone rows M4–M6 of
`TAOXICHUAN_STAGE6_PLAN.md` (2026-10-24), which remain the source for the mirror/reference
recipe (§7 there) and the process-model analysis (§5 there). Nothing here is verified until
its mini-gate runs; unverified statements are marked (U#) and settled in §6.

## 0. Leaf-crate survey first (the owner's rule)

`diff` of `akuma-*` dependencies: `amd64/Cargo.toml` lists **76**, `taoxichuan/Cargo.toml`
lists **18** (`akuma-alloc amd64 boot-amd64 comconsole cpu ext2 fdtable gdt idt-amd64 net
net-nic pmm primitives ring3-amd64 ryzen-amd64 trap-amd64 vfs virtio`). **60 crates are
amd64-only; all 60 were checked** (`grep -m1 '^//!' crates/<n>/src/lib.rs`). Most are
metal-only (nvme, xhci, usb, hda, i2c-hid, hid, touchpad, rtw89, wifi, wpa, ieee80211,
net-rtl8169, fbcon, fbdev, dwi2c, pci, dma, mmu, pagetable, pte-amd64, cow, fpcache(-rw),
svm-amd64, kvm-abi, vm-spec, multiboot2, power, kacho, threading, bkl, locks-rw, isolation,
dmesg, selftest, config, boot, procfs…) and irrelevant to stage 6. The relevant ones:

| crate | what it is (first `//!` line) | covers |
|---|---|---|
| `akuma-sntp` | pure SNTP (RFC 4330) client + boot-time wiring | clock (alternative path) |
| `akuma-cmos-rtc` | the PC CMOS real-time clock, read | clock (rejected, see §3) |
| `akuma-syscalls-time` | time syscalls: itimers, clock_gettime/settime/getres; re-exports `akuma_sntp::{boot,sntp}` (`src/lib.rs:39-42`) | clock |
| `akuma-syscalls-poll` | the `epoll`/`ppoll`/`pselect6` family's pure logic | apk's readiness waits |
| `akuma-syscalls-mem` | the memory-syscall family's pure decisions | apk.static PIE + ld-musl mappings |
| `akuma-syscalls-net` | read-only network-introspection surface | **not needed** (no `ifconfig` gate) |
| `akuma-syscalls-linux` | Linux syscall ABI: numbers, `repr(C)` structs | wire-type dedup (= trim step 2) |
| `akuma-syscalls-abi` | which syscall a number means, per arch | same |
| `akuma-syscalls-glue` | the Linux syscall dispatcher + per-family impls | reference for arm shape |
| `akuma-pipe` / `akuma-pipes` | pure byte-FIFO / pipe table with waiters | .post-install contingency only |
| `akuma-exec-core` | the unsafe-free core of `akuma-exec` | exec seam (see below) |
| `akuma-exec` | (empty `//!` — the logic sits in the lib body) | exec seam |
| `akuma-port` | x86 legacy port I/O, as devices | CMOS path only (rejected) |
| `akuma-tty` | the terminal stack, as pure state machines | **not needed** (no pty on the gate path) |
| `akuma-elf` | ELF loader; `register_vfs_hooks`, PT_INTERP walk noted (`src/lib.rs:123`) | curl (interp decision, partial) |
| `akuma-mmap`, `akuma-user-access`, `akuma-user-space`, `akuma-vfs-glue` | mmap bookkeeping / user-pointer boundary / frame ledger / kernel VFS+mount table | reference shapes for the M7 loader work |

**The exec seam.** The useful exec logic (image load, argv/env copy, auxv, run-to-completion
nesting) is welded to `akuma-exec`/`Process` on the metal side; taoxichuan re-implemented its
own run-to-completion spawn (`taoxichuan/src/child.rs`, `exec_replace` at 392). Two ways to
free shared logic, both already proven in this codebase:

- **Seam (leaf crate)** — as `akuma-sntp` was cut out of `akuma-syscalls-time`
  (`akuma-syscalls-time/src/lib.rs:39-42` documents the move): extract the pure parts
  (auxv block layout, argv/env copy sizes, ELF decision checks) into a `#![forbid(unsafe_code)]`
  leaf; cost ≈ a day of picking plus moving the host tests.
- **Cargo feature** — make `akuma-exec`'s `Process` machinery optional so the pure core
  (`akuma-exec-core`, which already exists) builds without it; cost ≈ a feature knob + CI
  matrix entry, but only pays off if `akuma-exec-core` is genuinely clean of `Process`.

For the rest of stage 6 neither is on the critical path: the gaps below are config, two small
arms, and one loader extraction (§1, M7).

## 1. The gap per milestone, diffed against what `taoxichuan/src` serves today

`taoxichuan/src` dispatches ~90 numbers today (measured: the `ring3::nr::` match arms), among
them everything the aarch64 `APK_MISSING_SYSCALLS.md` list once lacked: `SELECT_PSELECT6`,
`DUP`, `FSTATFS`, `RENAMEAT`, `SYMLINKAT`, `READLINKAT`, `LINKAT`, `MREMAP`, `FCHDIR`,
`FCHMODAT`, plus `MMAP/MPROTECT/MREMAP`, `PIPE2`, `CLONE`, `WAIT4`, `EXECVE`, `FUTEX`,
`PR_LIMIT64`, `SIGALTSTACK`, `SET_ROBUST_LIST`, `SET_TID_ADDRESS`, `GETDENTS64`,
`SOCKET/BIND/LISTEN/ACCEPT/CONNECT/SENDTO/RECVFROM/…`, `CLOCK_GETTIME`, `GETTIMEOFDAY`.
So **the aarch64 syscall gap list is already covered**; what remains is per milestone:

| milestone | gap vs today | existing crate / amd64 fn that already does it | verdict |
|---|---|---|---|
| **DNS** | `/etc/resolv.conf` in image; guest sends UDP:53 itself (musl apk.static is its own resolver) | relay DNS proxy answers from `10.0.2.3:53` (`KVM_OUTBOUND_NETWORKING.md:22,29-30`; `dns_fwd`/`dns_answered` counters already in vm-dev.log's relay lines); guest UDP via `sock.rs` `socket_arm:113`/`sendto_arm:294`/`recvfrom_arm:344`; `akuma-net` carries smoltcp UDP (`udp_socket_close`, `crates/akuma-net/src/socket.rs:565-566`) | **depend** (no new code; image file + counters as evidence) |
| **wall clock** | `CLOCK_GETTIME` returns boot-relative seconds ("still no wall clock", `taoxichuan/src/main.rs:2007-2023`); same for `GETTIMEOFDAY` | the offset-store pattern: `akuma_timer::{utc_time_us,set_utc_time_us}` (`docs/reference/subsystems/syscalls/time.md:50`); pure protocol if SNTP ever wanted: `akuma-sntp` | **write new**, ~30 lines: parse `epoch=` from cmdline, store an AtomicU64 offset, add it in the two arms (see §3) |
| **`apk update` (LAN mirror)** | repositories file + keys in image; everything syscall-side already served | host reference recipe §7 of `TAOXICHUAN_STAGE6_PLAN.md`; outbound-to-LAN path proven by H1; structs (stat/statfs/dirent/time) via `akuma-syscalls-linux`+`akuma-syscalls-abi` (= trim step 2, dedup not gap); readiness waits `akuma-syscalls-poll` (dedup, optional) | **depend** (config + trim dedup; no new arms) |
| **`apk add curl` + world** | heavier write/rename/symlink traffic than G3's five writes; world file; keys question (U1) | ext2 write path proven (G3 PASS, RESULT-D); arms all served; `akuma-pipe`/`akuma-pipes` only if scripts appear (U2 says they don't) | **depend** (existing arms; contingency already built: `CLONE`+`EXECVE`+`WAIT4`+`PIPE2`) |
| **`curl --version`** | `curl` is dynamically linked (U3): needs `PT_INTERP` → `/lib/ld-musl-x86_64.so.1`, auxv, MAP_FIXED in the MMAP arm (U4) | `amd64/src/loader.rs` already reads `PT_INTERP` and maps the dynamic linker (`loader.rs:6`, static-PIE `p_filesz<=1` case at `:78-80`), `amd64/src/usermode.rs:5052` errors a missing interp file; `akuma-elf` has the PT_INTERP walk note (`lib.rs:123`) | **extract**: pull the interp+auxv decision core out of `amd64/src/loader.rs` into a leaf crate (`akuma-elf-exec` or extend `akuma-elf`), `amd64/` depends on it too, then `taoxichuan/` (composition rule 3) |

Per the composition rule, the per-change labels for the whole rest-of-stage-6: everything is
**pulled** (deps added) or **extracted** (M7) except the epoch anchor, which is **written new**
(because it is ~30 lines of guest-cmdline policy that exists on the metal only as a different
mechanism — RTC/NTP behind `akuma_timer`).

## 2. The LAN HTTP mirror, reproducible

The box has internet **today** (verified 2026-10-26: fetched `APKINDEX.tar.gz` from dl-cdn in
under 4 s); the *guest* never gets internet or TLS — the plan freezes bytes now and serves them
from the LAN forever after.

1. **Freeze (host, once):** pin `latest-stable` at today's date. Resolve the closure with the
   host's apk (`apk-tools 3.0.8` at `/bin/apk`, `/var/cache/apk` already holds the two
   APKINDEXes) in a throwaway root, then `apk fetch -R -o /srv/alpine-mirror/main/x86_64`
   the closure (expected: `curl musl libcurl brotli nghttp2 zlib openssl ca-certificates-bundle
   alpine-keys` + their deps — exact set is U2) plus `APKINDEX.tar.gz` + `APKINDEX.sha256`.
   Write `/srv/alpine-mirror/MANIFEST.sha256` (sha256 of every file). Copy the exact apks also
   to `/root/dev-cycle/taoxichuan-s8/apks/` so the mirror survives `/srv` cleaning.
2. **Serve (host, LAN only):** `python3 -m http.server 8123 --bind 192.168.1.159 --directory
   /srv/alpine-mirror` (the same :8123 H1 used; keep it out of herd's enabled set after stage
   work — ops rule). Every boot script starts it with `sha256sum -c MANIFEST.sha256` first;
   a mismatch aborts the boot (no boot wasted on corrupted bytes).
3. **Signed, not `--allow-untrusted`:** the image script unpacks `alpine-keys` into
   `/etc/apk/keys`, and the guest repositories file says
   `http://192.168.1.159:8123/main/x86_64`. `--allow-untrusted` is the documented fallback
   **only** if U1 settles against keys being present — and then the MANIFEST is the integrity
   contract. G3's `--force-non-repository` flag was for the offline local-file path and is
   *not* evidence about keys (U1).
4. **Checksums vs a Linux reference:** the box is x86_64, so the *same* apk.static produces the
   reference on the host (`TAOXICHUAN_STAGE6_PLAN.md` §7): `--root /tmp/refroot --initdb
   --repositories-file http://127.0.0.1:8123/main/x86_64 add curl`; compare after each guest
   boot — `APKINDEX.tar.gz` sha256, `/etc/apk/world` line-set, `apk info -L curl` file list,
   sha256 of `/usr/bin/curl` + the shared libs — guest vs refroot, byte-equal. The supervisor's
   comparison needs nothing new; the gate command list is in §5.

## 3. The clock: cheapest correct source

**`epoch=<date +%s>` on `vm_cmdline`** (herd injects it at start). The guest kernel scans the
cmdline once at boot, stores `unix = epoch + 0` and an `uptime_us()` sample into one
AtomicU64 pair (the exact pattern of `akuma_timer::set_utc_time_us(unix, at)` — the metal's
UTC offset store, `docs/reference/subsystems/syscalls/time.md:50`), and the existing
`CLOCK_GETTIME` (`main.rs:2007`) and `GETTIMEOFDAY` arms add the offset. Zero new syscalls,
zero relay work, deterministic; drift over a minutes-long gate is irrelevant. apk on plain HTTP
needs no trustworthy clock at all — this exists because `curl --version` prints dates and
because any future TLS step is blocked without it.

Rejected alternatives, on evidence: **SNTP** — `akuma-sntp` is pure and host-tested, but the
relay forwards DNS UDP only ("no non-DNS UDP", `KVM_OUTBOUND_NETWORKING.md:46`); UDP:123 needs
new relay work first. **CMOS RTC** — `akuma-cmos-rtc` reads ports 0x70/0x71 via `akuma-port`;
under the KVM guest that needs an IOIO-exit arm in the *host* kernel: strictly more work than
the cmdline anchor for the same answer.

## 4. Dynamic linking: is `curl` static?

`curl` on Alpine x86_64 is **dynamically linked** (U3 to confirm with `file` in M0: it needs
`libcurl.so.4`, `libssl.so.3`, … and `/lib/ld-musl-x86_64.so.1`, delivered by `apk add musl` —
note `musl` must join the mirror closure in §2). The taoxichuan loader today runs static ELFs
and static-PIE (`PIE_BASE` is respected — `taoxichuan/src/main.rs:504` keeps the mmap region
clear of it), but has **no PT_INTERP path**: the only interp handling in the tree is
`amd64/src/loader.rs` (reads `PT_INTERP`, maps ldso, `p_filesz<=1` static-PIE special case at
`:78-80`) and a comment in `crates/akuma-elf/src/lib.rs:123`. M7 is therefore the one real
kernel change: extract that decision core into a leaf crate and give taoxichuan the second
consumer. The ldso's own syscall needs are all served already (`mprotect`, `mmap`,
`futex`, `prlimit64`, `sigaltstack`, `set_robust_list`, `set_tid_address`) — except possibly
`mmap`'s `MAP_FIXED` flag (U4).

## 5. Milestones, mini-gates, boots, tokens, sessions, trim interleaving

| # | milestone | build | mini-gate (over `/bin/ssh -p 2224 root@localhost '<cmd>'`, plus `run_gates.sh` re-run per boot) | boots (guess) | tokens (guess) |
|---|---|---|---|---|---|
| M0 | mirror + reference (host only) | §2 steps 1-4; `file` on curl + dep closure; `tar -tzf` script check | host refroot populated; MANIFEST verifies; script check clean | 0 | ~0.3M |
| M1 | **clock anchor** | epoch= parse + offset store + 2 arms | `date` (busybox) prints host wall time ±2 min; `run_gates.sh` all PASS | 1-2 | 0.5-1M |
| M2 | **DNS** | resolv.conf in image (script), nothing else | guest `nslookup alpine 10.0.2.3` or `getent`-less probe; host log/counter `dns_answered>0` | 0-1 | 0.3-0.5M |
| M3 | **`apk update`** | stage7 conf (`vm_outbound=true`, the tracked `vm-dev.conf.stage7-hostrelay`), repositories file, keys per U1 | `apk update` rc 0; sha256 of `/var/cache/apk/APKINDEX.tar.gz` == refroot's | 1-3 | 1-2M |
| M4 | **`apk add curl`** | nothing new expected (U2) | `apk add curl` rc 0; `/etc/apk/world` line-set == refroot; `apk info -L curl` == refroot | 2-4 | 1.5-3M |
| M5 | **PT_INTERP extract** (`akuma-elf-exec`) + `curl --version` | the §1 M7 extraction; MAP_FIXED per U4 | `/usr/bin/curl --version` first line == host reference byte-for-byte | 3-6 | 3-6M |
| M6 | contingency (only if U2 surprises) | scripts path: `CLONE`+`EXECVE`+`WAIT4`+`PIPE2` onto SPAWN nesting | `apk add <pkg-with-script>` runs its `.post-install`, rc 0 | 4-8 | 2-4M |

**Cheapest first: M0 then M1** — M0 boots nothing; M1 is ~30 lines in one file and its boot is
the one that must re-run the gate battery anyway (`run_gates.sh`, one boot, `restarts=0`).
**Session boundaries:** one milestone per session, `RESULT-<x>.md` each, never carry console
dumps across contexts; M5 is the only multi-session candidate (extraction and consumption can
split, with the amd64 consumer switched first so the extraction is provably behavior-preserving
via `run_gates.sh` on the metal path it feeds).

**Where trim steps 2-3 interleave** (`TAOXICHUAN_STAGE7_TRIM_PLAN.md` §2): **step 2 (wire
dedup, `akuma-syscalls-abi`+`akuma-syscalls-linux`) goes between M2 and M3** — M3/M4 hammer
stat/statfs/dirent/time structs, so dedup *before* the first real apk-network boot means the
shared structs are what the gate asserts; **step 3 (net.rs hygiene: `akuma-port` + tsc + rng,
write-arm merge) goes between M4 and M5** — H1 is its canary, and M5 stacks the most new
networking on top, so hygiene lands while only H1+M3/M4 exercise the path. Neither trim step
touches the clock or the loader work.

## 6. Unknowns that could kill the plan (each with its read-only settle)

| # | unknown | settle (read-only) |
|---|---|---|
| U1 | are `/etc/apk/keys` present in `akuma-dev-root-apk.img.orig`, and which apk.static (2.x static-PIE) is in it — G3's `--force-non-repository` proves nothing here | host-side: loop-mount/`debugfs` the `.orig` image, `ls etc/apk/keys`, `apk.static --version` (0 boots) |
| U2 | exact curl dep closure; does any dep ship `.post-install`/trigger scripts (decides whether M6 exists) | host: `apk fetch -R -o … curl` in a temp root; `tar -tzf *.apk \| grep -E 'post-install\|trigger'` (0 boots) |
| U3 | curl really dynamically linked (vs Alpine's static-curl variant) | host: `file` on the frozen `.apk` members (M0, 0 boots) |
| U4 | does taoxichuan's `MMAP` arm honor `MAP_FIXED` (ldso places libs at fixed addresses) | `grep -n 'MAP_FIXED' taoxichuan/src/main.rs`; if the flag is ignored, one boot with a 3-line probe under M5 (0 boots to know, ≤1 to fix) |
| U5 | how welded the interp logic is to `amd64` `Process`/`usermode` (decides extract cost of M5) | `analyze amd64/src/loader.rs` + focus on the interp function before touching anything (0 boots) |

## 7. Ops facts this plan inherits

- The idle-machine state after this session is the **stage4 conf, `vm_outbound` off, vm-dev
  disabled, vm-serve-mc up** (restored 2026-10-26). M3 onward needs the tracked stage7 template
  (`vm-dev.conf.stage7-hostrelay`, `vm_outbound=true`) copied to BOTH `available/` and
  `enabled/` before `disable`+`enable` — herd re-parses only there; the truth is
  `/var/run/herd/vm-dev.vm` (RESULT-D findings 1-2).
- kvm0 is exclusive: `herd stop vm-serve-mc` before any dev boot, restore after.
- The ~30-connections-per-boot host-forward degradation (RESULT-D:129-131) still caps any boot
  at well under 30 ssh sessions; gates that need more (apk's one-command-per-connection style)
  must restart the VM or batch via `sh -c` inside one session.
