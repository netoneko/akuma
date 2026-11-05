# Shanghai: the tutorial kernel

**Shanghai is a teaching kernel.** It is the smallest complete example of a kernel built on
[`akuma-tokio-kernel`](../crates/akuma-tokio-kernel/README.md): one short `main.rs`, a `Cargo.toml`, a `build.rs`, and a banner.
It runs unmodified Linux musl programs (busybox, sshd, tokio programs) on two machines from the same source: amd64 (PVH) and
aarch64 (device tree), under QEMU, Firecracker, or (amd64) Akuma's own hypervisor. Read it to learn how a kernel is put together here;
copy it to start your own.

It is not Akuma's main kernel and not a product. Nothing in it is a design to preserve. It exists so that the next kernel
(or the next agent asked to write one) has a working, gated, small thing to start from.

## Read it in this order

| file | what it teaches |
|---|---|
| [`src/main.rs`](src/main.rs) | the whole kernel: a name, a banner, and the devices it wants (`.console().disk().net().smp()`), then `run_init()` |
| [`Cargo.toml`](Cargo.toml) | one dependency, `akuma-tokio-kernel`; every other thing below is a **feature**, off by default |
| [`build.rs`](build.rs) | how the linker is told about the machine (a script on amd64, a boot stack size on aarch64) |
| [`src/banner.txt`](src/banner.txt) | the console banner (plain ASCII, within the size the console allows) |
| [`src/middleware.rs`](src/middleware.rs) | the **second lesson**: one table of syscall hooks, with every tier shown by one entry (feature `middleware`) |
| [`MIDDLEWARE_COMPOSITION.md`](MIDDLEWARE_COMPOSITION.md) | how two middleware crates compose into the one table; the design and what is built |
| [`RESULT.md`](RESULT.md) | the record of the original build, gate by gate |

The brief that explains `main.rs` line by line, how to build, boot and run tokio on it, is the
[`akuma-tokio-kernel` README](../crates/akuma-tokio-kernel/README.md). This README is only the map.

## Lesson 1: a kernel is a handful of lines

```rust
akuma_tokio_kernel::entry!(main);

fn main(kernel: akuma_tokio_kernel::Kernel) -> ! {
    kernel.name("Shanghai").banner(include_str!("banner.txt")).console().disk().net().smp().run_init()
}
```

Build it for either machine and boot it with an `init=` on the command line:

```sh
cargo build -p shanghai --target x86_64-unknown-none  --release     # amd64
cargo build -p shanghai --target aarch64-unknown-none --release     # aarch64
```

## Lesson 2: features are how a kernel grows, and every one is off by default

Nothing below is in the default Shanghai. Each is a Cargo feature, there is no runtime switch, and a kernel built without one
carries none of its code.

| feature | what it adds | where the code is |
|---|---|---|
| `middleware` | the sample hook table: a syscall of our own, a refusal, a result rewrite, a lock-free counter, and boot parameters (`mw.sample.*`) | [`src/middleware.rs`](src/middleware.rs) |
| `no-dynamic-loading` | refuse dynamically linked programs and every way of turning a file into code (implies `middleware` and `no-dynamic-elf`) | the crate [`akuma-mw-no-dynamic-loading`](../crates/akuma-mw-no-dynamic-loading) |
| `signed-exec` | signed code only: an image runs if its SHA-256 is allowlisted for its path or a detached `.sig` verifies under a scoped key (implies `middleware`) | the crate [`akuma-mw-signed-exec`](../crates/akuma-mw-signed-exec) |
| `no-dynamic-elf` | the build-time version of the dynamic-image ban: the ELF crate says no, with no hook and no table | `akuma-elf-exec`'s `refuse-dynamic` |

The two policy crates are the **worked example of a middleware that lives outside the kernel using it**: plain functions, no
table of their own (there is one table per kernel and nothing chains), host-tested, composed by the kernel in
[`src/middleware.rs`](src/middleware.rs) where the composition can be read. The guide for the mechanism is
[`crates/akuma-tokio-kernel-core/docs/middleware.md`](../crates/akuma-tokio-kernel-core/docs/middleware.md).

## Try it: the gate

```sh
SMP=4 sh shanghai/mw_gate.sh all      # needs e2fsprogs; network on first use (Alpine's musl, busybox, xz)
```

It builds the kernels, boots each on both machines under QEMU TCG, runs [`mwprobe`](mwprobe/mwprobe.rs) as init, and judges
each by the probe's own result line. `x86_64` or `aarch64` instead of `all` runs one machine.

## What Shanghai is not

- **Not hardened.** The bans are demonstrations of a mechanism, and each avenue they do not close is listed in the guide.
- **Not minimal in size.** It is minimal in *code you write*; the kernel under it is the whole `akuma-tokio-kernel` stack.
- **Not stable.** Names, features and the middleware API change as the lessons do. The guide and the code win over this page.
