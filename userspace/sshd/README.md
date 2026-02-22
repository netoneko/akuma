# Akuma SSHD (Userspace)

A userspace implementation of the SSH-2 server for Akuma OS.

## Features
- **Standalone Execution**: Runs as a standard userspace process.
- **Protocol Support**: SSH-2 with `curve25519-sha256` key exchange and `ssh-ed25519` host keys.
- **Default Shell**: Integrates with `/bin/paws` for a rich interactive experience.
- **Fallback Shell**: Includes a built-in rudimentary shell for system recovery.
- **Libakuma Based**: Uses the standard Akuma userspace library for all system interactions.

## Installation
The `sshd` binary should be placed in `/bin/sshd`. 
Configuration and keys are expected in `/etc/sshd/`.

## Usage
To start the SSH server from the shell:
```bash
/bin/sshd
```

## Configuration
Configuration is loaded from `/etc/sshd/sshd.conf`.
Host keys are loaded from `/etc/sshd/ssh_host_ed25519_key`.

## Development
See `docs/IMPLEMENTATION_PLAN.md` for detailed architecture and migration notes.
