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

Both gaps mean the *class* of bug may still be open. The workarounds in step 3
are unaffected either way.

## 3. Work around it

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

## 4. The config the bootstrap installs (and what it is actually for)

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

## 5. What would actually fix it

Ranked in `../archive/CARGO_CRATES_IO_CONNECT_FAIL.md` § "Fix options". Two cheap
steps come before any kernel change:

1. **Re-test on a current kernel.** The diagnosis dates from 2026-08-11, before the
   `benchmarks-improved-networking` work. That branch fixed two lost-wake bugs in
   the socket path (the NIC doorbell re-arm,
   `../archive/AKUMA_NET_ISSUES.md` §9, and the `blocking_relax` yield, §12) — and
   "`connect` → `EINPROGRESS` → `poll(POLLOUT)` never fires" is a lost-wake
   signature. Nobody has re-run the reproducer since. This is the cheapest
   experiment available and it may simply be fixed.
2. **Rebuild the nettest probe with `static-curl` + `static-ssl`** so it links the
   same vendored libcurl the nightly toolchain does, then bisect build flags until
   the trigger is one flag. The current probe links apk libcurl (c-ares) and
   passes all four modes, so it does not reproduce the bug — which is also why the
   stated root cause is not yet confirmed (step 2).

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
