# libakuma-tls: error handling fix

## Problem

Four functions in `libakuma-tls/src/http.rs` silently swallowed read errors,
making it impossible for callers to distinguish a clean EOF from a dropped
connection.

### `read_response_tls` / `read_response_tcp` (non-streaming)

```rust
Err(_) => break, // returns Ok(response) with truncated data
```

A TLS or TCP read error broke out of the loop and returned `Ok(response)` even
if the response was incomplete. The caller (e.g., `https_fetch`, `https_get`,
`https_post`) received a truncated body and passed it to `parse_http_response`.
If the error occurred before any data arrived, this produced an empty response
that failed at header parsing with a confusing "Invalid HTTP response" error
instead of the real I/O error.

### `HttpStream::read_chunk` / `HttpStreamTls::read_chunk` (streaming)

```rust
Err(_) => StreamResult::Done,
```

Connection failures were returned as `StreamResult::Done`, indistinguishable from
a clean end-of-stream. Callers like meow's streaming API client already had a
`StreamResult::Error` handler, but it was never triggered — errors silently ended
the stream, potentially producing truncated LLM responses.

## Fix

### Non-streaming: fail early on zero-data errors

```rust
Err(_) => {
    if response.is_empty() {
        return Err(Error::IoError);
    }
    break;
}
```

If the error occurs before any data arrives, return `Err(IoError)` immediately.
If some data was already received, break and let `parse_http_response` validate
the response (some servers close TCP without a clean TLS `close_notify` on
HTTP/1.0, so a mid-stream error after receiving a complete response is tolerable).

Same treatment applied to `read_response_tcp`, including the `WouldBlock`/
`TimedOut` path: a timeout with zero data is now an error, not a silent empty
response.

### Streaming: report errors through `StreamResult::Error`

`HttpStream::read_chunk` (TCP):

```rust
// Before:
Err(_) => StreamResult::Done,

// After:
Err(_) => StreamResult::Error(Error::IoError),
```

`HttpStreamTls::read_chunk` (TLS):

```rust
// Before:
Err(_) => StreamResult::Done,

// After:
Err(e) => StreamResult::Error(e),
```

The TLS variant now passes the actual `Error` value through (preserving whether
it was a TLS-level or I/O-level failure) rather than discarding it.

## Impact

- **meow**: Streaming LLM responses now correctly report "Server returned error"
  on connection drops instead of silently treating a truncated response as
  complete. The existing `StreamResult::Error` handler at
  `userspace/meow/src/api/client.rs` is now actually reachable.

- **scratch**: Uses its own streaming implementation (`process_pack_streaming`)
  rather than libakuma-tls streaming, but `https_fetch`/`https_get` calls for
  ref discovery benefit from the non-streaming fix.

- **wget**: Uses `https_fetch` which calls `read_response_tls` — will now get a
  clear `IoError` instead of an empty/truncated response on connection failure.

## Related

- `docs/SIDEBAND_PARSER_FIX.md` — sideband demuxer fix in scratch
- `docs/SCRATCH_CLONE_DECOMPRESSION_FIX.md` — earlier TLS error swallowing fix
  in scratch's `process_pack_streaming`
