# Akuma OS

**A bare-metal AArch64 operating system written in Rust — preemptive kernel, Linux-compatible syscalls, SSH server, containers, package managers, a C compiler, a JS runtime, a Git client, DOOM, and an AI coding assistant**

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

## What is Akuma?

Akuma is a bare-metal operating system for the **AArch64** architecture, written entirely in **Rust** (`no_std`, ~36k lines of code across kernel and 8 extracted crates, plus ~7k lines of tests). It runs on QEMU's `virt` machine, booting into a preemptively multitasking kernel that executes standard ELF binaries via a Linux-compatible syscall interface.

The system provides a Unix-like environment with multiple shells, 100+ standard utilities, networking, filesystems, containers, development tools, and even games — all accessible over SSH.

## Capabilities

### Kernel

| Feature | Details |
|---|---|
| **Preemptive multitasking** | 32-thread pool, 10ms round-robin scheduling, hybrid threads + embassy async executor |
| **Memory management** | MMU-based address space isolation per process, demand paging, physical memory manager, talc heap allocator (~63 MB) |
| **Process model** | fork, execve, wait, signals, process groups, parent-child relationships, per-process file descriptor tables, `CLONE_VM` threads |
| **Linux syscall ABI** | ~140 AArch64 Linux-compatible syscalls covering files, networking, memory, processes, and IPC |
| **ELF loader** | Static, static-PIE, and dynamically linked ELF binaries; loads `ld-musl-aarch64.so.1` for dynamic linking |
| **Demand paging** | Lazy anonymous and file-backed page allocation on first access, readahead, partial munmap with region splitting |
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
| **Interactive shell** | Built-in kernel shell with pipelines (`\|`), redirection (`>`, `>>`), chaining (`;`, `&&`), variable expansion |
| **dash** | POSIX-compliant shell (static musl binary) |
| **paws** | Main interactive shell |
| **sbase** | 100+ Unix utilities — `grep`, `sed`, `awk`, `find`, `sort`, `tar`, `diff`, `wc`, `xargs`, `tee`, and many more |
| **System commands** | `ps`, `top`, `kill`, `free`, `df`, `mount`, `uname`, `env` |
| **File commands** | `ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `chmod`, `ln`, `touch`, `find` |

### Development Tools

| Feature | Details |
|---|---|
| **C compiler (TCC)** | Tiny C Compiler with musl libc — compile and run C programs on-target |
| **JavaScript (Bun)** | Bun runtime for running JS/TS scripts |
| **JavaScript (QuickJS)** | ES2020 runtime — BigInt, Promises, async/await, console API |
| **Git client (scratch)** | Clone, fetch, pull, push, commit, branch, tag, status — Git Smart HTTP protocol over HTTPS |
| **Vi editor (neatvi)** | Vi-like text editor, compilable on-target with TCC |

### Services & Applications

| Feature | Details |
|---|---|
| **Process supervisor (herd)** | Manages background services with auto-restart, logging, config files in `/etc/herd/` |
| **Container manager (box)** | `box open/close/stop/ps/inspect` for managing isolated containers |
| **SQLite server (sqld)** | TCP-based SQLite daemon for executing SQL queries over the network |
| **AI assistant (meow)** | LLM chat client connecting to Ollama — streaming responses, filesystem and network tool calling |
| **Package managers** | Built-in `pkg install`, plus `xbps` (Void Linux) and `apk` (Alpine Linux) for real package repositories |
| **DOOM** | Playable DOOM — renders to framebuffer and as ANSI art over SSH |

### Terminal

| Feature | Details |
|---|---|
| **Rich terminal** | Raw and cooked modes, cursor control, screen clearing, ONLCR translation |
| **SSH terminal** | Full interactive terminal over SSH with per-session state |

## Getting Started

### Prerequisites
- Rust nightly toolchain (`rust-toolchain.toml` will handle this)
- The `aarch64-unknown-none` Rust target
- QEMU for AArch64 (`qemu-system-aarch64`)

```bash
# Install the required Rust target
rustup target add aarch64-unknown-none

# Install QEMU (macOS)
brew install qemu

# Install QEMU (Ubuntu/Debian)
sudo apt-get install qemu-system-arm
```

### Build & Run

```bash
git clone https://github.com/netoneko/akuma.git
cd akuma
cargo run --release
```

To build and populate the userspace disk image:

```bash
scripts/create_disk.sh       # Create ext2 disk image
userspace/build.sh           # Build all userspace binaries
scripts/populate_disk.sh     # Populate disk with binaries
```

### Connect via SSH

```bash
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null user@localhost -p 2222
```

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                          Userspace                               │
│  ┌──────┐ ┌─────┐ ┌─────┐ ┌───────┐ ┌─────┐ ┌──────┐ ┌──────┐  │
│  │ dash │ │ tcc │ │ bun │ │scratch│ │ doom│ │ meow │ │ sbase│  │
│  └──────┘ └─────┘ └─────┘ └───────┘ └─────┘ └──────┘ └──────┘  │
│  ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐ ┌──────────────┐  │
│  │ herd │ │ box  │ │ sqld │ │ httpd│ │ xbps │ │ apk          │  │
│  └──────┘ └──────┘ └──────┘ └──────┘ └──────┘ └──────────────┘  │
├──────────────────────────────────────────────────────────────────┤
│  Syscall Interface (Linux AArch64 ABI, ~140 syscalls)            │
├──────────────────────────────────────────────────────────────────┤
│                      Kernel  (~18k lines)                        │
│  ┌──────────┐ ┌──────────────┐ ┌──────────┐ ┌───────────────┐   │
│  │Exceptions│ │ Syscalls     │ │ IRQ      │ │ SSH Server    │   │
│  │ (EL0/EL1)│ │ (140 calls)  │ │ dispatch │ │ (SSH-2)       │   │
│  └──────────┘ └──────────────┘ └──────────┘ └───────────────┘   │
│  ┌──────────┐ ┌──────────────┐ ┌──────────┐ ┌───────────────┐   │
│  │ GIC      │ │ VirtIO       │ │ Timer    │ │ Console       │   │
│  │ (GICv2)  │ │ net/blk/rng  │ │ PL031    │ │ PL011 UART    │   │
│  │          │ │              │ │ ARM CNTP │ │               │   │
│  └──────────┘ └──────────────┘ └──────────┘ └───────────────┘   │
├──────────────────────────────────────────────────────────────────┤
│               Extracted Crates  (~17k lines)                     │
│  ┌───────────────────────────────────────────────────────────┐   │
│  │  akuma-exec (8.7k) — threading, process, MMU, ELF loader  │   │
│  └───────────────────────────────────────────────────────────┘   │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌───────────────────┐   │
│  │akuma-net │ │akuma-ext2│ │akuma-vfs │ │ akuma-shell       │   │
│  │ (2.9k)   │ │ (1.7k)   │ │ (0.8k)   │ │ (1.1k)            │   │
│  └──────────┘ └──────────┘ └──────────┘ └───────────────────┘   │
│  ┌──────────┐ ┌──────────────┐ ┌────────────────────────────┐   │
│  │akuma-ssh │ │akuma-ssh-    │ │ akuma-terminal (0.5k)      │   │
│  │ (0.7k)   │ │crypto (0.7k) │ │                            │   │
│  └──────────┘ └──────────────┘ └────────────────────────────┘   │
├──────────────────────────────────────────────────────────────────┤
│  Hardware: QEMU virt — VirtIO-net, VirtIO-blk, VirtIO-rng,      │
│            GICv2, PL011 UART, PL031 RTC, ramfb                   │
└──────────────────────────────────────────────────────────────────┘
```

### Crate Structure

The kernel is split into a monolithic core (`src/`, ~18k lines) and 8 extracted crates (`crates/`, ~17k lines), with ~7k lines of tests:

| Crate | Lines | Purpose |
|---|---|---|
| `akuma-exec` | 8,730 | Threading, process management, MMU page tables, ELF loader — the execution engine |
| `akuma-net` | 2,940 | Socket layer, TCP/UDP abstractions, smoltcp integration |
| `akuma-ext2` | 1,746 | ext2 filesystem implementation with read/write support |
| `akuma-shell` | 1,050 | Shell parser, command pipeline, redirection, variable expansion |
| `akuma-vfs` | 838 | Virtual filesystem types, mount table, path resolution |
| `akuma-ssh-crypto` | 670 | SSH cryptographic primitives (Ed25519, x25519, AES-128-CTR, HMAC) |
| `akuma-ssh` | 733 | SSH-2 protocol handling, channel management, auth |
| `akuma-terminal` | 538 | Terminal emulation, raw/cooked modes, escape sequences |

### Memory Layout

```
0x40000000  ┌──────────────────┐
            │ Kernel code+stack│  32 MB
0x42000000  ├──────────────────┤
            │ Kernel heap      │  ~63 MB (talc allocator)
0x45FC0000  ├──────────────────┤
            │ User pages (PMM) │  ~159 MB (demand-paged)
0x4FF00000  ├──────────────────┤
            │ DTB              │
0x50000000  └──────────────────┘
```

User processes get isolated virtual address spaces (up to 4 GB) with demand-paged anonymous and file-backed memory. The kernel uses identity mapping; device MMIO is accessed via remapped VAs under L0[1].

## Project Layout

```
src/              Kernel source (~18k lines of no_std Rust, ~5k lines of tests)
crates/           Extracted kernel crates (8 crates, ~17k lines, ~1k lines of tests)
userspace/        Userspace applications and libraries
  libakuma/         Rust syscall wrapper library
  meow/             AI coding assistant
  quickjs/          JavaScript interpreter
  tcc/              Tiny C Compiler
  herd/             Process supervisor
  sbase/            Unix utilities
  dash/             POSIX shell
  paws/             Interactive shell
docs/             Architecture notes and design docs
scripts/          Build and debug scripts
config/           Configuration files
linker.ld         Kernel linker script
```

## License

MIT

If a userspace application is under a license different from MIT (like GPL2 or LGPL2), then the associated userspace programs and the code around them follows their respective license.
