# nca — native-cli-ai for Akuma

Akuma build wrapper for [native-cli-ai](https://github.com/netoneko/native-cli-ai)
(`v0.4.0-akuma` branch — the user's personal fork carrying the Akuma-specific
patches directly as commits; see that branch's own log rather than
`upstream-akuma-patches.patch`, which was a pre-fork snapshot mechanism and has
been removed).

`nca` is an AI CLI assistant (tokio + reqwest + ratatui). This wrapper cross-compiles it
for `aarch64-unknown-linux-musl` and installs it to `bootstrap/bin/nca`. It can also be
built natively *inside* Akuma (self-hosted) — currently blocked, see
[Building inside Akuma](#building-inside-akuma-self-hosted-currently-blocked) below.

## Submodule

```
userspace/nca/native-cli-ai  →  github.com/netoneko/native-cli-ai@v0.4.0-akuma
```

Init after cloning:

```bash
git submodule update --init userspace/nca/native-cli-ai
```

## Build

```bash
userspace/build.sh --nca-only
```

Requires the musl AArch64 cross toolchain (`aarch64-linux-musl-gcc`) and
the `aarch64-unknown-linux-musl` Rust target:

```bash
rustup target add aarch64-unknown-linux-musl
```

**Gotcha, found 2026-08-18:** `userspace/nca/build.rs` declares
`cargo:rerun-if-changed` for only 4 specific files (`build.rs`,
`native-cli-ai/Cargo.toml`, `native-cli-ai/crates/cli/src/main.rs`,
`native-cli-ai/crates/core/src/lib.rs`) — declaring *any* `rerun-if-changed`
disables cargo's default "rerun if anything in the package changed"
behaviour, so after the first build, `userspace/build.sh --nca-only` silently
does **nothing** (prints "Finished in 0.0Xs" and leaves the stale binary in
place) for any change outside those 4 files — which is most of them,
including everything under `crates/tui/`. Nothing errors; you just get an old
binary. Until `build.rs` is fixed, build the inner crate directly instead:

```bash
cd userspace/nca/native-cli-ai
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc \
CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc \
CXX_aarch64_unknown_linux_musl=aarch64-linux-musl-g++ \
AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar \
RUSTFLAGS="-C opt-level=3 -C lto=fat -C codegen-units=1 -C panic=abort -C overflow-checks=off -C target-feature=+neon,+fp16,+dotprod -C link-arg=-static" \
cargo build --release --no-default-features --target aarch64-unknown-linux-musl -p nca-cli
# then copy + strip into bootstrap/bin/nca yourself:
cp target/aarch64-unknown-linux-musl/release/nca ../../../bootstrap/bin/nca
aarch64-linux-musl-strip ../../../bootstrap/bin/nca
```

(matches the flags/env `build.rs` itself uses — see its source for the
authoritative list if this drifts).

## Build flags

| Flag | Value | Reason |
|------|-------|--------|
| `opt-level` | `3` | Speed (nca has a hot inference loop; size is secondary) |
| `lto` | `fat` | Full cross-crate inlining — upstream uses `thin` |
| `codegen-units` | `1` | Required for fat LTO |
| `panic` | `abort` | No unwinding overhead |
| `target-feature` | `+neon,+fp16,+dotprod` | All SIMD extensions on qemu-virt |
| link | `-static` | No dynamic loader on Akuma |

## Clipboard

`arboard` (system clipboard) is disabled (`--no-default-features`) — nca runs over
SSH on Akuma and there is no display server. The `/image paste` command will return
a "clipboard not available" error; `/image <path>` (file import) still works.

## Memory estimate

Run on Akuma after boot:

```bash
nca --help          # baseline RSS
/usr/bin/top        # watch RSS while running a prompt
```

Compare against `meow` to decide which to ship.

## Deploying a fresh binary to a live VM

Do **not** write `disk.img`/`devbox.img` under a running QEMU (a live guest
holds the image open; `scripts/populate_disk.sh` under it corrupts the disk —
same rule as `userspace/ncaprobe`'s README). Two SSH-based paths were tried
this session; only one works reliably:

- **`scp` / sftp: does not work.** This sshd doesn't implement the SFTP
  subsystem; the client hangs indefinitely (`Timeout, server localhost not
  responding` after ~60s once it gives up). Kill the stray client if you try
  it — it won't recover on its own.
- **`ssh ... 'cat > file' < localfile` (piping the binary through an exec
  channel's stdin): unreliable for anything past ~1 MiB** — reproducibly
  stalled at exactly 1,048,576 bytes twice in a row before the connection
  dropped. Root cause not investigated; smells like a fixed buffer/window
  somewhere in the exec-channel path, not a general throughput problem.
- **HTTP + `curl`: works, fast, use this one.** QEMU's `-netdev user` (SLIRP)
  always makes the host reachable from the guest at `10.0.2.2`, no port
  forward needed for guest→host:

  ```bash
  # host: serve the freshly built binary
  mkdir -p /tmp/nca_serve && cp bootstrap/bin/nca /tmp/nca_serve/
  (cd /tmp/nca_serve && python3 -m http.server 8765 --bind 0.0.0.0) &

  # guest: fetch, verify, swap in
  ssh -p 2222 root@localhost \
    'curl -s -o /usr/local/bin/nca.new http://10.0.2.2:8765/nca && chmod +x /usr/local/bin/nca.new'
  # compare `shasum -a 256` on both ends before swapping, then:
  ssh -p 2222 root@localhost \
    'mv /usr/local/bin/nca /usr/local/bin/nca.bak && mv /usr/local/bin/nca.new /usr/local/bin/nca'
  ```

  `userspace/ncaprobe/build-musl.sh --serve` already wraps this exact pattern
  for that tool; nca has no equivalent script yet. The running `nca` process
  keeps using the *old* binary in memory until it's restarted (`Ctrl+X Q`,
  relaunch) — swapping the file doesn't hot-reload it.

## Building inside Akuma (self-hosted, currently blocked)

Tried 2026-08-18 with a full nightly Rust toolchain already installed on the
guest disk (`scripts/populate_disk.sh --with-rust-toolchain`) and the
project's source staged at `/tmp/native-cli-ai`. Fails immediately, on the
very first proc-macro build script:

```
error: could not compile `proc-macro2` (build script)
Caused by:
  could not execute process `rustc --crate-name build_script_build ...` (never executed)
Caused by:
  Bad address (os error 14)
```

This is a known, still-open Akuma kernel issue, not specific to nca — see
[`../../docs/archive/NCA_MISSING_SYSCALLS.md`](../../docs/archive/NCA_MISSING_SYSCALLS.md)
§1 for the existing investigation (ruled out so far: argv/envp size,
`env_clear`, `current_dir`, threaded spawn, and a kernel-returned `EFAULT` —
the kernel logs zero `EFAULT`s while cargo reports the error, and no
`[syscall] execve` line appears for the failing spawn at all, so the errno is
born either pre-`execve` in the child-side `chdir`/`dup2`/`CLOEXEC`-pipe setup,
or in userspace). Full self-host of nca is blocked on that being root-caused;
cross-compiling from the host (`## Build` above) remains the only working
path. Detailed writeup, including the exact staged-source caveat (that
snapshot predates the `crates/tui` split — see `AKUMA_BUILD.md` in the
submodule for what was actually staged) and full command transcript:
[`native-cli-ai/AKUMA_BUILD.md`](native-cli-ai/AKUMA_BUILD.md).
