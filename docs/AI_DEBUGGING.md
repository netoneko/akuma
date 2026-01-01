# AI Debugging Flow for Userspace Processes

This document describes the workflow for debugging userspace processes in Akuma.

## Overview

When debugging userspace packages (e.g., `stdcheck`, `echo2`), follow this workflow to verify changes and test execution.

## GDB Debugging

For low-level kernel debugging, you can run QEMU with a GDB server that waits for a debugger connection before starting the kernel.

### Start QEMU with GDB Server

```bash
./scripts/run_with_gdb.sh
```

This launches QEMU with:
- `-s` - Opens GDB server on port 1234
- `-S` - Freezes CPU at startup (waits for GDB to connect)

### Connect with GDB

In a separate terminal:

```bash
# Using gdb-multiarch (Linux)
gdb-multiarch -ex 'target remote :1234' target/aarch64-unknown-none/release/akuma

# Using lldb (macOS)
lldb -o 'gdb-remote 1234' target/aarch64-unknown-none/release/akuma
```

### Common GDB Commands

| Command | Description |
|---------|-------------|
| `continue` / `c` | Resume execution |
| `break <symbol>` | Set breakpoint at function |
| `break *0x40000000` | Set breakpoint at address |
| `info registers` | Show all registers |
| `x/10i $pc` | Disassemble 10 instructions at PC |
| `stepi` / `si` | Step one instruction |
| `next` / `n` | Step over (source level) |
| `bt` | Backtrace |

### Debugging Tips

- Build with debug symbols: `cargo build` (not `--release`) for better debugging
- Set breakpoints on panic handlers to catch kernel panics
- Use `monitor info registers` for QEMU-specific register info

## Debugging Steps

### 1. Build the Kernel (Release)

After making changes to the kernel or userspace code, build the release version:

```bash
cargo build --release
```

### 2. Build the Userspace Package (Release)

Make sure to build the release version of the userspace package you're debugging so it can be installed after boot:

```bash
cd userspace
cargo build --release -p stdcheck
```

Replace `stdcheck` with the name of the package you're debugging.

### 3. Start a Web Server for Package Downloads

Before installing packages via `pkg install`, you need to serve the userspace binaries over HTTP. Run a Python web server from the userspace directory:

```bash
cd userspace
python3 -m http.server 8000
```

This serves the built packages at `http://localhost:8000/`, allowing `pkg install` to download them.

### 4. Run and Test via SSH

Connect to the running system and execute the userspace process:

```bash
ssh -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=no user@localhost -p 2222 stdcheck
```

### 5. Reinstall Package After Boot

If you need to install/reinstall the userspace package after the system has booted (requires the web server from step 3 to be running):

```bash
ssh -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=no user@localhost -p 2222 "pkg install stdcheck"
```

## Quick Reference

| Action | Command |
|--------|---------|
| Build kernel | `cargo build --release` |
| Build kernel (debug) | `cargo build` |
| Build userspace package | `cd userspace && cargo build --release -p <package>` |
| Start package server | `cd userspace && python3 -m http.server 8000` |
| Run userspace process | `ssh -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=no user@localhost -p 2222 <package>` |
| Install package | `ssh -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=no user@localhost -p 2222 "pkg install <package>"` |
| Run with GDB | `./scripts/run_with_gdb.sh` |
| Connect GDB (Linux) | `gdb-multiarch -ex 'target remote :1234' target/aarch64-unknown-none/release/akuma` |
| Connect GDB (macOS) | `lldb -o 'gdb-remote 1234' target/aarch64-unknown-none/release/akuma` |

## Notes

- `stdcheck` is used as an example; substitute it for any userspace package you're debugging
- Always use the release build for proper testing
- The SSH connection uses port 2222 and the `user` account
- Host key checking is disabled (`-o StrictHostKeyChecking=no`) for convenience during development
- The Python web server must be running on port 8000 for `pkg install` to work

