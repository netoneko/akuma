# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Akuma is a bare-metal ARM64 kernel written in Rust that boots directly on QEMU's ARM virt machine. It includes an SSH server, preemptive multithreading, async networking, ext2 filesystem, and a userspace ecosystem with ELF binary support.

## Build Commands

```bash
# Build kernel
cargo build --release

# Build and run in QEMU
cargo run --release

# Build userspace packages
cd userspace && cargo build --release

# Build userspace and copy to bootstrap
cd userspace && ./build.sh

# Serve userspace packages for `pkg install` (run from userspace/)
python3 -m http.server 8000
```

## Running

```bash
# Run with cargo (recommended)
cargo run --release

# Or manually with script
./scripts/run.sh

# Connect via SSH
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null user@localhost -p 2222

# Connect via telnet
telnet localhost 2323
```

Port forwarding: SSH=2222, Telnet=2323, HTTP=8080

## Architecture

**Hybrid Execution Model:**
- **Preemptive threads**: 32-thread pool, 10ms time quantum, timer-driven scheduling
- **Cooperative async**: Embassy executor (single-threaded) for networking/I/O

**Main loop pattern** (src/main.rs): polls network runners, SSH/web servers, executor, then yields.

**Key modules:**
- `src/main.rs` - Entry point, async main loop
- `src/threading.rs` - Thread pool, scheduler, context switching
- `src/executor.rs` - Embassy async integration
- `src/vfs/` - Virtual filesystem (ext2, memfs, procfs)
- `src/ssh/` - SSH-2.0 server implementation
- `src/allocator.rs` - Talc heap allocator

**Hardware abstraction:** GIC (interrupts), PL011 (UART), PL031 (RTC), VirtIO (net/block)

## Critical Constraints

### No Heap Allocation in IRQ Handlers

`format!` is banned via clippy—use `safe_print!` instead. This macro allocates on the stack and is safe in exception/IRQ handlers.

```rust
// ❌ Don't use format! in kernel code
let s = format!("value: {}", x);

// ✅ Use safe_print! macro
safe_print!("value: {}", x);
```

### Lock Hierarchy (acquire in this order)

```
Level 1: MOUNT_TABLE
Level 2: ext2.state / MemoryFilesystem.root
Level 3: BLOCK_DEVICE
Level 4: TALC (always with IRQs disabled)
```

Special locks:
- `POOL` - Use `with_irqs_disabled()` for non-scheduler access
- `IRQ_HANDLERS` - Copy-out pattern (release lock before calling handler)
- `FS_MUTEX` - Embassy async mutex (can hold across await)

### Threading Safety Rules

- Never hold spinlocks across `await` points
- Use `with_irqs_disabled()` when accessing `POOL` from non-scheduler code
- Drop locks before calling wakers
- Never access `POOL` from interrupt handlers except through `sgi_scheduler_handler`

## Memory Layout

| Region | Address | Size |
|--------|---------|------|
| Kernel Entry | 0x40000000 | - |
| Stack | 0x40100000 | 8 MB |
| Heap | After stack | ~120 MB |

Total QEMU memory: 128 MB

## Userspace Development

Userspace programs are `no_std` ELF binaries using `libakuma` for syscalls:

```rust
#![no_std]
#![no_main]

use libakuma::*;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Your code
    exit(0);
}
```

Build target: `aarch64-unknown-none`

## Docker Deployment

```bash
cargo build --release
./scripts/build_docker.sh
cd scripts/docker && docker compose up
```

Includes healthcheck polling HTTP on port 8080 with autoheal restart.

## Key Dependencies

All `no_std` compatible:
- `embassy-*` - Async runtime (not tokio)
- `smoltcp` - TCP/IP stack
- `virtio-drivers` - VirtIO device drivers
- `talc` - Heap allocator
- `curve25519-dalek`, `ed25519-dalek`, `aes`, `sha2` - SSH crypto

## Self-Editing with meow-local

The `tools/meow-local` tool is an AI chat client that can edit the Akuma source code. It connects to a local Ollama LLM server and has tools for code navigation and editing.

```bash
# Build meow-local
cd tools/meow-local && cargo build --release

# Run from akuma root (sandbox defaults to current directory)
./tools/meow-local/target/release/meow-local

# Or specify working directory
meow-local -C /path/to/akuma
```

**Code editing tools:**
- `FileReadLines` - Read specific line ranges from files
- `CodeSearch` - Grep-like regex search across .rs files
- `FileEdit` - Search-and-replace with unique match requirement

**Run tests:** `cd tools/meow-local && cargo test`

## Documentation

- `docs/ARCHITECTURE.md` - System architecture
- `docs/CONCURRENCY.md` - Synchronization details and lock hierarchy
- `docs/LOCK_REFERENCE.md` - Quick lock reference card
- `docs/PACKAGES.md` - Userspace package management
- `docs/MEOW.md` - Meow chat client (userspace and local versions)
