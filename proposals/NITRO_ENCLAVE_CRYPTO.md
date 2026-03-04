# Nitro Enclave Crypto: Hardware-Isolated Key Material for Akuma

Proposal for using AWS Nitro Enclaves as a hardware security module (HSM) for SSH host keys, TLS private keys, and cryptographic attestation — so key material never exists in kernel memory.

## The Problem

Akuma's SSH host key (`/etc/sshd/id_ed25519`) is generated at boot, stored on the ext2 filesystem, and loaded into kernel memory (`src/ssh/keys.rs`). The Ed25519 `SigningKey` lives in a `Spinlock<Option<SigningKey>>` global for the lifetime of the kernel. Anyone with kernel memory access — a bug, a crash dump, a compromised SSH session — can extract it.

This is the same problem every SSH server has, but Akuma is in a unique position to fix it: running on Firecracker on Graviton means Nitro Enclaves are available, and the modularization plan's generic trait boundaries make the enclave a drop-in replacement for the in-kernel crypto.

## What Nitro Enclaves Provide

A Nitro Enclave is an isolated virtual machine with:

- **No network access** — cannot be reached from the internet or the parent instance's network
- **No persistent storage** — no disk, no filesystem
- **No interactive access** — no SSH, no console, no shell
- **Only vsock** — a single VirtIO socket connection to the parent instance
- **Cryptographic attestation** — a signed document proving what code is running, signed by AWS

The enclave is a black box that accepts requests over vsock and returns results. The parent instance cannot inspect its memory, even with root access.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  EC2 Graviton instance                                      │
│                                                             │
│  ┌───────────────────────────────────────────────────────┐  │
│  │  Firecracker microVM                                  │  │
│  │                                                       │  │
│  │  ┌───────────────────────────────────────────────┐    │  │
│  │  │  Akuma kernel                                 │    │  │
│  │  │                                               │    │  │
│  │  │  SSH server ──── NitroEnclaveCrypto            │    │  │
│  │  │                      │                         │    │  │
│  │  │                      │ vsock (CID 16, port 5000) │  │  │
│  │  │                      │                         │    │  │
│  │  └──────────────────────┼─────────────────────────┘    │  │
│  └─────────────────────────┼─────────────────────────────┘  │
│                            │                                │
│  ┌─────────────────────────┼─────────────────────────────┐  │
│  │  Nitro Enclave          │                             │  │
│  │                         ▼                             │  │
│  │  ┌─────────────────────────────────────────────┐      │  │
│  │  │  enclave-crypto (minimal binary)            │      │  │
│  │  │                                             │      │  │
│  │  │  Ed25519 SigningKey (generated at boot,     │      │  │
│  │  │  never exported, never touches disk)        │      │  │
│  │  │                                             │      │  │
│  │  │  Operations:                                │      │  │
│  │  │  ┌─────────────────────────────────────┐    │      │  │
│  │  │  │ GET_PUBLIC_KEY → public key bytes    │    │      │  │
│  │  │  │ SIGN(data)     → Ed25519 signature  │    │      │  │
│  │  │  │ ATTEST(nonce)  → attestation doc    │    │      │  │
│  │  │  │ GET_FINGERPRINT → SHA256 fingerprint│    │      │  │
│  │  │  └─────────────────────────────────────┘    │      │  │
│  │  └─────────────────────────────────────────────┘      │  │
│  │                                                       │  │
│  │  No network. No disk. No shell. Only vsock.           │  │
│  └───────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

## How It Maps to the Modularization Plan

The kernel modularization plan (Phase 4) extracts SSH crypto into `akuma-ssh-crypto` with an `Rng` trait. The key management in `src/ssh/keys.rs` currently:

1. Checks `/etc/sshd/id_ed25519` on the filesystem
2. If missing, generates a new Ed25519 keypair using `SimpleRng`
3. Stores the `SigningKey` in a global `Spinlock<Option<SigningKey>>`
4. Signs SSH handshake challenges via `ed25519_dalek::Signer::sign()`

With the generic kernel struct, swapping the crypto backend is a type parameter change:

```rust
// Standard — key material lives in kernel memory
type StandardKernel = Kernel<VirtioBlock, Ext2Fs<VirtioBlock>, SmoltcpNet, InMemorySshCrypto<HwRng>>;

// Enclave — key material never leaves the enclave
type EnclaveKernel = Kernel<VirtioBlock, Ext2Fs<VirtioBlock>, SmoltcpNet, NitroEnclaveSshCrypto>;
```

`NitroEnclaveSshCrypto` implements the same trait but forwards `sign()` over vsock instead of calling `ed25519_dalek` directly.

## Vsock Protocol (Enclave Communication)

Simple request/response protocol over AF_VSOCK. The enclave listens on CID 16 (assigned by Nitro), port 5000.

### Message format

```
┌──────────┬──────────┬──────────────────┐
│ op (u8)  │ len (u32)│ payload (bytes)  │
└──────────┴──────────┴──────────────────┘
```

### Operations

| Op | Name | Request payload | Response payload |
|----|------|-----------------|------------------|
| 0x01 | GET_PUBLIC_KEY | (empty) | 32-byte Ed25519 public key |
| 0x02 | SIGN | data to sign (variable length) | 64-byte Ed25519 signature |
| 0x03 | ATTEST | 32-byte nonce | CBOR-encoded attestation document |
| 0x04 | GET_FINGERPRINT | (empty) | 32-byte SHA256 of public key |
| 0x10 | DH_EXCHANGE | 32-byte X25519 client public | 32-byte X25519 server public + 32-byte shared secret (encrypted to enclave's ephemeral key) |

### Error response

```
┌──────────┬──────────┬──────────┐
│ 0xFF     │ 4        │ error u32│
└──────────┴──────────┴──────────┘
```

Error codes: 1 = unknown op, 2 = invalid payload, 3 = internal error.

## Attestation: Proving the Host Key Is Genuine

When a client connects via SSH, the server can provide cryptographic proof that:

1. **The host key was generated inside the enclave** — it has never existed anywhere else
2. **The enclave is running specific, measured code** — PCR0 (image hash), PCR2 (application hash)
3. **The key cannot be exported** — the enclave has no mechanism to output the private key
4. **The enclave runs on genuine AWS Nitro hardware** — the attestation document is signed by the AWS Nitro Attestation PKI

### Attestation document structure (CBOR/COSE)

```
{
  "module_id": "i-0abc123...-enc0abc...",
  "timestamp": 1709567890000,
  "digest": "SHA384",
  "pcrs": {
    0: <enclave image hash>,
    1: <kernel + bootstrap hash>,
    2: <application hash>,
    3: <parent IAM role hash>,
    4: <parent instance ID hash>,
    8: <signing certificate hash>
  },
  "certificate": <DER-encoded certificate>,
  "cabundle": [<root CA>, <intermediate CAs>...],
  "public_key": <Ed25519 public key>,
  "user_data": <custom data>,
  "nonce": <client-provided nonce>
}
```

### Embedding attestation in SSH

Two approaches:

**Option A: SSH banner**

Include the attestation hash in the SSH banner (displayed before auth). The `akuma_40.txt` ASCII art banner already appears via `src/ssh/protocol.rs`. Append the attestation fingerprint:

```
                      =#=      .-
                      +*#*:.:-**
                      ... (akuma art) ...

  Host key attestation: nitro:sha384:a3b4c5d6...
  Verify: https://verify.akuma.dev/<fingerprint>
```

**Option B: SSH extension**

Use SSH user auth extension (`publickey-hostbound@openssh.com` or a custom extension) to pass the full attestation document during key exchange. A custom SSH client or verification tool can validate it against the AWS Nitro Attestation PKI root certificate.

## Implementation Plan

### Phase 1 — VirtIO-vsock Driver (~300 lines)

Add a VirtIO-vsock device driver to Akuma. Vsock uses the standard VirtIO transport (same as virtio-blk and virtio-net, which Akuma already supports).

**VirtIO device ID:** 19 (vsock)

**Virtqueues:**
- Queue 0: RX (receive from host/enclave)
- Queue 1: TX (send to host/enclave)
- Queue 2: Event (connection events)

**Syscall interface:**
```rust
// New socket type: AF_VSOCK = 40
sys_socket(AF_VSOCK, SOCK_STREAM, 0) -> fd
sys_connect(fd, vsock_addr { cid, port }) -> 0
sys_read(fd, buf) / sys_write(fd, buf)
```

Alternatively, the vsock driver can be kernel-internal only (no syscall exposure) — `NitroEnclaveSshCrypto` uses it directly without going through the socket layer.

### Phase 2 — Enclave Crypto Binary (~200 lines)

A minimal Rust binary that runs inside the Nitro Enclave. Built as a static aarch64 binary, packaged into an Enclave Image File (EIF) using `nitro-cli build-enclave`.

```rust
fn main() {
    // Generate Ed25519 keypair (in enclave memory only)
    let mut rng = EnclaveRng::new();  // /dev/random inside enclave
    let signing_key = SigningKey::generate(&mut rng);
    let verifying_key = signing_key.verifying_key();

    // Listen on vsock port 5000
    let listener = VsockListener::bind(VMADDR_CID_ANY, 5000);

    loop {
        let stream = listener.accept();
        let op = stream.read_u8();
        match op {
            GET_PUBLIC_KEY => {
                stream.write(&verifying_key.to_bytes());
            }
            SIGN => {
                let data = stream.read_payload();
                let sig = signing_key.sign(&data);
                stream.write(&sig.to_bytes());
            }
            ATTEST => {
                let nonce = stream.read_payload();
                let doc = nsm_get_attestation_document(
                    Some(&verifying_key.to_bytes()),
                    None,
                    Some(&nonce),
                );
                stream.write(&doc);
            }
            _ => stream.write_error(ERR_UNKNOWN_OP),
        }
    }
}
```

Dependencies: `ed25519-dalek`, `aws-nitro-enclaves-nsm-api` (for attestation), vsock bindings.

### Phase 3 — NitroEnclaveSshCrypto (~150 lines)

Kernel-side implementation that replaces `src/ssh/keys.rs` behavior:

```rust
pub struct NitroEnclaveSshCrypto {
    vsock_cid: u32,
    vsock_port: u32,
    cached_public_key: Option<[u8; 32]>,
}

impl SshHostKey for NitroEnclaveSshCrypto {
    fn public_key(&self) -> &[u8; 32] {
        if self.cached_public_key.is_none() {
            self.cached_public_key = Some(self.vsock_call(GET_PUBLIC_KEY, &[]));
        }
        self.cached_public_key.as_ref().unwrap()
    }

    fn sign(&self, data: &[u8]) -> [u8; 64] {
        self.vsock_call(SIGN, data)
    }
}
```

The public key is cached (it never changes during enclave lifetime). Each `sign()` call is a vsock round-trip (~50-100us), which is fine since SSH key exchange happens once per connection.

### Phase 4 — Attestation Display (~50 lines)

Add the attestation fingerprint to the SSH banner in `src/ssh/protocol.rs`. On first connection, the SSH server requests an attestation document from the enclave and caches it.

### Phase 5 — Extend to TLS and Disk Encryption (future)

The same enclave can hold multiple key types:

```rust
pub trait CryptoHsm {
    fn ed25519_sign(&self, key_id: &str, data: &[u8]) -> Result<[u8; 64], HsmError>;
    fn ed25519_public_key(&self, key_id: &str) -> Result<[u8; 32], HsmError>;
    fn x25519_dh(&self, key_id: &str, their_public: &[u8; 32]) -> Result<[u8; 32], HsmError>;
    fn aes_encrypt(&self, key_id: &str, plaintext: &[u8]) -> Result<Vec<u8>, HsmError>;
    fn aes_decrypt(&self, key_id: &str, ciphertext: &[u8]) -> Result<Vec<u8>, HsmError>;
    fn attest(&self, nonce: &[u8]) -> Result<Vec<u8>, HsmError>;
}
```

Use cases beyond SSH:
- **TLS server private key** — HTTPS never exposes the key to kernel memory
- **Disk encryption key** — ext2 encryption key provisioned via KMS into enclave
- **API signing** — outbound requests signed by enclave-held keys
- **Container image verification** — enclave verifies OCI image signatures before allowing `box run`

## Security Properties

| Threat | Without enclave | With enclave |
|--------|----------------|--------------|
| Kernel memory dump | Private key exposed | Key never in kernel memory |
| Kernel RCE | Attacker can sign as host | Attacker can request signatures but cannot extract key |
| Disk forensics | Key at `/etc/sshd/id_ed25519` | No key on disk — generated in enclave at boot |
| Cold boot attack | Key in DRAM | Key in enclave memory (separate, encrypted) |
| Supply chain (tampered kernel) | No detection | Attestation proves enclave code is genuine |
| MITM on first connect | TOFU — trust on first use | Attestation doc proves key authenticity |

**What the enclave does NOT protect against:**
- Compromised enclave code itself (but PCR2 measures it — auditable)
- Denial of service (attacker can crash the enclave, but can't extract keys)
- Side-channel attacks against the enclave CPU (theoretical, no known practical attacks on Nitro)

## Performance Impact

| Operation | Without enclave | With enclave | Impact |
|-----------|----------------|--------------|--------|
| SSH key exchange (once per connection) | ~50 us (ed25519 sign) | ~150 us (sign + vsock round-trip) | 3x slower, but happens once per connection — invisible |
| SSH packet encrypt/decrypt | Unchanged | Unchanged | AES-CTR stays in kernel (symmetric key derived during handshake) |
| SSH banner display | ~0 us | ~200 us (first attestation fetch, then cached) | One-time cost |
| TLS handshake (future) | ~100 us | ~250 us | Same pattern — asymmetric ops in enclave, symmetric in kernel |

The enclave only handles asymmetric operations (signing, DH exchange). Symmetric encryption (AES-CTR, HMAC) stays in kernel with the session-derived keys — these are ephemeral and not worth protecting with the enclave.

## Prerequisites

- Firecracker on Graviton (from DEMO_PROPOSAL.md)
- VirtIO-vsock driver in Akuma
- Nitro CLI on the host instance (`nitro-cli build-enclave`, `nitro-cli run-enclave`)
- EC2 instance with enclave support enabled (`--enclave-options 'Enabled=true'`)

## Estimated Effort

| Component | Lines | Depends on |
|-----------|-------|------------|
| VirtIO-vsock driver | ~300 | Existing VirtIO infrastructure |
| Enclave crypto binary | ~200 | ed25519-dalek, nsm-api |
| NitroEnclaveSshCrypto | ~150 | vsock driver, SSH trait extraction |
| Attestation in SSH banner | ~50 | Phase 3 |
| **Total** | **~700** | |

Plus build tooling: a `Makefile` or script to build the enclave EIF and launch it alongside Firecracker.

## Success Criteria

1. SSH host key is generated inside the Nitro Enclave and never touches kernel memory or disk
2. `ssh -p 2222 akuma@<host>` works normally — clients see no difference except the attestation in the banner
3. The attestation document can be independently verified against the AWS Nitro Attestation PKI
4. Restarting the enclave generates a new host key (no persistence — by design)
5. Dumping all kernel memory produces zero bytes of Ed25519 private key material
