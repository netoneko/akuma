# SocketSet Exhaustion Panic Fix

**Date:** February 26, 2026  
**Issue:** Kernel panic when all smoltcp socket slots are consumed.

## Symptom

After extended uptime (~131M heartbeat loops), the kernel panics:

```
!!! PANIC !!!
Location: .../smoltcp-0.12.0/src/iface/socket_set.rs:83
Message: adding a socket to a full SocketSet
```

The panic fires inside `SocketSet::add()` because the socket storage is a
fixed-size borrowed slice (`ManagedSlice::Borrowed`), which panics instead of
growing when full.

## Root Cause

Two problems combine to exhaust the socket pool:

1. **No capacity guard.** `socket_create()` called `net.sockets.add()` without
   checking whether slots were available. The function signature
   (`-> Option<SocketHandle>`) implied fallibility, but the panic fired before
   any `None` could be returned.

2. **Stuck sockets in pending_removal.** `socket_close()` initiates a TCP FIN
   handshake and queues the handle for garbage collection. The GC loop (inside
   `poll()`) only removes sockets that have reached `tcp::State::Closed`. If the
   remote never responds (dead connection, NAT timeout, network loss), the
   socket stays in FIN-WAIT / TIME-WAIT / LAST-ACK indefinitely, permanently
   occupying a slot.

Over long uptimes, leaked slots accumulate until the pool (128 slots) is
exhausted. Any subsequent `socket_create()` — from SSH, userspace syscalls,
or HTTP requests — triggers the panic.

## Fix

All changes in `src/smoltcp_net.rs`:

### 1. Capacity guard in `socket_create()`

Count occupied slots before calling `add()`. If full, return `None` without
allocating the 128KB of TCP buffers. All callers already handle `None`/`Err`
gracefully.

### 2. Timeout-based forced GC

Changed `pending_removal` from `Vec<SocketHandle>` to
`Vec<(SocketHandle, u64)>`, recording the close timestamp in microseconds.

The GC loop now force-aborts and removes any socket that has been pending for
more than 30 seconds (`SOCKET_GC_TIMEOUT_US = 30_000_000`), regardless of its
TCP state. This prevents indefinite slot leaks from unresponsive peers.

### 3. Increased MAX_SOCKETS

Bumped from 128 to 256. Empty `SocketStorage` slots are cheap (a few bytes
each); the real memory cost is only paid when sockets are created (128KB per
socket for RX/TX buffers). This provides headroom while the GC keeps the pool
healthy.
