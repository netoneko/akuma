# Akuma OS

**A bohemian bare-metal AArch64 operating system written in Rust, featuring a preemptive kernel, userspace applications, a built-in SSH server, an interactive shell, some limited Linux compatibility, and `xbps` and `apk` package managers**

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

Akuma is a hobby operating system for the **AArch64** architecture, written entirely in **Rust**. It's designed to run directly on hardware (or a QEMU `virt` machine) without any underlying OS. It boots from a custom bootloader into a preemptively multitasking kernel that can run userspace ELF binaries.

The system provides a familiar Unix-like environment through its interactive shell, featuring commands like `ls`, `ps`, `grep`, and `curl`, complete with I/O redirection and command pipelines.

## Core Features

| Category | Feature | Description |
|---|---|---|
| **Kernel** | **Preemptive Multitasking** | Round-robin scheduler for concurrent kernel and user threads. |
| | **MMU-based Memory** | Hardware-enforced memory isolation between kernel and userspace. |
| | **Userspace Support** | Loads and executes standard ELF binaries with Linux-compatible syscalls. |
| **Networking** | **Built-in SSH Server** | Full SSH 2.0 with modern crypto (Curve25519, Ed25519, AES-CTR). |
| | **TCP/IP Stack** | `smoltcp` for all networking, with a VirtIO-net driver. |
| | **Services & Tools** | Includes a web server, DNS client (`nslookup`), and an HTTP client (`curl`). |
| | **Package Manager** | `pkg install` command to download and install userspace binaries. |
| **Filesystem** | **Virtual Filesystem (VFS)** | Supports multiple mount points (e.g., `ext2` on disk, `memfs` in RAM). |
| | **Drivers** | Includes drivers for `ext2` filesystems and VirtIO block devices. |
| **Shell** | **Interactive Unix-like Shell** | Supports pipelines (`|`), redirection (`>`, `>>`), and common commands. |
| | **System Management** | `ps`, `kthreads`, `kill`, `free`, `df`, `mount` for system inspection. |
| | **File Management** | `ls`, `cd`, `pwd`, `cat`, `rm`, `mv`, `cp`, `mkdir`, `find`. |

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
Clone the repository and use Cargo to build and launch the kernel in QEMU.

```bash
git clone https://github.com/netoneko/akuma.git
cd akuma
cargo run --release
```

### Connect via SSH
Once running, you can connect to the built-in SSH server.

```bash
# The default user is 'user' and there is no password
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null user@localhost -p 2222
```

## Architecture

Akuma is split into two main privilege levels: the kernel and userspace.

```
┌─────────────────────────────────────────────────────────────┐
│                        Userspace                            │
│ ┌────────────────┐ ┌──────────────────┐ ┌──────────────────┐ │
│ │ /bin/sh (Shell)│ │ /bin/tcc         │ │ Other ELF Binaries │ │
│ └────────────────┘ └──────────────────┘ └──────────────────┘ │
├─────────────────────────────────────────────────────────────┤
│ Syscall Interface (Linux AArch64 ABI)                       │
├─────────────────────────────────────────────────────────────┤
│                         Kernel                              │
│ ┌───────────┐ ┌──────────┐ ┌─────────┐ ┌───────────────────┐ │
│ │ Scheduler │ │ VFS(ext2)│ │ Network │ │ Built-in Services │ │
│ │ (Threads) │ │ (virtio) │ │(smoltcp)│ │ (SSH, HTTP)       │ │
│ └───────────┘ └──────────┘ └─────────┘ └───────────────────┘ │
├─────────────────────────────────────────────────────────────┤
│                    Hardware (QEMU virt)                     │
└─────────────────────────────────────────────────────────────┘
```

- **Kernel**: Manages hardware, scheduling, memory, and provides core services. It handles interrupts, drives devices (VirtIO), and manages the filesystem and network stacks.
- **Userspace**: Applications run as standard ELF binaries. They interact with the kernel through a POSIX-like syscall interface, allowing programs compiled with standard toolchains (like `musl-gcc` or the included `tcc`) to run on Akuma.

For more details, see the [Architecture Document](docs/ARCHITECTURE.md).

## License

MIT

If a userspace application is under a license different from MIT (like GPL2 or LGPL2), then the associated userspace programs and the code around them follows their respective license.

