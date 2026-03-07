# Box Test Suite

`box test` runs an in-binary test suite that validates the box utility's core logic. Tests execute directly on Akuma — no host toolchain or separate test crate needed.

## Usage

```
box test            # Run offline tests only (JSON, OCI ref, HTTP parsing)
box test --net      # Also run network integration tests (downloads from Docker Hub)
```

## Test categories

### JSON parser (`json.rs`)

Tests the minimal hand-rolled JSON extraction used for OCI manifest/config parsing:

- String extraction (basic, with escape sequences)
- Object extraction
- Array extraction and iteration
- String array parsing (for Entrypoint/Cmd)
- Manifest list detection (with/without top-level `mediaType`)
- Real Docker manifest parsing
- Platform matching (arm64/aarch64 resolution)

### OCI ref parser (`oci.rs`)

Tests image reference parsing:

- Simple name (`busybox` → `registry-1.docker.io/library/busybox:latest`)
- Name with tag (`ubuntu:22.04`)
- Name with user namespace (`myuser/myapp:v1`)
- Custom registry (`ghcr.io/owner/repo:sha-abc`)
- Docker Hub rewrite (`docker.io/...` → `registry-1.docker.io/...`)
- Registry with port (`localhost:5000/myimage:dev`)

### HTTP header parsing (`libakuma_tls`)

Tests the HTTP header parsing functions used by the download pipeline:

- `find_headers_end` — locates `\r\n\r\n` boundary
- Missing headers detection

### Network integration (requires `--net`)

End-to-end tests that hit Docker Hub over HTTPS:

- **busybox_manifest**: Fetches auth token, downloads manifest list, verifies it contains a `manifests` array with an arm64 entry
- **busybox_layer_size**: Full download pipeline — fetches manifest, resolves arm64 platform, downloads the layer blob via `download_file_with_headers`, and verifies the downloaded file size matches the manifest's declared size exactly

The `busybox_layer_size` test catches the TLS download truncation regression that was caused by undersized TLS record buffers (see below).

## TLS buffer truncation bug (fixed)

**Symptom**: `box pull busybox` downloaded only ~217KB of a ~1.9MB layer.

**Root cause**: `TLS_RECORD_SIZE` was set to 16384 bytes, but TLS 1.3 records on the wire can be up to 16406 bytes (5-byte header + 16384 plaintext + 1-byte content type + 16-byte AES-GCM tag). When the CDN sent a full-size TLS record, `embedded-tls` couldn't fit it in the buffer and returned an error. Since `TlsStream::read` maps all errors to `IoError`, the retry loop exhausted its 200-attempt budget and the download stopped.

**Fix**: Increased `TLS_RECORD_SIZE` from 16384 to 17408 in `libakuma-tls/src/lib.rs`.
