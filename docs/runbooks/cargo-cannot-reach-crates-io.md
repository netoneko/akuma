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
is the known bug: **non-blocking TCP connects issued by nightly cargo's
vendored libcurl never complete on the smoltcp kernel.** The kernel log shows
the signature — 110 `socket()` + `connect() = EINPROGRESS` cycles with **zero**
completions, happy-eyeballs rotating the four Fastly IPs at ~300 ms each.

DNS is not the problem; the A records resolve and appear in the log.

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

Ranked in `../archive/CARGO_CRATES_IO_CONNECT_FAIL.md` § "Fix options". The
cheapest next step is **not** a kernel change: rebuild the nettest probe with
`static-curl` + `static-ssl` so it links the same vendored libcurl the nightly
toolchain does, and bisect build flags until the trigger is one flag. The
current probe links apk libcurl (c-ares) and therefore passes all four modes —
it does not reproduce the bug yet.

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
  — root cause isolated 2026-08-11, fix not yet chosen. The four ruled-out
  hypotheses and the reproducer live here.
- [`debug-thread-spawn-segv.md`](debug-thread-spawn-segv.md) §"cargo cannot
  reach crates.io" — the earlier, **superseded** multiplexing diagnosis.
- [`../archive/DEVBOX_ISSUES.md`](../archive/DEVBOX_ISSUES.md) — `CARGO_NET_RETRY=20`,
  `CARGO_HTTP_MULTIPLEXING=false` and `CARGO_HTTP_TIMEOUT=120` all tried and all
  ineffective for this symptom.
- [`debug-network.md`](debug-network.md) — for when the network genuinely is down.
