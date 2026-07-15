# VirtIO Receive Fix (Feb 2026)

## Problem

After migrating from `embassy-net` to direct `smoltcp 0.12.0`, external network connectivity (SSH, HTTP) was completely broken. Loopback traffic worked because it bypasses VirtIO entirely via `LoopbackAwareDevice`.

## Root Causes

Two bugs in `VirtioSmoltcpDevice::receive()` in `src/smoltcp_net.rs` prevented any external packets from being received.

### Bug 1: Receive buffer never posted to VirtIO device

VirtIO networking requires the driver to submit receive buffers *before* the device can DMA incoming packets into them. The API is three-phase:

1. `receive_begin(buffer)` — post a buffer to the device's available ring, get a token
2. `poll_receive()` — check if the device has filled any buffer (returns the token)
3. `receive_complete(token, buffer)` — retrieve the completed receive

The old `embassy_virtio_driver.rs` correctly maintained an `rx_pending_token: Option<u16>` across calls:

```rust
// Old driver pattern (correct)
if let Some(token) = self.rx_pending_token {
    if self.inner.poll_receive().is_some() {
        self.inner.receive_complete(token, &mut rx.buffer[..])  // complete previous
    }
} else {
    let token = self.inner.receive_begin(&mut rx.buffer[..]);   // post new buffer
    self.rx_pending_token = Some(token);
}
```

The new code checked `poll_receive()` first and only called `receive_begin()` inside:

```rust
// New driver (broken)
if self.inner.poll_receive().is_some() {    // always None — no buffer posted!
    self.inner.receive_begin(...)            // never reached
    self.inner.receive_complete(...)
}
```

Since `poll_receive()` returns `None` when no buffer has been submitted, `receive_begin()` was never called, and the device never had a buffer to fill. This created a permanent deadlock where no external packets could ever be received.

### Bug 2: VirtIO net header not stripped from receive buffer

`VirtIONetRaw::receive_complete()` returns `(hdr_len, pkt_len)` where the buffer layout is:

```
rx_buffer: [VirtIO header (10 bytes)] [Ethernet frame (pkt_len bytes)]
```

The old driver correctly offset into the buffer:

```rust
rx.offset = hdr_len;
let data = &mut rx.buffer[offset..offset + len];  // skip VirtIO header
```

The new code started from byte 0:

```rust
buffer: &mut rx_buffer[0..pkt_len]  // includes VirtIO header, truncates frame
```

This meant smoltcp would see VirtIO header bytes where it expected Ethernet headers, corrupting every received packet.

### Bug 3: Incorrect Ethernet MTU

The `max_transmission_unit` was set to 1500 (IP payload size). For `Medium::Ethernet`, smoltcp subtracts the 14-byte Ethernet header to compute IP MTU, resulting in a 1486-byte IP MTU instead of the standard 1500. The old driver used 1514.

## Fixes

All changes in `src/smoltcp_net.rs`:

1. **Added `rx_token: Option<u16>` field** to `VirtioSmoltcpDevice` to track pending receive buffers across calls.

2. **Rewrote `receive()` with two-phase pattern**: Phase 1 posts a buffer if none is pending; Phase 2 checks for completion and retrieves the packet. Applied to both `VirtioSmoltcpDevice::receive()` and `LoopbackAwareDevice::receive()` (VirtIO fallback path).

3. **Offset receive buffer by `hdr_len`** to skip the VirtIO net header, so smoltcp sees the actual Ethernet frame.

4. **Changed MTU from 1500 to 1514** for correct Ethernet frame sizing.
