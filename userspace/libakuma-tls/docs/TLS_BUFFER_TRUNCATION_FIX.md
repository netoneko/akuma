# TLS Buffer Truncation Fix

## Symptom

Large file downloads via `download_file_with_headers` were silently truncated. Specifically, `box pull busybox` downloaded only ~217KB of a ~1.9MB layer blob from Docker Hub's CDN.

The download appeared to succeed (no error returned), but the file on disk was incomplete.

## Root cause

`TLS_RECORD_SIZE` was set to **16384** bytes (16KB). This value is the maximum TLS 1.3 *plaintext* size, but the internal buffers passed to `embedded-tls` (`TlsConnection::new(transport, read_buf, write_buf)`) must hold an entire **encrypted** TLS record, which is larger:

```
TLS 1.3 record on the wire:
  5 bytes   record header (content type, version, length)
  N bytes   encrypted payload:
              up to 16384 bytes plaintext
              + 1 byte  inner content type
              + 16 bytes AES-128-GCM authentication tag
```

Maximum encrypted record = 5 + 16384 + 1 + 16 = **16406 bytes**.

With a 16384-byte buffer, any TLS record carrying close to 16384 bytes of plaintext would overflow the buffer. `embedded-tls` would return an error, which `TlsStream::read` mapped to `Error::IoError`. The streaming download loop in `stream_body_to_fd_tls` treated `IoError` as a transient failure and retried up to 200 times — but since the buffer was permanently too small, every retry also failed. After 200 consecutive failures, the loop gave up and the download stopped.

### Why ~217KB specifically?

The first few TLS records from the CDN (HTTP response headers, initial body chunks) may be smaller than the maximum. Once the CDN fills a TLS record to capacity — which happens after roughly 13-14 records worth of body data — the buffer overflow occurs. 13 × 16384 ≈ 213KB, close to the observed 217KB (the difference accounts for HTTP headers and partial records).

## Fix

Changed `TLS_RECORD_SIZE` from 16384 to **17408** (17KB) in `src/lib.rs`:

```rust
pub const TLS_RECORD_SIZE: usize = 17408;
```

17408 provides 1002 bytes of headroom beyond the maximum encrypted record size (16406), which accommodates the record header, AEAD tag, content type byte, and any potential padding.

All code paths that allocate TLS buffers use this constant, so the fix propagates to every TLS connection:
- `https_fetch` / `https_get` / `https_post` (in-memory responses)
- `download_file` (streaming to disk)
- `download_file_with_headers` (streaming with custom headers + redirect following)
- `HttpStreamTls` (streaming response reader)

## Regression test

`box test --net` includes `busybox_layer_size` which:
1. Fetches the busybox arm64 manifest from Docker Hub
2. Downloads the layer blob via `download_file_with_headers`
3. Reads the downloaded file and compares its size to the manifest's declared `size` field
4. Fails if any bytes are missing

This test would have caught the original bug and will catch any future buffer size regressions.

## Related

- `stream_body_to_fd_tls` in `src/http.rs` — the streaming loop affected by this bug
- `TlsStream::read` in `src/lib.rs` — maps all `embedded-tls` errors to `IoError`, making the root cause non-obvious (all errors look like transient I/O failures)
- `embedded-tls` crate docs note that buffers should be "larger than 16KB"
