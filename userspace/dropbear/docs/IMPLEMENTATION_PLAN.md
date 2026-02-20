# Dropbear SSH Server Implementation Plan

This plan outlines the integration of the [Dropbear SSH server](https://github.com/mkj/dropbear) into the Akuma userspace environment. Dropbear will provide a robust, userspace-alternative to the built-in kernel SSH server.

## Goals
1.  **Git Submodule**: Add Dropbear as a git submodule for easy updates.
2.  **Rust Build Process**: Build Dropbear using `build.rs` and the `cc` crate, adhering to the "Building Userspace Software" guide.
3.  **No Shell Scripts**: Use only Rust and `build.rs` for compilation and packaging.
4.  **Packaging**: Produce a `.tar` archive suitable for `pkg install`.
5.  **Service Management**: Integrate with `herd` for automatic startup.
6.  **Compatibility**: Use the existing `/etc/sshd/authorized_keys` for authentication.
7.  **Shell Integration**: Launch `/bin/paws` on login.
8.  **Kernel SSH Port**: Make the built-in SSH server port configurable for debugging.

## Phase 1: Repository Setup
- Add git submodule: `git submodule add https://github.com/mkj/dropbear userspace/dropbear/dropbear`
- Create directory structure:
  - `userspace/dropbear/Cargo.toml`
  - `userspace/dropbear/build.rs`
  - `userspace/dropbear/src/main.rs` (Rust wrapper/launcher)

## Phase 2: Kernel Configuration
- **Port Configurability**: 
  - Move `SSH_PORT` from `src/ssh/server.rs` to `src/config.rs`.
  - Update `src/ssh/server.rs` to use `config::SSH_PORT`.
  - Update `src/main.rs` print statements to reflect the configured port.
  - This allows setting kernel SSH to a backup port (e.g., 2223) while Dropbear uses 22.

## Phase 3: Building Dropbear (`build.rs`)
- **Configuration**: Run Dropbear's `configure` (or provide a static `config.h`) to disable features not supported by Akuma (like PAM, UTMP, shadows passwords).
- **Compilation**:
  - Use `cc` crate to compile all necessary Dropbear `.c` files.
  - Include `../musl/dist/include` for POSIX compatibility.
  - Define `DROPBEAR_SFTPSERVER_PATH` to `/bin/sftp-server` (if implemented later).
  - Define `DEFAULT_PATH` to include `/bin`.
- **Packaging**:
  - Create a staging directory.
  - Copy `dropbear` binary to `usr/bin/dropbear`.
  - Create a `usr/bin/sh` symlink (or copy) to `/bin/paws` if needed by dropbear, or ensure `DROPBEAR_PATH_SSH_PROGRAM` points to paws.
  - Use `tar` with the specific Akuma settings:
    - `COPYFILE_DISABLE=1`
    - `--no-xattrs`
    - `--format=ustar`

## Phase 4: Integration & Service Configuration
- **Authorized Keys**: Ensure Dropbear looks for keys in `/etc/sshd/authorized_keys`. This might require a small patch to Dropbear or a symlink.
- **Login Shell**: Configure Dropbear to execute `/bin/paws` for the `user` account.
- **Herd Configuration**:
  - Create `/etc/herd/available/dropbear.conf`:
    ```ini
    command = /usr/bin/dropbear
    args = -F -E -p 22 -r /etc/dropbear/dropbear_rsa_host_key
    restart_delay = 5000
    ```
- **Host Keys**: Dropbear requires host keys. A startup script or the Rust wrapper should generate them if they don't exist:
  ```bash
  dropbearkey -t rsa -f /etc/dropbear/dropbear_rsa_host_key
  ```

## Phase 5: Testing & Validation
1.  **Build**: `cargo build -p dropbear`
2.  **Package**: Ensure `dist/dropbear.tar` is created.
3.  **Serve**: Start Python web server in `bootstrap/`.
4.  **Install**: Connect via kernel SSH (on backup port) and run `pkg install dropbear`.
5.  **Enable**: `herd enable dropbear`.
6.  **Connect**: Attempt to connect to port 22 and verify `/bin/paws` shell.

## Key Considerations for Gemini (Agent)
- Use `libakuma` for any host-key generation logic in the Rust wrapper.
- Ensure `build.rs` handles the specific `musl` include paths correctly.
- If `dropbear` requires `pw_shell` from `getpwnam`, a stub in `musl` or the Rust wrapper might be needed to return `/bin/paws`.
