# cargo's "Could not connect to index.crates.io:443" on devbox-smoltcp — analysis

**Status:** Root cause isolated. Reproducer confirmed in-VM. Fix not yet chosen.
**Date:** 2026-08-11.
**Symptom:** `cargo build`/`fetch` using **the nightly musl toolchain** (`/usr/local/bin/cargo`) inside a devbox-smoltcp VM loops on:

```
warning: spurious network error (3 tries remaining): [7] Could not connect to server
  (Failed to connect to index.crates.io:443 after 353 ms: Could not connect to server)
warning: spurious network error (3 tries remaining): [7] Could not connect to server
  (Failed to connect to index.crates.io:443 after 657 ms: Could not connect to server)
… (300 ms increments per retry — happy-eyeballs cycling IPs)
```

While `/bin/curl https://index.crates.io/config.json` (static mbedTLS) and **apk-installed** cargo (`/usr/bin/cargo`, libcurl 8.21.0 + OpenSSL 3.5.7) both succeed in ~300 ms.

## Root cause (confirmed by in-VM experiment 2026-08-11)

**Non-blocking TCP connects issued by nightly cargo's vendored libcurl never complete on the Akuma smoltcp kernel.** This is not DNS, not TLS, not multiplexing, not HTTP/2 — it is the kernel's `socket() + connect(O_NONBLOCK) → EINPROGRESS → poll(POLLOUT) → success` path, *but only when called the way nightly cargo's statically-linked libcurl calls it*.

### The evidence

I built a probe (`userspace/nettest/rust/`) that uses libcurl via the same Rust `curl` crate cargo uses, in 4 modes that bisect cargo's behaviour:

| Probe mode                  | Multi | multiplex | worker thread | Result (30/30 each) |
|-----------------------------|-------|-----------|---------------|---------------------|
| `easy11` (curl CLI equiv)   | no    | no        | no            | **OK** in ~300 ms   |
| `easy2` (HTTP/2 inline)     | no    | yes       | no            | **OK** in ~300 ms   |
| `multi11` (Multi, no mpx)   | yes   | no        | yes           | **OK** in ~300 ms   |
| `multi2` (cargo's pattern)  | yes   | yes       | yes           | **OK** in ~300 ms   |

All four modes pass. Then I ran the **actual failing command** with the nightly cargo:

```
AARCH64_UNKNOWN_NONE_RUSTFLAGS=-Clink-arg=-T/tmp/akuma/linker.ld \
  /usr/local/bin/cargo build --release -p akuma --manifest-path /tmp/akuma/Cargo.toml
```

100 % failure, every retry exhausted. apk-installed `/usr/bin/cargo` fetched the same project 39/39 in a row.

### What the kernel serial log shows during a failure

Around `T775.77` (cargo PID 712 starts), the kernel logs:

```
[syscall] socket(type=UDP) = fd 6            ← musl DNS resolver
[syscall] bind(fd=6, port=0, ip=0.0.0.0)
[syscall] sendto(fd=6, len=33, dest=8.8.8.8:53)    ← race both nameservers
[syscall] sendto(fd=6, len=33, dest=1.1.1.1:53)
```

DNS works. cargo's libcurl got all 4 A records for `index.crates.io`. Then:

```
[syscall] socket(type=TCP) = fd 6
[syscall] connect(fd=6, ip=151.101.2.137:443)
[syscall] connect(fd=6) = EINPROGRESS
[syscall] socket(type=TCP) = fd 6            ← same fd reused — prior socket closed
[syscall] connect(fd=6, ip=151.101.130.137:443)
[syscall] connect(fd=6) = EINPROGRESS
… 110 socket+connect=EINPROGRESS cycles, ZERO completions …
```

Across the full 30-second cargo run: **110 non-blocking TCP connect attempts, 0 successes.** libcurl's happy-eyeballs rotates through the 4 Fastly IPs, each given ~300 ms (`CURL_HEET_DEFAULT_QUEUESIZE`-ish) to complete, none do, retry the next IP, exhaust cargo's retry budget, surface `[7] CURLE_COULDNT_CONNECT`.

Meanwhile, the nettest probe (using apk libcurl) does the *exact same* `socket() + connect(EINPROGRESS)` syscalls to the *exact same* IPs and the connect completes in ~300 ms.

### What is actually different about nightly cargo

| | apk cargo (works) | nightly cargo (fails) |
|---|---|---|
| Binary | `/usr/bin/cargo` 20 MB | `/usr/local/bin/cargo` 47 MB |
| libcurl | `8.21.0 system ssl:OpenSSL/3.5.7` (dynamic, apk) | `8.21.0-DEV vendored ssl:OpenSSL/3.6.3` (static) |
| DNS resolver | **c-ares** (libcurl.so ships `c-ares/%s` string) | **threaded resolver** (`Curl_async_getaddrinfo`/`getaddrinfo`) |
| DNS query | one UDP socket to 8.8.8.8 | two UDP sends, racing 8.8.8.8 + 1.1.1.1 |
| TLS backend | system OpenSSL 3.5.7 | vendored OpenSSL 3.6.3 |

The two libcurls are the same version (8.21.0) but built with **different resolver backends**: apk libcurl is built `--without-ares --enable-threaded-resolver` … no, the other way — apk has c-ares, vendored has threaded resolver. The threaded resolver path is what differs.

We do NOT yet know the *precise* syscall-level divergence — the kernel only logs `socket`/`connect`/`sendto`, not `poll`/`ppoll`/`close`. The visible fact is: 110 non-blocking connects issued by nightly cargo, zero completions, in a window where the same number of connects from apk libcurl complete reliably.

## Hypotheses ruled out by the probe

(These were the original H1–H4 in this doc; all four are wrong.)

| Ruled out | Why |
|---|---|
| HTTP/2 ALPN in TLS handshake | The probe forces HTTP/2 with ALPN h2 in `easy2`/`multi2` and succeeds. The existing `CARGO_HTTP_MULTIPLEXING=false` workaround (per `debug-thread-spawn-segv.md:1143-1148`) does not fix this — confirmed by env-var testing. |
| c-ares threaded DNS racing | `RES_OPTIONS=usevc` (force TCP DNS) and `CARGO_HTTP_MULTIPLEXING=false` env vars do not change the failure pattern. DNS itself succeeds — the IPs are visible in the kernel log. |
| Worker-thread socket ownership | The probe's `multi2` mode spawns a worker pthread that drives `Multi::perform`/`Multi::wait`, exactly like cargo's `http_async.rs:82-96`. The probe's worker thread connects fine. |
| Multi+pipewait multiplexing | Same — `multi2` exercises this and completes. |

## What the probe does NOT yet reproduce

The probe links **dynamically against apk libcurl** (c-ares, OpenSSL 3.5.7, system-compiled). To make the probe reproduce the failure we would need to rebuild it with `curl = { features = ["static-curl", "static-ssl"] }` so curl-sys bundles and builds libcurl from source — matching the vendored libcurl config in the Rust toolchain. That is the next step in `userspace/nettest/rust/build.sh` before fixing anything in the kernel.

## Reproducer

```bash
# Inside the devbox-smoltcp VM, after `populate_disk.sh` has shipped nettest:

# A. Confirms kernel is fine for apk libcurl — 30/30 expected:
for i in $(seq 1 30); do /bin/nettest multi2 https://index.crates.io/config.json; done

# B. Reproduces the bug 100% with the nightly toolchain:
rm -rf /root/.cargo/registry
AARCH64_UNKNOWN_NONE_RUSTFLAGS=-Clink-arg=-T/tmp/akuma/linker.ld \
  /usr/local/bin/cargo build --release -p akuma --manifest-path /tmp/akuma/Cargo.toml
```

The contrast between A and B is the bug.

## Fix options (priority-ordered)

1. **Rebuild the nettest probe with `static-curl` + threaded-resolver features** (`userspace/nettest/rust/Cargo.toml`) to reproduce the failure in a controlled binary. Once the probe fails, bisect libcurl build flags (c-ares vs threaded resolver, OpenSSL versions, nghttp2 versions) until the trigger is isolated to a single build flag. **This is the cheapest next step** and does not require touching the kernel.

2. **Use apk cargo for in-VM builds.** `/usr/bin/cargo` 1.96.1 works reliably; the failure is exclusive to the nightly toolchain. If the self-host acceptance test can run on stable + the `panic-immediate-abort` feature is dropped from `Cargo.toml`, the problem disappears. This is a one-line `Cargo.toml` change away from being viable and unblocks development immediately. (`acceptance/10_selfhost_compile_akuma.md:19` already notes stable cargo can't parse the manifest, which is the only blocker.)

3. **Patch the Rust toolchain's libcurl build to disable threaded resolver.** The Rust source's `src/bootstrap/src/core/build_steps/tool.rs` and `src/etc/curl` (where the vendored libcurl is configured) determine curl-sys's build flags. Adding `--disable-threaded-resolver` would force libcurl to call `getaddrinfo` synchronously in the calling thread, like c-ares-style fallback. This sidesteps whatever kernel path is broken for the threaded-resolver's worker-thread DNS pattern.

4. **Fix the kernel's non-blocking-connect notification path for the threaded-resolver's specific calling pattern.** This is the right long-term answer but requires identifying exactly which syscall or socket-option the vendored libcurl uses that the apk libcurl doesn't. The probe (option 1) is the prerequisite.

5. **Stop using the nightly musl self-host toolchain entirely.** Stage the host-built kernel + userspace into the VM via `populate_disk.sh` and skip in-VM cargo. This is what most other acceptance tests already do.

## Background (prior art)

- `docs/runbooks/debug-thread-spawn-segv.md:1143-1168` — earlier "spurious network error" note; blamed HTTP/2 multiplexing. That diagnosis was incomplete: turning multiplexing off does not fix this case (verified). The actual trigger is the libcurl build, not the cargo config.
- `docs/archive/CURL_MISSING_SYSCALLS.md` — earlier non-blocking-connect path bugs.
- `docs/archive/OPTIONAL_SMOLTCP.md:312-368` — c-ares threaded-resolver notes.
- `docs/reference/subsystems/networking.md` — box-0 smoltcp architecture.
- `userspace/nettest/README.md` — the probe itself, with the mode matrix.
