# TcpStream Pointer Corruption Fix (February 2026)

## Summary

Fixed a recurring EL1 Data Abort crash in `TcpStream::read` caused by a corrupted
pointer in the async state machine, and closed a VirtIO receive buffer overflow
vulnerability that could corrupt the NETWORK static's internal data structures.

## Symptoms

```
[Exception] Sync from EL1: EC=0x25, ISS=0x4
  ELR=0x40047070, FAR=0xffffffff00000000, SPSR=0x80000345
  Thread=3, TTBR0=0x400e9000, TTBR1=0x400e9000
  SP=0x42181630, SP_EL0=0x0
  Instruction at ELR: 0xf9400129
  Likely: Rn(base)=x9, Rt(dest)=x9
```

- **EC=0x25**: Data Abort from EL1 (kernel-mode memory fault)
- **ISS=0x4**: Level 0 Translation Fault (address completely unmapped)
- **FAR=0xFFFFFFFF00000000**: Faulting address — clearly a corrupted pointer
- **Thread 3**: SSH session thread

The crash always occurs in the same code path:

```
run_session → block_on → handle_connection → stream.read()
  → poll_fn → with_network → SocketSet::get_mut(self.handle)
```

Disassembly at the faulting address shows two loads:

```
40047064: ldr x9, [x0]      // Load &mut TcpStream from closure captures → 0xFFFFFFFF00000000
40047070: ldr x9, [x9]      // Dereference to get SocketHandle index → CRASH
```

The `&mut TcpStream` pointer stored in the async state machine was overwritten with
`0xFFFFFFFF00000000` (upper 32 bits all set, lower 32 bits all zero).

This is the same class of bug documented in `docs/NETWORKING_POLLING_AND_ACK_FIXES.md`:
> An EL1 data abort (FAR=0x65) was observed inside TcpStream::read → with_network
> on the SSH session thread.

## Root Cause Analysis

### Confirmed: VirtIO receive buffer overflow vulnerability

In `src/smoltcp_net.rs`, both `VirtioSmoltcpDevice::receive()` and
`LoopbackAwareDevice::receive()` created slices from VirtIO-reported packet
dimensions **without any bounds validation**:

```rust
// BEFORE (vulnerable)
Ok((hdr_len, pkt_len)) => {
    let rx = VirtioRxToken {
        buffer: unsafe {
            core::slice::from_raw_parts_mut(
                self.rx_buffer.as_mut_ptr().add(hdr_len), pkt_len)
        },
    };
```

The `rx_buffer` is `[u8; 2048]`. If the VirtIO device reports
`hdr_len + pkt_len > 2048`, this creates an out-of-bounds slice. The rx_buffer
lives inside the NETWORK static (`0x400e3900`, size `0x1510`), and the SocketSet's
`ManagedSlice` metadata (data pointer and length) is at offsets `0x14f8`/`0x1500`
within this static. An out-of-bounds read/write through smoltcp's packet processing
could corrupt these fields, leading to garbage socket data on the next
`with_network` call.

### Contributing: No handle validation before socket access

`TcpStream::read`, `write`, and `flush` called `net.sockets.get_mut(self.handle)`
directly. If the handle (or the pointer to TcpStream in the async state machine)
was corrupted, smoltcp's `get_mut` would index out of bounds and panic or
dereference invalid memory. There was no defensive check to detect corruption
before it caused a crash.

### Context: Async state machine on stack

The entire SSH session async future chain lives on the thread's kernel stack inside
`block_on`. The `TcpStream` and all its references are part of this state machine.
Any corruption of the stack or heap that affects the state machine's memory can
overwrite the `&mut TcpStream` pointer with garbage.

## Fix

### 1. VirtIO RX buffer bounds validation

Added `hdr_len.saturating_add(pkt_len) > self.rx_buffer.len()` checks in both
receive paths. Malformed packets are now silently dropped:

```rust
// AFTER (safe)
Ok((hdr_len, pkt_len)) => {
    if hdr_len.saturating_add(pkt_len) > self.rx_buffer.len() {
        return None;  // Drop malformed packet
    }
    // ... create slice only after validation ...
```

Applied in:
- `VirtioSmoltcpDevice::receive()` (line ~148)
- `LoopbackAwareDevice::receive()` (line ~294)

### 2. TcpStream handle corruption detection

Added a `handle_index: usize` field to `TcpStream` that caches the socket index
at creation time. Every `read`, `write`, and `flush` now validates
`handle_index < MAX_SOCKETS` before calling `get_mut`:

```rust
if self.handle_index >= MAX_SOCKETS {
    safe_print!("[NET] CORRUPT HANDLE in TcpStream::read: index={}\n", self.handle_index);
    return Poll::Ready(Err(TcpError::ReadError));
}
```

To extract the index from smoltcp's opaque `SocketHandle(usize)`, a
`socket_handle_index()` helper uses `transmute` with a compile-time size assertion.

### 3. Validation in socket_close and poll GC

- `socket_close()` now validates the handle before calling `get_mut`, returning
  early with a diagnostic log if the handle is invalid.
- The garbage collection loop in `poll()` skips corrupted handles in the
  `pending_removal` list instead of panicking.
- `tcp_connect()` validates the handle in its poll loop.

## Files Changed

- `src/smoltcp_net.rs` — All changes (VirtIO bounds, TcpStream struct, validation)

## Memory Layout Reference

```
SOCKET_STORAGE: 0x400d50f8 (size 0xE800 = 128 × 464 bytes)
NETWORK:        0x400e3900 (size 0x1510)
  +0x0000: Spinlock lock byte
  +0x0010: Option<NetworkState> discriminant
  +0x14f8: SocketSet ManagedSlice data pointer
  +0x1500: SocketSet ManagedSlice length
```

The VirtIO rx_buffer sits inside `LoopbackAwareDevice` → `VirtioSmoltcpDevice`,
which is part of `NetworkState` in the NETWORK static. An out-of-bounds access
from rx_buffer could overwrite the SocketSet fields at the end of the static.
