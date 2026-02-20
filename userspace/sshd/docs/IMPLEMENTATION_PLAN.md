# Implementation Plan: Migrating SSHD to Userspace

This document outlines the plan for migrating the SSH server (`sshd`) from kernel space to user space in Akuma OS.

## 1. Objectives
- Port the current kernel-based SSH server to a standalone userspace application.
- Maintain the kernel implementation under a feature flag/config for debugging.
- Use `libakuma` for networking, process management, and file I/O.
- Integrated `/bin/paws` as the default interactive shell.
- Ensure no changes are made to the existing kernel `src/shell` implementation.

## 2. Architecture Comparison

| Feature | Kernel Implementation (`src/ssh/`) | Userspace Implementation (`userspace/sshd/`) |
|---------|------------------------------------|----------------------------------------------|
| Networking | `smoltcp` direct integration | `libakuma::net` (Sockets syscalls) |
| Multi-threading | `threading::spawn_system_thread` | `libakuma::spawn` (or single-thread with poll) |
| File I/O | Internal VFS calls | `libakuma` (Open, Read, Write syscalls) |
| Entropy | Internal RNG | `libakuma::getrandom` |
| Shell | `src/shell` (Kernel-internal) | `/bin/paws` (External binary) |
| Async/Wait | `block_on` + `smoltcp_net::poll` | Synchronous with threads OR simple async loop |

## 3. Component Migration Strategy

### 3.1 Dependencies
The current SSH implementation uses several `no_std` crates that are compatible with userspace:
- `ed25519-dalek`
- `x25519-dalek`
- `sha2`
- `aes`
- `ctr`
- `hmac`
- `embedded-io-async` (Requires a simple adapter for `libakuma::net::TcpStream`)

### 3.2 Key Changes Required
1. **Network Adapter**: Create a `SshStream` wrapper for `libakuma::net::TcpStream` that implements `embedded_io_async::Read` and `Write`.
2. **Process Execution**: Replace the direct call to `run_shell_session` (which uses the kernel `shell` module) with a `spawn` call to `/bin/paws`.
3. **I/O Bridging**: Implement a "bridge" that pipes data between the SSH channel and the spawned `/bin/paws` process (stdin/stdout/stderr).
4. **Entropy**: Replace `SimpleRng` or internal RNG calls with `libakuma::getrandom`.
5. **Logging**: Replace `safe_print!` and `console::print` with `libakuma::println` or `eprintln`.

## 4. Implementation Steps

### Phase 1: Scaffolding & Library Porting
1. Create `userspace/sshd/Cargo.toml` with necessary dependencies.
2. Copy `src/ssh/*.rs` to `userspace/sshd/src/`.
3. Fix imports to use `libakuma` instead of `crate::...`.
4. Implement `embedded_io_async` traits for `libakuma::net::TcpStream`.

### Phase 2: Core Server Logic
1. Adapt `server.rs` to use `TcpListener`.
2. Update `auth.rs` and `keys.rs` to use `libakuma` file APIs for loading host keys and `authorized_keys`.
3. Ensure `crypto.rs` uses `getrandom`.

### Phase 3: Shell Integration
1. Implement the session handler to `spawn("/bin/paws", ["-i"])`.
2. If `/bin/paws` is unavailable, fall back to a rudimentary internal shell that supports `help`, `exit`, and basic echo.
3. Bridge SSH channel data to process stdin and process stdout to the SSH channel.

### Phase 4: Integration & Flags
1. Add a kernel config flag `CONFIG_USERSPACE_SSHD` (default false).
2. Modify `src/main.rs` to only start the kernel SSH server if `!CONFIG_USERSPACE_SSHD`.
3. Update `scripts/populate_disk.sh` to include the new `sshd` binary in `/bin/sshd`.

## 5. Shell Fallback Implementation
If `/bin/paws` cannot be spawned, a simple loop will:
1. Print a welcome message.
2. Provide a prompt `akuma-fallback> `.
3. Handle `help` (list basic commands), `exit` (close session), and `echo`.

## 6. Verification Plan
1. Build `sshd` userspace binary.
2. Verify it starts and listens on the configured port (e.g., 2222).
3. Connect via standard `ssh` client.
4. Verify authentication works.
5. Verify `paws` shell is interactive and functional.
6. Verify fallback shell works if `paws` is renamed/missing.
