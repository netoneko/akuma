# Taoxichuan stage 7, session C — the trim PLAN (2026-10-25, planning only)

Branch `taoxichuan-s4` @ `b7646dd9`. This commit contains **no kernel code**. Numbers read off the
tree today are facts; anything else is marked **(guess)**. `taoxichuan/src` today: `main.rs` 2928 +
`child.rs` 676 + `sock.rs` 466 + `net.rs` 380 = **4450 lines** (stage 4 end: 3268). The composition
rule is the lens: `taoxichuan/` must consume the crates, not re-own them.

**Headline finding.** The trim is real but smaller than the 1,200-line growth: ~300 lines
(guess ±60) are removable by rewiring onto crates that already exist — **zero new crates needed**.
The irreducible rest (`child.rs`, `sock.rs`'s wait loops, `net.rs`'s calibration, the uaccess walk,
`open_common`'s open decisions) has **no callable twin**: the full kernels' equivalents are welded
to `akuma-exec`/`Process`/`SharedFdTable` (verified per row below), and this kernel's fixed
two-task, one-root model is the experiment. One genuine rules violation found and fixed by the
trim: `net.rs` open-codes `in`/`out` (`net.rs:43-72`), which `CLAUDE.md` § akuma-port forbids.

## 1. Inventory: every arm/subsystem, what exists, the verdict

Line spans are `taoxichuan/src/main.rs` unless noted (±3 lines each, from the arm grep).

| Group | Arms / helpers (lines) | Existing implementation of the same job | Verdict |
|---|---|---|---|
| fd table | `DUP` 2194-2208, `DUP2\|DUP3` 2209-2245, `CLOSE` 1790-1811, `FCNTL` 1753-1789, `fd_table()` 756-762, `clone_desc` 1119-1135 | `akuma-fdtable` (`alloc_fd_from`, `dup_to`, `free_below`, `set_cloexec`) — **already a dependency and already the only writer** | **already composed — keep** |
| open/close | `OPENAT` 1812-1827, `OPEN` 1828-1836, `open_common` 793-874, `resolve_at` 977-1009, `read_link_target` 712-732 | symlink chase **already** `akuma_vfs::resolve_symlink_chain` (`main.rs:733-735` — verified). The rest: glue's `sys_openat` is welded to `Process`+mount table; `read_link_target` is the ext2 provider closure | **keep** (the shared half is already called) |
| read/write/writev/lseek | `WRITE` 1278-1316, `WRITEV` 1317-1407, `READ` 1550-1628, `LSEEK` 2006-2046, `write_file_arm{_inner}` 875-933, `writev_file_arm` 934-976, `emit` 763-792 | amd64's `sys_read`/`sys_write_file` (`amd64/src/fd.rs:1184,1548`) do the same job but go through `SharedFdTable`/`FileDescriptor` — welded, not callable. The ext2 write (`write_at_by_inode`) **is** the shared path and is already called. The **routing order** is the bug — see §3 H3 | **rewire** (fd-table-first routing, §3); merge `write_file_arm_inner`+`writev_file_arm` into one gather-then-write helper (−35, guess); optional: parse iovecs via `akuma_syscalls_linux::io::IoVec` (−10, guess) |
| stat family | `STAT` 1672-1704, `LSTAT` 1705-1752, `FSTAT` 1979 + `fstat_fd` 1087-1118, `NEWFSTATAT` 1980-2005, `stat_path`/`lstat_path` 1010-1060, `fill_stat64` 1253-1275 | `akuma_syscalls_abi::stat::{AsmGeneric (akuma_syscalls_linux::Stat), X8664, to_x86_64}` — the x86_64 144-byte layout with `offset_of!` assertions (`akuma-syscalls-abi/src/lib.rs:822-897`); amd64's `sys_fstat` (`fd.rs:1800`) fills asm-generic then converts through exactly this. taoxichuan's `fill_stat64` hand-spells those same offsets as raw bytes | **replace by dependency**: build `Stat{..}` → `to_x86_64` → `copy_to_user` (−11). Keep `lstat_path`'s parent-dir walk (glue's is welded). Keep `fstat_fd`'s inode re-stat (per-kernel table) |
| statfs | `STATFS`/`FSTATFS` 2072-2082 + `fill_statfs` 1061-1086 | `akuma_syscalls_linux::stat::Statfs` — asm-generic == x86_64 for statfs (pinned by amd64 `fd.rs:1896` comment) | **replace by dependency** (−8) |
| dirs | `GETDENTS64` 1837-1904, `MKDIRAT`/`UNLINKAT`/`RENAMEAT`/`SYMLINKAT`/`LINKAT` 2114-2176 | `akuma_syscalls_linux::dirent::{encode, fill}` (amd64's `sys_getdents64` consumes it via glue). The C-phase arms are thin ext2 trait calls already | **GETDENTS64: replace** record encoding by `dirent::encode` (−22). Rest **keep** |
| sockets | `SOCKET..SHUTDOWN` 2296-2306 + `sock.rs` (466) | **already composed on `akuma_net::socket`** (`alloc_socket`, `socket_bind/listen/connect/send`, `SockAddrIn` — verified imports `sock.rs:26-27`). `connect_wait` 243-293 is the blocking shape; the shared wait loop is `akuma-net-yarn`, which needs a scheduler | **keep** (optional: route `sockaddr_in_from_user/to_user` 71-112 through `SockAddrIn` accessors, −15, guess) |
| exec/spawn/argv | `EXECVE` 2322-2352, `SPAWN`/`WAITPID`/`CLOSE_CHILD_STDIN` 2307-2309 + `child.rs` (676) + `load_task` 2555-2706 + `spawn` 2707-2895 | already on `akuma-ring3-amd64` (`EntryFrame`, `build_stack`, ELF `parse`). amd64's `sys_spawn` (`usermode.rs:5745`) is welded to `SpawnTable`/`Process` | **keep** (nowhere callable) |
| mmap/brk | `MMAP` 1494-1539, `MUNMAP\|MPROTECT` 1540-1549, `BRK` 1427-1450, `MREMAP` 2246-2294 | `akuma-syscalls-mem` (`mmap::plan`, `fixed_addr_unaligned_einval`, `fixed_overlaps_kernel_va`, `mremap::plan`) exists and is pure — but adopting it **changes behavior**: today unaligned `MAP_FIXED` silently rounds down; `plan()` would `EINVAL`. The slab model is this kernel's deliberate own | **keep**; optional P3 guardrail: adopt `fixed_addr_unaligned_einval` only with a boot budgeted for the EINVAL fallout |
| time | `CLOCK_GETTIME` 1912-1931, `NANOSLEEP\|CLOCK_NANOSLEEP` 1932-1962, `GETTIMEOFDAY` 2310-2321, `UTIMENSAT` 1629-1653 | `akuma-syscalls-time`'s `sys_*` bodies are **welded** (Cargo.toml depends on `akuma-exec`; verified) — not callable. The **wire types** are pure: `akuma_syscalls_linux::time::{Timespec, Timeval}` | **replace wire-only** by the structs (−25, guess); the sleep/park loop stays |
| misc wire | `UNAME` 2047-2071; `nr_name` 1155-1252 | `akuma_syscalls_linux::uts::Field`; `akuma_syscalls_abi::Syscall::from_x86_64(nr)` + `Debug` gives the nr→name checklist (the enum derives `Debug`, one row per call) | **replace both** (`nr_name` 98 → ~10 lines, −88; UNAME −8) |
| entropy | `GETRANDOM` 1963-1978 + `net.rs:205-277` (`has_rdrand`/`weak_fill`/`rng_fill`, 73 lines) | `akuma_primitives::rng::fill_bytes` (RDRAND-backed, host-stubbed; `akuma-primitives` **already a dependency**) | **replace by dependency** (−78, guess) |
| x86 plumbing | `net.rs:43-72` hand-written `outb`/`inb`/`pause`; `rdtsc` 74-95 (two cfg copies) | `akuma_port::PIT` (`load_channel0_oneshot`, `channel0_status`, `channel0_output_high`); `akuma_cpu::tsc::rdtsc` (`akuma-cpu` **already a dependency**) | **replace by dependency** — this also cures the open-coded-`in`/`out` rules violation (−35, guess). `calibrate`/`pit_present` 129-192 **keep**: the TSC-vs-PIT calibration exists nowhere else (amd64 has none); someday `akuma-tsc-calib`, not this trim |
| uaccess | `copy_from_user`/`copy_to_user`/`user_ok` 623-711 | `akuma-user-access` is welded to `akuma-mmu`'s AArch64 range walk (its Cargo.toml names `akuma-mmu`; verified); amd64's `crate::uaccess` is per-page-table-root, amd64-internal | **keep** — a shared uaccess crate is its own project, not a trim step |
| arch | `ARCH_PRCTL` 1451-1487 already calls `akuma_cpu::msr::set_fs_base`; pid/uid/`IOCTL`/`SET_TID_ADDRESS`/`UPTIME` one-liners | — | **keep** |

**Totals** (guess ±60): removes ≈ **300** lines, adds ≈ **40** (3 Cargo dep lines + glue) →
4450 → ~4190. Honesty: the growth stages 6-7 added is mostly the irreducible model; the trim's
value is (a) rules compliance, (b) de-duplicating wire formats the crates already assert, (c) the
H3 routing fix, not line count.

## 2. The H1-preserving core: step order, each boot re-runs all gates

Gates: **G1** `sh -c "echo hi"` → `hi` rc 0; **G2** `apk.static --version`; **G3** offline
`apk add --force-non-repository /pkgs/ca-certificates-bundle-20261020-r0.apk` (hash `5961c0a3…`),
"OK: 174 KiB in 1 packages" rc 0; **G4** `ls /etc/ssl1.1/certs`; **H1** guest
`busybox wget -qO- http://192.168.1.159:8123/` → `hello-from-host` + host listener logs the GET.
All run over `/bin/ssh -p 2224 root@localhost '<cmd>'`, one exec per connection. Boot dance per
RESULT-A ops: `kbuild -j 4 --features no-tests`; install **only** with explicit dest
`scripts/install_kernel_amd64.sh <elf> /boot/akuma-guest-dev` (the script's default overwrites the
HOST kernel — stage-7a trap); `herd stop vm-serve-mc` first (kvm0 exclusive); stage-7 conf snapshot
`/root/dev-cycle/taoxichuan-s7/vm-dev.conf.stage7-hostrelay` (the `hostrelay=` cmdline key is inert
since `11fe2194` deleted its parse — harmless); fresh `.orig` image copy per boot, re-run the G3
install in it. H1 needs the `:8123` listener up (`python3 -m http.server` over
`/root/dev-cycle/taoxichuan-s7/www`).

| Step | Change (one coherent group = one commit) | Gates after (one boot) | Revert alone? |
|---|---|---|---|
| 0 (host, no boot) | Fix the **already-red** `check_unsafe_budget.py` base: `akuma-ring3-amd64` 5 sites vs budget 2 (grew in s6b/s7a). Shrink to 2 or `--update` with the review reason. `cargo test -p akuma-syscalls-abi -p akuma-syscalls-linux -p akuma-port -p akuma-primitives` + `cargo clippy` | n/a | yes |
| 1 | **H3 routing rewire** (§3): `WRITE`/`READ`/`LSEEK`/`FSTAT` become table-first, matching the order `WRITEV` has had since `2c494d96`/`6879eb47`. No new deps | G1-G4, H1, **H2** (`dd if=/etc/hostname of=/dev/null` — first gated run of the ungated `OpenFile::Null` arm), **H3 single-command** (`sh -c 'echo x > /tmp/f'`, then separate exec `wc -c /tmp/f` → `2`), plus the 3-command form to settle U3 | yes — one commit, one file |
| 2 | **Wire dedup**: `akuma-syscalls-abi` + `akuma-syscalls-linux` deps; stat/statfs/dirent/uname/time structs + `nr_name`. Commit per family so any single revert is clean | G1-G4 (the apk path exercises stat/utimensat/getdents hardest) + `abi_write_probe` subset in the guest | yes, per family |
| 3 | **net.rs hygiene**: `akuma-port` + `akuma_cpu::tsc` + `akuma_primitives::rng`; merge the two write arms | G1-G4 + **H1** (the sensitive one: calibration/RNG feed connect+poll) + H2/H3 re-check | yes |
| 4 | H3 fork decision from step-1 evidence (U3): gate split host-side (zero kernel), or open the separate H3-f session (§3) | — | n/a |

H1 is never at risk alone: steps 2-3 do not touch routing or the socket table; step 1's routing
change is caught by G1/G2 in the same boot (sshd's own fds 1/2 are unbound → console unchanged).

## 3. H2 and H3 inside the same rewiring

### H2 — `dd if=/etc/hostname of=/dev/null` exits 0

- **Who owns /dev/null in the full kernels**: *what exists* = `akuma_vfs::dev`
  (`crates/akuma-vfs/src/dev.rs::lookup/list`, pure, host-tested; vfs.md § /dev). *What open does*
  = per-device arms welded to each kernel's process model (`src/syscall/fs.rs` aarch64;
  `amd64/src/fd.rs:861 sys_openat` + the DevNull-style descriptor `fd.rs:522-543`). Neither open
  side is callable from taoxichuan.
- **Does the trim make the hand-written arm unnecessary? No.** The arm is 6 lines
  (`main.rs:799-803`, `path == "/dev/null"` → `OpenFile::Null`) — it *is* the minimal shape of
  amd64's DevNull descriptor, and it is what the trim gates. Optional P3 (not required for dd):
  key open+stat+getdents on `akuma_vfs::dev::lookup(DevProbe { audio: false, block_slots: 1,
  in_box: false }, name)` so `/dev/zero` and `stat("/dev/null")` come from the shared table;
  budget a boot for the fstat answer change (today `fstat_fd` answers Null as a regular file,
  `main.rs:1095-1104` — fact).
- **Status at `b7646dd9`**: open (arm committed ungated, `2c494d96`; reads EOF, writes take the
  full count). Step-1's boot is its first evidence. Reference behavior list: amd64's own boot
  suite `fd.rs:2988-3018` (open-for-write accepts every byte, read is immediate EOF, fstat
  succeeds, close succeeds) — hand-run the same four in the guest shell.
- **Probes**: no new userspace needed; `dd` itself is the probe.

### H3 — `sh -c 'echo x > /tmp/f; cat /tmp/f; wc -c /tmp/f'` → `x`, then `2`

- **The correct path**: glue's `sys_write` File arm → `akuma_vfs_glue::fs::write_at_open_file_or_path`
  → ext2 `write_at` — i.e. the same `akuma-ext2::write_at_by_inode` taoxichuan **already calls**
  (`write_file_arm_inner:907`). The filesystem side is right; the divergence is **which route the
  write takes**, one layer up.
- **Code fact (the bug)**: `WRITE` checks `fd == 1 || fd == 2 → console` **before** consulting the
  fd table (`main.rs:1292-1297`), while `WRITEV` consults the table first and only falls back to
  the console for unbound fds (`main.rs:1358-1381` — the shape `2c494d96`/`6879eb47` already gave
  writev). A shell redirect dup2's a `File` description onto fd 1; `echo x > /tmp/f` (single
  command = last command = no fork needed) then flushes through **write(2)** and the bytes go to
  the console, leaving the file empty — exactly the b7646dd9 probe's "creates the file, bytes do
  not land". (That busybox ash flushes via write(2), not stdio writev, is inference from the
  probe result + ash's own output machinery; the arm order is verified. The step-1 boot's existing
  `[w]` probe `main.rs:879` settles which call the bytes ride.)
- **Exact change (rewiring, no new arm)**: make `WRITE`/`READ`/`LSEEK`/`FSTAT` match `WRITEV`'s
  dispatch order — nested-child pipe first, fd-table kind next, **unbound** 0/1/2 console (write) /
  EOF (read) / `EBADF` (seek/stat) last. ~10 lines moved per arm. Same class fix for `READ`
  (`fd == 0 → EOF` unconditionally today: a dup'd file on fd 0 reads empty) and `LSEEK`/`fstat_fd`
  (`fd <= 2 → EBADF` today: a dup'd file on fd 1 cannot seek or fstat).
- **The `;`-sequence half**: `fork`/`clone` have **no arm** (grep: `CLONE` appears only in
  `nr_name`, `main.rs:1210`), and `1ee0dcdd` recorded that busybox sh cannot run any non-final
  command without fork. So the one-line gate **cannot pass by rewiring alone**. Options: (a) run
  the gate as three ssh execs — `echo…`, `cat…`, `wc -c…` — host-side only, and call H3 met when
  the three outputs compose; (b) **H3-f**: a run-to-completion fork arm shaped like SPAWN (copy the
  task's tracked user ranges via the existing `add_region` bookkeeping, run nested, reap) — exists
  nowhere callable (full kernel's `sys_fork` is welded to Process/CoW), new machinery, **separate
  session, high risk**; only if the gate must stay one line.
- **Probes and how**: `userspace/abiprobe/c` (`abi_write_probe` — byte-for-byte serialization of
  stat/statfs/utsname/getdents-shaped records: the step-2 gate), `userspace/ext2probe/c`
  (create/write/read/list before-and-after a stress tree: run it if step 2/3 touches anything on
  the ext2 path — the verdict line is `NO REGRESSION`), `userspace/forktest/c_stress/`
  (mmap/fault/futex/thread C probes — only the arms taoxichuan serves will pass; expect the
  unhandled-nr checklist to name the rest, which is fine). All are musl-static from
  `userspace/build.sh` (docs/reference/userspace-layout.md); how they reach the guest image is
  **(guess)** — same flow as the gate's `/pkgs` payload or a debugfs copy before boot; budget one
  host step, no boot, to place them.

## 4. Portability and gates for the executing session

- New dependencies for `taoxichuan`: `akuma-syscalls-abi`, `akuma-syscalls-linux` (step 2),
  `akuma-port` (step 3). All are consumed by the full kernels and live in default-members with
  host tests; none is amd64-only. Before depending on `akuma-port`, confirm on the host:
  `cargo test -p akuma-port` (its device stubs are host-shaped, but verify — one command, U4).
- **Nothing enters the root `default-members`**: `taoxichuan` stays amd64-only and out (pinned in
  its Cargo.toml comment). No new crates → no new unsafe budgets, no `#[cfg(target_arch)]`
  needed inside `taoxichuan` (it is never built for aarch64).
- `scripts/check_unsafe_budget.py` **and** `cargo clippy` (pre-commit) must pass for every commit;
  the base is currently red (§2 step 0) — fix before the first trim commit so the trim's diff is
  judgeable.
- Host tests per extraction: **no extractions**, so none new — the wire crates already carry their
  own (`offset_of!` assertions in `akuma-syscalls-abi`, layout tests in `akuma-syscalls-linux`);
  run them in step 0 and after step 2.
- Never `origin`; push `litter taoxichuan-s4`; never write `/boot/akuma-serve-root.img` or the
  default install destination; `/root/.config/goose` untouched.

## 5. Sessions, boots, tokens (all guesses; fresh context ≈ 30k, this session's running average ≈ 100k)

| Session | Content | Tokens | Boots | Risk (why) |
|---|---|---|---|---|
| S0 | step 0 (budget base, host tests) | ~0.3M | 0 | trivial |
| S1 | step 1: H3 routing rewire + the big gate boot (G1-G4+H1+H2+H3) | 1.5-3M | 1-2 | **low-medium**: routing is behavior, but G1/G2 catch a wrong fallback in the same boot; single-file diff |
| S2 | step 2: wire dedup, per-family commits | 1.5-3M | 1 (+0-1) | **low**: asserted structs; abi probe cross-checks |
| S3 | step 3: net.rs hygiene + write-arm merge | 2-4M | 1-2 | **medium**: PIT/TSC/RNG feed connect+poll; H1 is the canary; keep latch values identical to today's |
| S4 | step 4: gate split or scope H3-f | 0.5-1M | 0 | low |
| H3-f (optional, separate) | run-to-completion fork | 8-15M | 4-8 | **high**: new task machinery on a 2-slot model |

**Cheapest first step: S1** — one file, ~40 lines moved, targets an open hypergate (H3), and its
boot is the one that must happen anyway to gate H2. End each session after its coherent group with
the gate set green (or the step reverted) and a one-paragraph RESULT note; never carry console
dumps across contexts.

## 6. Unknowns that could kill the plan (each with its read-only settle)

1. **Does the routing rewire actually land echo's bytes, or does sh ride another call?** Settle:
   the step-1 boot's existing `[w]` write probe (`main.rs:879`) prints fd/len/ret for every file
   write — no new code, the boot already scheduled. Host-only fallback: objdump the image's
   busybox for `write` vs `writev` in ash's flush.
2. **A dependent on the old misrouting** (something wanting fd1/2 writes on the console *while
   bound*). Settle: G1-G4 + H1 in the step-1 boot; sshd's fds are unbound → console path unchanged.
3. **The `;`-sequence on this busybox** — does sh really skip non-final commands (fork ENOSYS)?
   Settle: run the 3-command form in the step-1 boot and read which outputs appear (already
   scheduled; costs nothing extra).
4. **`akuma-port` host-test status as a new taoxichuan dependency.** Settle: `cargo test -p
   akuma-port` before step 3 (0 boots). If it is amd64-none-only, gate the dep behind a taoxichuan
   feature and keep it out of default-members (it already is).
5. **The red unsafe-budget base**: which of ring3's 5 sites the counter sees and whether 3 can go.
   Settle: read `scripts/check_unsafe_budget.py`'s counting rule and the 5 lines (host-only, 15
   min). If they are irreducible (naked-entry asm), `--update` with the reason is the honest move.

— goose-glm, stage 7 session C. Verified by reading `taoxichuan/src`, `amd64/src/{usermode,fd,fs}.rs`,
`crates/{akuma-syscalls-abi,akuma-syscalls-linux,akuma-syscalls-time,akuma-syscalls-glue,akuma-syscalls-mem,
akuma-vfs,akuma-ext2,akuma-user-access,akuma-port,akuma-primitives,akuma-cpu}`, and the stage-6/7
docs + RESULT-A. No kernel code, no boots, no /boot writes in this commit.
