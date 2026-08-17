# nettest — guest-side network client probes

Three probes live here. They exist for two different investigations, share
nothing but a directory, and are built by two different scripts.

| Probe | Directory | Client stack | Investigation | Build |
|---|---|---|---|---|
| `nettest` | `rust/` | libcurl (vendored, static OpenSSL + nghttp2) | cargo-vs-curl HTTPS divergence | `rust/build.sh` (Alpine docker) |
| `nettest-std` | `rust/stdlib/` | `std::net` + `poll(2)` + sync rustls — no runtime | delayed first byte | `rust/build-musl.sh` (host cross) |
| `nettest-reqwest` | `rust/reqwest/` | tokio + hyper 1.x + reqwest 0.12 + rustls — nca's stack | delayed first byte | `rust/build-musl.sh` (host cross) |

---

# Part 1 — `nettest-std` / `nettest-reqwest`: the delayed-first-byte bisect

A guest client hangs when the server takes more than a few seconds to send its
**first** response byte, while the identical request answered within ~1 s
streams perfectly
([`docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`](../../docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md)).
The only client that reproduced it was nca — tokio + hyper + reqwest + rustls +
an agent loop — so the investigation could not say *which layer* stalls.

**Outcome (2026-08-17): four kernel defects found and fixed.** A blocking TCP
read capped at 30 s and a write at 5 s (spurious `ETIMEDOUT`);
`SO_RCVTIMEO`/`SO_SNDTIMEO` accepted and silently dropped; the `EPOLLET` write
edge never re-armed; and — the dominant one — a socket still in `SynSent`
reported as read-closed (`EPOLLIN` + `EPOLLRDHUP`, `recv() == Ok(0)`), which
made a tokio client park forever without ever sending its request. Details in
[`docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`](../../docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md)
§ Resolution. The probes stay as the regression harness — in particular
`nettest-reqwest post <url> 64` repeated a dozen times, which is what caught the
two races.

These two probes cut that stack into axes that can be tested one at a time:

| probe / mode | sockets | HTTP | TLS | isolates |
|---|---|---|---|---|
| `nettest-std raw` | blocking `std::net` | hand-rolled | none | the kernel's blocking recv path (`socket_recv` → `wait_until`) |
| `nettest-std poll` | nonblocking + `poll(2)` | hand-rolled | none | readiness reporting without epoll (`sys_ppoll`) |
| `nettest-std tls` | blocking `std::net` | hand-rolled | rustls (sync) | rustls without an async runtime |
| `nettest-reqwest get` | tokio/mio + `epoll_pwait` | hyper 1.x | rustls (async) | nca's whole stack |

Both print the same `[probe]` line vocabulary, so their output diffs directly.

## Build and run

```bash
# host: cross-build both -> bootstrap/bin/nettest-{std,reqwest}
userspace/nettest/rust/build-musl.sh
scripts/populate_disk.sh                    # -> /bin in the image

# host: the timing server the probes measure against
scripts/net_delay_server.py --port 18080 --verbose
```

In the guest (`10.0.2.2` is the host over SLIRP — no `hostfwd` rule needed):

```
nettest-std     sweep    http://10.0.2.2:18080          # delay ladder, blocking
nettest-std     sweep    http://10.0.2.2:18080 0,5,35 poll
nettest-std     gap      http://10.0.2.2:18080 0 20     # first byte fast, 20 s mid-stream idle
nettest-std     rcvtimeo http://10.0.2.2:18080/delay/30 2
nettest-std     tls      https://example.com/
nettest-reqwest sweep    http://10.0.2.2:18080
nettest-reqwest stream   http://10.0.2.2:18080/sse/1/10
nettest-reqwest post     http://10.0.2.2:18080/delay/10 64
```

Both probes also build for the **development host**
(`cargo build --release --target "$(rustc -vV | grep '^host:' | cut -d' ' -f2)"`).
Run them there against the same delay server first: that is the control. A
sweep that fails in the guest and passes on the host localises the fault to the
kernel; one that fails on both is a probe or server bug.

The step-by-step procedure, the result matrix, and the kernel traces to collect
are in
[`docs/runbooks/debug-delayed-first-byte.md`](../../docs/runbooks/debug-delayed-first-byte.md).
The audit these probes were designed against — poll drivers, RX buffering,
readiness predicates, and the kernel's undeclared timeouts — is
[`docs/reference/subsystems/networking.md`](../../docs/reference/subsystems/networking.md)
§ "The native data path".

## Why these two are cross-built on the host, not in docker

`rust/build-musl.sh` uses `aarch64-unknown-linux-musl` +
`aarch64-linux-musl-gcc` — the **same toolchain `userspace/nca` uses for nca
itself** (`userspace/nca/build.rs`). That is the design constraint, not a
convenience: "nca hangs but the probe does not" is only informative if the two
binaries came out of the same compiler against the same libc. The `reqwest`
probe's dependency line is copied verbatim from nca's
`[workspace.dependencies]` for the same reason.

The curl probe below has the opposite requirement (match apk/nightly cargo's
libcurl), which is why it keeps its own container build.

---

# Part 2 — `nettest`: the cargo-vs-curl HTTPS divergence probe

Why this exists, what it tests, and how to run it.
The root-cause analysis lives in
[`docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md`](../../docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md).

## TL;DR

`cargo fetch` inside a devbox-smoltcp VM fails with
`[7] Could not connect to index.crates.io:443 after ~300 ms` while `curl https://index.crates.io`
in the same shell returns 200 in ~300 ms. The probe distills cargo's exact libcurl
client into a 4-mode binary so we can bisect what cargo does that curl does not.

## Why a probe

cargo's HTTPS path (master `rust-lang/cargo`, `src/cargo/sources/registry/http_remote.rs`
→ `src/cargo/util/network/http_async.rs` + `http.rs`) is more than just "use libcurl":

| Thing cargo does                                       | curl CLI does     |
|--------------------------------------------------------|-------------------|
| `curl::multi::Multi` driven from a worker pthread      | single Easy handle in main thread |
| `multi.pipelining(false, /*multiplex=*/true)`          | no Multi at all |
| `multi.set_max_host_connections(2)`                    | n/a |
| per-handle `http_version(HttpVersion::V2)`             | HTTP/1.1 unless `--http2` |
| per-handle `pipewait(true)`                            | off |
| apk libcurl + OpenSSL + nghttp2 + c-ares               | static mbedTLS (in `/bin/curl`) |

The four modes toggle these axes independently so a single run tells you which axis
triggers the kernel bug.

## Build

```bash
# Host: docker (Alpine arm64). Produces bootstrap/bin/nettest.
userspace/nettest/rust/build.sh
```

`nettest` is a Linux/musl binary, NOT a no_std kernel binary, so it is **not** a member
of `userspace/Cargo.toml`'s workspace. The standalone `[workspace]` table in
`userspace/nettest/rust/Cargo.toml` and the `.cargo/config.toml` override are load-bearing
— without them the build inherits the kernel target (`aarch64-unknown-none`) and fails
to find `std`.

The build links dynamically against apk's `libcurl.so.4` + `libssl.so.3` +
`libnghttp2.so.14` + `libcares.so.2` — the exact sonames apk-installed cargo links
against inside the VM. This is intentional: a statically-bundled libcurl would defeat
the comparison.

## Run (inside the VM, after `populate_disk.sh` has shipped the binary to `/bin/nettest`)

```
# baseline — should always work (mirrors /bin/curl CLI)
nettest easy11
nettest easy11 https://index.crates.io/config.json

# cargo-pattern variants
nettest easy2    https://index.crates.io/config.json
nettest multi11  https://index.crates.io/config.json
nettest multi2   https://index.crates.io/config.json   # cargo's exact setup

# big payload — compare against the user's curl-downloaded flac
nettest easy2    https://example.com/big-file.bin
nettest multi2   https://example.com/big-file.bin
```

Every mode prints `[nettest] mode=… OK status=… body=…B perform=…s total=…s` on success
or `[nettest] mode=… FAIL after …s: <curl error>` on failure. libcurl verbose output
(`* Trying IP:port…`, `* SSL connection using TLS…`, `* CONNECTED …`) goes to stderr
so you can watch exactly where each mode dies.

## Reading the results

| `easy11` | `easy2` | `multi11` | `multi2` | Diagnosis |
|----------|---------|-----------|----------|-----------|
| OK       | OK      | OK        | **FAIL** | Hypothesis H2 confirmed: HTTP/2 multiplexing specifically triggers the kernel bug. The existing `CARGO_HTTP_MULTIPLEXING=false` workaround is the correct fix. |
| OK       | OK      | **FAIL**  | **FAIL** | H1/H3/H4 confirmed: any Multi+worker pattern breaks, regardless of multiplexing. The `CARGO_HTTP_MULTIPLEXING=false` workaround is incomplete; the bug is in the kernel's `Multi::wait` / `poll` / worker-thread path. |
| OK       | **FAIL**| **FAIL**  | **FAIL** | HTTP/2 itself is the trigger (TLS ALPN h2). Multiplexing is downstream of that. |
| OK       | OK      | OK        | OK       | Probe did not reproduce — try larger payloads or repeat the failing `cargo fetch` and compare verbose output. |

See [`docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md`](../../docs/archive/CARGO_CRATES_IO_CONNECT_FAIL.md)
§ "Hypothesis" for the full H1–H4 definitions.

## What to capture when it fails

For each failing mode, grab:

1. The probe's stdout/stderr (it carries libcurl's `*`-prefixed verbose trace).
2. The kernel serial log around the failure — filter for
   `[syscall] connect(fd=N, ip=A.B.C.D:443)` and the matching `= OK` / `= -ERR` line.
   `src/syscall/net.rs:306` is the log site.
3. `curl -v https://index.crates.io/config.json` from the same shell, for the diff.

That triad is enough to identify which hypothesis above is correct.
