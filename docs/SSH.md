# SSH Server Implementation

This document describes the SSH-2 server implementation in Akuma, including the protocol, authentication, and configuration.

## Overview

Akuma implements a minimal but functional SSH-2 server that supports:

- **Key Exchange**: curve25519-sha256 (ECDH)
- **Host Key**: ssh-ed25519
- **Encryption**: aes128-ctr
- **MAC**: hmac-sha2-256
- **Compression**: none
- **Authentication**: publickey (Ed25519)
- **Multiple concurrent sessions**: Up to 4 simultaneous connections

## Module Structure

```
src/ssh/
├── mod.rs           # Module exports and re-exports
├── protocol.rs      # SSH-2 protocol state machine and packet handling
├── server.rs        # TCP accept loop with connection pooling
├── crypto.rs        # Cryptographic primitives (AES-CTR, HMAC, key derivation)
├── keys.rs          # Host key management (load/generate/persist)
├── config.rs        # Configuration file parsing
└── auth.rs          # User authentication (publickey verification)
```

## Configuration

### Config File Location

```
/etc/sshd/sshd.conf
```

### Available Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `disable_key_verification` | bool | `false` | When `true`, accepts any authentication without verifying keys. **WARNING: Insecure, for testing only.** |

### Example Configuration

```ini
# SSH Server Configuration

# Set to true to accept any authentication without verifying keys
# WARNING: This is insecure and should only be used for testing
disable_key_verification = false
```

## Host Keys

### Key Storage

Host keys are stored in `/etc/sshd/`:

| File | Description |
|------|-------------|
| `id_ed25519` | Private key (raw 32 bytes) |
| `id_ed25519.pub` | Public key in SSH format |
| `authorized_keys` | Authorized client public keys |

### Key Generation

On first startup, if no host key exists:

1. A new Ed25519 keypair is generated using hardware RNG (or timer-based fallback)
2. Private key is saved to `/etc/sshd/id_ed25519`
3. Public key is saved to `/etc/sshd/id_ed25519.pub` in SSH format
4. The public key is also added to `/etc/sshd/authorized_keys`

### Authorized Keys Format

The `authorized_keys` file uses standard OpenSSH format:

```
# Comments start with #
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... optional-comment
ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... another-key
```

## Authentication Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                    SSH Authentication Flow                       │
└─────────────────────────────────────────────────────────────────┘

Client                                              Server
  │                                                    │
  │  SSH_MSG_USERAUTH_REQUEST (method="none")          │
  ├───────────────────────────────────────────────────►│
  │                                                    │
  │  SSH_MSG_USERAUTH_FAILURE (methods="publickey")    │
  │◄───────────────────────────────────────────────────┤
  │                                                    │
  │  SSH_MSG_USERAUTH_REQUEST (method="publickey",     │
  │                            has_signature=false)    │
  ├───────────────────────────────────────────────────►│
  │                                                    │  Check if key
  │                                                    │  is in authorized_keys
  │  SSH_MSG_USERAUTH_PK_OK (key accepted)             │
  │◄───────────────────────────────────────────────────┤
  │                                                    │
  │  SSH_MSG_USERAUTH_REQUEST (method="publickey",     │
  │                            has_signature=true,     │
  │                            signature)              │
  ├───────────────────────────────────────────────────►│
  │                                                    │  Verify signature
  │                                                    │  against session_id
  │  SSH_MSG_USERAUTH_SUCCESS                          │
  │◄───────────────────────────────────────────────────┤
  │                                                    │
```

## Protocol State Machine

```
┌──────────────────┐
│  AwaitingVersion │  ◄── Initial state
└────────┬─────────┘
         │ Receive client version
         ▼
┌──────────────────┐
│  AwaitingKexInit │
└────────┬─────────┘
         │ Exchange KEXINIT
         ▼
┌────────────────────┐
│ AwaitingKexEcdhInit│
└────────┬───────────┘
         │ ECDH key exchange
         ▼
┌──────────────────┐
│  AwaitingNewKeys │
└────────┬─────────┘
         │ Encryption activated
         ▼
┌────────────────────────┐
│ AwaitingServiceRequest │
└────────┬───────────────┘
         │ Service "ssh-userauth"
         ▼
┌──────────────────┐
│ AwaitingUserAuth │  ◄── May loop for retries
└────────┬─────────┘
         │ Authentication success
         ▼
┌──────────────────┐
│  Authenticated   │
└────────┬─────────┘
         │ Channel open, shell request
         ▼
┌──────────────────┐
│   Shell Session  │
└────────┬─────────┘
         │ Exit/disconnect
         ▼
┌──────────────────┐
│   Disconnected   │
└──────────────────┘
```

## Connecting

### From Host (with QEMU port forwarding)

```bash
ssh -o StrictHostKeyChecking=no -p 2222 user@localhost
```

### SSH Client Configuration

To avoid host key warnings during development:

```
# ~/.ssh/config
Host akuma
    HostName localhost
    Port 2222
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
```

Then connect with:

```bash
ssh akuma
```

## Security Considerations

1. **Host Key Persistence**: The host key is stored unencrypted in the filesystem. In a production system, consider encrypting at rest.

2. **Key Verification**: The `disable_key_verification` option should **never** be enabled in production. It allows any client to connect without authentication.

3. **Authorized Keys**: Only Ed25519 keys are supported. RSA and other key types are rejected.

4. **Session Security**: Each session uses unique encryption keys derived from the ECDH shared secret and session ID.

## API Reference

### Public Functions

#### `ssh::run(stack: Stack<'static>)`

Starts the SSH server on port 22. This is an async function that runs indefinitely, accepting and handling connections.

#### `ssh::init_host_key()`

Synchronous initialization of a temporary host key. Called for backward compatibility.

#### `ssh::init_host_key_async()`

Async initialization that loads the host key from `/etc/sshd/id_ed25519` or generates a new one if not present.

### Configuration Types

#### `SshdConfig`

```rust
pub struct SshdConfig {
    pub disable_key_verification: bool,
}
```

### Authentication Types

#### `AuthResult`

```rust
pub enum AuthResult {
    Success,                    // Authentication successful
    Failure,                    // Authentication failed
    PublicKeyOk(Vec<u8>),      // Key is acceptable (query response)
}
```

## Troubleshooting

### "Key not in authorized_keys"

The client's public key is not listed in `/etc/sshd/authorized_keys`. Add the client's public key to this file.

### "Unsupported key algorithm"

Only `ssh-ed25519` keys are supported. Use an Ed25519 key:

```bash
ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519_akuma
```

### "Connection refused"

Ensure the SSH server is running and the port (22, or 2222 via QEMU) is accessible.

### Host key changes on reboot

If the filesystem is not persistent, a new host key is generated on each boot. This triggers SSH warnings. Use `StrictHostKeyChecking=no` during development.

