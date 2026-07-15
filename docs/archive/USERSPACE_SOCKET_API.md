# Userspace Socket API

This document describes the socket API for userspace programs, enabling network-capable applications like HTTP servers and wget.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        Userspace                            │
├─────────────────────────────────────────────────────────────┤
│  httpd / wget                                               │
│       │                                                     │
│       ▼                                                     │
│  libakuma::net (TcpListener, TcpStream)                     │
│       │                                                     │
│       ▼                                                     │
│  libakuma syscall wrappers (socket, bind, connect, etc.)    │
│       │                                                     │
│       ▼ (SVC #0)                                            │
├─────────────────────────────────────────────────────────────┤
│                         Kernel                              │
├─────────────────────────────────────────────────────────────┤
│  syscall.rs handlers                                        │
│       │                                                     │
│       ▼                                                     │
│  socket.rs (KernelSocket, SOCKET_TABLE)                     │
│       │                                                     │
│       ▼                                                     │
│  embassy-net TcpSocket (async)                              │
│       │                                                     │
│       ▼                                                     │
│  Network Runner (Thread 0)                                  │
└─────────────────────────────────────────────────────────────┘
```

## Syscall Reference

### Socket Syscalls (Linux aarch64 compatible)

| Syscall   | Number | Signature                                         |
|-----------|--------|---------------------------------------------------|
| socket    | 198    | `socket(domain, type, protocol) -> fd`            |
| bind      | 200    | `bind(fd, addr_ptr, addr_len) -> 0/-errno`        |
| listen    | 201    | `listen(fd, backlog) -> 0/-errno`                 |
| accept    | 202    | `accept(fd, addr_ptr, addr_len_ptr) -> new_fd`    |
| connect   | 203    | `connect(fd, addr_ptr, addr_len) -> 0/-errno`     |
| sendto    | 206    | `sendto(fd, buf, len, flags, ...) -> bytes`       |
| recvfrom  | 207    | `recvfrom(fd, buf, len, flags, ...) -> bytes`     |
| shutdown  | 210    | `shutdown(fd, how) -> 0/-errno`                   |
| close     | 57     | `close(fd) -> 0/-errno`                           |

### File I/O Syscalls

| Syscall | Number | Signature                                           |
|---------|--------|-----------------------------------------------------|
| openat  | 56     | `openat(dirfd, path_ptr, path_len, flags, mode) -> fd` |
| lseek   | 62     | `lseek(fd, offset, whence) -> new_offset`           |
| fstat   | 80     | `fstat(fd, statbuf_ptr) -> 0/-errno`                |
| read    | 1      | `read(fd, buf_ptr, len) -> bytes` (extended)        |
| write   | 2      | `write(fd, buf_ptr, len) -> bytes` (extended)       |

### DNS Syscall (Custom)

| Syscall      | Number | Signature                                        |
|--------------|--------|--------------------------------------------------|
| resolve_host | 300    | `resolve_host(hostname_ptr, len, result_ptr) -> 0/-errno` |

Result is 4 bytes IPv4 in network byte order.

## libakuma API

### Low-Level Syscall Wrappers

```rust
// Sockets
fn socket(domain: i32, sock_type: i32, protocol: i32) -> i32;
fn bind(fd: i32, addr: &SocketAddrV4) -> i32;
fn listen(fd: i32, backlog: i32) -> i32;
fn accept(fd: i32) -> i32;
fn connect(fd: i32, addr: &SocketAddrV4) -> i32;
fn send(fd: i32, buf: &[u8], flags: i32) -> isize;
fn recv(fd: i32, buf: &mut [u8], flags: i32) -> isize;
fn shutdown(fd: i32, how: i32) -> i32;
fn close(fd: i32) -> i32;

// DNS
fn resolve_host(hostname: &str) -> Result<[u8; 4], i32>;

// Files
fn open(path: &str, flags: u32) -> i32;
fn fstat(fd: i32) -> Result<Stat, i32>;
fn lseek(fd: i32, offset: i64, whence: i32) -> i64;
fn read_fd(fd: i32, buf: &mut [u8]) -> isize;
fn write_fd(fd: i32, buf: &[u8]) -> isize;
```

### High-Level std::net-Compatible API

```rust
use libakuma::net::{TcpListener, TcpStream, Error};

// Server
let listener = TcpListener::bind("0.0.0.0:8080")?;
let (stream, addr) = listener.accept()?;

// Client  
let stream = TcpStream::connect("192.168.1.1:80")?;
stream.write_all(b"GET / HTTP/1.0\r\n\r\n")?;
let mut buf = [0u8; 1024];
let n = stream.read(&mut buf)?;

// DNS
let ip = libakuma::net::resolve("example.com")?;
```

## Kernel Data Structures

### File Descriptor Table (per-process)

```rust
pub enum FileDescriptor {
    Stdin,
    Stdout,
    Stderr,
    Socket(usize),      // Index into SOCKET_TABLE
    File(KernelFile),   // Open file handle
}

pub struct KernelFile {
    pub path: String,
    pub position: usize,
    pub flags: u32,
}
```

FDs 0-2 are pre-allocated for stdin/stdout/stderr. New FDs start at 3.

### Socket Table (global)

```rust
pub struct KernelSocket {
    pub state: SocketState,
    pub buffer_slot: usize,     // Index into BUFFER_POOL
    pub ref_count: AtomicU32,   // For close-during-use protection
    pub socket_type: i32,
    pub is_listener: bool,
}

pub enum SocketState {
    Unbound,
    Bound { local_addr: SocketAddrV4 },
    Listening { local_addr: SocketAddrV4, backlog: usize },
    Connected { local_addr: SocketAddrV4, remote_addr: SocketAddrV4 },
    Closing,
    Closed,
}
```

### Buffer Pool

Static allocation of 32 socket buffer pairs (4KB RX + 4KB TX each). Uses atomic flags for lock-free allocation. Deferred cleanup queue ensures buffers aren't freed while embassy-net references them.

## Concurrency Safety

### Lock Hierarchy

```
Level 1: MOUNT_TABLE
Level 2: ext2.state, MemoryFilesystem.root
Level 2.5: SOCKET_TABLE
Level 3: BLOCK_DEVICE
Level 4: TALC (always with IRQs disabled)

Per-process: FD_TABLE -> SOCKET_TABLE -> (no further locks)
```

### Key Patterns

1. **FD table before socket table**: Copy data out of FD table before acquiring socket table lock
2. **Release locks before yielding**: Blocking syscalls must not hold locks across yield points
3. **Preemption disabled for embassy-net**: Embassy uses RefCell internally; disable preemption during poll
4. **Deferred buffer cleanup**: Queue buffers for cleanup by network runner after polling

## Userspace Programs

### httpd (HTTP Server)

- Listens on port 8080
- Serves files from `/public/` directory
- Supports GET and HEAD methods
- Returns proper Content-Type headers

```
# Start the server
/bin/httpd
```

### wget (HTTP Client)

- Downloads files via HTTP
- Resolves hostnames via DNS syscall
- Parses HTTP responses

```
# Download a file
wget http://example.com/file.txt
wget http://192.168.1.1:8080/data.json output.json
```

## Files Modified/Created

### Kernel
- `src/process.rs` - FD table, FileDescriptor enum, KernelFile
- `src/socket.rs` - NEW: KernelSocket, buffer pool, socket table
- `src/syscall.rs` - Socket, DNS, and file I/O syscall handlers
- `src/main.rs` - Added `mod socket`

### Userspace
- `userspace/libakuma/src/lib.rs` - Syscall wrappers
- `userspace/libakuma/src/net.rs` - NEW: TcpListener, TcpStream
- `userspace/httpd/` - NEW: HTTP server crate
- `userspace/wget/` - NEW: wget utility crate
- `userspace/Cargo.toml` - Added httpd, wget to workspace

## Future Work

- Integrate accept/connect/send/recv with embassy-net async operations
- Add SO_REUSEADDR and other socket options
- Support non-blocking sockets with O_NONBLOCK
- Add UDP socket support (SOCK_DGRAM)
- IPv6 support
