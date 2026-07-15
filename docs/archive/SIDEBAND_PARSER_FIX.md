# Sideband pkt-line parser fix

## Symptom

`scratch clone` fails quickly with "TLS connection lost during chunked transfer"
after the sideband buffer grows indefinitely:

```
scratch: sideband buffer 50548 bytes, first bytes: <binary>
scratch: sideband buffer 51918 bytes, first bytes: <binary>
...
scratch: sideband buffer 65618 bytes, first bytes: <binary>
scratch: clone failed: TLS connection lost during chunked transfer
```

The buffer grows by ~1370 bytes per iteration (one TLS record) with zero
consumption. The server eventually gives up waiting and drops the connection.

## Root Cause

The `SidebandState` pkt-line demuxer in `userspace/scratch/src/protocol.rs` had
two bugs that caused it to get permanently stuck on non-hex data:

### 1. Lossy hex parsing via `from_utf8().unwrap_or("0000")`

```rust
let len_str = core::str::from_utf8(len_hex).unwrap_or("0000");
let len = match u16::from_str_radix(len_str, 16) { ... };
```

When the first 4 bytes of the buffer were not valid UTF-8 (binary pack data),
`from_utf8` failed and `unwrap_or("0000")` substituted a flush packet. This
consumed 4 bytes of binary data as if they were a pkt-line header, misaligning
the parser from pkt-line boundaries.

### 2. Permanent stall on non-hex ASCII

Once misaligned, the parser eventually encountered 4 bytes that were valid UTF-8
but not valid hex (e.g., `\x01` channel byte followed by ASCII). The `Err` branch
only checked for `b"PACK"` at position 0:

```rust
Err(_) => {
    if self.buffer.starts_with(b"PACK") {
        pack_data.extend_from_slice(&self.buffer);
        self.buffer.clear();
    }
    break;  // nothing consumed — stuck forever
}
```

If the buffer didn't start with "PACK", the parser broke without consuming
anything. Every subsequent call added more data to the buffer but hit the same
break on the same stuck bytes. The buffer grew until the TLS connection timed out.

## Fix

### Strict hex validation (`userspace/scratch/src/protocol.rs`)

Replaced the lossy `from_utf8`/`unwrap_or` pattern with explicit byte-level
hex validation:

```rust
let valid_hex = len_hex.iter().all(|b| matches!(b,
    b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'));
```

If not valid hex, the parser scans the entire buffer for `b"PACK"` magic to
resynchronize. This handles:

- Misaligned pkt-line boundaries
- Raw pack data after the sideband section ends
- Unexpected data between NAK and sideband frames

### Raw passthrough mode

Once PACK magic is found (or after sideband framing ends), the parser sets
`in_pack_data = true` and bypasses pkt-line parsing entirely on all subsequent
calls. This eliminates any further risk of getting stuck.

### Drain on no magic

If the buffer has non-hex data but no PACK magic yet (split across calls), the
parser drains everything except the last 3 bytes (in case "PACK" spans the
boundary), preventing unbounded buffer growth.

## Also fixed: chunked transfer CRLF consumption

`ChunkedState::ExpectingCrlf` in `userspace/scratch/src/stream.rs` had a related
bug: when the trailing 2 bytes after chunk data were not `\r\n`, it transitioned
to `ExpectingSize` without consuming them. Those stale bytes became the start of
the next "chunk size line," causing a misparse. Fixed to always consume 2 bytes
in this state.

## Also fixed: silent error swallowing in libakuma-tls

The same "silent break on error" pattern existed in `libakuma-tls/src/http.rs`.
See `userspace/libakuma-tls/docs/ERROR_HANDLING_FIX.md` for details.

## Related

- `docs/SCRATCH_CLONE_DECOMPRESSION_FIX.md` — earlier TLS error swallowing fix
  and O_TRUNC bug
- `docs/WRITE_AT_SYSCALL.md` — sys_write performance fix that reduced TLS
  timeouts caused by slow disk I/O
