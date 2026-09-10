# amd64 C1 step 4b, batch 4b: `poll`, `select`, `ppoll`, `pselect6`, `ioctl`

**Date:** 2026-09-10
**Status:** landed.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessor:** `AKUMA_AMD64_4B_FOLD_BATCH3C.md` (`fcntl`/`dup`/`pipe2`/…),
whose § "What is left in `fd.rs`" names these two as the arms that "need care
rather than a forward".
**Prompt:** `proposals/NEXT_AGENT_AMD64_4B_POLL_IOCTL.md`.

Five arms, and unlike batch 3c none of them is a forward. `amd64/src/fd.rs`:
**3 083 → 3 008** at the fold, then 3 192 with the boot checks and the console
fix that shares this session (`AMD64_CONSOLE_NONBLOCK_READ.md`). Diff:
`+560 / −399` across nine files, most of it doc comments stating divergences.

Both arms hinge on the one thing the two kernels model differently — the
**console**. Glue's world has a `ProcessChannel` a process either has or has
not; amd64's world answers a serial line *by fd number* and **no process here
has a channel at all**.

## 1. `poll` / `select` / `ppoll` / `pselect6`

### 1.1 What was there

| number | arm | shape |
|---|---|---|
| x86_64 `7` (`poll`) | `fd::sys_poll` | a hand-rolled **yield budget** over `fd::poll_ready` |
| x86_64 `23` (`select`) | `fd::sys_select` | the same loop, `akuma_syscalls_poll::fdset` for the bit math |
| `Syscall::Ppoll` (271) | folded the `timespec` to **milliseconds** and called `sys_poll` | — |
| `pselect6` (x86_64 270) | **not dispatched** | `ENOSYS` |

### 1.2 The seam: one function, and the timeout is the only difference

`glue::poll::sys_ppoll` and `sys_pselect6` each read a `struct timespec` from
user memory as their first act. amd64's `poll(2)` has no `timespec` to point at
— its ABI carries an `int` of milliseconds in a register — and fabricating a
user pointer for one would be the wrong shape. So each glue arm split in two:

```rust
pub fn sys_ppoll(fds, nfds, timeout_ptr, sigmask) -> SysResult {
    ppoll_timeout_us(fds, nfds, time::read_timeout_us(timeout_ptr)?)
}
pub fn ppoll_timeout_us(fds, nfds, timeout_us: Option<u64>) -> SysResult { … }
```

**The `Option` is the ABI difference.** `ppoll` says "wait forever" with a NULL
`struct timespec *`; `poll` says it with a negative `int`; `select` says it with
a NULL `struct timeval *`. None is representable as a plain `timeout_us`, and a
sentinel would be a third spelling to get wrong. Reading the timeout *before*
validating `fds_ptr` is also Linux's order (`do_ppoll` calls `get_timespec64`
at the top, ahead of `do_sys_poll`), so the reordering is a convergence, not a
cost.

amd64's four arms are now: `7` converts ms → `Option<u64>`; `23` converts
`struct timeval`; `271` and `270` forward.

### 1.3 The console is a hook, not a preamble

The prompt proposed a preamble in `fd::sys_poll` — `console_end(fd).is_some()
&& !is_bound(fd)`. That is wrong in both halves and a preamble is the wrong
mechanism.

**Wrong test.** `!is_bound(fd)` excludes exactly the case that matters. A
*registered* process's fd 0 **is** bound — `SharedFdTable::with_stdio` puts
`FileDescriptor::Stdin` there — so `INIT=/bin/busybox INITARGS=sh` on the serial
line would have been skipped by that guard and answered by glue's `Stdin` arm,
which finds `current_channel() == None` and reports never-readable. The right
test is plain `console_end(fd)`, which covers **both** spellings (by-number for
the boot task, by-descriptor for a registered process) and answers `None` for a
*redirected* 0/1/2 — a spawned child's fd 0 is a `PipeRead` and must fall
through to glue's pipe arm, the one with a real waker.

**Wrong mechanism.** One `poll` call mixes console fds with pipes and sockets
and must wait on all of them together, so the answer cannot be a wrapper around
the syscall; it has to be inside the per-fd scan. `SyscallHooks` gained an
eighth field:

```rust
pub poll_console_state: fn(u32) -> Option<akuma_syscalls_poll::readiness::FdState>,
```

consulted at the very top of `epoll_check_fd_readiness` — **before** the
fd-table lookup, because an unbound console fd is not in the table. AArch64
registers `|_| None` and its arms are untouched.

It returns an **`FdState`, not a bitmask**, so the console is mapped by the same
host-tested `readiness()` table as every other resource instead of beside it:
`ConsoleEnd::Read` → `FdState::Stdin { has_data: input::has_byte() }`,
`ConsoleEnd::Write` → `FdState::Sink`.

Without the hook the two answers glue gives are both wrong, and the *unbound*
one is worse than "not ready": an fd that is not in the table is
`FdState::Missing`, which is `EPOLLHUP | EPOLLERR` — "this fd is finished" — on
the console a shell is about to read from.

### 1.4 What the fold fixed

The old loop was a lap counter: re-scan every fd, `sched::yield_now()`, and
approximate a finite timeout at "200 laps per millisecond, capped at two
million". Glue's is `akuma_net_yarn::WaitMachine` under `WaitPolicy::epoll`.

- **A finite timeout is a timeout.** The budget approximated one with a lap
  count on a target whose lap cost is not fixed. `clock::uptime_us` is real
  here and the machine reads it.
- **`nfds == 0` blocks.** It used to yield once and return 0. musl's `pause()`
  is exactly `ppoll(NULL, 0, NULL, …)`, so `alarm()` + `pause()` was a no-op
  that returned instantly.
- **A signal interrupts the wait.** The budget loop had no interrupt check at
  all, so a `poll(-1)` slept through its own `SIGALRM`.
- **A blocked `poll` stops spinning.** It was a `yield_now` loop consuming a
  full share of every scheduling round; it parks now, capped at the 10 ms
  blocking-poll interval.
- **A sub-millisecond `ppoll` survives.** The old `Ppoll` arm folded the
  `timespec` to milliseconds, so a 500 µs timeout became 0 and returned
  immediately.
- **A listening TCP socket is readable when a connection is waiting.** The old
  probe called `akuma_net::socket::socket_tcp_ready`, which resolves *one*
  smoltcp handle and answers `(false, false)` for a listener — which is a pool
  of `MAX_BACKLOG` handles, not one. So `poll`/`select` on a listening fd could
  never report a pending connection and an event-driven server that waits before
  calling `accept` waited forever. Glue's probe asks `listener_ready`, which also
  reaps backlog handles that died unaccepted.
- **Dead and half-closed TCP report `EPOLLHUP` / `EPOLLRDHUP`.** The old probe
  had neither.
- **`pselect6` (270) is dispatched.** It answered `ENOSYS`.

### 1.5 The unbounded allocation, and the cap that replaced three

`glue::poll::sys_ppoll` did `alloc::vec![PollFd; nfds]` with **no bound**, and
`nfds` is a ring-3 `usize`. A `poll` with `nfds = 2^40` asked the allocator for
8 TiB on the failure path of an infallible `Vec`, which aborts. There were three
different answers to one question in the tree: 64 in amd64's `poll`,
`fdset::nfds_ok` (1024) in `pselect6`, and nothing in `ppoll`. There is one now
— `fdset::nfds_ok`, in `ppoll_timeout_us` — so the cap moved *into* glue rather
than staying a per-target preamble, which is a departure from the
`MAX_FDS`-is-a-lookup-bound pattern batches 2d/3c established and is deliberate:
this is not a lookup bound, it is an array length, and it was a real hole on
both kernels.

**That change is what broke the boot.** The suite's `poll with too many fds is
EINVAL` check passed `nfds = 999` — chosen against amd64's 64 — which is now a
*legal* `nfds`. The suite runs inside `BypassValidationGuard`, so the range check
that would refuse a ring-3 caller 7 992 bytes of unmapped `struct pollfd` passed,
and `poll` duly wrote 7 992 bytes of `revents` back over an **8-byte stack
array**, taking the return addresses with it:

```
[EXCEPTION] #GP general protection err=0x0000000000000000
  rip=0x0000ffff802b6ab0 rsp=0xffff800000248e28
```

`rip` is non-canonical — bit 47 set, bits 63:48 clear — i.e. the CPU tried to
fetch from a truncated address, which is what a corrupted return address looks
like here. The check now asks for `fdset::MAX_FDS + 1` so the call is refused
before any copy happens. Nothing ring 3 can do reproduces it: without the bypass
the range is unmapped and the answer is `EFAULT`.

### 1.6 Pinned divergences

- **`sigmask` is ignored**, on both kernels — glue takes it as `_sigmask`. A
  `ppoll`/`pselect6` that atomically swaps the signal mask for the wait is the
  whole reason those calls exist over `poll`/`select`. Nothing in the tree
  passes a non-NULL mask.
- **`select`'s `struct timeval` is not updated on return.** Linux writes the
  remaining time back.
- **`select` with `nfds == 0` returns 0 immediately** instead of sleeping out
  the timeout — the opposite of `ppoll`'s, which this batch fixed. Both now sit
  in one file.
- **A `poll` on an fd the process does not have reports `POLLHUP|POLLERR`**
  where the old arm reported "not ready". Linux reports `POLLNVAL`. The new
  answer is the better of the two: "not ready" makes a poll on a closed fd hang
  forever.
- **`Pselect6` is dispatched but not exercised.** Measured from musl's
  `src/select/select.c`: `#ifdef SYS_pselect6_time64` is false on x86_64 (a
  32-bit-arch path) and `#ifdef SYS_select` is true, so `select()` issues **23**
  here and falls through to `pselect6` only on an architecture without number 23
  — which is aarch64. So the prompt's question "does amd64 need a `Pselect6`
  arm at all?" answers *not for musl*; the row exists because a program calling
  it by hand got `ENOSYS` from a kernel that implements the call.

## 2. `ioctl`

### 2.1 The divergence is the problem, not an accident

This is the one place in the 4b series where the two kernels disagree on
purpose. They implement **two different interactive-shell architectures** and
each answers `TCGETS` on a pipe the way its own architecture requires.

- **amd64** answers `TCGETS` for `fd < FIRST_FILE_FD` deliberately: a spawned
  child's stdin *is* a pipe (`bind_stdio`), there is no `ProcessChannel` and no
  PTY, so the pipe has to *be* the terminal, faked at `ioctl`. An interactive
  `busybox sh` that gets a failing `TCGETS` prints no prompt, does no line
  editing and reads to EOF — over an ssh channel, indistinguishable from a hang.
- **glue** checks the fd *table entry* and answers `ENOTTY` for a `PipeRead`,
  equally deliberately — *"so shells like busybox run non-interactively over the
  SSH-into-box bridge instead of launching a line editor that hangs on an
  `ESC[6n` cursor query"* (`TTY_SHENANIGANS.md` round 3). On that kernel the
  exec bridge hands the child a channel that reports `is_terminal()` for a real
  PTY session, *separately* from its stdio pipes, so the table entry is free to
  be the ground truth.

Neither is wrong for its own kernel and neither can adopt the other's. So amd64
keeps its own answers for the seven requests that decide the question —
`TCGETS`, `TIOCGWINSZ`, `TCSETS`/`TCSETSW`/`TCSETSF`, `TIOCSWINSZ`,
`TIOCGPGRP`/`TIOCSPGRP`/`TIOCSCTTY` — in a `console_ioctl` that returns
`Option<u64>`, and delegates everything else. `None` means "not one of mine".

The gate is `fd < FIRST_FILE_FD || console_end(fd).is_some() || dev_node_of(fd)
== Some("tty")`. The first term is load-bearing (the fake tty). The second adds
the descriptor spelling. The third is the fd a pager opens on `/dev/tty`, which
is never 0/1/2.

### 2.2 What glue adds

Each of these has a caller in the tree and amd64 had none of them: `FIONBIO`
(the non-blocking flag, which `fcntl` already tracked and `ioctl` could not
set), `FIONREAD` (bytes available, answered per fd kind — a pipe and a socket
give real counts), `FIOCLEX`/`FIONCLEX`, `FIOASYNC` (a no-op success, without
which nginx's `ngx_spawn_process` refuses to fork at all).

### 2.3 What left: a byte-for-byte duplicate

`fd::interfaces()` and `fd::siocgif()` — ~70 lines of `struct ifreq` / `struct
ifconf` marshalling behind `busybox ifconfig` — were a **byte-for-byte**
duplicate of `glue::net`'s `net_ifaces` + `sys_ioctl_siocgifconf` /
`sys_ioctl_siocgifreq`, right down to rebuilding the two-interface array on
every call so a DHCP change shows up. Both sides already marshalled through the
same `akuma-syscalls-net`, so there was one layout and two copies of the
user-copy loop around it.

Delegating them is a **behaviour change**: glue gates `SIOCGIF*` on
`FileDescriptor::Socket(_)` and otherwise answers `ENOTTY`, where amd64 answered
on *any* descriptor. That is Linux — they are socket ioctls — and `busybox
ifconfig` always holds an `AF_INET` socket. The boot suite was the only caller
that noticed: its four checks passed an **unopened fd 3** and moved to
`sock::smoke_test`, which has a real socket, plus a fifth asserting the gate.

### 2.4 Second carried change

**A request on a non-console fd with no registered process is `ESRCH`, not
`ENOTTY`.** Glue's first line is `current_process_shared()`. Only the boot task
can be in that state and `boot_row_register` is the answer the suite already
uses for it (batch 2b).

## 3. Found, not fixed: glue's `struct termios` is one byte off

`glue::term::sys_ioctl`'s `TCGETS` builds a `[u32; 9]` and copies
`TerminalState::cc` (20 bytes) onto `kernel_buf[4..]`, i.e. **byte offset 16**.
The kernel `struct termios` is

```c
tcflag_t c_iflag, c_oflag, c_cflag, c_lflag;  /* 0 .. 15 */
cc_t     c_line;                              /* 16      */
cc_t     c_cc[NCCS];                           /* 17 .. 35 */
```

so `c_cc[0]` is at byte **17**, and `akuma_terminal::cc_index::VINTR` is `0`.
Every control character glue reports is therefore shifted one byte: `c_line`
receives `VINTR`, and a program reading `c_cc[VERASE]` gets `VQUIT`'s value.
`TCSETS` reads it back the same way, so a get/modify/set round-trip is
self-consistent — which is why it has survived: only a program that *hardcodes*
an expected `c_cc` value can see it.

**Not fixed here.** It is a one-line change on the AArch64 tty path, this
session cannot boot that kernel (§5), and a wrong guess there breaks every
interactive ssh session. amd64's own `console_ioctl` writes byte 17 and carries
a comment pointing at this section.

## 4. What left `fd.rs`

- `sys_poll`'s and `sys_select`'s yield-budget loops, and `poll_ready` — the
  local readiness probe. `glue::poll::epoll_check_fd_readiness` +
  `akuma_syscalls_poll::readiness` is the one map now, and the console arm of
  the old probe survives as `poll_console_state`, registered as a hook.
- `interfaces()` / `siocgif()` — §2.3.
- `sys_ioctl`'s `SIOCGIF*` dispatch and its five hardcoded terminal arms' outer
  `match` (the arms themselves moved into `console_ioctl`).

## 5. Verification

| gate | before 4b | after fold | after the console fix |
|---|---|---|---|
| QEMU/TCG `SMP=1` | 596/0 | 609/0 | **616/0** |
| QEMU/TCG `SMP=4` | 606/0 | 619/0 | **626/0** |
| host tests | 1372 | 1372 | **1372** |
| clippy — aarch64 `release`, `extreme-size`, amd64 | clean | clean | **clean** |
| amd64 `no-tests` build | OK | OK | **OK** |
| `apk update` (QEMU) | OK | — | **OK**, 28 641 packages |
| `apk add file` (QEMU) | OK | — | **OK**, 3 packages / 11.0 MiB |
| `fd.rs` lines | 3 083 | 3 008 | 3 192 |

`apk` is the real-workload gate for this batch specifically: its DNS lookups
`poll` a UDP socket for the reply and its TLS fetches wait for post-connect TCP
writability through `select`. Both now run entirely on glue's arms.

**+20 checks over the batch**, and the poll ones are new coverage rather than
moved: an idle console reporting *neither* readable *nor* `POLLHUP|POLLERR` (the
hook, §1.3), `poll(stdout, POLLOUT)` ready (`FdState::Sink`), a pipe pair before
and after a byte (real readiness with a real waker), and `select` on that pair
returning 2 with `exceptfds` cleared — the libcurl `CURL_CSELECT_ERR` rule in
`docs/runbooks/cargo-cannot-reach-crates-io.md`, which amd64 had implemented and
never tested.

### Still owed

- **Firecracker/KVM** `SMP=1` 580/0 and `SMP=4` 590/0, and **bare metal** 596/0
  — not re-run. The counts will be +20 there too.
- **`amd64_ring3_check --smp 1 -n 40` / `-n 60`**, `lazybuf` 8/8, `openflags`
  20/20.
- **`apk update` + `apk add file` on the metal.**
- **An interactive `busybox sh` over ssh, `less`, and a serial-console shell** —
  the `ioctl` preamble's whole purpose, and the check no boot suite can make.
- **The AArch64 side-by-side.** The `crates/` diff is *not* `pub(super)` → `pub`
  only this time: `poll.rs` gained the hook call and the `nfds_ok` cap, and
  `SyscallHooks` gained a field. HVF asserts (`hvf_handle_exception … isv`) and
  TCG panics in a pre-existing self-test (`test_spawn_ext_passes_env`,
  `AKUMA_SELF_HOSTING_AMD64.md` Open issue 4), so a committed-HEAD-vs-change
  boot to the panic point via `scripts/lima_aarch64_run.sh` is what this owes.
  The AArch64 kernel does *build* clean and its clippy is clean.

## 6. After this: `fd.rs` is at its floor

What remains is genuinely this target's: `sys_write` (serial console +
`WRITE_SEQ` + the `O_ACCMODE` refusal), `sys_read`/`sys_pread64`/`sys_lseek`
(console / `ESPIPE` / `/dev`-node preambles), `sys_poll_input_event` (313,
x86-only, the USB keyboard), `console_ioctl`, `poll_console_state`, and the
module's own surface (`pid_map_rows`, `bind_stdio`, `alloc_socket_fd`/
`alloc_pipe_fd`, `install`, `console_end`, `dev_node_of`, the boot checks).

Four of those preambles exist for one reason, and it is the same reason
`AMD64_CONSOLE_NONBLOCK_READ.md` §6 gives: **no amd64 process has a
`ProcessChannel`**. Giving one to an sshd session's child — the deferred
`/proc/<pid>/fd/0` + `delegate_pid` work in `AKUMA_AMD64_4B_FOLD_BATCH2A.md`
§ `/proc` — retires the `read`/`write`/`poll`/`ioctl` preambles together, fixes
raw mode and `/dev/tty`, and closes the last `fd.rs`-owned `/proc` path. That is
now the highest-value item in this file, ahead of the **ring-3 entry seam** the
prompt names as the C1 box's next structural piece.

## Background

- `proposals/NEXT_AGENT_AMD64_4B_POLL_IOCTL.md` — the prompt, including the
  console-preamble test this batch had to correct (§1.3).
- `docs/archive/AMD64_CONSOLE_NONBLOCK_READ.md` — the same session's console
  `O_NONBLOCK` fix, which this batch's hook is what makes useful.
- `docs/archive/AKUMA_AMD64_4B_FOLD_BATCH3C.md`, `…3B.md`, `…3A.md`, `…2D.md`,
  `…2A.md` — the series.
- `docs/reference/subsystems/syscalls/poll.md` — the readiness seam, the wait
  loop's six-field divergence between the two families, and the seven pinned
  Linux divergences.
- `docs/runbooks/cargo-cannot-reach-crates-io.md` — `exceptfds`, and why a set
  the kernel received but did not write is a bug.
- `docs/archive/TTY_SHENANIGANS.md` round 3 — the AArch64 interactive-shell
  architecture §2.1 contrasts with.
- `docs/archive/APK_MISSING_SYSCALLS.md` — the AArch64 twin of `apk` wedging on
  a missing readiness call.
