# Akuma

**A bare-metal ARM64 kernel with a built-in SSH server. Because why run an OS when you can BE the OS?**

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
 %@%@@@%%%@@#%@%%@@@@%@@@@@@@@@@%@@@@@@@@@@@%@@@@@@@@@@@@@@@@@@%%=
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

Akuma is a **bare-metal ARM64 kernel** written entirely in Rust. It boots directly on QEMU's ARM virt machine and provides:

- **Full SSH-2.0 Server** - Connect via SSH with proper cryptographic authentication
- **Preemptive Multithreading** - 32-thread scheduler with 10ms time slicing
- **Async Networking** - Embassy-based async runtime with VirtIO network driver
- **No OS Required** - Runs directly on hardware (or QEMU), no Linux, no dependencies


## Running

You can probably run it with Docker: [docs/DOCKER.md](docs/DOCKER.md)

## Features

| Feature | Details |
|---------|---------|
| **SSH Server** | Curve25519 key exchange, AES-128-CTR encryption, Ed25519 signatures |
| **Threading** | Preemptive scheduling, 32KB stacks, context switching in assembly |
| **Networking** | smoltcp TCP/IP stack, VirtIO-net driver, Embassy async |
| **Memory** | Talc allocator with 120MB heap, IRQ-safe allocation |
| **Hardware** | GICv2 interrupts, PL011 UART, PL031 RTC, ARM Generic Timer |
| **Standard C** | Full `musl` libc integration, self-hosted `tcc` compiler |

## Quick Start

### Prerequisites

- Rust nightly toolchain
- QEMU with ARM64 support (`qemu-system-aarch64`)
- The `aarch64-unknown-none` target

```bash
# Install the target
rustup target add aarch64-unknown-none

# Install QEMU (macOS)
brew install qemu

# Install QEMU (Ubuntu/Debian)
sudo apt install qemu-system-arm
```

### Build & Run

```bash
# Clone and run
git clone https://github.com/hyperbach/akuma.git
cd akuma

# Build and launch in QEMU
cargo run --release
```

The kernel boots instantly and starts listening for connections.

### Connect via SSH

```bash
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null user@localhost -p 2222
```

### Connect via Telnet

```bash
telnet localhost 2323
```

Type `cat` in the telnet session to see the demon.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        Akuma Kernel                         │
├─────────────────────────────────────────────────────────────┤
│  SSH Server  │  Netcat Server  │  Embassy Async Runtime     │
├──────────────┴─────────────────┴────────────────────────────┤
│                     smoltcp TCP/IP Stack                    │
├─────────────────────────────────────────────────────────────┤
│              VirtIO Network Driver (MMIO)                   │
├─────────────────────────────────────────────────────────────┤
│  Threading  │  Timer  │  Allocator  │  GIC  │  UART  │ RTC  │
├─────────────────────────────────────────────────────────────┤
│                    ARM64 Hardware (QEMU virt)               │
└─────────────────────────────────────────────────────────────┘
```

## Memory Layout

| Region | Address | Size |
|--------|---------|------|
| Kernel Entry | `0x40000000` | - |
| Stack | `0x40100000` | 8 MB |
| Heap | After stack | 120 MB |

## Dependencies

All dependencies are `no_std` compatible:

- **Memory**: `talc`, `spinning_top`
- **Network**: `smoltcp`, `virtio-drivers`, `embassy-net`
- **Async**: `embassy-executor`, `embassy-time`, `embassy-sync`
- **Crypto**: `curve25519-dalek`, `x25519-dalek`, `ed25519-dalek`, `aes`, `sha2`, `hmac`
- **Hardware**: `arm_pl031`, `fdt`

## License

MIT

---

*Built with Rust. Runs on nothing.*
