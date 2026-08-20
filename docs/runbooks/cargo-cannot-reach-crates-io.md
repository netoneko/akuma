# In-guest `cargo` cannot reach crates.io

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
| `/usr/local/bin/cargo` (nightly) | **This is the broken one.** Go to step 2. |
| `/usr/bin/cargo` (apk, 1.96.x) | Works — 39/39 fetches in the reference run. If it fails, the network really is down; go to [`debug-network.md`](debug-network.md). |

Since 2026-08-19 the devbox puts nightly first on `PATH`, so a bare `cargo` is
the failing one by default.

## 2. Confirm it is the known bug, not your network

```sh
curl -o /dev/null -w '%{http_code}\n' https://index.crates.io/config.json   # expect 200
/usr/bin/cargo fetch                                                        # expect success
```

If `curl` returns `200` and apk cargo fetches while nightly cargo cannot, this
is the known bug. The **observation** is solid: the kernel log shows 110
`socket()` + `connect() = EINPROGRESS` cycles with **zero** completions across a
30 s nightly-cargo run, while apk libcurl issues the same syscalls to the same
IPs and connects fine. DNS is not the problem; the A records resolve and appear
in the log.

The **explanation** is not settled, and two things should stop you treating it as
settled:

- **No probe reproduces it.** All four `nettest` modes — including `multi2`, which
  is cargo's exact Multi + multiplex + worker-thread pattern — pass 30/30. The
  only reproducer is cargo itself. The probe links apk libcurl, so it has never
  exercised the vendored build (step 5).
- **The ~300 ms give-up time contradicts "connects never complete."** A connect
  that hangs burns `CURLOPT_CONNECTTIMEOUT`, not 353 ms. Failing in roughly one
  successful round trip is the signature of an **error being returned** —
  `POLLERR`, `ECONNRESET`, `EHOSTUNREACH` — not of silence. Nothing has yet
  reconciled the timing with the stated mechanism. (`CURL_HEET_DEFAULT_QUEUESIZE`,
  cited in the archive doc for the ~300 ms spacing, does not govern connect
  timing; happy-eyeballs is `CURLOPT_HAPPY_EYEBALLS_TIMEOUT_MS`, default 200 ms.)

Both gaps mean the *class* of bug may still be open. The workarounds in step 4
are unaffected either way.

The second gap is now closed on paper — see
[§ 3. The mechanism, traced through source](#3-the-mechanism-traced-through-source-2026-08-20).
The failing connects are being **refused**, not ignored, and the trace narrows
the possible causes to three, each with a distinct `poll` fingerprint you can
read off one probe run.

## 3. The mechanism, traced through source (2026-08-20)

Nothing below was measured in a VM. It is a read of the two code bases that
meet at this failure — the vendored libcurl the nightly toolchain links
(`curl-sys-0.4.90+curl-8.21.0`, unpacked in `~/.cargo/registry`) and the
kernel's readiness path — and it settles the timing contradiction in step 2 on
its own.

### 3.1 The 353 ms is a refusal, not a hang

`cf-socket.c`, `cf_tcp_connect()`, is the whole decision:

```c
rc = SOCKET_WRITABLE(ctx->sock, 0);          /* poll(POLLOUT), timeout 0 */
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

### 3.3 A `SynSent` socket cannot die on its own

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

### 3.4 Therefore: three candidate origins, and `revents` tells them apart

A `POLLHUP` on a *live connecting fd* within one RTT can only be:

| `revents` | origin | `SO_ERROR` reads |
|---|---|---|
| `HUP` alone | smoltcp reached `!is_active()`: **an RST actually arrived**, or the handle was recycled under it | `ECONNREFUSED`, synthesised from state (`src/syscall/net.rs:797`) |
| `ERR\|HUP` | `current_process_shared()` / `get_fd()` returned `None` (`src/syscall/poll.rs:466`) — **no socket state was consulted at all** | `0` (`net.rs:810`, the fd-miss arm) |
| `OUT` with `ERR`/`HUP` | impossible on Akuma — you are reading the Linux control arm | — |

The middle row is the one that would explain why *only* nightly cargo
reproduces this with nothing wrong on the wire. It is thread-shaped:
`current_pid()` (`crates/akuma-exec/src/process/children.rs:693`) resolves via
`THREAD_PID_MAP` first and the ProcessInfo page second, and the nightly
toolchain's libcurl uses the **threaded resolver** (`USE_RESOLV_THREADED` in
curl-sys' `build.rs`, compiling `asyn-thrdd.c`) — it spawns a pthread per
resolution, which apk libcurl's c-ares build never does. Every other difference
between the two libcurls has already been ruled out by experiment; this one has
not been tested.

The top row is not a kernel bug in the readiness path at all — it moves the
question to why a SYN is being reset (SLIRP, the peer, or a malformed segment).

### 3.5 Reading the answer off one run

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
| `SUMMARY he verdict=COULDNT_CONNECT` at a few hundred ms | reproduced without libcurl; read the `hint:` line for which row |
| `verdict=PENDING` at 5 s | the connects really do hang — then §3.1 says libcurl could not have reported this at 353 ms, and the divergence is somewhere else |
| `verdict=CONNECTED` throughout | not reproducible on this kernel; go to step 6 item 1 and close the whole thing |

Same static binary runs under Docker Linux as the control arm
(`../archive/LINUX_AB_PROBE.md`). Full probe documentation, including the
`--wait poll0|poll|select|epoll` bisect of `sys_ppoll` / `sys_pselect6` /
`sys_epoll_pwait`, is in `userspace/nettest/README.md` § Part 3.

## 4. Work around it

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

## 6. What would actually fix it

Ranked in `../archive/CARGO_CRATES_IO_CONNECT_FAIL.md` § "Fix options". Two cheap
steps come before any kernel change:

1. **Re-test on a current kernel.** The diagnosis dates from 2026-08-11, before the
   `benchmarks-improved-networking` work. That branch fixed two lost-wake bugs in
   the socket path (the NIC doorbell re-arm,
   `../archive/AKUMA_NET_ISSUES.md` §9, and the `blocking_relax` yield, §12) — and
   "`connect` → `EINPROGRESS` → `poll(POLLOUT)` never fires" is a lost-wake
   signature. Nobody has re-run the reproducer since. This is the cheapest
   experiment available and it may simply be fixed.
2. **Run `nettest-connect`** (§ 3.5) and read `revents` off the failing attempt.
   That picks one of the three rows in § 3.4 and turns the remaining work into a
   single question instead of a search.

   Do **not** start by rebuilding the `nettest` curl probe with `static-curl` —
   the archive doc names that as the cheapest next step and that text is stale.
   `userspace/nettest/rust/Cargo.toml` already carries
   `features = ["http2", "ssl", "static-curl", "static-ssl"]`, and the built
   binary (`bootstrap/bin/nettest`) contains vendored `curl/lib/vtls/openssl.c`
   plus the string `OpenSSL 3.6.3` — the same vendored OpenSSL the nightly
   toolchain reports. It was built twenty minutes *before* the commit that added
   the doc (`0c9d96ce`). The vendored probe exists and has shipped; what is
   unrecorded is whether the 30/30 pass was measured with it or with the earlier
   dynamic build.

Do **not** start by debugging the smoltcp stack. Four hypotheses were already
ruled out by experiment (HTTP/2 ALPN, c-ares DNS racing, worker-thread socket
ownership, Multi+pipewait multiplexing); the table is in that doc.

## Verify

On a freshly bootstrapped devbox image:

```sh
ssh -o StrictHostKeyChecking=no -p 2222 root@localhost 'cat /root/.cargo/config.toml'
```

Expect the `[net] retry = 20` / `[http] multiplexing = false` block above — that
confirms step 7c ran and the toolchain shipped with its network policy.

```sh
ssh ... '/usr/bin/cargo fetch && cargo build --release --offline'
```

Expect the fetch to succeed and the offline build to proceed without a single
`spurious network error` line. If nightly cargo is *not* offline and does emit
them, that is the known bug, not a regression.

## Background

- `userspace/nettest/README.md` § Part 3 — `nettest-connect`, the probe § 3.5
  runs, its mode/verdict tables, and the Linux control-arm reference output.
- [`../archive/AKUMA_NET_ISSUES.md`](../archive/AKUMA_NET_ISSUES.md) §9, §12 —
  the two lost-wake fixes on `benchmarks-improved-networking` that make step 6
  item 1 worth doing before anything else.
- [`../archive/CARGO_CRATES_IO_CONNECT_FAIL.md`](../archive/CARGO_CRATES_IO_CONNECT_FAIL.md)
  — the 2026-08-11 investigation: four ruled-out hypotheses and the cargo-only
  reproducer. Its header claims "root cause isolated"; read that as *observation
  isolated* — its own § "What the probe does NOT yet reproduce" concedes no
  controlled binary reproduces the failure. See step 2 above.
- [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md) §"cargo cannot
  reach crates.io" — the earlier, **superseded** multiplexing diagnosis.
- [`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) — `CARGO_NET_RETRY=20`,
  `CARGO_HTTP_MULTIPLEXING=false` and `CARGO_HTTP_TIMEOUT=120` all tried and all
  ineffective for this symptom.
- [`debug-network.md`](debug-network.md) — for when the network genuinely is down.
