# Akuma Devbox

A reproducible "sit down and develop **inside** Akuma" image. You SSH into it and use it
like a tiny Unix workstation: rump networking, a real editor (`neatvi`), C + Rust
toolchains, `scratch` as git-to-GitHub, and `meow` wired to **z.ai's GLM-5.2**.

The point is **dogfooding** — surfacing and fixing the concrete papercuts that only show
up under daily use. First workloads: compiling `meow` from source in-VM, and (later) a
music player evolved from `wavplay`.

Everything here is self-contained under `overlays/devbox/` and does not disturb the
default `bootstrap/` tree or the existing run scripts.

---

## Quick start

```bash
# 1. Build the image (host). ~8–12 GB; pulls the Rust toolchain + clones the repo in.
overlays/devbox/bootstrap.sh

# 2. Boot it (single kernel, rump networking, 4 GB RAM).
overlays/devbox/run.sh

# 3. SSH in once you see the rump DHCP lease + "[SSH Server] Listening" in the log.
ssh -o StrictHostKeyChecking=no -p 2223 root@localhost
```

Then fill in your secrets **inside the VM** (see [Secrets](#secrets-fill-in-after-ssh)).

---

## Design

### Single kernel, no SMP
The devbox boots one kernel (`cargo run --release`, no `--features smp`, `SMP` unset). The
`release` profile is deliberate: it compiles in both `rump` (networking) and `sound`
(virtio-sound for `wavplay`).

### rump is the only stack you touch
Networking uses the **NetBSD rump TCP/IP stack**, not the native smoltcp stack. This is
achieved by running everything you interact with **inside one `stack=rump` box rooted at
`/`**:

- `herd` supervises a single `rump_server` (the box's networking), and
- the userspace `sshd` **joins that box** (`join_box = rumpnet`), so your SSH session — and
  every process you spawn from it (shells, `curl`, `cargo`, `meow`) — lives in the box.

For any process in a `stack=rump` box, the kernel intercepts the AF_INET socket syscalls
(`socket`/`connect`/`bind`/`listen`/`accept`/`send`/`recv`, syscalls 198/203/206/…) and
routes them to the box's `rump_server` over a sysproxy channel on the server's fd 3. smoltcp
still exists in the kernel but nothing you run touches it. The kernel's dispatch is hardened
so a `stack=rump` box can never fall through to smoltcp.

Requires the second NIC (`/dev/net/tap0`) — `run.sh` sets `RUMP_NIC=1`, which adds a
virtio-net on `virtio-mmio-bus.4` and forwards host port **2223** → box port 22. rump DHCPs
`10.0.2.15` from QEMU SLIRP on boot.

### meow → rump, and the "does TLS go through smoltcp?" question
**No — meow's TLS goes through rump, not smoltcp, once meow runs in the box.** This trips
people up, so here is the exact mechanism (verified in `userspace/libakuma`):

- meow does HTTPS via `libakuma-tls` (a pure-Rust `embedded-tls` client). Its TLS record
  I/O is plain socket `connect`/`send`/`recv`.
- Those socket calls in `libakuma` **already use the standard Linux aarch64 socket syscall
  numbers** (`SOCKET=198`, `CONNECT=203`, `SENDTO=206` — `lib.rs:64-69`, ungated by any
  feature). They are exactly the syscalls the kernel intercepts for a `stack=rump` box.
  → **TLS traffic is routed to rump automatically. No smoltcp. `libakuma-tls` stays.**
- The one thing that would leak is **DNS**: by default meow resolves hostnames via the
  Akuma-custom `RESOLVE_HOST` syscall (300), which is *not* intercepted (and isn't the rump
  resolver). meow's **`linux-net` feature** (`userspace/meow/src/linux_net.rs`) replaces
  that with ordinary UDP-socket DNS, which **is** intercepted → resolves `api.z.ai` through
  rump. It also swaps `UPTIME` (319) for `clock_gettime`.

So the correct build is: **`--features linux-net`, target `aarch64-unknown-linux-musl`,
keep `libakuma-tls`.** That is what makes meow fully rump-routed (DNS + TLS). No meow code
change is needed. `linux-net` also enables `libakuma/linux-abi`, which fixes `getpid` on the
linux-musl target.

```bash
# How the devbox meow (shipped + rebuilt in-VM) is built. meow is a freestanding no_std
# binary that provides its own _start, so it links with rust-lld directly (no crt, no libc,
# no external gcc) and build-std supplies the mem intrinsics — same model as the bare-metal
# build, just with linux-abi syscall numbers:
cd userspace/meow
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-Clinker=rust-lld -Clinker-flavor=ld.lld -Clink-self-contained=no" \
cargo build --release --target aarch64-unknown-linux-musl --features linux-net \
  -Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem
```

### Filesystem
The rump box is rooted at `/` — the whole disk **is** the devbox. No isolated subtree, so
the full toolchain and source tree are directly visible in your SSH session with no
duplication.

---

## What's on the image

| Area | Contents |
|------|----------|
| **Shell / tools** | `busybox` with a **full applet symlink set** (`awk`, `sed`, `find`, `tar`, `ps`, …) so common commands work without full paths. `apk` package manager present. |
| **Editor** | `neatvi` at `/bin/vi` (a small ISC-licensed C vi clone — see [`NOTICE`](./NOTICE)). |
| **C toolchain** | `tcc` (`/bin/tcc`) + musl-dev headers/static libs + `libtcc1`. Compile with `tcc -static`. |
| **Rust toolchain** | Nightly `rustc`/`cargo` (musl host) under `/usr/local`, with `rust-src` and std for `aarch64-unknown-linux-musl` and `aarch64-unknown-none`. |
| **git** | `scratch` (symlinked `/bin/git`): `clone`/`push` over HTTPS with a GitHub PAT. |
| **Agent** | `meow` (built `--features linux-net`) → z.ai GLM-5.2. |
| **Audio** | `wavplay` → `/dev/dsp` (virtio-sound). *(Not exercised by verification — test it yourself.)* |
| **Source** | The full Akuma repo cloned from GitHub **with submodules** into `/src/github.com/netoneko/akuma/` — the tree you develop against in-VM (includes the `meow` submodule). Optionally a pre-seeded `/root/.cargo` cache for offline builds. |

Services are configured via `herd` (`/etc/herd/enabled/`):
- `rumpnet.conf` — the boxed `rump_server` (`box_root = /`, `stack = rump`).
- `sshd.conf` — userspace `sshd`, `join_box = rumpnet`, port 22, `start_delay_ms` to let the
  rump handshake settle. This is the **only** sshd; the base image's native httpd/sshd are
  not carried into the devbox enabled set.

---

## Secrets (fill in after SSH)

The image ships with **placeholders** — no secrets are baked in at build time. After you
SSH in:

**z.ai / GLM-5.2 API key** — edit `/etc/meow/config` and set the key under the `zai`
provider, or run `meow init`:
```
[provider:zai]
base_url=https://api.z.ai/api/paas/v4
type=openai
api_key=YOUR_ZAI_KEY_HERE
```
```bash
vi /etc/meow/config          # set api_key=…
meow -c "hello"              # smoke test one turn
```

**GitHub PAT** (for `scratch`/`git` push/clone of private repos):
```bash
scratch config credential.token ghp_your_token_here
# or per-command:  scratch push main --token ghp_...
git clone https://github.com/<you>/<repo>.git
```
`/root/DEVBOX.txt` on the image has these commands paste-ready.

---

## Daily workflow

```bash
ssh -o StrictHostKeyChecking=no -p 2223 root@localhost

# edit
vi somefile.c

# C
tcc -static -o hello hello.c && ./hello

# Rust / rebuild meow from source (linux ABI → rump-routed)
export PATH=/usr/local/bin:$PATH
cd /src/github.com/netoneko/akuma/userspace/meow
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-Clinker=rust-lld -Clinker-flavor=ld.lld -Clink-self-contained=no" \
cargo build --release --target aarch64-unknown-linux-musl --features linux-net \
  -Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem
./target/aarch64-unknown-linux-musl/release/meow --help

# git via scratch (after PAT)
git clone https://github.com/<you>/<repo>.git
git add . && git commit -m "wip" && git push

# talk to GLM-5.2 (after api_key)
meow                          # interactive TUI
meow -c "explain this diff"   # one-shot
```

When something is awkward, slow, or broken — that's the point. Note it; those papercuts are
the backlog this devbox exists to generate and fix.

---

## Files in this overlay

```
overlays/devbox/
  bootstrap.sh   # end-to-end host build: userspace → neatvi → create → populate → clone → overlay
  run.sh         # launch QEMU: single kernel, RUMP_NIC=1, MEMORY=4096, DISK=devbox.img
  README.md      # this file
  NOTICE         # third-party attribution (neatvi)
  rootfs/        # files overlaid on top of the base bootstrap into the image
    etc/herd/enabled/rumpnet.conf
    etc/herd/enabled/sshd.conf
    etc/meow/config
    etc/resolv.conf
    root/DEVBOX.txt
```

`bootstrap.sh` env knobs (all optional):

| Var | Default | Meaning |
|-----|---------|---------|
| `DEVBOX_DISK` | `devbox.img` | Output image path (`DISK=` passthrough). |
| `DEVBOX_DISK_MB` | `12288` | Image size in MB (toolchain + repo + submodules + cargo cache). |
| `DEVBOX_MEMORY` | `4096` | Default RAM for `run.sh` (rustc needs ≥2 GB). |
| `AKUMA_GIT_URL` | `https://github.com/netoneko/akuma.git` | Repo to clone into `/src/...` (all clones are public, no auth needed). Set to a local path/`file://` for offline. |
| `GITHUB_PAT` | *(unset)* | Only needed if you point `AKUMA_GIT_URL` at a private fork; injected into HTTPS clone URLs. |
| `DEVBOX_ALL_SUBMODULES` | `true` | Clone all submodules (incl. large `src-netbsd`); set `false` for a lighter image with just the source needed to build meow. |

---

## Verification

1. `overlays/devbox/bootstrap.sh` completes; the image exists.
2. `overlays/devbox/run.sh`; wait for the rump DHCP lease (`10.0.2.15`) and
   `[SSH Server] Listening` in the log.
3. `ssh -p 2223 root@localhost` lands in the rump-box shell.
4. In-VM smoke tests:
   - busybox applets resolve without full paths (`awk`, `sed`, `find`, `tar`, `ps`).
   - `apk --version` works.
   - `vi` (neatvi) opens and edits a file.
   - `tcc -static hello.c && ./a.out`; rebuild neatvi from source with `tcc`.
   - `curl -sS https://ifconfig.me` succeeds (proves HTTPS through rump).
   - after PAT: `git clone` / `git push` a repo.
   - `meow --help`; after key set, one live GLM-5.2 turn.
5. Compile meow from source (see [Daily workflow](#daily-workflow)) and run the fresh
   binary's `--help`.
6. Log every papercut hit during 4–5 — those become the follow-up fix tasks.

---

## Backlog (papercuts)

Papercuts surfaced by dogfooding, to fix later:

- **`ps` shows nothing** despite `/proc` being full of per-process data. `ps` returns
  empty output even though `/proc/<pid>/…` is populated — so the applet isn't reading
  the procfs data the kernel already exposes. Needs investigation on the `ps` side
  (which `/proc` layout it expects) vs. what our procfs presents.
