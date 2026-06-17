# Acceptance: Akuma self-hosts — compile the akuma kernel with rustc *inside* Akuma

The sibling exercise to the `rustc hello.rs` bring-up (`docs/RUST_TOOLCHAIN.md`):
that proved a single dependency-free `.rs` file compiles + links + runs inside the
VM. This one raises the bar all the way to **self-hosting** — building Akuma's own
kernel (`cargo build --release`) with the in-VM `rustc`, against a pre-staged,
network-free source tree.

This is **exploratory**. We do not yet know how far it gets — the point is to find
the wall. The disk is prepared up to the moment of `cargo build`; you SSH in and
drive the build.

---

## Why nightly, and why a release build is the realistic target

| Fact | Consequence |
|------|-------------|
| The root `Cargo.toml` opens with `cargo-features = ["panic-immediate-abort"]` | **Any** cargo invocation needs **nightly cargo**. Stable rejects the unknown cargo-feature at manifest-parse time, so Alpine's apk `rust` (stable 1.91) is a non-starter — it can't even read the manifest. |
| `profile.release` = `panic = "abort"` (plain abort, *not* immediate-abort) | A **release** build links against the **precompiled `aarch64-unknown-none` std** → **no `build-std` needed**. This is the achievable self-host target. |
| `profile.size` / `profile.extreme-size` = `panic = "immediate-abort"` | Those profiles *do* need `-Z build-std` + `rust-src`. We ship `rust-src` so they're attemptable, but start with `--release`. |
| Akuma userspace is **musl** | The toolchain must be the `aarch64-unknown-linux-musl` **host** toolchain. A glibc `rustc` binary would not run on Akuma. Nightly *does* ship a musl-host `rustc`/`cargo` (verified on static.rust-lang.org). |

So: **nightly, musl-host, `cargo build --release`, deps vendored offline.**

---

## What's on the prepared disk (`disk_selfhost.img`, 8 GB ext2)

- `busybox-static` + `musl-dev` (apk) — working shell + C headers/static libs
- C toolchain (apk): `clang`, `lld`, `gcc`, `binutils`, `make`
- **Nightly Rust toolchain** under `/usr/local` (installed host-side via Docker):
  `rustc`, `cargo`, host `rust-std` (`aarch64-unknown-linux-musl`), target
  `rust-std` (`aarch64-unknown-none`), and `rust-src`
- `/root/akuma` — a fresh `git clone --depth 1` of
  `https://github.com/netoneko/akuma.git` (the real repo, pristine `.cargo/config.toml`)

The kernel's `rust-lld` (bundled with the `rustc` component) is the default linker
for `aarch64-unknown-none`, so no external linker config is required for the kernel
link itself.

> **Deps / network:** the clone uses the upstream `.cargo/config.toml`, so
> `cargo build` will try to fetch crates from crates.io over the VM's QEMU
> user-mode network. cargo-over-TLS inside Akuma is **unproven** and may be the
> first wall. If it is, fall back to **offline vendored deps** (44 MB) — see the
> "offline fallback" note in step 5.

---

## Preparation (host) — already scripted

### 1. Create the 8 GB image (separate from the primary `disk.img`)

```bash
DISK=disk_selfhost.img bash scripts/create_disk.sh 8192
```

### 2. Populate it: bootstrap + busybox + musl-dev + nightly toolchain

```bash
DISK=disk_selfhost.img bash scripts/populate_disk.sh \
    --with-apk --with-musl-dev --with-rust-toolchain
```

`--with-rust-toolchain` (new flag) downloads the nightly musl-host components from
`static.rust-lang.org` and installs them under `/usr/local` in the image, plus
apk-installs the C toolchain. Takes a few minutes (~200 MB of downloads).

### 3. Clone akuma from GitHub into the disk

```bash
docker run --rm --privileged \
    -v "$(pwd)/disk_selfhost.img:/disk.img" \
    alpine:latest sh -c '
        set -e
        apk add --no-cache git >/dev/null
        mkdir -p /mnt/disk && mount -o loop /disk.img /mnt/disk
        rm -rf /mnt/disk/root/akuma
        git clone --depth 1 https://github.com/netoneko/akuma.git /mnt/disk/root/akuma
        sync && umount /mnt/disk && echo "cloned to /root/akuma"'
```

> The toolchain install (step 2) and this clone are also runnable together; that is
> exactly what was done to prepare the image — see `/tmp/akuma_toolchain_clone.log`.

---

## Boot the VM with lots of RAM

A full kernel build runs hundreds of `rustc` invocations and the `fork` CoW path is
slow/heavy on Akuma (`docs/RUST_TOOLCHAIN.md` §5b), so give it room. `rustc` needed
≥2 GB just for `hello.rs`; a self-host build wants far more.

```bash
# Builds the host's release kernel, then boots IT against the prepared disk.
MEMORY=14336 DISK=disk_selfhost.img SNAPSHOT=1 INSTANCE=1 cargo run --release \
    2>&1 | tee 10_selfhost.log
```

- `MEMORY=14336` — 14 GB guest RAM. Boot-to-SSH is verified at 6/8/10/12/14/16 GB
  (`scripts/boot_ram_sweep.sh`); the ceiling is host RAM, not the kernel. (The
  earlier 8 GB boot crash was a boot self-test VA bug, now fixed — see
  `docs/AKUMA_SELF_HOSTING.md` §3.) Give the build as much as your host allows;
  `-j1` still bounds the peak per-`rustc` footprint.
- `SNAPSHOT=1` — writes are discarded on shutdown, so the prepared image stays pristine and re-runnable (the build artifacts live in the qemu overlay; they vanish on exit, which is fine for the test)
- `INSTANCE=1` — SSH on **port 2322** (avoids colliding with a 2222 VM)

Poll for boot (never `wait` on the QEMU process — it runs forever):

```bash
until grep -q "\[SSH Server\] Listening" 10_selfhost.log 2>/dev/null; do sleep 2; done
```

SSH helper (the in-VM shell is a custom mini-shell, not `/bin/sh`; use Python):

```python
import subprocess, re
def ssh(cmd, timeout=1800):
    r = subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no",
         "-o", "UserKnownHostsFile=/dev/null",
         "-p", "2322", "root@localhost", cmd],
        capture_output=True, text=True, timeout=timeout)
    out = re.sub(r'\x1b\[[0-9;]*[KmHm]', '', r.stdout).strip()
    err = '\n'.join(l for l in re.sub(r'\x1b\[[0-9;]*[KmHm]', '', r.stderr)
                    .strip().splitlines()
                    if '@@@@' not in l and 'Warning: Permanently' not in l).strip()
    return r.returncode, out, err
```

---

## Steps (in VM)

### 4. Verify the toolchain is present

```python
_, out, _ = ssh("/usr/local/bin/rustc --version")
print(out)                          # expect: rustc 1.xx.0-nightly (... )
assert "nightly" in out, f"nightly rustc missing: {out}"
_, out, _ = ssh("/usr/local/bin/cargo --version")
print(out)
```

### 5. Kick off the self-host build

The mini-shell does not support `2>&1`, `$?`, or env-var prefixing, so export
`PATH` first and keep the command single-line. Start with `-j1` to bound peak
memory and the `fork` storm; raise `-j` later if it survives.

```python
rc, out, err = ssh(
    "export PATH=/usr/local/bin:/usr/bin:/bin:$PATH && "
    "export CARGO_HOME=/root/.cargo && "
    "cd /root/akuma && "
    "cargo build --release -j1",
    timeout=7200,   # self-host is slow; give it room
)
print(f"rc={rc}\n--- stdout ---\n{out}\n--- stderr ---\n{err}")
```

> **Offline fallback (if cargo can't fetch from crates.io inside Akuma).** Vendor
> the deps on the host (`cargo vendor selfhost_vendor` → 44 MB, includes the
> `embedded-tls` git fork), copy `selfhost_vendor` into `/root/akuma/vendor` in the
> image, append a `[source.crates-io] replace-with = "vendored-sources"` block (with
> `directory = "/root/akuma/vendor"`) to `/root/akuma/.cargo/config.toml`, and add
> `--offline` to the `cargo build`. This removes the network dependency entirely.

### 6. Verify (whatever the outcome, capture the wall)

```python
# Success looks like a produced kernel ELF:
_, out, _ = ssh("ls -l /root/akuma/target/aarch64-unknown-none/release/akuma")
print(out)
# If it didn't finish, the value is in WHERE it stopped — record the last
# compiling crate, the errno/signal, and free RAM at the time.
```

---

## Expected outcome — unknown; this is a probe

There is no established pass/fail line yet. Likely milestones, in order of ambition:

1. **cargo parses the manifest + resolves offline** — proves nightly cargo + vendor wiring. (Stable would already have failed here.)
2. **build scripts + proc-macros compile and *run*** — `build.rs` and any
   proc-macro deps execute host (`aarch64-unknown-linux-musl`) binaries the build
   just produced. Exercises fork/exec of freshly-compiled code.
3. **dependency crates compile for `aarch64-unknown-none`** — the bulk of the work.
4. **the `akuma` crate itself compiles**.
5. **`rust-lld` links the kernel ELF** — full self-host.

Record the highest milestone reached in this file once run.

---

## Failure modes

| Symptom | Diagnosis |
|---|---|
| `cargo: not found` / `rustc: not found` | `PATH` missing `/usr/local/bin`; export it (step 5). Confirm step 2 ran with `--with-rust-toolchain`. |
| `error: failed to parse manifest` / unknown cargo-feature | You're on stable, not nightly — wrong toolchain on the disk. |
| `error: no matching package ... offline` | Vendor incomplete or `Cargo.lock` drifted from the source; re-run `cargo vendor` (step 3) against the *same* commit you `git archive`d. |
| `rc=137` / process killed / `SIGSEGV` during a `rustc` | OOM — raise `MEMORY`, keep `-j1`. This is the expected floor-finding signal. |
| boot crash `EC=0x25` in `map_user_page` self-test, `FAR≈0x1c0180000` | The old boot self-test VA bug at `MEMORY≥8G` — **fixed** (`docs/AKUMA_SELF_HOSTING.md` §3). If you still see it, your kernel predates the fix; rebuild. |
| whole-kernel `brk #1` / EC=0x3c abort in `10_selfhost.log` | Kernel-side OOM under genuine PMM pressure (see memory notes in `CLAUDE.md`); raise `MEMORY`. |
| build hangs for minutes with no progress | Likely the slow `fork` CoW copying the multithreaded rustc address space (`docs/RUST_TOOLCHAIN.md` §5b). Be patient or reduce parallelism. |
| `[ENOSYS] nr=NNN` in the kernel log | A syscall the kernel build needs that `hello.rs` didn't. Decode `nr` against the asm-generic table and file it — this is exactly the kind of gap this test exists to surface. |
| disk changes lost after reboot | `SNAPSHOT=1` discards writes by design; that's intended so the prepared image stays clean. Drop `SNAPSHOT` to persist `target/` across boots (at the cost of mutating the image). |

---

## Relationship to the other tests

| | `rustc hello.rs` (`docs/RUST_TOOLCHAIN.md`) | **10 (this)** |
|---|---|---|
| Toolchain | Alpine apk `rust` (stable, musl) | nightly musl-host (`/usr/local`) |
| Input | one dep-free `.rs` file | the whole akuma workspace |
| build-std | n/a | not for `--release`; `rust-src` shipped for size/extreme |
| Deps | none | 44 MB vendored, `--offline` |
| RAM | ≥2 GB | 8 GB+ recommended |
| Goal | compile + run one binary | self-host the kernel |
