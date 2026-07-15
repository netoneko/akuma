# Userspace Networking Implementation

This document describes the successful implementation of userspace networking in Akuma, enabling user programs to use BSD-style socket APIs via Linux-compatible syscalls.

## Overview

The implementation provides a `std::net`-like API in `libakuma` that translates to blocking syscalls, which the kernel implements using embassy-net's async TCP stack.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Userspace (httpd, wget)                  │
│                                                             │
│   libakuma::net::TcpListener, TcpStream                     │
│         │                                                   │
│         ▼                                                   │
│   socket(), bind(), listen(), accept(), send(), recv()      │
└─────────────────────────────────────────────────────────────┘
                              │ syscall
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                         Kernel                              │
│                                                             │
│   sys_socket, sys_bind, sys_listen, sys_accept,             │
│   sys_sendto, sys_recvfrom, sys_close                       │
│         │                                                   │
│         ▼                                                   │
│   socket::KernelSocket + socket::SocketHandle               │
│   (Fixed array SOCKET_TABLE, Box<TcpSocket>)                │
│         │                                                   │
│         ▼                                                   │
│   embassy-net TcpSocket (async, polled synchronously)       │
│         │                                                   │
│         ▼                                                   │
│   VirtIO network driver                                     │
└─────────────────────────────────────────────────────────────┘
```

## Key Implementation Details

### 1. TcpSocket Boxing (Critical Fix)

Embassy-net's `TcpSocket` cannot be moved after `accept()` completes. Moving it corrupts internal state and causes crashes. The solution is to Box the socket immediately upon creation:

```rust
// In block_on_accept():
let socket_boxed = alloc::boxed::Box::new(TcpSocket::new(stack, rx_buf, tx_buf));
// Socket stays at fixed heap location throughout its lifetime
```

### 2. VirtIO MMIO Mapping Fix

The user address space page tables were missing mappings for VirtIO MMIO devices (0x0a000000+). This caused crashes when the kernel tried to access the network device:

```rust
// In mmu.rs add_kernel_mappings():
// Map 0x08000000-0x0BFFFFFF (GIC, UART, VirtIO MMIO)
for i in 64..96 {  // Extended from 64..80
    let pa = (i as u64) * 0x200000;
    core::ptr::write_volatile(l2_ptr.add(i), pa | device_block_flags);
}
```

### 3. Blocking Syscall Pattern

All socket syscalls use a polling loop pattern with preemption control:

```rust
fn block_on_accept(...) -> Result<usize, i32> {
    let socket_boxed = Box::new(TcpSocket::new(stack, rx_buf, tx_buf));
    let socket_cell = UnsafeCell::new(socket_boxed);
    
    loop {
        crate::threading::disable_preemption();
        let result = accept_fut.poll(&mut cx);
        crate::threading::enable_preemption();
        
        match result {
            Poll::Ready(Ok(())) => { /* store socket, return fd */ }
            Poll::Pending => {
                crate::threading::yield_now();
            }
        }
    }
}
```

### 4. Graceful Socket Close

To ensure buffered data is transmitted before connection termination:

```rust
pub fn socket_close(idx: usize) -> Result<(), i32> {
    // Mark as closing
    socket.state = SocketState::Closing;
    
    // Graceful close - sends FIN
    socket.close();
    
    // Yield to let network stack transmit
    for _ in 0..10 {
        crate::threading::yield_now();
    }
    
    remove_socket(idx);
}
```

### 5. Send Flush

After writing all data, the syscall flushes to ensure transmission:

```rust
fn sys_sendto(...) {
    // ... write data ...
    
    if total_written >= len {
        // Flush to ensure transmission
        socket.flush();
        // Yield to network stack
    }
}
```

## Syscall Numbers (Linux-compatible)

| Syscall | Number | Description |
|---------|--------|-------------|
| SOCKET | 198 | Create socket |
| BIND | 200 | Bind to address |
| LISTEN | 201 | Mark as listening |
| ACCEPT | 202 | Accept connection |
| CONNECT | 203 | Connect to remote |
| SENDTO | 206 | Send data |
| RECVFROM | 207 | Receive data |
| SHUTDOWN | 210 | Shutdown socket |

## Buffer Management

- Fixed buffer pool with 32 slots
- Each slot: 4KB RX + 4KB TX buffers
- Deferred cleanup via queue (freed by network runner)

## Working Example

The `httpd` userspace program successfully serves HTTP requests:

```
$ curl http://localhost:8080/
<!DOCTYPE html>
<html>
<head>
    <title>Akuma HTTP Server</title>
</head>
<body>
    <h1>Welcome to Akuma!</h1>
    <p>This page is being served by a userspace HTTP server.</p>
</body>
</html>
```

## Debugging Journey

The implementation required solving several challenging bugs:

1. **Initial crash after accept**: FAR=0xa000250 - traced to VirtIO MMIO unmapped in user address space
2. **Empty responses**: Socket abort() called before data transmitted - fixed with graceful close()
3. **TcpSocket corruption**: Moving TcpSocket after accept() - fixed by Boxing immediately

## Files Modified

- `src/syscall.rs` - Socket syscall implementations
- `src/socket.rs` - KernelSocket, SocketHandle, buffer pool
- `src/mmu.rs` - Extended device memory mapping
- `userspace/libakuma/src/net.rs` - TcpListener, TcpStream
- `userspace/libakuma/src/lib.rs` - Syscall wrappers
- `userspace/httpd/src/main.rs` - HTTP server

## Date

January 24, 2026
