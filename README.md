# Akuma OS

Bohemian operating system, now with an official soundtrack, [*Omegashima*](https://tokyorider.bandcamp.com/album/omegashima) by *Tokyo Rider*.

**This project is built with various AI tools as an experiment** to understand if models at the time could be used to produce some working software and dive into a domain I had some familiarity with but never got to explore. Reading Andrew Tannenbaum 20 years ago certainly helped but looking at the code and putting stuff togeter and getting to the point of running real software on custom bare bones kernel is an exciting hobby even if it only runs in QEMU.

**Bare-metal AArch64 OS in Rust — preemptive kernel, Linux-compatible syscalls, SSH, containers, apk, TCC/Clang/GCC/rustc, JS runtime, Git**

```
                                             %#%:                +
                                            #####%            %%=
                                            #*###*%        -%%#=
                                            #*####%#**+**%@%**#
                                            *%@%%#%%@#%#*##+%#*
                                            *@@@%@%@%#@%%*%+#%#
                                            %@@%%%%#*%%@@@*+%%#+
                                            *%@@@@%%%@@#@@#++%##
                                            %@@@@@@@@@@@%%%#+%%#
                                            @@@@@@@@@@@@@@%#*-++
                                   **+**####@@@@@@@@@@@%@@@%#+=
                  ########**######%%%%%%@@%%@@@@@@@%%%%@@%#%#+*=
             ###%%%%@@%%%%@@@@%@@@@@@@@@@@#@%@@@@%@@@@@@@%#+#@@#+
         #######%%%@@@@%%%@@@@@@@@@@@@@@@@%%@@@@@@@@@@@@@*#@@@@%%+
       ##%%%%%@%@@%%%%@%@@%%%@@@@@@@@@@@@%%%@@@@@@#@@@@@%@@@@@@@+%
    *#%%%%@%%%%@%%%@%%@%%@%@@@@@@@@@@@@@@@%@%@@@@@@@@@@@@@@@@@@@%**
  *%%%%%%%@@@@%@@%%@@@@@%@@@@@@@@@@@@@@@@%%@@@@@@@@@@@@@@*@@@@@@%*
 %@%@@@%%%@@#%@%%@@@@@%@@@@@@@@@@%@@@@@@@@@@@%@@@@@@@@@@@@@@@@@@%%=
%@@@@@%%%%%%@@%@@@@@@@%%@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@%#@
%@@%@@@%@@@@@@@@@%@@@%@@@@%@@%%%%%%%%@@@@@@#@@@@@@@@@@@@%@@@@@@%@@@@
*%%%@@@%@%@@@@@@@@@@@@@@@@@@%@@@@@%%#%%@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@%
 %#@@@@@@@@@%@@@@@@@@@@@@@@@@@@@@@@%##@@@@@%%%@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@%%
       ++#@@@@@@%@@@@@@@@@@@@@@@@@@@%@@@@@@@%%%@@@@@@@@%#         #@@*@@@@@@@@%
                *@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@%@@@@%%              %@@@@%*
                        #@@@@@@@@@@@@@@@@@@@@@@@%@@@@@@@@@#
                                 @@@@@@@@%@@@@@@@%@@@@@%@%#+
                                                 %@@@@@%@%*+
                                                   %@%@@@%*
```

*悪魔 (Akuma) - "Demon" in Japanese*

---

## Capabilities

**Can run a coding client and tcc on 4mb of RAM:**

```
meow -c "statically compile /akuma-playground/hello.c with /bin/tcc, put binary in /tmp/h4mb and run it, run commands one by one using shell tool"
```

**Self-hosting — compiles its own kernel on-target:** a nightly musl Rust toolchain
runs inside Akuma and builds the full Akuma kernel from source — all 147 crates
plus the final link — over a single in-VM `cargo build --release`. The resulting
ELF boots and reaches the SSH server. *Akuma compiles Akuma, and the result runs.*
See [`docs/AKUMA_SELF_HOSTING.md`](docs/AKUMA_SELF_HOSTING.md).

### Kernel

| Feature | Details |
|---|---|
| **Preemptive multitasking** | 32-thread pool, 10ms round-robin scheduling, hybrid threads |
| **Memory management** | MMU-based address space isolation per process, demand paging, physical memory manager, talc heap allocator (~63 MB) |
| **Process model** | fork, execve, wait, signals, process groups, parent-child relationships, per-process file descriptor tables, `CLONE_VM` threads |
| **Linux syscall ABI** | ~140 AArch64 Linux-compatible syscalls covering files, networking, memory, processes, and IPC |
| **ELF loader** | Static, static-PIE, and dynamically linked ELF binaries; loads `ld-musl-aarch64.so.1` for dynamic linking |
| **Pipes & IPC** | Kernel pipes (`pipe2`), `eventfd2`, `futex`, `pselect6`, `ppoll` |
| **Signals** | `kill`, SIGSEGV handling, Ctrl+C interrupt propagation |
| **Containers** | Lightweight process isolation ("boxes") with per-container root filesystems, process namespaces, and socket isolation |

### Networking

| Feature | Details |
|---|---|
| **TCP/IP stack** | smoltcp with VirtIO-net driver, TCP and UDP sockets, loopback, DHCP |
| **SSH-2 server** | In-kernel, port 2222 — curve25519-sha256 key exchange, Ed25519 host keys, AES-128-CTR, public key auth, up to 4 concurrent sessions |
| **HTTP server** | Serves static files and CGI scripts (JS, ELF binaries) |
| **HTTP client** | Built-in `curl` for HTTP GET with streaming downloads |
| **TLS 1.3** | Kernel (async) and userspace (blocking) via `embedded-tls` |
| **DNS** | Built-in DNS resolver, `nslookup` command |

### Filesystems

| Feature | Details |
|---|---|
| **VFS layer** | Mount table, path resolution with symlink following, cross-filesystem operations |
| **ext2** | Read/write ext2 on VirtIO block device — directories, symlinks, permissions, metadata |
| **procfs** | `/proc/<pid>/fd/` for process I/O, process listing filtered by container |

### Shells & Utilities

| Feature | Details |
|---|---|
| **Built-in shell** | Pipelines (`\|`), redirection (`>`, `>>`), chaining (`;`, `&&`), variable expansion |
| **busybox** | Main interactive shell |
| **System commands** | `ps`, `top`, `kill`, `free`, `df`, `mount`, `uname`, `env` |
| **File commands** | `ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `chmod`, `ln`, `touch`, `find` |

### Development Tools

| Feature | Details |
|---|---|
| **C compiler (TCC)** | Tiny C Compiler with musl libc — compile and run C programs on-target |
| **Rust compiler (rustc)** | Two toolchains run on-target: Alpine `rustc` 1.91 (`rustc -C linker=clang hello.rs` → runnable native binary) and a **nightly musl toolchain** with the `aarch64-unknown-none` std, which compiles and links the **entire Akuma kernel in-VM** (`cargo build --release` over a populated disk, ≥6 GB RAM). Needed in-kernel support: `MAP_SHARED` writeback for linker output, 128 KB argv strings, the futex/`exit_group` thread-group reaping fixes, and `getpriority`. See [`docs/AKUMA_SELF_HOSTING.md`](docs/AKUMA_SELF_HOSTING.md) and [`docs/RUST_TOOLCHAIN.md`](docs/RUST_TOOLCHAIN.md) |
| **C compiler (Clang/GCC)** | LLVM `clang`/`clang-21` and GCC/binutils (`cc`, `ld`, `as`) from Alpine apk |
| **JavaScript (Bun)** | Bun runtime for running JS/TS scripts |
| **JavaScript (QuickJS)** | ES2020 runtime — BigInt, Promises, async/await, console API |
| **Git** | `git` from Alpine apk — `apk add git` |
| **Vi editor (neatvi)** | Vi-like text editor, compilable on-target with TCC |

### Services & Applications

| Feature | Details |
|---|---|
| **Process supervisor (herd)** | Background services with auto-restart, logging, config in `/etc/herd/` |
| **Container manager (box)** | `box open/close/stop/ps/inspect` |
| **AI assistant (meow)** | LLM chat client connecting to Ollama — streaming responses, filesystem and network tool calling |
| **Package managers** | Built-in `pkg install`, plus `apk` (Alpine Linux) |

## Build & Run

```bash
rustup target add aarch64-unknown-none
# macOS: brew install qemu  |  Ubuntu: sudo apt-get install qemu-system-arm

git clone https://github.com/netoneko/akuma.git
cd akuma
cargo run --release
```

To build and populate the userspace disk image:

```bash
scripts/create_disk.sh
userspace/build.sh
scripts/populate_disk.sh
```

Tests (host target required since kernel target is `aarch64-unknown-none`):

```bash
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

Connect via SSH:

```bash
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null user@localhost -p 2222
```

## Crate Structure

| Crate | Purpose |
|---|---|
| `akuma-exec` | Threading, process management, MMU page tables, ELF loader |
| `akuma-net` | Socket layer, TCP/UDP abstractions, smoltcp integration |
| `akuma-ext2` | ext2 filesystem read/write |
| `akuma-shell` | Shell parser, command pipeline, redirection, variable expansion |
| `akuma-vfs` | Virtual filesystem types, mount table, path resolution |
| `akuma-ssh-crypto` | SSH cryptographic primitives (Ed25519, x25519, AES-128-CTR, HMAC) |
| `akuma-ssh` | SSH-2 protocol handling, channel management, auth |
| `akuma-terminal` | Terminal emulation, raw/cooked modes, escape sequences |

## Project Layout

```
src/              Kernel source (no_std Rust)
crates/           Extracted kernel crates
userspace/
  libakuma/         Rust syscall wrapper library
  meow/             AI coding assistant
  quickjs/          JavaScript interpreter
  tcc/              Tiny C Compiler
  herd/             Process supervisor
  scratch/          minimal Git implementation (HTTPS only)
docs/             Architecture notes and design docs
scripts/          Build and debug scripts
config/           Configuration files
linker.ld         Kernel linker script
```

## License

MIT. Userspace components under different licenses (GPL2, LGPL2) follow their respective licenses.
