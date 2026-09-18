# Stage the nightly Rust toolchain in the Akuma/amd64 Firecracker guest

**Stability: B.** The procedure is reliable; what the toolchain can then *do* in
the guest is moving — see
[`docs/archive/RUST_TOOLCHAIN_AMD64.md`](../archive/RUST_TOOLCHAIN_AMD64.md).

Puts a nightly `rustc` + `cargo` inside the amd64 guest's ext2 root image so the
guest can compile Rust. Runs entirely on the trashcan's **Ubuntu** personality;
the dev Mac is not involved and the box is not rebooted.

## Why nightly, and why musl

* **Nightly**, because the kernel's own `Cargo.toml` opens with
  `cargo-features = ["panic-immediate-abort"]` and stable cargo cannot parse the
  manifest at all. Alpine's `apk add rust cargo` ships stable, which is why this
  is not that.
* **musl host**, because Akuma's userspace is musl. A gnu-host `rustc` will not
  run. rustup calls it a non-host toolchain (the box's own host is gnu) and
  needs `--force-non-host` to install it.

## Procedure

Everything below runs as root on Ubuntu at `192.168.1.123:22` — through
`python3 scripts/utils/hpbox.py ub '<cmd>'`, or `hpbox.ubuntu()` from Python.
**Never plain `ssh root@192.168.1.123`**: that alias reaches Akuma
([the loop runbook](amd64-bare-metal-loop.md)).

### 1. Install the toolchain on the box

```bash
export PATH=$HOME/.cargo/bin:$PATH
rustup toolchain install nightly-x86_64-unknown-linux-musl \
    --profile minimal --force-non-host
du -sh ~/.rustup/toolchains/nightly-x86_64-unknown-linux-musl   # ~777M, 161 files
```

`--profile minimal` is `rustc` + `cargo` + that triple's `rust-std`, and
includes `rust-lld` under `lib/rustlib/<triple>/bin/`, which is the only linker
the image has. Add `rust-src` only if you intend `-Z build-std`.

### 2. Stop the VM before touching its disk

The image is attached read-write. Mounting it under a live Firecracker corrupts
it.

```bash
for p in $(pgrep -x firecracker); do kill -9 $p; done
```

`pgrep -x firecracker`, never `pkill -f` — the pattern would match the script you
just sent over ssh and kill your own session.

### 3. Copy it into the image

```bash
IMG=/root/akuma/target/x86_64-unknown-none/release/amd64-root.img
T=/root/.rustup/toolchains/nightly-x86_64-unknown-linux-musl
mkdir -p /mnt/akuma-img && mount -o loop $IMG /mnt/akuma-img
mkdir -p /mnt/akuma-img/usr/local
rm -rf /mnt/akuma-img/usr/local/rust
cp -a $T /mnt/akuma-img/usr/local/rust
sync && umount /mnt/akuma-img
```

Loop-mount and `cp -a`, not `debugfs -w -R write` per file: `amd64/mkdisk.sh`
uses debugfs because it must work unprivileged on macOS and places a handful of
files. This is the box, it is root, and 161 files through one debugfs
invocation each is not worth it.

**The image must have room.** `amd64/mkdisk.sh <img> <size-mib>` — 4096 leaves
~2.8 GiB free after the toolchain. The default is 512, which does not fit.

### 4. Restart the VM

```bash
setsid nohup firecracker --no-api --config-file /tmp/hpbox-fc.json \
    --api-sock /tmp/hpbox-fc.sock > /tmp/hpbox-fc.log 2>&1 < /dev/null &
```

## Verify

From the box (the guest is at `10.0.2.15:2222` on `tap0`):

```bash
ssh -i /root/akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key \
    -p 2222 root@10.0.2.15 \
    'busybox env LD_LIBRARY_PATH=/usr/local/rust/lib /usr/local/rust/bin/rustc --version'
# rustc 1.100.0-nightly (0fc141305 2026-09-11)
```

And a full compile-link-run, which is the check that matters:

```bash
printf 'fn main(){println!("hello from akuma amd64");}\n' > /tmp/hello.rs
busybox env LD_LIBRARY_PATH=/usr/local/rust/lib /usr/local/rust/bin/rustc \
    -C linker-flavor=ld \
    -C linker=/usr/local/rust/lib/rustlib/x86_64-unknown-linux-musl/bin/gcc-ld/ld.lld \
    -C link-self-contained=yes -o /tmp/hello /tmp/hello.rs
/tmp/hello
# hello from akuma amd64      (4.8 MB static binary, ~5 s)
```

## Traps

* **`LD_LIBRARY_PATH=/usr/local/rust/lib` is mandatory on every invocation.**
  `rustc` is a 9 KB shim over `librustc_driver-*.so` and finds it through
  `DT_RUNPATH` `$ORIGIN/../lib`; musl expands `$ORIGIN` by reading
  `/proc/self/exe`, and this target has no procfs. Without it: `Error loading
  shared library librustc_driver-….so`.
* **There is no `cc` on the image**, so the default linker flavour fails. Use
  the toolchain's own `rust-lld` as above, or `apk add gcc musl-dev binutils`
  for the `collect2` route.
* **`mkdisk.sh` rebuilds the image from scratch**, so anything that runs it
  (`amd64/run-firecracker.sh` with no `DISK=`, `hpbox.stage()`) wipes the
  toolchain. Re-run step 3 after any of those, or keep a copy of the staged
  image and point the config at that.
* **Keep Firecracker at 1 vCPU** while using the toolchain: CoW fork is SMP=1
  only on this target, and every compiler spawns. **This holds on bare metal
  too** — `nosmp` on the command line — and 2026-09-18 put three signatures on
  it, all from the same in-guest kernel build at SMP=4:

  | what you see | where |
  |---|---|
  | `error: linking with rust-lld failed: signal: 11 (SIGSEGV)` at the final link | deterministic, 3/3 |
  | `rustc` killed by a `#PF` whose `cr2` is **ASCII** (`0x00004d5f4e4f4964` = `dION_M`) — a pointer overwritten with string data | ~24 crates in |
  | build reaches 95/95 crates then stops forever, vCPU idle, children present at `0:00` CPU | Firecracker `-j4` |

  The common factor is **multi-threaded user processes**, which is what makes
  the linker the cheapest reproducer: `rust-lld` is multi-threaded by default,
  and re-running the exact invocation with `--threads=1` links cleanly (`rc=0`)
  where the default crashes (`rc=142`). LLD prints its own argv in the crash
  dump, so the failing command can be replayed straight out of the build log. If
  you must build with SMP on, put `-C link-arg=--threads=1` in the
  `x86_64-unknown-none` rustflags — it gets you past the link, but not past the
  `rustc` corruption above.

## Staging the **bare-metal** root instead of the guest image

This procedure targets the Firecracker guest's `root.img`. The bare-metal root
(`/dev/sdb1` on the Ubuntu side, `/dev/sda1` to Akuma) is a different filesystem
and had only `x86_64-unknown-linux-musl` in its `lib/rustlib/`, so a kernel build
there dies at the second crate with **`E0463: can't find crate for core`**. The
rlibs must come from the *same* rustc build, so a laptop's nightly will not do.
With Ubuntu up:

```sh
mount /dev/sdb1 /mnt/ak
mount -o loop /root/akuma-fc-rust.img /mnt/fcrust
rsync -a /mnt/fcrust/usr/local/rust/ /mnt/ak/usr/local/rust/   # both targets
umount /mnt/fcrust /mnt/ak
```

The source tree and its `vendor/` are staged separately — see
[`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) "Rules that cost time to
learn", which also covers why `vendor/` must be produced by `cargo vendor`
against the checkout rather than copied out of the guest image.

## Background

- [`docs/archive/RUST_TOOLCHAIN_AMD64.md`](../archive/RUST_TOOLCHAIN_AMD64.md) —
  what works in the guest, what does not, and the kernel defects each failure
  turned out to be.
- [`docs/archive/AKUMA_SELF_HOSTING.md`](../archive/AKUMA_SELF_HOSTING.md) — the
  AArch64 equivalent, which reached the same nightly-and-musl conclusion first.
- [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) — the box, its two
  personalities, and `hpbox.py`.
