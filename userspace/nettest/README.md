# nettest — cargo-vs-curl HTTPS divergence probe

Why this exists, what it tests, and how to run it, all in one place.
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
