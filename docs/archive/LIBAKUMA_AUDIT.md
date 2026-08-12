# libakuma Family Audit

**Date:** 2026-08-12
**Scope:** `userspace/libakuma/` and `userspace/libakuma-tls/` — the shared
userspace runtime + TLS/HTTP client used by every native Akuma binary.
**Method:** read every `.rs` in both crates, both `Cargo.toml`s, all 8
co-located `docs/`, and the consumer call sites (22 `Cargo.toml` dependents,
~16 actual binaries). Cross-checked suspicious sites against the kernel side
and against `docs/archive/` history.

This is a *current-state* audit. It is not a changelog; the historical fix
notes under `userspace/libakuma/docs/` and `userspace/libakuma-tls/docs/` are
linked from the relevant findings and not reproduced.

> **Status (2026-08-12):** items **1-13 and 15** of the §5 prioritized list
> are done. Items 1-4 landed in commit `9cd7a24` ("half baked libakuma
> fixes"); items 5-13 and 15 landed later the same day in `cf03840`
> ("multiple libakuma fixes"). A follow-up, uncommitted as of this writing,
> replaced part of item 11's `paws` fix after QEMU verification caught it
> not actually working — see the `cf03840` fix log entry's tail and the
> addendum right after it. Only item **14** (extract pure-logic cores + host
> tests) is still open. Everything below is the audit as written; what
> landed, what landed differently, and what is still open is recorded in
> **§6 Fix log**, and the affected findings carry an inline *Fixed* note.

## TL;DR

| Crate | LOC | Tests | Consumers | Health |
|---|---|---|---|---|
| `libakuma` | 2,933 (lib 2,468 + net 406 + fs 59) | **0** | 18 binaries | Functional but accumulated. 1 confirmed bug (`fstatat`), several footguns, one stale ABI struct, one unused feature. |
| `libakuma-tls` | 1,604 (http 1,249 + lib 156 + transport 134 + rng 66) | **0** | 3 binaries (`box`, `meow`, `scratch`) | Functional, **security-limited** (verify always disabled), heavy duplication, no chunked-encoding support. |

Top three things to fix, in order:

1. **`libakuma::fstatat` is missing null-termination** (`lib.rs:977`). The only
   caller (`box/src/images.rs:21,26`) silently compensates by pre-terminating.
   Any other caller will read past the `&str` into unrelated memory. §2.1.
2. **`ProcessInfo` doc comment lies about the struct layout** (`lib.rs:177-203`)
   and two derived constants (`ARGV_DATA_SIZE`, `CWD_DATA_SIZE`) are dead. The
   "must match kernel exactly" warning is contradicted by the struct itself. §2.4.
3. **`libakuma-tls` has no certificate verification path and never did.**
   `TlsOptions::insecure`, the `_insecure` parameter on `https_fetch`, and the
   "Phase 2" comments are 7-month-old dead code. Every HTTPS fetch is MITM-able.
   §3.2.

---

## 1. Family Overview

```
userspace/libakuma/                  userspace/libakuma-tls/
├── Cargo.toml                       ├── Cargo.toml        (dep: libakuma)
│   features:                        │
│   - chunked-allocator (default)    ├── lib.rs   TlsStream, Error, TlsOptions (dead)
│   - net-async (sshd only)          ├── rng.rs   TlsRng (GETRANDOM → rand_core)
│   - linux-abi (host tests)         ├── transport.rs  TcpTransport (embedded-io adapter)
│   deps: talc 4, embedded-io-async  ├── http.rs  1249 LOC, the real surface
│                                    └── deps: embedded-tls 0.17, embedded-io 0.6,
└── src/                                rand_core 0.6
    ├── lib.rs  2468 LOC  syscall shim + globals
    ├── net.rs   406 LOC  TcpListener / TcpStream
    └── fs.rs     59 LOC  read / write / exists
```

`libakuma` owns the process: `_start`, `#[global_allocator]`,
`#[alloc_error_handler]`, `#[panic_handler]`, syscall numbers, `Spinlock`,
argv/envp. This is the libc layer. `libakuma-tls` is a pure client built on
the networking subset of libakuma.

**18 binaries link `libakuma`** (grep over `userspace/*/Cargo.toml`); 3 also
link `libakuma-tls`. Because libakuma registers `#[global_allocator]` /
`#[panic_handler]`, **no consumer can define its own**, and host-testing a
consumer crate requires `--no-default-features` plus a `--lib` split — exactly
the dance the root `CLAUDE.md` documents for `sshd` and `box`.

---

## 2. `libakuma` — per-file findings

### 2.1 Correctness bugs

#### BUG-1 — `fstatat` does not null-terminate its path (`lib.rs:977-993`)

```rust
pub fn fstatat(dirfd: i32, path: &str, flags: u32) -> Result<Stat, i32> {
    let mut stat = Stat::default();
    let ret = syscall(
        syscall::NEWFSTATAT,
        dirfd as u64,
        path.as_ptr() as u64,   // <-- raw &str ptr, no NUL
        ...
```

Every other path-taking wrapper in this crate (`open`, `chdir`, `mkdir`,
`unlink`, `rename`, `access`, `accessat`, `mount`, `symlink`, `chmod`,
`rmdir`, `mount_in_ns`, `umount`) does `alloc::format!("{}\0", path)`.
`fstatat` is the lone exception. The kernel's `copy_from_user_str` walks
until `\0`, so a non-terminated `&str` reads into adjacent memory until it
finds one — usually harmless, occasionally EFAULT, rarely a different file.

The sole caller knows this and pre-terminates by hand:

```rust
// userspace/box/src/images.rs:19-21
pub fn path_exists(path: &str) -> bool {
    let path_c = format!("{}\0", path);          // caller-side band-aid
    libakuma::fstatat(-100, &path_c, 0).is_ok()
}
```

**Severity:** medium. Latent today (one caller), but the signature advertises
a `&str` like every other wrapper, so the next caller will get it wrong.
Fix is one line: `let path_c = alloc::format!("{}\0", path);` and pass
`path_c.as_ptr()`. Drop the caller-side termination in `images.rs` afterwards.

> **Fixed** in `9cd7a24` exactly as described: `fstatat` terminates its own
> path, and `path_exists`/`dir_exists` in `box/src/images.rs` pass the `&str`
> straight through. §6.

#### BUG-2 — `accept` discards the peer address (`net.rs:150-175`, `186-200`)

`accept(2)` is documented to fill the caller's `sockaddr`; libakuma allocates
one, passes it, and throws it away, returning the hardcoded
`SocketAddrV4::new([0,0,0,0], 0)`. `TcpStream::peer_addr()` on an accepted
stream therefore returns `0.0.0.0:0`. The TODO at `net.rs:155` has been there
since the file was written.

Callers that log or filter by peer (sshd per-source limits, httpd access
logs) get garbage. sshd currently does not filter by peer, so this is silent;
the moment it tries to, it will be wrong.

**Severity:** low-medium. Fix: parse `sockaddr` with the existing
`SockAddrIn::to_addr()` and store it on the `TcpStream`.

#### BUG-3 — `ProcessInfo` struct contradicts its own doc comment (`lib.rs:177-203`)

The doc comment describes a 1024-byte struct with fields `pid, ppid, argc,
argv_len, cwd_len, _reserved, cwd_data[256], argv_data[744]` and warns
"Must match kernel's ProcessInfo struct exactly!" The actual struct is:

```rust
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub box_id: u64,
    pub _reserved: [u8; 1008],
}
```

No `argc`, no `argv_len`, no `cwd_data`, no `argv_data`. The two constants
`ARGV_DATA_SIZE = 744` and `CWD_DATA_SIZE = 256` (`lib.rs:154,157`) are
defined and **never read by anything** — they describe a layout this crate
no longer uses. `getcwd` (`lib.rs:235`) gets cwd from a syscall, not from
this struct; argv is read straight off `INITIAL_SP`.

Whose ABI is it? Either:

- the kernel still lays out the page the documented way and the struct is
  wrong (so `box_id` reads bytes 8-15, which the doc says are `argc`/`argv_len`
  — meaning `geteuid`-style consumers of `box_id` are reading garbage), **or**
- the struct is right and the doc + two constants are stale.

This needs a 5-minute check against `src/` (kernel `ProcessInfo`) before
someone trusts either side. **Severity: potentially high** if the struct is
the side that's wrong; **cosmetic** otherwise.

> **Fixed** in `9cd7a24`, and the question resolved the benign way: the
> *struct* was right on both sides (`crates/akuma-exec/src/process/types.rs`
> declares the same `pid`/`ppid`/`box_id`/`_reserved` layout and asserts
> `size_of::<ProcessInfo>() == 1024` at compile time), so `box_id` was never
> reading garbage — the doc comment and the two constants were the stale side.
> Both `ARGV_DATA_SIZE`/`CWD_DATA_SIZE` pairs are deleted and both doc
> comments now say where argv (entry stack) and cwd (`GETCWD`) actually come
> from. §6.

#### BUG-4 — `getcwd` uses `from_utf8_unchecked` on kernel output (`lib.rs:248`)

```rust
core::str::from_utf8_unchecked(&CWD_BUF[..result as usize - 1])
```

The kernel is trusted to write UTF-8 today, but this is exactly the kind of
invariant that silently breaks under a future path-encoding change. There is
no upside to `_unchecked` here — the fallible version returns a `&str` of the
same lifetime. Also `result as usize - 1` underflows if `result == 0`, though
that path is guarded by `if result > 0`.

**Severity:** low. Defensive fix only.

#### BUG-5 — `read`/`write` take `u64` fd, everything else takes `i32` (`lib.rs:504,520`)

```rust
pub fn read(fd: u64, buf: &mut [u8]) -> isize { ... }
pub fn write(fd: u64, buf: &[u8]) -> isize { ... }
```

vs. `read_fd(fd: i32, …)`, `write_fd(fd: i32, …)`, `close(fd: i32)`,
`fstat(fd: i32)`, `lseek(fd: i32, …)`, `recv(fd: i32, …)`, etc. The `fd`
module declares `STDIN/STDOUT/STDERR` as `u64`, which only fits the first two
functions. Every caller that holds a real fd (an `i32`) and wants to use
`write` must cast `fd as u64`; every caller that mixes `write` and `write_fd`
must remember which side of the struct the fd type lives on. This is pure
noise.

**Severity:** low (usability), but it has already produced one redundant API
pair (`read` vs `read_fd` exist for no other reason).

### 2.2 API design issues

- **`kill` is signal-0-only by name** (`lib.rs:1589`). The doc says so and
  points to `kill_signal` as the real thing. This has already bitten herd
  (`docs/archive/BUG_FIX_LIST.md:672`, `SIGNAL_EXIT_HANDLING`). Consider
  renaming to `kill_probe` and adding `kill(pid, sig)` matching POSIX, or at
  least `#[deprecated]` on the no-sig variant.
- **`waitpid` returns the high byte only** (`lib.rs:1787`). A child killed by
  a signal reports `exit_code() == 0`, indistinguishable from success. The
  doc cross-references `waitpid_status` / `wait_any` — the fix exists — but
  every consumer written before Aug 2026 still calls `waitpid`. The
  README symptom-matrix row at `docs/README.md:112` is the third time this
  has bitten someone. Either change `waitpid` to return `Option<WaitStatus>`
  (breaking) or `#[deprecated]` it.
- **`spawn` returns `Option<SpawnResult>`**, throwing away errno
  (`lib.rs:1423`). `spawn_with_stdin`, `spawn_with_env`, `spawn_pty` all do
  the same. A failed spawn is undiagnosable — caller can't tell ENOENT from
  ENOMEM. Should be `Result<SpawnResult, i32>`.
- **The `Output` trait + `Stdout` struct + `AkumaOutput` alias**
  (`lib.rs:1347-1405`) is 60 lines of abstraction over 4 one-line free
  functions (`print`/`println`/`eprint`/`eprintln`). No consumer in the tree
  implements `Output` for anything other than `Stdout`; the `AkumaOutput`
  alias is `#[deprecated]` since 0.2.0. This looks like speculative API
  surface. If nothing overrides it, delete it; if something will, mark it
  experimental.
- **`print_allocator_info` / `print_hex` / `print_dec`** (`lib.rs:2349-2412`)
  are stack-based integer printers that exist purely because the OOM handler
  cannot allocate. They are `pub`, so they are part of the API, but their
  only caller is `alloc_error`. They invite allocation-unfriendly code to
  call them for diagnostics that could just use `format!`. Keep them
  `pub(crate)`.
- **`fb_init` / `fb_draw` / `fb_info`** (`lib.rs:2430-2461`) are framebuffer
  wrappers sitting in the libc layer. They have one consumer (`doom`). Unlike
  net/fs/process/terminal, they aren't a "everyone needs this" surface.
  Consider moving to a `libakuma-fb` (or just inline them in `doom`).
- **`pipe`** (`lib.rs:996`) is a thin wrapper that passes `flags = 0` and
  ignores `O_NONBLOCK`. The doc on it says "currently stubbed" (SYSCALLS.md:35)
  — verify against kernel; if real, expose the flags arg.

### 2.3 Safety / UB concerns

- **`static mut CWD_BUF` in `getcwd`** (`lib.rs:236`). `static mut` is UB
  under concurrent access (two threads calling `getcwd` race the buffer).
  libakuma is linked into multi-threaded processes (musl `pthread` users in
  the devbox). Replace with a `Spinlock<[u8; 256]>` or pass the buffer in
  from the caller.
- **`Spinlock`** (`lib.rs:278`) uses `compare_exchange_weak` + `spin_loop`
  with no backoff and no fairness. Under contention this is fine for short
  critical sections (the talc allocator lock is the main user), but it is
  **not a fair lock** — a waiter can starve indefinitely. Document that, or
  add a ticket counter.
- **`brk_alloc` is racy** (`lib.rs:2224-2239`). It does
  `load head; load end; compute; store head` non-atomically. Two threads can
  both read the same `head`, both allocate, and the second `store` overwrites
  the first — returning overlapping memory. **This path is dead while
  `USE_MMAP_ALLOCATOR = true`** (the default), but flipping that const (or
  someone reading the code and copying the pattern) reactivates a silent
  heap-corruption bug. Either delete the brk path or gate it behind a
  feature and mark it `unsafe`/single-thread.
- **`mmap_alloc` (non-chunked) returns page-aligned memory** (`lib.rs:2151`)
  and ignores `layout.align()` for `align > PAGE_SIZE`. Over-aligned types
  (e.g. `#[repr(align(4096))]`, rare but legal) get mis-aligned memory.
  Chunked allocator doesn't have this (talc honors align).
- **argv/envp iterators return `&'static str`** (`lib.rs:342,447`) but the
  data lives for the process, not `'static`. Fine in practice for a
  no-dlopen binary; technically wrong. Document or reborrow.

### 2.4 Allocator notes

`HybridAllocator` (`lib.rs:2027-2304`) is the most subtle code in the crate.
Current state:

- Default path (chunked + talc) is sound and the recommended one; the doc
  at `userspace/libakuma/docs/ALLOCATOR_OPTIONS.md` is accurate.
- `USE_MMAP_ALLOCATOR` is a `pub const bool = true` (`lib.rs:2025`). The
  `false` arm is the racy brk path above. **This switch should not exist as
  a source constant** — flipping it is UB. Make it a Cargo feature
  (`brk-allocator`, off by default, marked experimental) so nobody flips it
  by accident, or delete the brk arm outright.
- The `#[cfg(not(feature = "chunked-allocator"))]` mmap path leaks on
  dealloc: it only calls `munmap_void` and updates `FREED_BYTES`, but
  `dealloc` is gated on `USE_MMAP_ALLOCATOR` (`lib.rs:2251-2255`) — if
  `USE_MMAP_ALLOCATOR` is false, **dealloc is a no-op and every allocation
  leaks**. The brk allocator has no dealloc at all by design (bump
  pointer). Document this.
- The `#repr(C, align(256))` + `_padding` on `HybridAllocator` is a cache
  alignment trick with a comment-free magic number. Worth a one-line
  comment, or drop it and let the allocator sit on whatever line it lands.
- The `mmap_alloc` retry on `talc.malloc` failure claims a new chunk and
  retries once; if the retry also fails (e.g. layout larger than
  `CHUNK_SIZE` and the chunk was too small), it returns null. The sizing
  heuristic `layout.size() + 1024 > CHUNK_SIZE` over-allocates by 1024 for
  the talc metadata, but the actual overhead is layout-dependent. Probably
  fine; worth a comment.

### 2.5 Documentation

- `SYSCALLS.md:35-37` lists `pipe` as "currently stubbed" and `execve` as a
  wrapper; neither matches the code (no `execve` wrapper exists, `pipe`
  calls `PIPE2` directly). Refresh.
- `TERMINAL_SYSCALLS.md` is a **proposal** doc — "the following syscalls are
  *proposed*" — but the syscalls have shipped and have wrappers at
  `lib.rs:1796-1867`. Move to past tense or delete.
- `ALLOCATOR_MEMORY_FIX.md` describes a `DeferredFreeQueue` that no longer
  exists in the source. The current allocator has no such queue (the
  non-chunked path munmaps eagerly, the chunked path never unmaps chunks).
  Mark historical.
- `MKDIR_P_IMPROVEMENTS.md` is still accurate.
- `POLL_INPUT_EVENT_FIX.md` is still accurate.

### 2.6 Testing

**Zero `#[test]` functions in the crate.** The crate can't be host-tested
as-is because it registers `#[global_allocator]`/`#[panic_handler]`/
`#[alloc_error_handler]`. The `linux-abi` feature is a partial workaround
(it switches `getpid` off the low page) but the allocator and panic handler
still collide with std.

Recommendation: extract the pure logic into a `libakuma-core` sub-crate
(parse helpers, `SocketAddrV4`, `Stat`, `WaitStatus`, syscall number tables,
`Spinlock`, `format_ip`, `parse_url`-equivalents) that has no
`#[global_allocator]` and is `no_std + alloc` only. Host-test that. Keep
`libakuma` as the thin process-runtime wrapper that depends on
`libakuma-core`. This mirrors the existing `crates/` extraction pattern
(`akuma-exec`, `akuma-ext2`, etc.).

---

## 3. `libakuma-tls` — per-file findings

### 3.1 Correctness bugs

#### TLS-BUG-1 — `TlsOptions` and the `_insecure` parameter are dead (`lib.rs:60-79`, `http.rs:75`)

```rust
pub fn https_fetch(url: &str, _insecure: bool, max_size: Option<usize>) -> ...
```

The parameter is named `_insecure` and never consulted. `TlsOptions::insecure`
is a builder that sets a field nothing reads. `TlsStream::connect` always
uses `NoVerify`:

```rust
// lib.rs:117
tls_conn.open::<TlsRng, NoVerify>(context)?;
```

The "Phase 2 would add proper certificate verification" comment dates to
Feb 2026 (7 months). Three consumers (`box`, `meow`, `scratch`) all do HTTPS
downloads over this channel. Anyone on the path between the VM and the
registry/API endpoint can impersonate the server.

This is **the** finding for this crate. It is not a bug in the sense of
"code does the wrong thing" — the code does exactly what it says (NoVerify).
It is a finding that **the API presents the verify flag as if it worked**,
which is worse than not having the flag. Either:

1. Ship cert verification (the `embedded-tls` API supports it; needs a root
   store, which needs a bundled CA list — non-trivial in a 4 MB image), or
2. Delete `TlsOptions`, the `_insecure` parameter, and the `insecure()`
   builder. Rename `https_fetch`'s signature to drop the lie. Document that
   verification is absent and the channel must not be trusted over an
   untrusted network.

Until one of those happens, this should be a `// SECURITY:` comment at the
top of `lib.rs` and a row in `docs/reference/subsystems/ssh.md` (which covers
the trust boundary).

> **Fixed** in `9cd7a24` by taking option 2: `TlsOptions`, `insecure()` and the
> `_insecure` parameter are gone (`https_fetch(url, max_size)`), and the crate
> doc opens with a `# SECURITY: certificate verification is disabled` section
> pointing back here. Verification itself is still absent — the channel is
> still MITM-able — and the `docs/reference/subsystems/ssh.md` row was **not**
> added. §6.

#### TLS-BUG-2 — No chunked transfer-encoding support (`http.rs` throughout)

All read paths assume "Content-Length OR connection-close". `response_complete`
(`http.rs:568`) only checks Content-Length; `read_response_tls`/`read_response_tcp`
fall back to "read until Ok(0)". The requests send `HTTP/1.0` + `Connection: close`,
so a well-behaved server closes the socket at end-of-body and this works.

It breaks on:

- HTTP/1.1 servers that ignore the request's `HTTP/1.0` and respond `1.1`
  with `Transfer-Encoding: chunked`. The body will be returned with the
  chunk-size lines mixed in. `box pull` and `meow` API calls against such
  servers silently corrupt.
- Servers that hold the connection open (keep-alive) despite `Connection:
  close`. The reader blocks until the kernel's 30s recv timeout, then the
  "200 retries × 1ms" loop in `read_response_tcp` (`http.rs:615-657`) kicks
  in, adding 200ms of latency per request.

There is no test either way, so we don't know which servers in the wild
actually break this. **Severity: medium**, depends entirely on the servers
the three consumers talk to (Docker Hub, OpenAI, Ollama). Docker Hub's CDN
responds HTTP/1.1 and has been observed sending chunked on some paths.

#### TLS-BUG-3 — `Error::IoError` collapses every read/write failure (`lib.rs:51,124-147`)

Already documented in `userspace/libakuma-tls/docs/TLS_BUFFER_TRUNCATION_FIX.md`
as the reason the buffer-truncation bug took a long time to diagnose. The fix
landed on the buffer size; the error collapsing was "reverted to generic
IoError" (see the inline comments at `lib.rs:51,124,129,137,146` — three of
which literally say `// Reverted to generic IoError`). The reversion
re-introduced the diagnosability problem the original fix was trying to
solve. Map to a richer error: at minimum split `TlsError` from raw I/O
errors; ideally preserve the `TlsError` enum (it already implements
`Debug`).

#### TLS-BUG-4 — `TlsRng::fill_bytes` panics on any kernel RNG failure (`rng.rs:49,53`)

```rust
Err(_) => panic!("TLS RNG: getrandom syscall failed"),
```

A transient kernel RNG hiccup (VirtIO RNG buffer empty under load, briefly)
kills the process with no recovery path. For a crypto RNG, panic-on-failure
is defensible (you really do want to refuse rather than return weak bytes),
but a single `Err` from the syscall is not necessarily fatal — the kernel
returns `Ok(n)` for partial fills. Worth a short retry budget before the
panic, or at least a `safe_print!`-style diagnostic so the OOM-style log
trail exists.

### 3.2 HTTP layer (`http.rs`, 1249 LOC)

#### Duplication

This file is 80% copy-paste. The four "top-level" operations —
`https_fetch`, `https_get`, `https_post`, `download_file` — each open the
TCP connection, allocate TLS buffers (`read_buf`/`write_buf`), build a
request, send it, read the response, and close. The TLS buffer allocation
literal `alloc::vec![0u8; TLS_RECORD_SIZE]` appears **8 times**. The
redirect-aware path (`download_with_redirects` →
`download_redirects_tls`/`download_redirects_tcp`) reimplements the
streaming loop a third and fourth time (`stream_to_file` exists but the
redirect path inlines `stream_body_to_fd_tls` and an inline TCP loop
instead of calling it).

Refactor sketch:

```rust
// One connect helper
fn dial(parsed: &ParsedUrl) -> Result<MaybeTlsStream, Error> { ... }

// One request-send helper
fn send_request(s: &mut MaybeTlsStream, host, req) -> Result<(), Error> { ... }

// One response-to-fd helper (used by streaming, download, and redirect paths)
fn stream_body_to_fd(s: &mut MaybeTlsStream, fd, initial, content_length) { ... }

// One response-to-buffer helper (used by fetch/get/post)
fn read_response(s: &mut MaybeTlsStream, max_size) -> Result<Vec<u8>, Error> { ... }
```

This would cut `http.rs` roughly in half and make the bug fixes
(especially chunked-encoding, when it lands) touch one site instead of four.

A unifying `enum MaybeTlsStream<'a> { Tcp(TcpStream), Tls(TlsStream<'a>) }`
with `read`/`write_all`/`flush` methods would replace the existing
`Streamer` enum (`http.rs:182`, which is asymmetric — `Tcp(&TcpStream)` is
non-mut, `Tls(&mut TlsStream)` is mut — and only used in one place).

> **Fixed** in `9cd7a24`, with a `trait HttpIo { io_read, io_write_all }` +
> `&mut dyn HttpIo` helpers instead of the `MaybeTlsStream` enum sketched here
> — same "one loop body" effect without threading the TLS lifetime through
> every signature. The asymmetric `Streamer` enum is gone. 1,249 → 906 LOC
> (−343, not the ~500 estimated, because the two *streaming* structs were left
> alone). §6 lists the duplication that survives.

#### Hot-path allocations

The header-inspection helpers allocate a `Vec<u8>` per header line to do a
case-insensitive prefix match:

```rust
// http.rs:244-247, 557-560, 1017, 1039-1042 — same pattern 4x
let lower: Vec<u8> = line.as_bytes().iter().take(16)
    .map(|b| b.to_ascii_lowercase()).collect();
lower.starts_with(b"content-length:")
```

This runs once per header line per `response_complete` check, and
`response_complete` runs **once per read chunk**. On a 1.9 MB download with
4 KB chunks, that's ~475 invocations, each scanning every header line and
allocating a Vec for each. Replace with `line.len() >= 15 &&
line[..15].eq_ignore_ascii_case(b"content-length:")`. Zero allocations, same
result. Same fix for `extract_location_header` (take 9) and `parse_cl_header`.

#### Other

- `MAX_CONSECUTIVE_ERRORS = 200` with `sleep_ms(1)` (`http.rs:269,616`) — a
  magic 200ms deadline applied to every transient error with no backing
  rationale. Make it a `const` with a comment, or derive it from a deadline.
- `read_response_tls` swallows non-empty errors (`http.rs:599-604`): on
  `Err(_)` after some data, it `break`s and returns what it has, which then
  either parses or fails at `parse_http_response`. This is the
  `ERROR_HANDLING_FIX.md` compromise (some servers botch `close_notify`) and
  is documented; keep.
- `build_http_request` / `build_get_request_with_headers` /
  `build_post_request` (`http.rs:358-393`) use Rust raw-string
  line-continuation (`\`), which eats leading whitespace on the continued
  line — so the header is `Host: {host}` not ` Host: {host}`, which is
  correct, but the visual indentation in the source is a lie. The same
  pattern is inlined again in `HttpStream::post` / `HttpStreamTls::post`
  (`http.rs:778,894`) rather than calling the helpers.
- `HttpStream::post` only takes a `&str` body; no `post_from_fd` for the TCP
  variant (the TLS variant has it, `http.rs:915`). Asymmetric.
- `parse_url` (`http.rs:322`) does not handle userinfo (`user:pass@host`) or
  query strings distinctly (query just becomes part of `path`, which is
  fine). Documented limitation; OK.
- `resolve_redirect_url` (`http.rs:1003`) handles four cases correctly but
  doesn't URL-decode — a redirect with an encoded path stays encoded.
  Probably fine.

### 3.3 `transport.rs` — layering and behaviour

- **`TcpTransport::new_with_dots`** (`transport.rs:29`) prints `.`
  characters to **stdout** during blocking reads, allegedly to keep SSH
  connections alive (`http.rs:881` uses it). This is a layering violation:
  the transport layer should not write to the process's stdout. If meow's
  output is redirected to a file or piped, the dots corrupt it; if it's
  going to a terminal over a slow link, the dots add bytes. SSH keepalive is
  a transport/session concern (SERVER_ALIVE_INTERVAL), not an
  application-stdout concern. The dot machinery (`wait_counter`,
  `dots_printed`, `print_dots`, `reset_dots`, the `% 50` heuristic) is ~40%
  of this file. Delete it; if keepalive is really needed, it belongs in
  sshd's channel layer.
- The Read/Write impls loop on `WouldBlock`/`TimedOut` "retry immediately
  without sleeping" (`transport.rs:99,121`). The kernel blocks, so these are
  rare — but if they do fire (non-blocking mode set somewhere), this is a
  busy-loop. Either yield/sleep, or assert the fd is blocking.
- `TransportError::from_net_error` (`transport.rs:61`) takes `&NetError` and
  copies the kind out, throwing away the message. Same collapsing-pattern as
  TLS-BUG-3.

### 3.4 Testing

**Zero `#[test]` functions.** Same problem as `libakuma`: the crate is
`no_std` and transitively pulls in the libakuma allocator/panic handler via
its `libakuma` dep, so host-testing is impossible without a `--no-default-features`
escape that doesn't exist for this crate (the TLS feature gates are not
behind libakuma features).

The pure logic — `parse_url`, `find_headers_end`, `parse_status_line`,
`parse_content_length`, `resolve_redirect_url`, `extract_location_header`,
`build_*_request` — is all host-testable. Extract to `libakuma-tls-core` (or
a `pure` submodule gated behind a host-test feature) and unit-test the
header parsing, redirect resolution, and URL parsing. These are exactly the
functions where the chunked-encoding bug and the case-sensitivity bugs will
get caught.

### 3.5 Documentation

- `TLS_BUFFER_TRUNCATION_FIX.md` is still accurate and explains
  `TLS_RECORD_SIZE = 17408`.
- `ERROR_HANDLING_FIX.md` is still accurate, but the
  "Reverted to generic IoError" inline comments in `lib.rs` show the fix was
  partially walked back. Reconcile the doc with the current state.
- No doc mentions that cert verification is **disabled**. This is the
  single most important fact about the crate and it appears only as a
  code comment (`// Phase 1`) and a `_insecure` parameter name.

---

## 4. Cross-cutting recommendations

These apply to both crates.

1. **Extract a pure-logic core for host testing.** Both crates are 0-test
   because their process-runtime wrappers (`#[global_allocator]`,
   `#[panic_handler]`) collide with std. Pattern exists in the repo already
   (`crates/akuma-*`, `userspace/box/boxlib`). Suggested split:
   - `libakuma-core` — `no_std + alloc`, no runtime: `SocketAddrV4`,
     `SockAddrIn`, `Stat`, `WaitStatus`, syscall number tables, `Spinlock`,
     `DirEntry64`, `format_ip`/`format_addr`, `parse_addr`. Host-tested.
   - `libakuma` — depends on `-core`, owns `_start`/allocator/panic.
   - `libakuma-tls-core` — `no_std + alloc`: `parse_url`, header parsing,
     request building, redirect URL resolution. Host-tested.
   - `libakuma-tls` — depends on `-tls-core`, owns `TlsStream` + transport.

2. **Pick an fd type and stick to it.** `i32` matches POSIX and the majority
   of the existing API. Change `read`/`write` and the `fd` module to `i32`.

3. **Stop collapsing errors.** Three separate places (TLS-BUG-3,
   `TransportError`, `spawn -> Option`) throw away diagnostic information
   that the kernel already produced. Every `Err(_) =>` should be an
   `Err(e) =>` and the error type should carry `e`.

4. **Delete dead/deprecated API surface aggressively.** `TlsOptions`,
   `_insecure`, `AkumaOutput`, `ARGV_DATA_SIZE`, `CWD_DATA_SIZE`,
   `TERMINAL_SYSCALLS.md` (proposal), the `transport.rs` dot-printer, the
   brk allocator path. Every line of dead code is a line someone has to read
   and rule out.

5. **Document the trust boundary.** `libakuma-tls` does not verify certs.
   This needs to be in `docs/reference/subsystems/networking.md` (or a new
   `ssh.md` row) and as a `// SECURITY:` banner on `lib.rs`. Today it is
   discoverable only by reading the code.

6. **Add a chunked-encoding path or assert against it.** Pick one: implement
   `Transfer-Encoding: chunked` decoding in the read path, or detect the
   header and return `Error::HttpError("chunked not supported")`. Silent
   mis-parsing is the worst option.

---

## 5. Prioritized fix list

Line numbers are as of the audit (`8b6ba40`); items 1-4 have since moved them.

| # | Sev | Effort | Item | Where | Status |
|---|---|---|---|---|---|
| 1 | **High** | S | Null-terminate `fstatat` path; drop caller-side band-aid in `box/images.rs` | `libakuma/src/lib.rs:977`, `box/src/images.rs:20,25` | **Done** `9cd7a24` |
| 2 | **High** | S | Reconcile `ProcessInfo` doc/struct/constants against the kernel side; delete stale parts | `libakuma/src/lib.rs:151,154,157,177-203` | **Done** `9cd7a24` |
| 3 | **High** | M | Delete `TlsOptions`/`_insecure`/`insecure()` (or implement verify). Add `// SECURITY:` banner + reference doc row. | `libakuma-tls/src/lib.rs:60-79`, `http.rs:75` | **Done** `9cd7a24` — minus the reference doc row |
| 4 | **Med** | M | Refactor `http.rs` to kill the 4× duplication; unify on one stream enum + one read helper. Cuts ~500 LOC. | `libakuma-tls/src/http.rs` | **Done** `9cd7a24` — −343 LOC; `HttpStream`/`HttpStreamTls` still twins |
| 5 | **Med** | S | Implement or refuse chunked transfer-encoding | `libakuma-tls/src/http.rs:568,612` | **Done** (2026-08-12, `cf03840`) — refuse, not implement |
| 6 | **Med** | S | Replace per-line `Vec<u8>` lowercasing with `eq_ignore_ascii_case` (4 sites) | `libakuma-tls/src/http.rs:244,557,1017,1039` | **Done** (2026-08-12, `cf03840`) |
| 7 | **Med** | S | `getcwd`: replace `static mut CWD_BUF` with a `Spinlock` and drop `from_utf8_unchecked` | `libakuma/src/lib.rs:236-248` | **Done** (2026-08-12, `cf03840`) |
| 8 | **Med** | S | `accept`: parse the returned `sockaddr`, fix `peer_addr` on accepted sockets | `libakuma/src/net.rs:150-200` | **Done** (2026-08-12, `cf03840`) |
| 9 | **Med** | S | Stop mapping every TLS I/O failure to `Error::IoError`; preserve `TlsError` | `libakuma-tls/src/lib.rs:51,124-147` | **Done** (2026-08-12, `cf03840`) |
| 10 | **Low** | S | Unify fd types on `i32` (`read`/`write`/`fd` module) | `libakuma/src/lib.rs:141-144,504,520` | **Done** (2026-08-12, `cf03840`) |
| 11 | **Low** | S | `#[deprecated]` `waitpid` and `kill` (the signal-0 variant), point at the `*_status`/`kill_signal` replacements | `libakuma/src/lib.rs:1589,1787` | **Done** (2026-08-12, `cf03840`) — also fixed 3 live call sites that were relying on the signal-0 no-op believing it killed a child |
| 12 | **Low** | S | Delete `transport.rs` dot-printer (layering violation) | `libakuma-tls/src/transport.rs:13-17,29-31,101-107` | **Done** (2026-08-12, `cf03840`) |
| 13 | **Low** | S | Delete or feature-gate the brk allocator path (racy, dead by default) | `libakuma/src/lib.rs:2025,2192-2240` | **Done** (2026-08-12, `cf03840`) — deleted |
| 14 | **Low** | M | Extract pure-logic cores (`libakuma-core`, `libakuma-tls-core`) and add the first host tests | both crates | Open — still 0 tests |
| 15 | **Low** | S | Refresh `SYSCALLS.md` / `TERMINAL_SYSCALLS.md` / `ALLOCATOR_MEMORY_FIX.md` against current code | `userspace/libakuma/docs/` | **Done** (2026-08-12, `cf03840`) — also refreshed `ALLOCATOR_OPTIONS.md` (found stale on inspection, not in the original list) |

"S" = ≤ 1 hour, "M" = a day or less. None of these require kernel changes
except item 2's verification step.

---

## 6. Fix log

### 2026-08-12 — `9cd7a24` "half baked libakuma fixes" (items 1-4)

Six files, +453/−826. Verified by building every userspace member
(`userspace/build.sh`): `libakuma`, `libakuma-tls`, `box`, `meow` and the other
15 members compile against the new signatures.

**Item 1 — `fstatat` (BUG-1).** `userspace/libakuma/src/lib.rs`: `fstatat` now
does `let path_c = alloc::format!("{}\0", path);` and passes `path_c.as_ptr()`,
matching all 13 sibling wrappers. `userspace/box/src/images.rs`:
`path_exists`/`dir_exists` dropped their own `format!("{}\0", path)`.

**Item 2 — `ProcessInfo` (BUG-3).** Reconciled on **both** sides, and the
struct — not the doc — turned out to be the truthful one, so nothing was ever
misreading `box_id`:

- `userspace/libakuma/src/lib.rs`: deleted `ARGV_DATA_SIZE`/`CWD_DATA_SIZE`;
  replaced the 1024-byte `argc`/`argv_len`/`cwd_data`/`argv_data` layout
  comment with what the page actually carries (`pid`/`ppid`/`box_id`,
  `_reserved` stays zeroed) and where argv (entry stack) and cwd (`GETCWD`)
  really come from.
- `crates/akuma-exec/src/process/types.rs`: deleted the kernel's copies of the
  same two constants and the "Layout must match libakuma exactly" wording. The
  compile-time `const _: () = assert!(size_of::<ProcessInfo>() == 1024)` (and
  its unit-test twin) stay — that is the invariant worth asserting.

**Item 3 — dead insecure-TLS API (TLS-BUG-1).** Option 2 of the two the audit
offered. `userspace/libakuma-tls/src/lib.rs`: `TlsOptions`, `TlsOptions::new`,
`insecure()` and the `_insecure` parameter are gone — `https_fetch(url,
max_size)` is the signature — and the crate doc now opens with a
`# SECURITY: certificate verification is disabled` section that names
`NoVerify`, says the channel is MITM-able, and points back at this audit. The
`meow` submodule was bumped to the new signature; `box` and `scratch` call
neither `https_fetch` nor `TlsOptions`, so they needed nothing.
**Still true: there is no verification.** Not done: the
`docs/reference/subsystems/ssh.md` trust-boundary row.

**Item 4 — `http.rs` de-duplication.** 1,249 → 906 LOC. Instead of the
`enum MaybeTlsStream` the audit sketched, a private
`trait HttpIo { io_read, io_write_all }` is implemented for `TlsStream` and
`TcpStream`, and every loop body now takes `&mut dyn HttpIo` and exists once:
`read_response`, `read_until_headers`, `stream_body_to_fd`,
`parse_content_length`, `response_complete`, `fetch_to_vec`, `download_impl`,
`process_pending`. The asymmetric `Streamer` enum is gone. The TCP-vs-TLS
difference that mattered — TLS needs a `flush` after `write_all`, and a TLS
read error is fatal where a TCP `WouldBlock` is not — survives as
`io_write_all`'s bundled flush and `read_response`'s `error_budget` parameter
(0 for TLS, `TCP_ERROR_BUDGET` for TCP).

What the refactor did **not** reach, hence "half baked":

- `HttpStream` (TCP) and `HttpStreamTls` (TLS) are still two structs with
  parallel `connect`/`post`/`read_chunk`/`status_code`/`headers_parsed`; they
  share only `process_pending`. They are why the cut was −343 LOC rather than
  the estimated ~500.
- `alloc::vec![0u8; TLS_RECORD_SIZE]` is down from 8 sites to 4
  (`http.rs:418,419` in `fetch_to_vec`, `570,571` in `download_impl`).
- Items 5, 6 and 9 were left deliberately: `response_complete` is still
  Content-Length-only (chunked still unhandled), 3 of the 4 lowercase-`Vec`
  header comparisons remain, and `HttpIo`'s doc comment explicitly defers the
  error-collapsing to item 9.

Untouched by this commit: BUG-2 (`accept` peer address), BUG-4 (`getcwd`
`from_utf8_unchecked`), BUG-5 (fd types), TLS-BUG-2/3/4, and items 5-15.

### 2026-08-12 (later) — `cf03840` "multiple libakuma fixes" — items 5-13, 15

**Item 5 — chunked transfer-encoding (TLS-BUG-2).** Took the "refuse"
option, not "implement": added `is_chunked_encoding(headers)` to `http.rs`
and call it at the three places headers get parsed (`parse_http_response` for
the in-memory `fetch_to_vec` path, `download_impl` before streaming to a
file, `process_pending` for the raw `HttpStream`/`HttpStreamTls` API). Each
now returns `Error::HttpError("chunked transfer-encoding not supported")`
instead of handing the caller raw chunk-size framing as if it were the body.

**Item 6 — header lowercasing (3 remaining sites).** `http.rs:221`
(`parse_content_length`), `530` (`extract_location_header`), `553`
(`parse_cl_header`) replaced the per-line `Vec<u8>` allocate-and-lowercase
with `bytes[..N].eq_ignore_ascii_case(b"...")`. Zero allocations, same
matching.

**Item 7 — `getcwd` (BUG-4).** `static mut CWD_BUF: [u8; 256]` replaced with
a `Spinlock<[u8; 256]>` that guards the syscall write; `from_utf8_unchecked`
replaced with checked `from_utf8`. The returned `&'static str` still outlives
the lock (documented on the function), so a second `getcwd()` call from
another thread can still overwrite bytes a caller holds a reference to — that
was always true and is a separate, larger fix (caller-supplied buffer, per
item 14's cross-cutting note); what changed is that concurrent *writes* to
the buffer are no longer literal UB.

**Item 8 — `accept` peer address (BUG-2).** Added `accept_addr(fd) ->
(i32, SocketAddrV4)` next to `accept` in `lib.rs`, parsing the `sockaddr` the
kernel already fills in via `SockAddrIn::to_addr()` instead of discarding it.
`net.rs`'s `TcpListener::accept`/`try_accept` now use it, so
`TcpStream::peer_addr()` on an accepted stream reports the real peer instead
of `0.0.0.0:0`.

**Item 9 — TLS error collapsing (TLS-BUG-3).** `TlsStream::{read,write,flush}`
in `libakuma-tls/src/lib.rs` now `map_err(Error::TlsError)` instead of
`map_err(|_| Error::IoError)`. This actually fixes a real behavior bug, not
just diagnostics: `http.rs`'s `read_until_headers` already special-cased
`Err(Error::IoError)` as retryable and everything else as fatal ("Non-I/O
errors (e.g. TLS record corruption) propagate immediately" — a comment that
predates this fix) — but with every TLS error forced into `Error::IoError`,
TLS record corruption was silently retried as if it were a transient TCP
hiccup. Now it propagates immediately, matching what the comment always
claimed.

**Item 10 — fd type unification.** `fd::{STDIN,STDOUT,STDERR}` and
`read`/`write` changed from `u64` to `i32`, matching `read_fd`/`write_fd`/
`close`/etc. The only other `u64`-fd functions in the crate,
`set_terminal_attributes`/`get_terminal_attributes`, were left alone (out of
this item's stated scope), so the handful of call sites that fed `fd::STDIN`
into those two (`termtest`, `meow/src/tui_app.rs`) needed an explicit
`as u64` added.

**Item 11 — deprecate `waitpid`/`kill`.** Both got `#[deprecated]` pointing at
`waitpid_status`/`wait_any` and `kill_signal`. Auditing every `kill()` call
site to add the annotation turned up 3 **live bugs**, not just a naming
footgun: `paws/src/main.rs`'s Ctrl-C handler and two sites in
`meow/src/tools/shell.rs` (on-chunk-error abort, 30s timeout abort) all called
`libakuma::kill(pid)` intending to terminate a child process — but `kill()`
only ever sends signal 0 (a liveness probe, delivers nothing), so none of the
three ever actually killed anything. `herd` already had this right
(`kill_signal(pid, SIGTERM)`, with a comment naming the exact footgun) —
it just hadn't propagated to the other three call sites. Fixed: paws now
sends `SIGINT` (added as a new libakuma constant — Ctrl-C's actual signal,
confirmed fatal-by-default in the kernel's `signal_is_fatal_default`), meow's
two sites send `SIGTERM` (matching herd's convention for programmatic stop).

**Item 12 — `transport.rs` dot-printer.** Deleted `wait_counter`,
`dots_printed`, `print_dots`, `new_with_dots`, `dots_printed()`,
`reset_dots()`, and the `% 50` heuristic. No caller read `dots_printed()`/
used `reset_dots()`; `new_with_dots`'s only caller was
`HttpStreamTls::connect`, now just `TcpTransport::new`. meow has its own,
separate dot-printing in `api/client.rs` for TUI progress display, unrelated
to this transport-layer mechanism, so the visible "..." while waiting for an
LLM response is unaffected.

**Item 13 — brk allocator path.** Deleted outright (not feature-gated):
`USE_MMAP_ALLOCATOR`, `brk_head`/`brk_end` fields, `brk_init`/`brk_expand`/
`brk_alloc`, and the `if USE_MMAP_ALLOCATOR {...} else {...}` branches in
`alloc`/`dealloc`/`realloc` — all now unconditionally mmap-backed. Nothing in
the tree ever set the constant to `false`, so there was no live behavior to
preserve, only a UB footgun (non-atomic load-head/load-end/compute/store-head
in `brk_alloc`) to remove. `print_allocator_info` (already dead — zero
callers in the tree) was updated to stop reading the now-deleted
`head_addr`/`head_value`/`end_value` and report the still-meaningful
mmap byte/allocation counters instead.

**Item 15 — doc refresh.** `SYSCALLS.md`: `pipe` no longer says "currently
stubbed" (it calls `PIPE2` directly); the `execve` row replaced with `spawn`
(no `execve` wrapper exists). `TERMINAL_SYSCALLS.md`: rewritten past-tense —
all 7 syscalls are shipped (verified numbers 307-313 against
`syscall::{SET_TERMINAL_ATTRIBUTES,...}` and each wrapper's real signature),
including resolving the doc's old "0-indexed or 1-indexed, TBD" on
`set_cursor_position` (it's 0-indexed; the kernel adds 1 before emitting the
VT100 sequence). `ALLOCATOR_MEMORY_FIX.md`: marked historical — the
`DeferredFreeQueue` it describes doesn't exist in the current allocator.
`ALLOCATOR_OPTIONS.md` (not in the original list, found stale while doing
this item): it described page-per-`mmap` as the default and
`chunked-allocator` as opt-in, backwards from `Cargo.toml`'s
`default = ["chunked-allocator"]` — flipped, and added a note that the old
brk arm (item 13) is gone.

Full `userspace/build.sh` run clean after each item; `cargo check` used
per-package during iteration. Item 14 (pure-logic core extraction + first
host tests for both crates) is the only item left open.

### 2026-08-12 (later still, uncommitted) — `paws` Ctrl-C actually fixed

Everything above this point was checked with `cargo build`/host tests only.
Booting a real QEMU instance and driving `paws` through an interactive PTY
(spawn a real foreground external child, send `0x03`) showed the item-11
`paws` fix landed in `cf03840` — `kill_signal(pid, SIGINT)` instead of the
signal-0 `kill(pid)` — **still didn't kill the child**. First diagnosis was
wrong: theorized `poll_input_event` needed raw terminal mode (canonical mode
line-buffers a lone Ctrl-C until Enter), so `cf03840` added
`set_terminal_attributes(..., RAW_MODE_ENABLE)` around `stream_output`'s
loop. That compiled and looked plausible but didn't fix the live test.

Root cause, found by instrumenting `sys_poll_input_event` and
`write_to_process_stdin` with temporary `tprint!`s and re-running the same
PTY test: `sys_poll_input_event` (`src/syscall/term.rs`) calls
`proc_channel.read_stdin` directly and **never consults `TerminalState` at
all** — canonical/raw mode is irrelevant to this syscall, so the `cf03840`
fix was inert. The trace showed `stream_output`'s poll loop calling
`poll_input_event` exactly twice after the child was spawned, then never
again for the rest of the session, while the child (an `httpd` instance)
sat alive and idle. That is `read_fd(stdout_fd)` blocking: the child's
stdout pipe is blocking by default, so once it stops producing output
(a server between requests, a build between output lines) `stream_output`
parks in that `read_fd` call and never gets back around to checking for
Ctrl-C. `userspace/sshd/src/protocol.rs`'s `bridge_process` hit and fixed
this exact deadlock class for its own child-stdout-vs-stdin bridge loop
(`set_nonblocking(stdout_fd, true)`, with a comment naming it); `paws`'s
`stream_output` never got the same treatment.

Fix: `userspace/paws/src/main.rs`'s `stream_output` now calls
`set_nonblocking(stdout_fd as i32, true)` before entering its loop,
mirroring sshd's fix. The `RAW_MODE_ENABLE`/`DISABLE` calls and the local
`mode_flags` module `cf03840` added are removed — they weren't wrong to
try, just not what was needed, and keeping code around that claims to fix
something it doesn't is worse than deleting it.

QEMU-reverified end to end: spawned `/bin/httpd 9099` as a real foreground
child under `paws`, confirmed via kernel-side `tprint!` tracing that
`poll_input_event` now keeps firing throughout (not just twice), sent
`0x03`, saw `^C` / `paws: process /bin/httpd exited with status 0` printed,
and confirmed via `ps aux` from a second session that the child process was
actually gone. All temporary debug instrumentation (in `src/syscall/term.rs`
and `crates/akuma-exec/src/process/mod.rs`) was reverted before this note
was written.

**Method lesson:** a plausible-sounding, source-reading-only diagnosis
("canonical mode buffers Ctrl-C") compiled clean and matched the *symptom*
description, but was wrong — only booting the actual system and tracing the
real syscall sequence caught it. Neither `cargo build` nor the host test
suites for this repo can catch this class of bug; only QEMU can.

---

## Background

- Per-fix history: `userspace/libakuma/docs/{ALLOCATOR_MEMORY_FIX,
  ALLOCATOR_OPTIONS, MKDIR_P_IMPROVEMENTS, POLL_INPUT_EVENT_FIX, SYSCALLS,
  TERMINAL_SYSCALLS}.md` and `userspace/libakuma-tls/docs/{ERROR_HANDLING_FIX,
  TLS_BUFFER_TRUNCATION_FIX}.md`.
- Cross-references from `docs/archive/`: `BUG_FIX_LIST.md` (the
  `kill`/`waitpid` history and the TLS error-swallowing class),
  `THREAD_SCHEDULING_INVESTIGATION.md` and `HEAP_CORRUPTION_ANALYSIS.md`
  (allocator history), `TLS_INFRASTRUCTURE.md` (the original Phase 1 plan
  that left verify disabled), `NETWORKING_PERFORMANCE_AND_THREAD_SAFETY_ANALYSIS.md`
  (the sleep tuning that became the current `MAX_CONSECUTIVE_ERRORS` loops),
  `UNIFIED_PROCESS_ABI_IMPLEMENTATION_ISSUES.md` (the missing-NUL bug class
  that BUG-1 is the last instance of), `TRIM_FAT_HERD_PLUS_BOX.md` (the
  libakuma-as-libc argument for keeping syscall numbers here).
- Root `CLAUDE.md` documents the host-test escape for `sshd`/`box` — the
  same constraint that item 14 addresses for these two crates.
