# TLS Infrastructure

This document describes the TLS (Transport Layer Security) infrastructure in Akuma, covering both kernel and userspace implementations.

## Overview

Akuma provides TLS 1.3 support in two separate environments:

| Environment | Library | Mode | Certificate Verification |
|-------------|---------|------|-------------------------|
| Kernel | embedded-tls 0.17 | Async | Full X.509 verification |
| Userspace | embedded-tls 0.17 | Blocking | NoVerify (Phase 1) |

Both use the same underlying TLS library (`embedded-tls`) but with different I/O modes to match their respective runtime environments.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         USERSPACE                                │
├─────────────────────────────────────────────────────────────────┤
│  meow / wget / other programs                                   │
│       │                                                          │
│       ▼                                                          │
│  libakuma-tls                                                    │
│  ├── TlsStream (blocking)                                        │
│  ├── https_fetch() helper                                        │
│  ├── TlsRng (uses GETRANDOM syscall)                            │
│  └── TcpTransport (embedded-io adapter)                         │
│       │                                                          │
│       ▼                                                          │
│  libakuma                                                        │
│  ├── TcpStream (TCP sockets)                                    │
│  ├── getrandom() syscall wrapper                                │
│  └── DNS resolution                                              │
├─────────────────────────────────────────────────────────────────┤
│                    SYSCALL BOUNDARY                              │
├─────────────────────────────────────────────────────────────────┤
│                          KERNEL                                  │
├─────────────────────────────────────────────────────────────────┤
│  Shell Commands (curl)                                          │
│       │                                                          │
│       ▼                                                          │
│  src/tls.rs                                                      │
│  ├── TlsStream (async)                                          │
│  ├── TlsOptions                                                  │
│  └── TlsContext                                                  │
│       │                                                          │
│       ├──────────────────────┐                                   │
│       ▼                      ▼                                   │
│  src/tls_rng.rs        src/tls_verifier.rs                      │
│  (VirtIO RNG)          (X.509 verification)                     │
│       │                      │                                   │
│       ▼                      ▼                                   │
│  src/rng.rs            Crypto libs                              │
│  (VirtIO driver)       (p256, ed25519, rsa, sha2)               │
└─────────────────────────────────────────────────────────────────┘
```

## Kernel TLS

### Files

| File | Purpose |
|------|---------|
| `src/tls.rs` | Async TLS stream wrapper using embedded-tls |
| `src/tls_rng.rs` | RNG adapter wrapping VirtIO RNG for TLS |
| `src/tls_verifier.rs` | Custom X.509 certificate verifier |

### Features

- **Async I/O**: Uses `embedded_io_async` traits, runs on Embassy executor
- **Full Certificate Verification**: Validates server certificates using X.509
- **Signature Algorithms**: ECDSA P-256, Ed25519, RSA (PKCS#1 v1.5, PSS)
- **Hostname Verification**: Checks Subject Alternative Names and Common Name
- **Insecure Mode**: Optional `-k` flag to skip verification (like curl)

### Usage (Kernel Shell)

```bash
# HTTPS with certificate verification
curl https://example.com

# HTTPS without certificate verification
curl -k https://self-signed.example.com
```

### Dependencies (Kernel)

```toml
embedded-tls = { version = "0.17", features = ["alloc"] }
x509-cert = { version = "0.2" }
der = { version = "0.7", features = ["alloc", "oid"] }
p256 = { version = "0.13", features = ["ecdsa"] }
ed25519-dalek = { version = "2", features = ["alloc"] }
rsa = { version = "0.9", features = ["sha2"] }
sha2 = { version = "0.10" }
```

## Userspace TLS

### Files

| File | Purpose |
|------|---------|
| `userspace/libakuma-tls/src/lib.rs` | TlsStream wrapper, Error types |
| `userspace/libakuma-tls/src/rng.rs` | RNG using GETRANDOM syscall |
| `userspace/libakuma-tls/src/transport.rs` | embedded-io adapter for TcpStream |
| `userspace/libakuma-tls/src/http.rs` | https_fetch() helper function |

### Features

- **Blocking I/O**: Uses `embedded_io` (sync) traits, no async runtime needed
- **NoVerify Mode**: Phase 1 skips certificate verification (like `curl -k`)
- **Simple API**: `https_fetch(url, insecure)` for easy HTTPS requests
- **HTTP + HTTPS**: Supports both protocols in one function

### Usage (Userspace)

```rust
use libakuma_tls::https_fetch;

// Fetch from HTTPS URL (insecure = true skips cert verification)
let body = https_fetch("https://raw.githubusercontent.com/user/repo/main/file.txt", true)?;
```

### Dependencies (Userspace)

```toml
[dependencies]
libakuma = { path = "../libakuma" }
embedded-tls = { version = "0.17", default-features = false, features = ["alloc"] }
embedded-io = { version = "0.6", default-features = false }
rand_core = { version = "0.6", default-features = false }
```

## GETRANDOM Syscall

The kernel provides random bytes to userspace via syscall 304.

### Kernel Side (`src/syscall.rs`)

```rust
pub const GETRANDOM: u64 = 304;

fn sys_getrandom(buf_ptr: u64, len: usize) -> u64 {
    // Fills userspace buffer with random bytes from VirtIO RNG
    // Max 256 bytes per call
}
```

### Userspace Side (`userspace/libakuma/src/lib.rs`)

```rust
pub fn getrandom(buf: &mut [u8]) -> Result<usize, i32> {
    syscall(syscall::GETRANDOM, buf.as_mut_ptr() as u64, buf.len() as u64, ...)
}
```

## TLS Buffer Requirements

TLS 1.3 requires 16KB buffers for record handling:

```rust
pub const TLS_RECORD_SIZE: usize = 16384;

// Allocate buffers before TLS handshake
let mut read_buf = vec![0u8; TLS_RECORD_SIZE];
let mut write_buf = vec![0u8; TLS_RECORD_SIZE];
```

**Memory Impact**: Each TLS connection requires ~32KB for buffers.

## Cipher Suites

Both kernel and userspace use `Aes128GcmSha256`:

- **Key Exchange**: ECDHE with P-256
- **Encryption**: AES-128-GCM
- **Hash**: SHA-256

## Differences: Kernel vs Userspace

| Aspect | Kernel | Userspace |
|--------|--------|-----------|
| I/O Mode | Async (`embedded_io_async`) | Blocking (`embedded_io`) |
| Runtime | Embassy executor | None |
| RNG Source | Direct VirtIO RNG | GETRANDOM syscall |
| Cert Verification | Full X.509 | NoVerify (Phase 1) |
| Binary Size Impact | N/A (part of kernel) | ~200-300KB per binary |
| Socket Type | `embassy_net::TcpSocket` | `libakuma::net::TcpStream` |

## Phase 2: Certificate Verification for Userspace

Future work to add proper certificate verification to userspace:

1. Port `src/tls_verifier.rs` to `libakuma-tls`
2. Add crypto dependencies: `x509-cert`, `der`, `p256`, `ed25519-dalek`, `rsa`
3. Implement `TlsVerifier` trait for userspace
4. Add `insecure` flag to `https_fetch()` for optional bypass

**Estimated Additional Size**: ~300-400KB per binary

## Error Handling

### Kernel Errors

```rust
pub enum TlsError {
    InvalidCertificate,
    InvalidSignature,
    InvalidSignatureScheme,
    // ... other embedded-tls errors
}
```

### Userspace Errors

```rust
pub enum Error {
    DnsError,
    ConnectionError(String),
    TlsError(TlsError),
    HttpError(String),
    InvalidUrl,
    IoError,
}
```

## Testing

### Kernel TLS

```bash
# In kernel shell
curl https://httpbin.org/get
curl -k https://self-signed.badssl.com/
```

### Userspace TLS (meow)

The meow AI assistant can use the HttpFetch tool:

```json
{
  "command": {
    "tool": "HttpFetch",
    "args": {"url": "https://raw.githubusercontent.com/user/repo/main/README.md"}
  }
}
```

## Security Considerations

1. **Phase 1 Userspace**: NoVerify mode is vulnerable to MITM attacks. Only use for trusted networks or development.

2. **VirtIO RNG**: Cryptographic security depends on QEMU/host providing good entropy.

3. **No Certificate Revocation**: Neither implementation checks CRL/OCSP.

4. **TLS 1.3 Only**: Older TLS versions not supported (by design - security).

## Related Documentation

- [USERSPACE_NETWORKING_SUCCESS.md](USERSPACE_NETWORKING_SUCCESS.md) - TCP socket implementation
- [USERSPACE_SOCKET_API.md](USERSPACE_SOCKET_API.md) - Socket syscall interface
- [SSH.md](SSH.md) - SSH implementation (uses same crypto primitives)
