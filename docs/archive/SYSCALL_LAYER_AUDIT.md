# Syscall layer duplication audit (2026-08-24)

**Scope:** `src/syscall/*.rs` — 21 files, 16,581 lines, run through PMD CPD
7.26.0 per `docs/runbooks/find-duplicated-code.md`, plus manual Type-2 hunting
in the shape the runbook's §5 and `TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §6
describe (CPD is Type-1 only; every number below is a floor).

**Established:** three drifted-clone families, one of them a live,
reachable, three-times-repeated bug in the fd-duplication path
(`dup`/`dup3`/`fcntl(F_DUPFD*)` silently drop refcounts on `EventFd` and
`RumpSocket` descriptors that a fourth, actively-maintained copy of the same
match — in a different crate — handles correctly, having already been bitten
once for the `RumpSocket` case via a different call path). The `poll.rs`
wait-loop family described in the assignment is confirmed from source,
independently, with one addition: the `pselect6` gap is not just asymmetric,
it is a live bug (a process blocked in `select()`/`pselect6()` cannot be
interrupted by `kill()` or SSH Ctrl-C). A third family, `fs.rs`'s
dirfd-relative path resolution, has **seven** independent hand-written copies
across four different error-handling behaviors, including one path
(`renameat`/`renameat2`/`symlinkat`/`linkat`/`readlinkat`, via the shared
`resolve_path_at` helper) that silently treats a bad `dirfd` as `/` instead of
returning `EBADF` like every sibling `*at` syscall does.

**Not established / not checked:** every syscall file's wait/park loop was
not individually re-derived from source the way `poll.rs` and the `fs.rs`
dirfd family were (see "What was not checked" below) — `unixsock.rs`,
`sync.rs`, `aio.rs`, `msgqueue.rs` and `term.rs` were skimmed via CPD output
only. `net.rs`'s `sendmsg`/`recvmsg`/`unix_recvmsg_entry` header-parsing clone
was read but not chased for drift. No fuzzing or differential testing was
done; every claim below is a source read, not a runtime observation, except
where a specific test name is cited as already covering the behavior.

---

## 1. Method and exact commands

```bash
cd /Users/netoneko/github.com/netoneko/akuma

# syscall-layer only, four thresholds
for t in 50 75 100 150; do
  pmd cpd --dir src/syscall --language rust --minimum-tokens $t --format text \
      > cpd_syscall_$t.txt || [ $? -eq 4 ]
done

# whole tree, for cross-file comparison and drift-since-2026-08-12 tracking
for t in 50 100; do
  pmd cpd --dir src --dir crates --language rust --minimum-tokens $t --format text \
      > cpd_whole_$t.txt || [ $? -eq 4 ]
done
```

Aggregated with the runbook's §3 union script (unmodified) against each
output file. `covered` = lines in at least one clone; `removable` = covered
minus one representative copy per group.

### Numbers

| scope | `--min-tokens` | blocks | covered | removable |
|---|---:|---:|---:|---:|
| `src/syscall` only | 50 | 82 | 1,570 | 766 |
| `src/syscall` only | 75 | 31 | 927 | 450 |
| `src/syscall` only | 100 | 17 | 583 | 278 |
| `src/syscall` only | 150 | 5 | 254 | 119 |
| whole tree (`src`+`crates`) | 50 | 336 | 5,967 | 2,964 |
| whole tree (`src`+`crates`) | 100 | 36 | 1,339 | 656 |

**Zero cross-file blocks** touch `src/syscall/*` at either 50 or 100 tokens in
the whole-tree run — CPD sees the syscall layer's duplication as entirely
intra-file (matches the 2026-08-12 doc's finding that duplication in this tree
is overwhelmingly copy-paste-within-a-file, not modules drifting apart). That
also means: **the fd-refcount clone in §2 below, which crosses from
`src/syscall/fs.rs` into `crates/akuma-exec/src/process/fd.rs`, is invisible
to CPD at every threshold tried** — it is a pure Type-2 finding (different
match arms present, different surrounding code), which is exactly the class
`--ignore-identifiers` cannot help with since it is a no-op for Rust (runbook
§4).

### What changed since the 2026-08-12 whole-tree baseline

`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §2/§7 ran the same whole-tree command
on 2026-08-12 and got blocks=461/covered=8,426 at 50 tokens and
blocks=92/covered=3,485 at 100 tokens. Today: blocks=336/covered=5,967 (50
tokens) and blocks=36/covered=1,339 (100 tokens) — a real drop, consistent
with that document's Phases 0–14 (all marked DONE) having actually landed,
not just been planned. Per-file, the same document's §2 table listed
`src/syscall/fs.rs` at 434 duplicated lines and `src/syscall/net.rs` at 165
(both whole-tree, 100 tokens, before any of this document's syscall-specific
work). Today, scoped to `src/syscall` alone at 100 tokens: `fs.rs` covers 345
lines, `net.rs` covers 130 — both down, most plausibly from the errno-table
consolidation (§5.7 there, DONE 2026-08-14: `fs.rs`'s local `EROFS`
pre-negated const and `mod.rs`'s bin-crate errno table are gone, replaced by
`akuma_primitives::errno` re-exports at `src/syscall/mod.rs:504-506`) removing
some of the token-identical match-arm runs CPD had been counting. **This
document does not have a syscall-scoped run from 2026-08-12 to diff against
directly** — the "434"/"165" figures above are whole-tree numbers being
compared to a syscall-scoped rerun, which is the best available comparison
but not a controlled A/B. Treat the direction (down) as solid and the exact
delta as approximate.

---

## 2. Finding: `dup`/`dup3`/`fcntl(F_DUPFD*)` silently under-refcount `EventFd` and `RumpSocket` fds — three copies, one bug, already hit once via a different path

**Sites (all `src/syscall/fs.rs`, all copies of the same match):**

| Copy | Function | Match at |
|---|---|---|
| 1 | `sys_dup` | `fs.rs:1519-1530` |
| 2 | `sys_dup3` | `fs.rs:1555-1566` |
| 3 | `sys_fcntl`, `F_DUPFD`/`F_DUPFD_CLOEXEC` arm | `fs.rs:2534-2545` |

All three are near-byte-identical:

```rust
match &entry {
    akuma_exec::process::FileDescriptor::PipeWrite(id) => super::pipe::pipe_clone_ref(*id, true),
    akuma_exec::process::FileDescriptor::PipeRead(id) => super::pipe::pipe_clone_ref(*id, false),
    akuma_exec::process::FileDescriptor::UnixSocket { rx, tx, sock } => {
        super::pipe::pipe_clone_ref(*rx, false);
        super::pipe::pipe_clone_ref(*tx, true);
        super::unixsock::unix_sock_clone_ref(*sock);
    }
    #[cfg(feature = "smoltcp")]
    akuma_exec::process::FileDescriptor::Socket(idx) => socket::socket_clone_ref(*idx),
    _ => {}
}
```

**The canonical fourth copy**, and the one that is actually kept current, is
`Process::clone_deep_for_fork` at `crates/akuma-exec/src/process/fd.rs:51-91`
— run on every `fork()`. It matches **six** descriptor kinds, not four:
`PipeWrite`, `PipeRead`, `UnixSocket`, `EventFd` (`fd.rs:71`), `Socket`
(`fd.rs:75`), and `RumpSocket` (`fd.rs:86-88`).

**What diverged, and why it matters.** `EventFd` and `RumpSocket` both fall
into the `_ => {}` arm in all three `src/syscall/fs.rs` copies — no refcount
bump. Both kinds *are* refcounted:

- `eventfd_clone_ref` (`src/syscall/eventfd.rs:97-105`) increments
  `KernelEventFd::ref_count`; `eventfd_close` (`:109-120`) decrements and
  removes the entry only at `ref_count == 0`. Created with `ref_count: 1`
  (`eventfd.rs:20-31`). **`eventfd_clone_ref`'s only caller in the whole tree
  is `fd.rs:71`** (confirmed by `grep -rn eventfd_clone_ref` across `src/` and
  `crates/`) — `sys_dup`/`sys_dup3`/`sys_fcntl` never call it.
- `RumpSocket { rump_fd, box_id, .. }` is refcounted through
  `rump_socket_clone_ref` (bump, called only at `fd.rs:86-88`) and
  `rump_fd_ref_drop` (drop, called from `proxy_close` at
  `src/rump_proxy.rs:603`, gated explicitly: *"iff this was the last
  descriptor referring to it"*, `rump_proxy.rs:587-596`). `dup`/`dup2`/`dup3`/
  `fcntl` are **not intercepted by the rump proxy at all**:
  `intercept_box_syscall` (`rump_proxy.rs:327`) only short-circuits for the
  syscall numbers `op_from_linux_sysno` maps
  (`crates/akuma-rump/src/syscall_translation.rs:48-72` —
  socket/bind/listen/accept/connect/getsockname/getpeername/sendto/recvfrom/
  sendmsg/recvmsg/setsockopt/getsockopt/shutdown/read/write/readv/writev/close),
  which does not include `dup` (23), `dup3` (24) or `fcntl` (25).
  `handle_syscall` (`src/syscall/mod.rs:684-686`) falls through to native
  dispatch whenever `intercept_box_syscall` returns `None`, so a
  `dup()`/`dup3()`/`fcntl(F_DUPFD*)` on a `RumpSocket` fd goes straight
  through `sys_dup`/`sys_dup3`/`sys_fcntl` in `fs.rs`, which is exactly the
  three-copy match above.

**Concrete failure.** `int b = dup(a)` (or `dup3`, or
`fcntl(a, F_DUPFD_CLOEXEC, 0)`) on an fd backed by `EventFd` or `RumpSocket`:
the duplicate is inserted into the fd table via `alloc_fd`/`alloc_fd_from`,
but the refcount the close path trusts is never bumped. `close(a)` then runs
as if `b` did not exist — `eventfd_close`/`rump_fd_ref_drop` sees the
refcount hit zero on the *first* close and tears the object down (removes the
`EVENTFDS` entry; for the rump case, sends a real NetBSD `close(rump_fd)` to
`rump_server`). `b` is left pointing at a dead id: any further
`read`/`write`/`poll` on it either hits eventfd's `Err(EBADF)` fallback
(`eventfd.rs:57`) or talks to a socket the server has already torn down.

**This is not hypothetical — it is the same bug, already found and fixed
once, via a different call path.** `fd.rs:76-85`'s comment on the
`RumpSocket` arm: *"Rump sockets need the same reference the native ones
take... sshd's process-per-session pattern is exactly the shape that breaks
on that: the parent `drop`s its copy of the accepted socket right after
`fork`, expecting the refcount to keep the child's alive, and instead
`proxy_close` sent a real NetBSD `close(rump_fd)`... destroyed the socket the
child was about to speak SSH over. Every session died at kex on the rump
devbox"* — full writeup at
[`RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`](RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md).
That fix landed in `clone_deep_for_fork` only. It never propagated to the
`src/syscall/fs.rs` copies, because they are a separate, independently
maintained clone of the same match, and CPD cannot see the relationship (§1).

**Verdict: live bug, not benign**, for both descriptor kinds, reachable by
ordinary `dup`/`dup2`/`dup3`/`fcntl(F_DUPFD, F_DUPFD_CLOEXEC)` — no rump-devbox
build even required for the `EventFd` half. Severity is bounded by how often
real programs `dup()` an eventfd or a rump socket rather than just closing it
directly; `dup2()`-onto-stdio (common in fork+exec shells and process
spawners) and `fcntl(F_DUPFD_CLOEXEC)` (used internally by some fd-cloning
library code) are both plausible triggers. Not verified against a live
running program — see "What was not checked."

**What it would take to collapse.** The four copies (three in `fs.rs`, one in
`fd.rs`) all walk the same six-variant `FileDescriptor` match to bump a
refcount; a single `fn clone_ref_for_fd(entry: &FileDescriptor)` in
`akuma-exec` (next to `clone_deep_for_fork`, which would call it too) removes
the divergence at the source and fixes the bug as a side effect of
deduplicating. `sys_dup`/`sys_dup3`/`sys_fcntl` would each shrink by ~10
lines. No compiler-checked way to *prove* the merge closes the gap other than
adding a host test that dups an `EventFd` and a `RumpSocket`-shaped fd (the
latter needs a `RumpSocket` test double, since real ones only exist behind
`RUMP_NIC=1`) and asserts the refcount before/after — worth doing regardless
of whether the merge happens first.

---

## 3. Finding: the `poll.rs` wait-loop triplet — confirmed, with one addition

The assignment's worked example is accurate. Read in full at their cited
ranges (`sys_epoll_pwait` 702-951, `sys_pselect6` 978-1097, `sys_ppoll`
1192-1295):

| | `sys_epoll_pwait` | `sys_pselect6` | `sys_ppoll` |
|---|---|---|---|
| waker passed to readiness check | `Some(&waker)` (`:816`) | **`None`** (`:1052`) | `Some(&waker)` (`:1258`) |
| `should_interrupt_blocking_syscall()` | yes (`:915`) | **no** | yes (`:1287`, comment explains it was added after ppoll hung on `alarm()+pause()`) |
| deadline computed via | `epoll_wait_deadline()` (`:933`, the tested helper) | inline `abs_deadline = ...` (`:1093-1094`) | inline `abs_deadline = ...` (`:1291`) |

**The `pselect6` gap is a live bug, not a cosmetic asymmetry.**
`should_interrupt_blocking_syscall()`
(`crates/akuma-exec/src/process/children.rs:345-351`) is the single function
that combines both interrupt sources this kernel has: the process-wide
Ctrl-C/`sys_kill` flag (`is_current_interrupted`, set by
`interrupt_thread`/`ProcessChannel::set_interrupted` — *"Used by the SSH
shell to send Ctrl+C"*, `children.rs:262-269`) and the per-thread
`pthread_kill` path. `sys_pselect6`'s loop (`poll.rs:1018-1096`) never calls
it. A process blocked in `select()`/`pselect6()` — including with an
**infinite timeout** (`timeout_ptr == 0`, `poll.rs:1001`) — cannot be
interrupted by SSH Ctrl-C or `kill()`; it only ever returns via readiness or
its own timeout expiring. Since `ppoll`'s sibling copy carries an explicit
comment saying this exact class of bug was found and fixed there
(`poll.rs:1281-1286`: *"Without this, a pending signal... just wakes
`schedule_blocking` below, finds nothing ready, and goes right back to
sleep... This is the same check `sys_epoll_pwait` makes above; `ppoll` was
missing it"*), the fix is documented in the file and simply was never applied
to the third copy sitting a few hundred lines above it.

**Reachability.** Any program that calls `select()`/`pselect()` with a long
or infinite timeout (musl's `select()` is implemented via `pselect6`) and is
then sent SIGINT via an SSH session's Ctrl-C, or `kill -9`'d from another
session, will not respond until the `select` call's own timeout — for
infinite timeout, never (short of a hard process teardown that bypasses this
loop entirely, which was not verified here).

**Collapsing it.** Same shape as the assignment suggested: the three loops
already agree on almost everything else (poll the network stack once per
iteration under the same BKL carve-out comment, snapshot readiness, check
ready-count, check timeout, check interrupt, park). A shared
`blocking_poll_loop` taking a per-iteration readiness closure and an
`Option<&Waker>` would make `pselect6`'s two omissions structurally
impossible to reintroduce, rather than relying on the next reader noticing
the sibling's comment. `epoll_wait_deadline()` (`poll.rs:155`) is already
`pub` and unit-tested-shaped for this; `pselect6`/`ppoll`'s inline deadline
math should call it instead of restating it a third and fourth time.

---

## 4. Finding: `fs.rs`'s dirfd-relative path resolution — seven copies, four behaviors

**The shared helper**, `resolve_path_at(dirfd, raw_path) -> String`
(`fs.rs:158-186`), is used by five syscalls:
`sys_renameat`/`sys_renameat2`/`sys_symlinkat`/`sys_linkat`/`sys_readlinkat`
(`fs.rs:2726-2814`). Its failure behavior: **no current process, or `dirfd`
not `AT_FDCWD`/not a valid `File` fd → silently resolve relative to `"/"`.**
It never returns an error.

**Six syscalls hand-roll their own copy of the same logic instead of calling
it**, and disagree with each other and with the helper on what a bad `dirfd`
means:

| Syscall | Site | no-current-process (`dirfd==-100`) | no-current-process (`dirfd>=0`) | fd exists but isn't `File` | `dirfd` negative, not `-100` |
|---|---|---|---|---|---|
| `resolve_path_at` (helper) | `:158` | `"/"` | `"/"` | `"/"` | `"/"` |
| `sys_openat` | `:1639-1663` | `"/"` | **`EBADF`** | **`EBADF`** | `"/"` |
| `sys_newfstatat` | `:2095-2118` | **`ESRCH`** | **`ESRCH`** | **`EBADF`** | **`EBADF`** |
| `sys_statx` | `:2350-2373` (+`:2337-2349` for `AT_EMPTY_PATH`) | **`EBADF`** | **`EBADF`** | **`EBADF`** | **`EINVAL`** |
| `sys_fchmodat` | `:2221-2239` | **`EBADF`** | **`EBADF`** | **`EBADF`** | **`EBADF`** |
| `sys_faccessat2` | `:2452-2470` | **`ESRCH`** | **`ESRCH`** | **`EBADF`** | **`EBADF`** |
| `sys_mkdirat` | `:2595-2613` | **`EBADF`** | **`EBADF`** | **`EBADF`** | **`EBADF`** |
| `sys_unlinkat` | `:2649-2667` | **`EBADF`** | **`EBADF`** | **`EBADF`** | **`EBADF`** |

Four distinct behaviors on the same four failure conditions, spread across
eight syscalls (plus the helper) in one file: silent `"/"` fallback
(`resolve_path_at`, and half of `sys_openat`), `ESRCH`
(`newfstatat`/`faccessat2`'s "no process" branches), `EBADF`
(`fchmodat`/`mkdirat`/`unlinkat`'s everywhere, and `statx`/`openat`'s bad-fd
branches), and `EINVAL` (`statx`'s negative-`dirfd` branch alone).

**The comment on `sys_unlinkat` asserts a consistency that does not exist.**
`fs.rs:2642-2644`: *"Build the dirfd-relative base path... matches
`sys_statx`/`sys_newfstatat`: the early-return `EBADF` paths must not pay for
a BKL drop/reacquire..."* — but `sys_newfstatat` returns `ESRCH`, not `EBADF`,
for exactly the case this comment is about (no current process). The author
believed three functions agreed; two of the three do not.

**The `"/"`-fallback path is the concrete bug**, and it's on the shared
helper feeding five syscalls, not on a dead one-off. `resolve_path_at`
never errors on a bad `dirfd` — it treats it as `AT_FDCWD`-at-root.
`renameat(bad_fd, "old", AT_FDCWD, "new")` with `bad_fd` closed, or pointing
at a socket/pipe rather than a directory, does not return `EBADF` the way
real Linux (and the way seven of this file's other eight `*at` syscalls) does
— it silently resolves `"old"` against `/` and may rename `/old`. This is
reachable by the ordinary caller mistake that motivates the `EBADF` check in
the first place: a stale, closed, or wrong-kind `dirfd` argument. Not proven
to be hit by any real userspace program in this survey — see below.

**Collapsing it.** This is the same shape as the `X`/`X_from_path` finding in
`TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §3: one design decision, copy-pasted
seven times as the syscall list grew, each copy answering "what does a bad
dirfd mean" for itself. Fixing it is a genuine behavior decision, not a pure
refactor — someone has to choose which of `EBADF`/`ESRCH`/`EINVAL`/`"/"` is
correct for each condition (Linux's answer: `EBADF` for a `dirfd` that is
neither `AT_FDCWD` nor a valid open fd, in all cases; the "no current
process" branch is arguably unreachable in a syscall handler and its
behavior may not matter, but if it can happen, it should not depend on which
`*at` syscall was called). Once decided, one `resolve_dirfd_relative(dirfd,
raw_path) -> Result<String, u64>` replacing all eight copies removes the
inconsistency at the type level.

---

## 5. What CPD found and was not chased to a verdict

For completeness — these appeared in the CPD output but were not read closely
enough to call "drifted" or "benign" with confidence:

| Family | Sites | What CPD shows | Status |
|---|---|---|---|
| `sendmsg`/`recvmsg`/`unix_recvmsg_entry` msghdr+iovec parsing preamble | `net.rs:1128`, `:1204`, `:1358` | 16-22 lines identical (msghdr copy-in, iovec size/validate/copy-in) | Read once; prologues match. Not checked for validation-order drift the way `poll.rs` was. |
| `sys_pread64`/`sys_pwrite64` offset+ptr validation | `fs.rs:811`, `:863` | 10 lines identical | Not read past the CPD excerpt. |
| `msgqueue.rs` internal clones | `:51`↔`:67`, `:169`↔`:208`, `:189`↔`:281`, `:296`↔`:307` | 4 blocks, 58 covered lines at 50 tokens | Not read at all this pass — msgqueue.rs touches message lifetime/ownership, which the runbook's 50-token rule flags as worth the dangerous-code threshold; **flagged for follow-up, not investigated here**. |
| `pipe_close_write`/`pipe_close_read` | `pipe.rs:255-296`, `:298-`  | Mirror-image bodies (write_count vs read_count), symmetric wake logic | Read in full. **No drift found** — both carry cross-referencing comments and the asymmetry that does exist (which wakes which class of waiter) is deliberate and tied to a named regression test (`test_pipe_close_read_wakes_blocked_writer`, referenced in-comment for the SIGPIPE fix). Low-priority style item only. |
| `writeback_shared_pages` call sites | `mem.rs:261-269` (`flush_and_clear_shared_file_mappings`), `:293-301` (`sys_msync`) | 9 lines identical (resolve region → writeback) | Read in full. Both correct, no drift; trivial `for (base,path,foff,mlen) in &entries { ... }` extraction would remove it, low value. |
| `sys_accept`/`sys_accept4` | `net.rs:263`, `:289` | 19-22 lines identical prologue | Read in full. `accept4` is a clean superset of `accept` (adds `SOCK_CLOEXEC`/`SOCK_NONBLOCK` flag handling after the shared body) — **not drift, the correct pattern**, low priority. |
| `sys_bind`/`sys_connect` addr-copy-in preamble | `net.rs:220`, `:320` | 16 lines identical (`SockAddrIn` copy-in) | Read in full. No drift, both correct. Low priority. |
| `wait4`'s reap-block repeated 3×  | `proc.rs:939-970`, `:991-1006`, `:1014-1029` | 18 lines pairwise identical | Read in full. **Deliberate** check-register-check idiom to avoid a missed-wakeup race (same pattern as `poll.rs`'s epoll snapshot-then-recheck); all three copies agree. Style/refactor candidate only, not a bug. |
| `sys_close`'s fd-cleanup match vs. `sys_dup`/`sys_dup3`/`fcntl(F_DUPFD*)`'s clone_ref match | `fs.rs:1888-1924` vs. §2's three sites | Different operation (teardown-on-close vs. bump-on-duplicate), so not the same clone family, but worth noting: `sys_close`'s match *does* handle `EventFd` (`:1904-1907`, calls `eventfd_close`) and does not need to handle `RumpSocket` (intercepted upstream by `rump_proxy::proxy_close` before reaching this dispatch — confirmed via `grep -n "Op::Close"` in `rump_proxy.rs`). So the close side is fine; only the duplicate side (§2) is missing arms. | Resolved as part of §2's investigation — noted here for anyone re-deriving the match-arm inventory. |

---

## 6. What was not checked

- **Not every file's wait/park loop was independently re-derived.**
  `unixsock.rs`, `sync.rs` (futex/mutex waits), `aio.rs`, `timerfd.rs` were
  not read function-by-function for the `poll.rs`-style waker/interrupt/
  deadline triplet. Given `poll.rs`'s pattern held for 3 of 3 functions
  checked, and `sync.rs`/`unixsock.rs` are plausible homes for the same
  blocking-loop shape, this is the highest-value follow-up if someone
  continues this audit.
- **No fuzzing, differential testing, or QEMU verification was done for any
  finding above.** All three are source reads. The `dup`/`fcntl` refcount gap
  in particular would be cheap to confirm in-VM: `eventfd(0,0)`, `dup()`,
  `close()` the original, then `read()`/`write()` the dup and check for
  `EBADF`/a hung/wrong-behaving fd; that was not run.
- **The `RumpSocket` half of §2 needs a `RUMP_NIC=1` rump-devbox build to
  reproduce live**; not attempted here (no VM was booted for this audit at
  all).
- **The 100-token whole-tree comparison to the 2026-08-12 baseline (§1) is
  not a controlled A/B** — no `git worktree` at the old commit was checked
  out; the "434"/"165" figures being compared are from that document's
  whole-tree run, not a syscall-scoped one, since no syscall-scoped run from
  that date exists to diff against.
- **`crates/akuma-net`'s socket layer**, which several `net.rs` syscalls call
  into (`socket::socket_accept`, `socket::socket_bind`, etc.), was not
  audited — this document's cross-file check only looked for clones where
  CPD or a targeted `grep` turned something up, not an exhaustive read of the
  crate.
- Every `#[repr(C)]` struct, syscall dispatch `match`, and errno constant
  table CPD flagged was excluded by inspection, per the assignment's
  instruction — not enumerated here since none produced a finding worth
  reporting.

---

## 7. Work list, by value

| # | Item | Type | Effort | Confidence it's a real bug |
|---|---|---|---|---|
| 1 | `dup`/`dup3`/`fcntl(F_DUPFD*)` missing `EventFd`/`RumpSocket` refcount arms (§2) | **Bug** | Small — one shared `clone_ref_for_fd` fn in `akuma-exec`, 3 call-site swaps in `fs.rs` | High. Structurally identical to a bug already hit and fixed once (fork path, `RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`). Not runtime-verified this pass. |
| 2 | `sys_pselect6` missing `should_interrupt_blocking_syscall()` (§3) | **Bug** | Trivial — one call, matching `ppoll`'s existing fix | High. Sibling function's comment documents the exact bug class; the fix pattern already exists two functions away. |
| 3 | `sys_pselect6` passing `None` instead of `Some(&waker)` (§3) | Bug (latency, not correctness) | Trivial | Medium — makes `pselect6` fall back to poll-interval-cadence wakeup instead of immediate, same class of latency bug `ppoll`'s own history describes fixing (`poll.rs:1225-1232`'s comment on why `ppoll` needed the waker) |
| 4 | `fs.rs` dirfd-relative-path family: decide one error-handling contract, replace 8 copies (§4) | Bug (the `resolve_path_at` `"/"`-fallback half) + cleanup (unifying the other 7) | Medium — behavior decision required before merging, not a pure refactor | Medium-high on the `"/"`-fallback specifically; the `ESRCH`-vs-`EBADF`-vs-`EINVAL` inconsistency for the "no current process" case is real but likely low-frequency (would need a race with process teardown mid-syscall) |
| 5 | Unify the three `poll.rs` wait loops behind one `blocking_poll_loop` + shared `epoll_wait_deadline()` calls (§3) | Cleanup (once 2-3 are fixed) | Medium | — (style/robustness, not a new bug once the two fixes above land) |
| 6 | `msgqueue.rs`'s 4 unread clone blocks (§5) | Unknown | — | Not assessed — flagged, not investigated |
| 7 | `sendmsg`/`recvmsg`/`unix_recvmsg_entry` shared msghdr-parsing preamble (§5) | Style | Small | Low — read once, no drift found in the shared portion, but not chased into the divergent tails |
| 8 | `pipe_close_write`/`pipe_close_read`, `wait4`'s triple reap-block, `writeback_shared_pages` call sites, `accept`/`accept4`, `bind`/`connect` (§5) | Style only | Small each | None — all read in full, all confirmed non-drifted |

Items 1-3 are the ones worth fixing on their own merits regardless of any
refactor. Item 4 needs a decision before it can be merged safely. Items 5-8
are line-count/maintainability cleanup with no bug behind them as far as this
audit went.

---

## Background

- [`../runbooks/find-duplicated-code.md`](../runbooks/find-duplicated-code.md)
  — the CPD method, thresholds, and known traps this audit followed.
- [`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
  — the 2026-08-12 whole-tree survey and its since-completed work list (Phases
  0-14, all DONE as of 2026-08-14); §5.7 (errno consolidation) and §6 (the
  `channel.rs` FIFO drift, same class of finding as this document's §2-§4)
  are the closest precedents.
- [`RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`](RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md)
  — the first, already-fixed instance of §2's bug, via the `fork()` path
  instead of `dup()`/`fcntl()`.
- [`NCA_MISSING_SYSCALLS.md`](NCA_MISSING_SYSCALLS.md) — referenced by the
  `sys_close` cleanup-match comment (`fs.rs:1871-1885`) for an unrelated but
  adjacent fd-lifetime race (`clear_nonblock` ordering).
- [`BKL_VFS_CARVE_OUT.md`](BKL_VFS_CARVE_OUT.md) — referenced by several of
  the `fs.rs` `*at` syscalls' comments (§4) for why the dirfd resolution runs
  before the VFS BKL guard is taken; not itself a duplication document, but
  the reason all seven copies in §4 exist with the same structural shape
  (resolve fd-table info first, VFS-BKL-free, then take the guard).

**Proposed `docs/README.md` triage-matrix row** (for review, not applied):

| Symptom | Read |
|---|---|
| `dup()`/`fcntl(F_DUPFD)` on an eventfd or rump socket behaves wrong after a `close()` of the other copy; a `select()`/`pselect6()`-blocked process won't respond to Ctrl-C/`kill` | [`archive/SYSCALL_LAYER_AUDIT.md`](archive/SYSCALL_LAYER_AUDIT.md) |
