# Akuma Architecture Overview

Akuma is a bare-metal kernel for AArch64, designed to run on QEMU's virt machine. This document provides an architectural overview of the system.

## System Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                         User Space                               │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────────┐  │
│  │ SSH Server  │  │ Web Server  │  │ Shell (commands, pipes) │  │
│  └──────┬──────┘  └──────┬──────┘  └────────────┬────────────┘  │
└─────────┼────────────────┼──────────────────────┼───────────────┘
          │                │                      │
┌─────────┼────────────────┼──────────────────────┼───────────────┐
│         ▼                ▼                      ▼               │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                    Async Runtime                         │    │
│  │  ┌──────────────┐  ┌──────────────┐  ┌───────────────┐  │    │
│  │  │ Embassy Net  │  │ Embassy Time │  │   Executor    │  │    │
│  │  └──────────────┘  └──────────────┘  └───────────────┘  │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                    Threading Layer                       │    │
│  │  ┌──────────────┐  ┌──────────────┐  ┌───────────────┐  │    │
│  │  │ Thread Pool  │  │  Scheduler   │  │ Context Switch│  │    │
│  │  └──────────────┘  └──────────────┘  └───────────────┘  │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  ┌──────────────────────┐  ┌────────────────────────────────┐   │
│  │   Virtual Filesystem │  │         Networking             │   │
│  │  ┌───────┐ ┌───────┐ │  │  ┌────────┐ ┌────────────────┐ │   │
│  │  │ ext2  │ │ memfs │ │  │  │ smoltcp│ │ embassy-net    │ │   │
│  │  └───────┘ └───────┘ │  │  └────────┘ └────────────────┘ │   │
│  └──────────┬───────────┘  └────────────────┬───────────────┘   │
│             │                               │                    │
│  ┌──────────▼───────────┐  ┌────────────────▼───────────────┐   │
│  │    Block Device      │  │      VirtIO Network            │   │
│  │   (virtio-blk)       │  │      (virtio-net)              │   │
│  └──────────────────────┘  └────────────────────────────────┘   │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                   Hardware Abstraction                   │    │
│  │  ┌───────┐ ┌───────┐ ┌─────────┐ ┌─────────┐ ┌───────┐  │    │
│  │  │  GIC  │ │ Timer │ │ Console │ │   RTC   │ │ VirtIO│  │    │
│  │  └───────┘ └───────┘ └─────────┘ └─────────┘ └───────┘  │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                      Core Services                       │    │
│  │  ┌───────────┐  ┌───────────┐  ┌───────────────────────┐│    │
│  │  │ Allocator │  │ Exception │  │ IRQ Handler Registry  ││    │
│  │  │  (talc)   │  │  Vectors  │  │                       ││    │
│  │  └───────────┘  └───────────┘  └───────────────────────┘│    │
│  └─────────────────────────────────────────────────────────┘    │
│                              Kernel                              │
└──────────────────────────────────────────────────────────────────┘
                               │
                               ▼
┌──────────────────────────────────────────────────────────────────┐
│                      QEMU virt Machine                           │
│  ARM Cortex-A53 │ GICv2 │ PL011 UART │ PL031 RTC │ VirtIO MMIO  │
└──────────────────────────────────────────────────────────────────┘
```

## Module Organization

### Core (`src/`)

| Module | Purpose |
|--------|---------|
| `main.rs` | Entry point, initialization, async main loop |
| `boot.rs` | Early boot code (assembly) |
| `exceptions.rs` | Exception vector table, IRQ handling |
| `allocator.rs` | Heap allocator using talc |
| `threading.rs` | Thread pool, scheduler, context switching |
| `executor.rs` | Embassy async executor integration |

### Hardware Abstraction

| Module | Purpose |
|--------|---------|
| `gic.rs` | ARM GICv2 interrupt controller |
| `timer.rs` | ARM Generic Timer, RTC |
| `console.rs` | PL011 UART driver |
| `block.rs` | VirtIO block device driver |
| `virtio_hal.rs` | VirtIO HAL implementation |

### Filesystem

| Module | Purpose |
|--------|---------|
| `vfs/mod.rs` | Virtual filesystem layer |
| `vfs/ext2.rs` | ext2 filesystem implementation |
| `vfs/memory.rs` | In-memory filesystem |
| `fs.rs` | High-level filesystem API |
| `async_fs.rs` | Async filesystem wrappers |

### Networking

| Module | Purpose |
|--------|---------|
| `network.rs` | Network initialization, statistics |
| `async_net.rs` | Embassy-net stack setup |
| `embassy_net_driver.rs` | Loopback device driver |
| `embassy_virtio_driver.rs` | VirtIO-net embassy driver |
| `dns.rs` | DNS resolver |

### Time

| Module | Purpose |
|--------|---------|
| `timer.rs` | Hardware timer, uptime tracking |
| `embassy_time_driver.rs` | Embassy time driver implementation |

### Services

| Module | Purpose |
|--------|---------|
| `ssh_server.rs` | SSH server |
| `ssh.rs` | SSH protocol implementation |
| `ssh_crypto.rs` | Cryptographic primitives |
| `web_server.rs` | HTTP server |
| `netcat_server.rs` | Netcat-style server |
| `shell/` | Interactive shell and commands |
| `irq.rs` | IRQ handler registration |

---

## Execution Model

### Boot Sequence

1. **Stage 1**: `boot.rs` - Setup stack, MMU, jump to Rust
2. **Stage 2**: `main.rs:kernel_main` - Initialize subsystems
3. **Stage 3**: Enable preemption, run tests
4. **Stage 4**: Initialize filesystem and network
5. **Stage 5**: Enter async main loop

### Threading Model

Akuma uses a **hybrid threading model**:

1. **Preemptive Threads** (threading.rs)
   - Fixed pool of 32 threads
   - Timer-driven preemption (10ms quantum)
   - Round-robin scheduling via SGI

2. **Cooperative Async Tasks** (executor.rs)
   - Embassy executor for async/await
   - Single-threaded (runs on main thread)
   - Used for networking and I/O

```
┌─────────────────────────────────────────────────────┐
│                    Main Loop                         │
│  ┌─────────────────────────────────────────────┐    │
│  │  loop {                                      │    │
│  │      poll(network_runner);                   │    │
│  │      poll(loopback_runner);                  │    │
│  │      poll(ssh_server);                       │    │
│  │      poll(web_server);                       │    │
│  │      executor::process_irq_work();           │    │
│  │      executor::run_once();                   │    │
│  │      threading::yield_now();  ◄── Voluntary  │    │
│  │  }                               yield       │    │
│  └─────────────────────────────────────────────┘    │
│                        │                             │
│                        ▼                             │
│  ┌─────────────────────────────────────────────┐    │
│  │  Timer IRQ (every 10ms)                      │    │
│  │      ├── Trigger SGI_SCHEDULER               │    │
│  │      └── Embassy time alarms                 │    │
│  └─────────────────────────────────────────────┘    │
│                        │                             │
│                        ▼                             │
│  ┌─────────────────────────────────────────────┐    │
│  │  SGI Handler (scheduler)                     │    │
│  │      ├── Select next thread                  │    │
│  │      └── Context switch                      │    │
│  └─────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────┘
```

### Interrupt Handling

```
Exception Vector Table
         │
         ▼
   IRQ Handler (asm)
         │
         ▼
  rust_irq_handler()
         │
    ┌────┴────┐
    ▼         ▼
SGI 0     Other IRQs
    │         │
    ▼         ▼
sgi_scheduler_handler()    irq::dispatch_irq()
    │                           │
    ▼                           ▼
Context Switch            Registered Handler
```

---

## Memory Layout

```
0x0000_0000 ┌─────────────────────┐
            │   QEMU virt Flash   │
0x0800_0000 ├─────────────────────┤
            │   GIC Distributor   │
0x0801_0000 ├─────────────────────┤
            │   GIC CPU Interface │
0x0900_0000 ├─────────────────────┤
            │   PL011 UART        │
0x0901_0000 ├─────────────────────┤
            │   PL031 RTC         │
0x0A00_0000 ├─────────────────────┤
            │   VirtIO MMIO       │
            │   (8 slots)         │
0x4000_0000 ├─────────────────────┤
            │   Kernel Code/Data  │
            ├─────────────────────┤
            │   Kernel Stack      │
            ├─────────────────────┤
            │   Heap              │
            │   (~120 MB)         │
            │                     │
            └─────────────────────┘
```

---

## Key Data Structures

### Thread Pool (threading.rs)

```rust
struct ThreadPool {
    slots: [ThreadSlot; 32],    // Thread slots
    stacks: [usize; 32],        // Pre-allocated stacks
    current_idx: usize,         // Currently running thread
}

struct ThreadSlot {
    state: ThreadState,         // Free/Ready/Running/Terminated
    context: Context,           // Saved CPU registers
    cooperative: bool,          // Preemptible?
    timeout_us: u64,            // Cooperative timeout
}
```

### VFS Mount Table (vfs/mod.rs)

```rust
struct MountTable {
    mounts: Vec<MountEntry>,    // Sorted by path length (longest first)
}

struct MountEntry {
    path: String,               // Mount point (e.g., "/", "/tmp")
    fs: Box<dyn Filesystem>,    // Filesystem implementation
}
```

### Embassy Time Queue (embassy_time_driver.rs)

```rust
struct EmbassyTimeDriver {
    queue: Mutex<RefCell<[ScheduledWake; 8]>>,
}

struct ScheduledWake {
    at: u64,                    // Wake time (microseconds)
    waker: Option<Waker>,       // Task to wake
}
```

---

## Userspace Environment

Akuma OS supports standard C applications through an integrated development environment:

### Standard Library (Musl)
- **Library**: `musl` libc is the primary C library.
- **Sysroot**: The system provides a standard sysroot at `/usr/lib` and `/usr/include`.
- **ABI**: The kernel implements the Linux AArch64 syscall ABI to ensure compatibility with standard Musl builds.

### Compiler (TCC)
- **Toolchain**: The Tiny C Compiler (TCC) is available as a self-hosted compiler (`/bin/tcc`).
- **Linkage**: All C programs compiled on Akuma are linked against Musl by default.
- **Static Linking**: Currently, Akuma targets a fully static execution model.

---

## See Also

- [CONCURRENCY.md](CONCURRENCY.md) - Detailed synchronization documentation
- [LOCK_REFERENCE.md](LOCK_REFERENCE.md) - Quick lock reference card

