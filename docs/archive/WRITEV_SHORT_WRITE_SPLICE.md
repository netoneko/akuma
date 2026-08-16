# `writev` ignored short writes and spliced the stream

**Date:** 2026-08-16
**Status:** FIXED, with an A/B that reproduces the corruption on demand.
**Short version:** `sys_writev` moved on to the next iovec after a *partial*
write, so the tail that never went out was replaced by the following iovec's
bytes. Every reply larger than smoltcp's 16 KB TX window came out of a socket
with a hole in it. `sys_readv` has always had the mirror guard; `writev` was
simply missed.

---

## 1. Symptom

```
$ redis-cli -p 4444
127.0.0.1:4444> keys *
Error: Protocol error, got "\n" as reply type byte
```

Also seen once as `got "t" as reply type byte`. Both from the *client's* parser:
it finished one reply, expected the next to begin with a RESP type byte
(`+ - : $ *`), and found something else.

Three properties made this hard to place:

- **It happened from the host *and* from inside the VM.** So not the QEMU port
  forward, and not anything client-side.
- **`PING` never failed.** Small replies were always fine; `KEYS *` on a
  populated database failed. Size-dependent, not command-dependent.
- **It was intermittent** at small database sizes, because the threshold is not
  the number of keys but the byte size of one reply against the free TX window.

## 2. The bug

`src/syscall/fs.rs`, `sys_writev`, before the fix:

```rust
for iov in kernel_iovs.iter().take(iov_cnt) {
    let written = sys_write(fd_num, iov.iov_base, iov.iov_len);
    if (written as i64) < 0 {
        if total_written == 0 { return written; }
        break;
    }
    total_written += written;
}                      // <-- no check that `written == iov.iov_len`
```

POSIX: a partial `writev` means everything after the written prefix was **not**
written, and the caller resumes from `total_written`. Continuing to the next
iovec instead writes it directly *after* the truncated bytes — and the caller,
told only a total, resumes from a point that never corresponds to what actually
went out. The stream now has a hole.

`sys_readv`, twenty lines above it, gets this right:

```rust
total_read += n;
if (n as usize) < iov.iov_len { break; }
```

## 3. Why it fires constantly rather than rarely

Short writes are the **normal** case on this kernel, not an edge case:

- `socket_send` returns whatever `smoltcp::tcp::Socket::send_slice` accepted,
  bounded by the 16 KB `TCP_TX_BUFFER_SIZE`.
- `alloc_net_bounce` degrades to a single 4 KiB page under memory pressure, and
  caps at `NET_BOUNCE_MAX` (64 KiB) regardless.

And the reason a dropped tail became a *splice* rather than a harmless stall:
`socket_send` ends with `smoltcp_net::poll()`, which pushes the queued bytes onto
the wire and frees TX space. So by the time the loop reaches the next iovec,
the socket can accept again — and does.

Redis writes replies with `writev` over many iovecs (`_writevToClient`, up to 64
of them), which is why it was the program that exposed this.

## 4. A/B

`SET bigkey <N bytes>` / `GET bigkey`, with a non-repeating payload so a hole
cannot hide behind identical neighbours, verified byte-for-byte
(`bigreply.py`-style RESP client; the reusable probe is
`scripts/redis_stream_integrity.py` for the connection-level check).

Same VM, same image, same Redis, only `sys_writev` differing:

| Reply size | Before | After |
|---|---|---|
| 4 KiB | OK | OK |
| 16 KiB | OK | OK |
| 64 KiB | **CORRUPT** — first mismatch at byte 17844 | OK |
| 256 KiB | **CORRUPT** — first mismatch at byte 199519 | OK |
| 1 MiB | **CORRUPT** — first mismatch at byte 985950 | OK |

The threshold sits between 16 KiB and 64 KiB — the TX buffer, as predicted.

The decisive detail is *which* byte arrives wrong. In all three failures the
payload byte was replaced by **`0x0d`** — a carriage return. That is the `\r\n`
terminator of the *next* iovec in Redis's reply, spliced in where the truncated
payload should have continued. It is also, read by a client one reply later,
precisely the `"\n"` in `got "\n" as reply type byte`.

## 5. Fix

Stop at the first short write, via a named pure predicate so the rule has a
place to be tested and explained:

```rust
pub const fn writev_stops_after(written: u64, want: usize) -> bool {
    written < want as u64
}
```

Zero-length iovecs are skipped rather than written (mirroring `readv`), so an
empty entry in the middle of a vector cannot be mistaken for a short write.

**Tests.** `run_writev_short_write_tests` (boot suite, registered in
`src/process_tests.rs`) covers the decision: fully-written continues, short
stops, zero-of-one stops, zero-of-zero continues, and the 16 KiB-of-64 KiB case
that corrupted Redis. It deliberately does **not** claim to cover the splice —
staging a real short write followed by an accepting one needs a peer draining
the far end concurrently, which the boot suite (single-threaded, no network)
cannot do. The end-to-end check is the A/B above against a live VM.

## 6. What to take from this

- **A short write is not a partial success to be worked around; it is a stop
  condition.** Any loop that writes a sequence of buffers has to honour it, and
  the one place that got it right (`readv`) was in the same file, twenty lines
  away.
- **The first wrong byte is worth more than the fact of corruption.** "Corrupt
  at byte 17844" only says something is broken; "the byte is `0x0d`, and `0x0d`
  is what the *next* buffer starts with" names the mechanism outright.
- **Trust the size threshold.** The failure appearing between 16 KiB and 64 KiB,
  and nowhere below, pointed at `TCP_TX_BUFFER_SIZE` before any code was read.
- The symptom was reported as a Redis protocol error, and Redis was blameless:
  it exposed the bug because it is the first program here to write large replies
  through a vectored write.

## Background

- `REDIS_END_TO_END.md` — the run in which this surfaced; §7 recorded the first
  sighting as unexplained before this was found.
- `../reference/subsystems/syscalls/fs.md` — `readv`/`writev` semantics.
- `../reference/subsystems/syscalls/net.md` — the socket write path and the TX
  window.
- `../runbooks/run-redis.md` — the recipe, and the `Protocol error` row in its
  troubleshooting table.
