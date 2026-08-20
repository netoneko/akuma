# In-guest `cargo` cannot reach crates.io

> **FIXED 2026-08-20.** Root cause was `sys_pselect6` never writing the caller's
> `exceptfds` set. If you are on a kernel built after that, this page is history
> — go to [§ 3](#3-the-mechanism-traced-through-source-2026-08-20) for the
> mechanism and skip the workarounds entirely. Nightly cargo is safe as the
> bootstrap default: three cold-registry `cargo fetch` runs in a row, 35 crates
> each, zero `spurious network error` lines.
>
> **One line:** the nightly toolchain's libcurl uses `select(2)`, not `poll(2)`;
> Akuma's `select` left `exceptfds` exactly as the caller passed it in; libcurl
> read that back as `POLLPRI`, called it `CURL_CSELECT_ERR`, and threw away every
> TCP connection about one RTT *after it had successfully connected*.
>
> If you still see this on a current kernel, it is a **new** bug — the runbook
> below still works as a diagnostic, and `nettest-connect` (§ 3.6) is the fastest
> way to tell a kernel fault from a network one.

**Symptom.** Inside a devbox VM, `cargo build` / `cargo fetch` loops on:

```
warning: spurious network error (3 tries remaining): [7] Could not connect to server
  (Failed to connect to index.crates.io:443 after 786 ms: Could not connect to server)
```

…while `curl` from the same shell gets `200` from the same host in ~300 ms.

**Read this first: there is no cargo config that fixes this.** An older note
(`debug-thread-spawn-segv.md`) blamed libcurl HTTP/2 multiplexing and
recommended `[http] multiplexing = false`. That diagnosis was **disproven by
experiment** on 2026-08-11 — the flag does not change the failure
(`../archive/CARGO_CRATES_IO_CONNECT_FAIL.md`). The config below is still worth
having, but for a *different* failure (flaky `static.crates.io` downloads), and
it will not make this symptom go away. Do not spend time tuning it.

## 1. Identify which cargo you are running — this decides everything

```sh
command -v cargo
cargo --version
```

| Result | Meaning |
|---|---|
| `/usr/local/bin/cargo` (nightly) | The one that used to fail. Its libcurl calls `select(2)`; everything else in the guest calls `poll(2)`. Go to step 2. |
| `/usr/bin/cargo` (apk, 1.96.x) | Never affected — its libcurl uses `poll(2)`. If it fails, the network really is down; go to [`debug-network.md`](debug-network.md). |

Since 2026-08-19 the devbox puts nightly first on `PATH`, so a bare `cargo` is
the `select(2)` one by default. That is fine on a kernel built after
2026-08-20; before it, that single difference was the whole bug.

## 2. Confirm which bug you have

```sh
curl -o /dev/null -w '%{http_code}\n' https://index.crates.io/config.json   # expect 200
cargo fetch                                                                 # expect success
```

On a kernel built after 2026-08-20 both succeed. If `cargo` still fails while
`curl` returns `200`, run the probe in [§ 3.6](#36-reading-the-answer-off-one-run)
before anything else — it separates a kernel fault from a network one in a single
command, and it will tell you immediately whether you are looking at a regression
of the `exceptfds` bug or at something new.

### What the original 2026-08-11 evidence actually showed

Kept because two of its conclusions were wrong in instructive ways, and both
misdirections cost real time:

- **"110 `connect() = EINPROGRESS` cycles with zero completions."** There were no
  completions *in the log* because `sys_connect` logs `= OK` only on the
  **blocking** path — a non-blocking connect always logs `EINPROGRESS` and its
  completion is invisible to that log site. The absence was not evidence. In fact
  every one of those connects succeeded; instrumentation on 2026-08-20 caught the
  sockets sitting in `Established` with `SO_ERROR == 0` at the exact moment
  libcurl declared them failed.
- **"No probe reproduces it."** True of the probes that existed — all four
  `nettest` modes pass 30/30, and `bootstrap/bin/nettest` really is the vendored
  static-libcurl build (the archive doc's claim that it links apk libcurl is
  stale). They all missed it because they bisected libcurl's *stack* — HTTP/2,
  multiplexing, resolver, worker threads — and the axis that mattered was which
  **readiness syscall** the client calls. `nettest-connect --wait poll` vs
  `--wait select` found it in one run.

The one contemporaneous observation that did point the right way was the timing:
libcurl gave up after ~300 ms, which no hung connect can explain. § 3.1 turns
that into a proof.

## 3. The mechanism, traced through source (2026-08-20)

§§ 3.1–3.3 are a read of the two code bases that meet at this failure — the
vendored libcurl the nightly toolchain links (`curl-sys-0.4.90+curl-8.21.0`,
unpacked in `~/.cargo/registry`) and the kernel's readiness path. They settle the
timing contradiction without a VM and narrow the search to a handful of
candidates. § 3.4 is the measurement that picked one, and § 3.5 is what is left
for any *future* stall in the same shape.

### 3.1 The 353 ms is a refusal, not a hang

`cf-socket.c`, `cf_tcp_connect()`, is the whole decision:

```c
rc = SOCKET_WRITABLE(ctx->sock, 0);   /* readiness query for OUT, timeout 0 */
if(rc == 0)                     /* "not connected yet" — attempt stays ONGOING */
else if(rc == CURL_CSELECT_OUT) /* verifyconnect(): getsockopt(SO_ERROR) */
else if(rc & CURL_CSELECT_ERR)  /* HARD FAIL, socket closed, fd released */
```

with `select.c`, `Curl_socket_check()`, mapping the write fd's revents:

```c
revents & (POLLWRNORM|POLLOUT)               -> CURL_CSELECT_OUT
revents & (POLLERR|POLLHUP|POLLPRI|POLLNVAL) -> CURL_CSELECT_ERR
```

Note the `==` in the middle branch: `POLLOUT|POLLHUP` together take the *error*
path, not the success path.

`cf-ip-happy.c` then raises `CURLE_COULDNT_CONNECT` — the observed `[7] Could
not connect to server` — **only when the address list is exhausted and no
attempt is still ongoing** (`cf_ip_ballers_run`, the `else if(!ongoing &&
dns_resolved)` arm). An attempt that hangs keeps `ongoing > 0` and cannot
produce that message at all. A failure that merely looks slow would burn
`CURLOPT_CONNECTTIMEOUT`, not a third of a second.

So the archive doc's wording — "non-blocking TCP connects … never complete" —
cannot be what happened. Every attempt **hard-failed within roughly one round
trip**, and `cf_tcp_connect`'s `out:` block closed each socket on the way out,
which is also what produces the "same fd 6 reused — prior socket closed"
pattern in that doc's own kernel log.

### 3.2 What the kernel can answer

`epoll_check_fd_readiness` (`src/syscall/poll.rs:498`) gives a TCP socket one of
two mutually exclusive branches:

```rust
if socket_is_dead_tcp(idx) { ready |= EPOLLHUP; }
else { /* EPOLLIN / EPOLLOUT / EPOLLRDHUP */ }
```

`socket_is_dead_tcp` (`src/syscall/net.rs:1223`) is `!smoltcp.is_active()` —
true in `Closed`, `TimeWait` and `Listen`. `sys_ppoll` (`src/syscall/poll.rs:1079`)
then passes `EPOLLHUP` through **unmasked**, regardless of what the caller
requested, exactly as Linux does.

Two consequences:

- A socket in `SynSent` polls as `0`. That is correct, and it is the "still
  connecting" case — it cannot produce the failure.
- **Akuma never reports `POLLOUT` together with `POLLHUP` for a socket.** Linux
  answers a refused connect with `OUT|ERR|HUP|WRNORM`; Akuma can only answer
  `HUP`. libcurl reaches the same verdict either way, but do not read the
  differing bits as the bug. It also means the `verifyconnect()`/`SO_ERROR`
  branch is effectively unreachable here: the hard failure always arrives via
  `CURL_CSELECT_ERR`.

### 3.3 A `SynSent` socket could not die on its own

This is the part that narrows the search. **Nothing in the tree ever calls
smoltcp's `set_timeout()`** — repo-wide, zero call sites. The field defaults to
`None` (`smoltcp-0.12.0/src/socket/tcp.rs:527`) and `timed_out()` is hardcoded
false while it is `None` (`tcp.rs:2118`), so an unanswered SYN is retransmitted
forever and never reaches `Closed`.

The only `close()`/`abort()` callers on a TCP handle are:

| site | when |
|---|---|
| `remove_socket()` — `crates/akuma-net/src/socket.rs:438` | the **last** fd referring to it is closed |
| the `poll()` GC sweep — `crates/akuma-net/src/smoltcp_net.rs:961` | handle already in `pending_removal` |
| `reclaim_pending_slots()` — `crates/akuma-net/src/smoltcp_net.rs:1201` | handle already in `pending_removal`, state `TimeWait`/`Closed` |

The latter two only ever touch handles whose last fd is already gone, and
`dup`/`dup2`/`F_DUPFD`/fork all refcount through `socket_clone_ref`
(`src/syscall/fs.rs:1360`, `:1395`, `:2360`), so a live fd's socket is not
reachable from either.

> **Changed 2026-08-20.** This unbounded-`SynSent` behaviour was a second, real
> defect found by the same investigation and is now fixed: `poll()` sweeps a
> small `connecting` list and abandons anything past `CONNECT_TIMEOUT_US` (10 s,
> matching the blocking path's existing cap), flagging the socket so `SO_ERROR`
> answers `ETIMEDOUT` rather than `ECONNREFUSED`. smoltcp's `set_timeout()` is
> still not used — it is an *inactivity* timeout armed in every state, so it
> would also abort idle `Established` connections. So on a current kernel a
> `HUP` on a connecting socket has one more possible origin than § 3.5's table
> had when it was written; `SO_ERROR` tells them apart. Details:
> `../reference/subsystems/networking.md` § `connect(2)` semantics.

### 3.4 The answer: `exceptfds` came back stale

Measured in the VM on 2026-08-20, with temporary logging in
`epoll_check_fd_readiness` and `sys_getsockopt`. Across a full failing
`cargo fetch`: **zero** `EPOLLHUP` from the socket branch, **zero** fd-lookup
misses, and every `SO_ERROR` read returned `val=0 state=Established`. The
sockets were connecting perfectly and libcurl was throwing them away anyway.

That ruled out both `POLLHUP` origins and left one possibility: libcurl was not
calling `poll` at all. It was not. `curl-sys`' `build.rs` defines `HAVE_POLL_H`
and `HAVE_POLL_FINE` but **never plain `HAVE_POLL`**, and `Curl_poll()` in
`select.c` is `#ifdef HAVE_POLL` — so the vendored libcurl the nightly toolchain
links compiles the **`select(2)`** branch. apk's libcurl and `/bin/curl` are
autotools builds that define `HAVE_POLL`, which is exactly why they always
worked.

The select branch does this:

```c
if(ufds[i].events & (POLLRDBAND | POLLPRI))
    FD_SET(ufds[i].fd, &fds_err);            /* -> exceptfds */
...
if(FD_ISSET(ufds[i].fd, &fds_err)) {
    if(ufds[i].events & POLLPRI)
        ufds[i].revents |= POLLPRI;
}
```

and `Curl_socket_check()` asks for `POLLWRNORM|POLLOUT|POLLPRI` on a connecting
socket — so the fd goes into `exceptfds`. `sys_pselect6` took `_exceptfds_ptr`
and never wrote it, so `FD_ISSET(sock, &fds_err)` was still true on return,
libcurl synthesised `POLLPRI`, and `Curl_socket_check` mapped `POLLPRI` into
`CURL_CSELECT_ERR`. `cf_tcp_connect` tests `rc == CURL_CSELECT_OUT` by
**equality**, so `OUT|ERR` took the error branch, `verifyconnect()` read
`SO_ERROR == 0`, and the attempt died as `CURLE_COULDNT_CONNECT` with
`ctx->sockerr == 0` — the `"No error information"` in the trace.

The probe isolates it to one syscall, same address, same moment:

```text
nettest-connect one index.crates.io 443 --wait poll     t=77.0ms revents=OUT      -> CONNECTED
nettest-connect one index.crates.io 443 --wait select   t=68.6ms revents=PRI|OUT  -> HARDFAIL
```

**Fix:** `sys_pselect6` now zeroes `exceptfds` on both the ready and the timeout
path (`src/syscall/poll.rs`). Regression test:
`run_pselect6_exceptfds_test` → `[PASS] pselect6_clears_exceptfds`.

Anything else in the guest that uses `select(2)` was affected the same way; this
was not a cargo-specific fault, only a cargo-specific *symptom*.

### 3.5 If a connect really does stall: the origins, and `revents` tells them apart

This table is what is left over for a *future* stall in the same shape — it is
not the 2026-08-20 bug, which produced no `POLLHUP` at all. A `POLLHUP` on a
**live connecting fd** can only be:

| `revents` | origin | `SO_ERROR` reads |
|---|---|---|
| `HUP` alone | an RST arrived, or the handle was recycled under the socket | `ECONNREFUSED`, synthesised from state (`src/syscall/net.rs`) |
| `HUP` alone, at ~10 s | the `CONNECT_TIMEOUT_US` sweep gave up on `SynSent` (§ 3.3) — the expected outcome for an unroutable peer, not a defect | `ETIMEDOUT` |
| `ERR\|HUP` | `current_process_shared()` / `get_fd()` returned `None` (`src/syscall/poll.rs`) — **no socket state was consulted at all** | `0`, from the fd-miss arm |
| `OUT` with `ERR`/`HUP` | impossible on Akuma — you are reading the Linux control arm | — |

`SO_ERROR` separates the first two rows, which is exactly why the connect
deadline reports `ETIMEDOUT` instead of reusing `ECONNREFUSED`: a connect nobody
answered is not a connect that was refused, and a log that conflates them sends
the next reader down the wrong path.

The `ERR|HUP` row is the interesting one if it ever appears — it is thread-shaped
(`current_pid()`, `crates/akuma-exec/src/process/children.rs`, resolves via
`THREAD_PID_MAP` first and the ProcessInfo page second), and it is
indistinguishable to the caller from a socket that died. It has never been
observed; instrumentation across a full failing `cargo fetch` counted zero.

### 3.6 Reading the answer off one run

`nettest-connect` (`userspace/nettest/rust/connect/`, built by
`userspace/nettest/rust/build-musl.sh connect`) makes the same syscalls with no
libcurl in the picture and prints the raw `revents`, the `SO_ERROR`, and which
row of the table above it matches:

```sh
nettest-connect he    index.crates.io 443     # cargo's happy-eyeballs, emulated
nettest-connect churn index.crates.io 443 120 # is it the Nth connect that breaks?
nettest-connect one   index.crates.io 443 --wait poll   # blocking wait vs poll0
```

| what it prints | what it means |
|---|---|
| `--wait poll` connects but `--wait select` does not | the 2026-08-20 bug, or a regression of it. Check `sys_pselect6` writes all three sets |
| `verdict=HARDFAIL_*` on every mode | read the `hint:` line for which row of § 3.5 |
| `verdict=PENDING` past `CONNECT_TIMEOUT_US` (10 s) | the `SynSent` sweep is not running; a connect should now always end |
| `verdict=CONNECTED` throughout | the guest's network is fine; the fault is in whatever client you were debugging |

Same static binary runs under Docker Linux as the control arm
(`../archive/LINUX_AB_PROBE.md`). Full probe documentation, including the
`--wait poll0|poll|select|epoll` bisect of `sys_ppoll` / `sys_pselect6` /
`sys_epoll_pwait`, is in `userspace/nettest/README.md` § Part 3.

## 4. Work around it

> Only needed on a kernel from before 2026-08-20. On a current kernel nightly cargo fetches normally — skip this section.

In order of preference:

**a. Use apk cargo for the fetch, nightly for the build.** The registry cache is
shared, so one tool can fill it for the other:

```sh
/usr/bin/cargo fetch                 # fills ~/.cargo/registry
cargo build --release --offline      # nightly, never touches the network
```

**b. Run everything offline after one warm fetch.** Once the cache is warm, add
`--offline` to every subsequent command so a long loop never touches the
network again.

> `--offline` can fail with `no matching package named <crate> found` even when
> `~/.cargo/registry/{cache,src}` both hold it. What is stale is the **index**
> cache, which a cargo upgrade invalidates — not the crate sources. Refresh once
> with `/usr/bin/cargo fetch`, then go offline again.

**c. Stage from the host instead.** Most acceptance tests already skip in-VM
cargo entirely and use `scripts/populate_disk.sh`.

## 5. The config the bootstrap installs (and what it is actually for)

`overlays/devbox/bootstrap.sh` step 7c writes `/root/.cargo/config.toml`
alongside the toolchain:

```toml
[net]
retry = 20

[http]
multiplexing = false
```

`retry = 20` is the useful one: even when the index is reachable,
`static.crates.io` **download** connections fail often enough that a default
retry budget of 3 aborts a large fetch. `multiplexing = false` is retained
because it is harmless and was the historical recommendation — **it does not
fix the symptom at the top of this page.**

To change the policy for one command without editing anything (env beats
config):

```sh
CARGO_NET_RETRY=50 cargo fetch
CARGO_NET_OFFLINE=true cargo build --release
```

## 6. What fixed it, and what to try first next time

**The fix:** `sys_pselect6` now writes `exceptfds` — zeroed, on both the ready
and the timeout path (`src/syscall/poll.rs`). None of the ranked options in
`../archive/CARGO_CRATES_IO_CONNECT_FAIL.md` § "Fix options" was the answer;
every one of them targeted libcurl's build or its resolver, and the fault was in
the kernel's `select`.

The two steps that actually got there, in order, and the right first moves for
anything in this shape:

1. **Re-test on a current kernel — done 2026-08-20, and it did *not* fix it.**
   The 2026-08-11 diagnosis predated `benchmarks-improved-networking`, whose two
   lost-wake fixes (NIC doorbell re-arm, `../archive/AKUMA_NET_ISSUES.md` §9;
   `blocking_relax` yield, §12) matched the reported "`connect` → `EINPROGRESS`
   → `POLLOUT` never fires" signature. Re-running the reproducer cost one boot
   and, by failing, produced the live reproducer everything else depended on.
   Cheap experiments are worth running even when you expect them to fail.
2. **Run `nettest-connect`** (§ 3.6) and read `revents` off the failing attempt.
   Comparing `--wait poll` against `--wait select` is what identified the bug —
   the axis nobody had varied was *which readiness syscall the client calls*.
   For a stall rather than a fast failure, § 3.5 maps the `revents` you get to
   an origin.

   Do **not** start by rebuilding the `nettest` curl probe with `static-curl` —
   the archive doc names that as the cheapest next step and that text is stale.
   `userspace/nettest/rust/Cargo.toml` already carries
   `features = ["http2", "ssl", "static-curl", "static-ssl"]`, and the built
   binary (`bootstrap/bin/nettest`) contains vendored `curl/lib/vtls/openssl.c`
   plus the string `OpenSSL 3.6.3` — the same vendored OpenSSL the nightly
   toolchain reports. It was built twenty minutes *before* the commit that added
   the doc (`0c9d96ce`).

Do **not** start by debugging the smoltcp stack, and do not spend time on
libcurl's HTTP/2, multiplexing, resolver or threading — all four were ruled out
by experiment in 2026-08-11, and the eventual cause was none of them.

## Verify

On a devbox image, with a **cold registry** — a warm one succeeds without
touching the network and proves nothing:

```sh
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost \
  'cd /tmp/akuma && rm -rf $HOME/.cargo/registry && cargo fetch 2>&1 | tail -5'
```

Expect it to finish with `Downloaded …` lines and **zero** `spurious network
error`. Reference run, 2026-08-20, three cold fetches back to back on
`devbox-smoltcp`, no `~/.cargo/config.toml` present:

```text
run 1: rc=0 spurious=0 downloaded=35
run 2: rc=0 spurious=0 downloaded=35
run 3: rc=0 spurious=0 downloaded=35
```

The syscall-level check, which does not need cargo at all and localises a
regression immediately:

```sh
for w in poll0 poll select epoll; do
    nettest-connect one index.crates.io 443 --wait $w --quiet
done
```

All four must report `verdict=CONNECTED`. `select` reporting
`revents=PRI|OUT -> HARDFAIL` while `poll` connects is this exact bug returning.

The kernel-side regression test is `run_pselect6_exceptfds_test`; a normal
`cargo run --release` boot prints `[PASS] pselect6_clears_exceptfds`.

And the connect deadline from § 3.3, which should end rather than hang:

```sh
nettest-connect one 10.255.255.1 443 --timeout-ms 30000   # ends ~10 s, so_error=110 ETIMEDOUT
```

## Background

- `userspace/nettest/README.md` § Part 3 — `nettest-connect`, the probe § 3.6
  runs, its mode/verdict tables, and the Linux control-arm reference output.
- [`../reference/subsystems/syscalls/poll.md`](../reference/subsystems/syscalls/poll.md)
  § `sys_pselect6` — the fixed `exceptfds` contract, as reference rather than
  narrative.
- [`../reference/subsystems/networking.md`](../reference/subsystems/networking.md)
  § `connect(2)` semantics — `CONNECT_TIMEOUT_US`, and why smoltcp's own
  `set_timeout()` is the wrong tool for it.
- [`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md) §9, §12 —
  the two lost-wake fixes on `benchmarks-improved-networking`. Re-testing on
  that kernel was the first thing tried here; it did **not** fix this, which is
  what produced the current reproducer to instrument.
- [`../archive/CARGO_CRATES_IO_CONNECT_FAIL.md`](../archive/CARGO_CRATES_IO_CONNECT_FAIL.md)
  — the 2026-08-11 investigation, kept verbatim with a corrected header. Its
  "root cause isolated" claim and its "connects never complete" reading are both
  wrong; step 2 above says how.
- [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md) §"cargo cannot
  reach crates.io" — the earlier, **superseded** multiplexing diagnosis.
- [`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) — `CARGO_NET_RETRY=20`,
  `CARGO_HTTP_MULTIPLEXING=false` and `CARGO_HTTP_TIMEOUT=120` all tried and all
  ineffective for this symptom.
- [`debug-network.md`](debug-network.md) — for when the network genuinely is down.
