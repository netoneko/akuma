# nca — native-cli-ai for Akuma

Akuma build wrapper for [native-cli-ai](https://github.com/netoneko/native-cli-ai) (dev branch).

`nca` is an AI CLI assistant (tokio + reqwest + ratatui). This wrapper cross-compiles it
for `aarch64-unknown-linux-musl` and installs it to `bootstrap/bin/nca`.

## Submodule

```
userspace/nca/native-cli-ai  →  github.com/netoneko/native-cli-ai@dev
```

Init after cloning:

```bash
git submodule update --init userspace/nca/native-cli-ai
```

## Build

```bash
cd userspace
cargo build --release -p native-cli-ai
# or via build.sh:
./build.sh --native-cli-ai-only
```

Requires the musl AArch64 cross toolchain (`aarch64-linux-musl-gcc`) and
the `aarch64-unknown-linux-musl` Rust target:

```bash
rustup target add aarch64-unknown-linux-musl
```

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
