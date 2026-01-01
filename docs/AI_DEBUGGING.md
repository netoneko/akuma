# AI Debugging Flow for Userspace Processes

This document describes the workflow for debugging userspace processes in Akuma.

## Overview

When debugging userspace packages (e.g., `stdcheck`, `echo2`), follow this workflow to verify changes and test execution.

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
| Build userspace package | `cd userspace && cargo build --release -p <package>` |
| Start package server | `cd userspace && python3 -m http.server 8000` |
| Run userspace process | `ssh -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=no user@localhost -p 2222 <package>` |
| Install package | `ssh -i ~/.ssh/id_ed25519 -o StrictHostKeyChecking=no user@localhost -p 2222 "pkg install <package>"` |

## Notes

- `stdcheck` is used as an example; substitute it for any userspace package you're debugging
- Always use the release build for proper testing
- The SSH connection uses port 2222 and the `user` account
- Host key checking is disabled (`-o StrictHostKeyChecking=no`) for convenience during development
- The Python web server must be running on port 8000 for `pkg install` to work

