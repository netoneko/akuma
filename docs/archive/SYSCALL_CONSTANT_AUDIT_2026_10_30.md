# Syscall-number constant audit — 2026-10-30

**Status: report only. Nothing here is fixed yet (the user's call: fix later).**
Tree: `rust-org-il-experiment` @ `6af48c06`. Tool: `scripts/audit_syscall_constants.py`.

## Why

A syscall number that is wrong does not fail: it reaches a *different* handler, which usually
answers something plausible. The tree has paid for that before — x86_64 88 (`symlink`) dispatched
to `sys_utimensat` and `ln -s` reported success for months (`CLAUDE.md`, `akuma-syscalls-abi`).
The last sweep was the `syscall_table!` consolidation (2026-09-07); since then two kernels and a
third x86_64 table (`akuma-ring3-amd64::nr`, taoxichuan's) have grown.

## Method

Truth: Linux 6.18's own tables — `arch/x86/entry/syscalls/syscall_64.tbl` (ABIs `common`, `64`)
and `scripts/syscall.tbl` (asm-generic; the ABIs arm64 enables: `common`, `64`, `renameat`,
`rlimit`, `memfd_secret`) — cross-checked against musl's `bits/syscall.h` for `x86_64` and
`aarch64` (musl-cross 0.9.11), which is the authority this tree's comments cite. **musl and Linux
agree on every number both define.**

Checked (657 constants):

| table | arch | constants |
|---|---|---:|
| `akuma-syscalls-linux::nr` | asm-generic / AArch64 | 205 |
| `akuma-syscalls-abi` rows (x86_64 literal + the `nr::` path of the same row) | both | 156 |
| `akuma-ring3-amd64::nr` (taoxichuan's dispatch) | x86_64 | 93 |
| libakuma `aarch64_nr` (sshd's syscall layer) | AArch64 | 76 |
| libakuma `x86_64_nr` | x86_64 | 53 |

Plus literal dispatch arms in `amd64/src/usermode.rs` and `taoxichuan/src/*.rs` against the call
their comment names, and explicit `name = N` / `name (N)` / `x86_64 N` claims in comments of those
files and of the two x86_64 tables. Other userspace (probes, `paws`, `klogd`) is out of scope;
sshd reaches the kernel through libakuma, which is in.

## Findings

### 1. BUG — `akuma-ring3-amd64::nr::GETPID = 38`, `SENDFILE = 39` (both off by one)

`crates/akuma-ring3-amd64/src/lib.rs:826-832`. On x86_64, `getpid` is **39**, `sendfile` is **40**,
and 38 is `setitimer`. The doc comments state the same wrong numbers and record the change that
introduced them as a fix: *"`getpid(2)` — x86_64 38. (Was 39: an off-by-one that ... made nr 39,
`sendfile` on x86_64, answer the pid.)"* — the evidence it cites, the guest logging
`unhandled ? (38)`, was a program calling `setitimer`.

Effect in taoxichuan (`taoxichuan/src/main.rs:3017`, the arm
`ring3::nr::GETPID | akuma_syscalls_abi::x86_64::GETPID => current_pid()`):

- `getpid` (39) still works, because the arm also matches the abi table's correct `GETPID`.
- **`setitimer` (38) returns the caller's pid**: a positive "success", no timer armed. taoxichuan
  has no `setitimer` arm, so nothing else catches 38.
- `SENDFILE = 39` contradicts taoxichuan's own literal `40 =>` sendfile arm (`main.rs:3172`); no
  arm uses the constant, so the only effect is a wrong label.

### 2. HAZARD — three x86_64 number tables; one has drifted

`akuma-syscalls-abi` (156 rows), `akuma-ring3-amd64::nr` (93), libakuma `x86_64_nr` (53). The abi
table exists so a number is written once and round-trip tested; `ring3::nr` predates its use in
taoxichuan and is the one with finding 1. libakuma's table agrees with the abi table everywhere.

### 3. HAZARD — `KILL` means three different things

| constant | value | meaning |
|---|---:|---|
| `akuma-syscalls-linux::nr::KILL`, libakuma `aarch64_nr::KILL` | 302 | Akuma-private (kill a spawned process) |
| libakuma `x86_64_nr::KILL` | `0x1000 + 302` | the same private call on x86_64 |
| `akuma-ring3-amd64::nr::KILL` | 62 | Linux `kill(2)` |

Linux `kill` on AArch64 is spelled `KILL_LINUX` (129). Every value is right for what it means; the
shared name is how the next wrong number gets written.

### 4. NAMING — two misleading names in `akuma-ring3-amd64::nr`

- `SELECT_PSELECT6 = 23` is `select`; `pselect6` is 270. The number is right, the name suggests the
  other call.
- `PR_LIMIT64 = 302` is `prlimit64` (spelling).

### 5. NOTE — literal arms outside any table

`taoxichuan/src/main.rs`: `26 =>` (`msync`), `40 =>` (`sendfile`), `326 =>` (`copy_file_range`);
`amd64/src/usermode.rs`: 31 native literal arms (`arch_prctl` 158, `stat`/`lstat`, `msync` 26,
`settimeofday` 164, `pidfd_*` …). All numbers are correct; they are invisible to the abi table's
round-trip tests, which is the gap finding 1 fell through.

### 6. OK — what checked clean

- Every Linux-named constant in `akuma-syscalls-linux::nr` (205), libakuma's two tables, and both
  halves of all 156 `akuma-syscalls-abi` rows. No row pairs two different calls (the
  `symlink`→`utimensat` class).
- The Akuma-private ranges do not shadow Linux: AArch64 300-328 sits in the asm-generic gap
  (295-402 are unassigned for 64-bit), and x86_64's private calls start at `0x1000`.

### Observations (coverage, not wrong constants)

- Handlers that exist in `akuma-syscalls-glue` but have no x86_64 route (no abi row, no native
  arm): `waitid` (247), `rt_sigsuspend` (130), `clone3` (435). Reachable on AArch64 only.
- The legacy `getdents` (78) is not served on amd64; `getdents64` (217) is.

## False positives the tool reports (read before re-running)

- Arms under `match nr - AKUMA_PRIVATE_BASE` (`usermode.rs:1232`) are private offsets, not Linux
  numbers.
- Inner `match`es on `fcntl` commands (`F_GETFL` 3, `F_OFD_*` 37/38) and trace checks (`nr == 1`).
- Comments that state correct pairs (`` `sendmsg`/`recvmsg` — x86_64 46/47 ``), cross-arch warnings
  in the abi table (*"x86_64 128 (`rt_sigtimedwait`) is asm-generic `msgctl`"*), and exit codes
  (`exit(241)`).

## To verify a fix

Re-run the script; findings 1-4 should be gone, and class A should report 0 apart from the
intentional private `KILL` (or none, if it is renamed). Inputs: see the script's docstring
(the Linux tables were taken from the 6.18 tree on the ryzen, `/root/linux-audit/linux-6.18`).

## Background

- `crates/akuma-syscalls-abi/src/lib.rs` — the single-source table and its two rules.
- `docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md` — why the vocabulary hop exists.
