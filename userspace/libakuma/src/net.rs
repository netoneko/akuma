//! std::net-compatible networking API
//!
//! Provides TcpListener and TcpStream types that mirror std::net's API.

use alloc::string::String;

use crate::{socket_const, SocketAddrV4};

/// Error type for network operations
#[derive(Debug, Clone)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    NotFound,
    PermissionDenied,
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    NotConnected,
    AddrInUse,
    AddrNotAvailable,
    BrokenPipe,
    AlreadyExists,
    WouldBlock,
    InvalidInput,
    InvalidData,
    TimedOut,
    WriteZero,
    Interrupted,
    UnexpectedEof,
    Other,
}

impl Error {
    pub fn new(kind: ErrorKind, message: &str) -> Self {
        Self {
            kind,
            message: String::from(message),
        }
    }

    pub fn from_errno(errno: i32) -> Self {
        let (kind, msg) = match errno {
            2 => (ErrorKind::NotFound, "No such file or directory"),
            4 => (ErrorKind::Interrupted, "Interrupted"),
            5 => (ErrorKind::Other, "I/O error"),
            9 => (ErrorKind::InvalidInput, "Bad file descriptor"),
            11 => (ErrorKind::WouldBlock, "Would block"),
            12 => (ErrorKind::Other, "Out of memory"),
            22 => (ErrorKind::InvalidInput, "Invalid argument"),
            98 => (ErrorKind::AddrInUse, "Address in use"),
            100 => (ErrorKind::Other, "Network is down"),
            106 => (ErrorKind::Other, "Already connected"),
            107 => (ErrorKind::NotConnected, "Not connected"),
            110 => (ErrorKind::TimedOut, "Connection timed out"),
            111 => (ErrorKind::ConnectionRefused, "Connection refused"),
            113 => (ErrorKind::Other, "Host unreachable"),
            _ => (ErrorKind::Other, "Unknown error"),
        };
        // For unknown errors, include the errno in the message for debugging
        if errno != 2 && errno != 4 && errno != 5 && errno != 9 && errno != 11 && 
           errno != 12 && errno != 22 && errno != 98 && errno != 100 && errno != 106 &&
           errno != 107 && errno != 110 && errno != 111 && errno != 113 {
            return Self {
                kind: ErrorKind::Other,
                message: alloc::format!("Unknown error (errno={})", errno),
            };
        }
        Self::new(kind, msg)
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl embedded_io_async::Error for Error {
    fn kind(&self) -> embedded_io_async::ErrorKind {
        match self.kind {
            ErrorKind::NotFound => embedded_io_async::ErrorKind::NotFound,
            ErrorKind::PermissionDenied => embedded_io_async::ErrorKind::PermissionDenied,
            ErrorKind::ConnectionRefused => embedded_io_async::ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset => embedded_io_async::ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted => embedded_io_async::ErrorKind::ConnectionAborted,
            ErrorKind::NotConnected => embedded_io_async::ErrorKind::NotConnected,
            ErrorKind::AddrInUse => embedded_io_async::ErrorKind::AddrInUse,
            ErrorKind::AddrNotAvailable => embedded_io_async::ErrorKind::AddrNotAvailable,
            ErrorKind::BrokenPipe => embedded_io_async::ErrorKind::BrokenPipe,
            ErrorKind::AlreadyExists => embedded_io_async::ErrorKind::AlreadyExists,
            ErrorKind::InvalidInput => embedded_io_async::ErrorKind::InvalidInput,
            ErrorKind::InvalidData => embedded_io_async::ErrorKind::InvalidData,
            // Map variants not present in embedded-io 0.6.1 to Other
            ErrorKind::WouldBlock |
            ErrorKind::TimedOut |
            ErrorKind::WriteZero |
            ErrorKind::Interrupted |
            ErrorKind::UnexpectedEof |
            ErrorKind::Other => embedded_io_async::ErrorKind::Other,
        }
    }
}

/// A TCP socket server, listening for connections.
pub struct TcpListener {
    fd: i32,
    local_addr: SocketAddrV4,
}

impl TcpListener {
    /// Creates a new TcpListener which will be bound to the specified address.
    pub fn bind(addr: &str) -> Result<Self, Error> {
        let socket_addr = parse_addr(addr)?;

        // Create socket
        let fd = crate::socket(
            socket_const::AF_INET,
            socket_const::SOCK_STREAM,
            0,
        );
        if fd < 0 {
            return Err(Error::from_errno(-fd));
        }

        // Bind
        let ret = crate::bind(fd, &socket_addr);
        if ret < 0 {
            crate::close(fd);
            return Err(Error::from_errno(-ret));
        }

        // Listen
        let ret = crate::listen(fd, 128);
        if ret < 0 {
            crate::close(fd);
            return Err(Error::from_errno(-ret));
        }

        Ok(Self {
            fd,
            local_addr: socket_addr,
        })
    }

    /// Accept a new incoming connection from this listener.
    pub fn accept(&self) -> Result<(TcpStream, SocketAddrV4), Error> {
        loop {
            let new_fd = crate::accept(self.fd);
            if new_fd >= 0 {
                // For now, we don't get the remote address from accept
                // TODO: Parse it from the sockaddr returned by accept
                let remote_addr = SocketAddrV4::new([0, 0, 0, 0], 0);

                return Ok((
                    TcpStream {
                        fd: new_fd,
                        local_addr: self.local_addr,
                        remote_addr,
                    },
                    remote_addr,
                ));
            }

            let errno = -new_fd;
            if errno == 11 { // EAGAIN/WouldBlock - kernel accept blocks, so this is rare
                continue;
            }

            return Err(Error::from_errno(errno as i32));
        }
    }

    /// Returns the local socket address of this listener.
    pub fn local_addr(&self) -> SocketAddrV4 {
        self.local_addr
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        crate::close(self.fd);
    }
}

/// A TCP stream between a local and a remote socket.
pub struct TcpStream {
    fd: i32,
    local_addr: SocketAddrV4,
    remote_addr: SocketAddrV4,
}

impl TcpStream {
    /// Opens a TCP connection to a remote host.
    pub fn connect(addr: &str) -> Result<Self, Error> {
        let socket_addr = parse_addr(addr)?;

        // Create socket
        let fd = crate::socket(
            socket_const::AF_INET,
            socket_const::SOCK_STREAM,
            0,
        );
        if fd < 0 {
            return Err(Error::from_errno(-fd));
        }

        // Connect
        let ret = crate::connect(fd, &socket_addr);
        if ret < 0 {
            crate::close(fd);
            return Err(Error::from_errno(-ret));
        }

        Ok(Self {
            fd,
            local_addr: SocketAddrV4::new([0, 0, 0, 0], 0),
            remote_addr: socket_addr,
        })
    }

    /// Returns the socket address of the remote peer of this TCP connection.
    pub fn peer_addr(&self) -> SocketAddrV4 {
        self.remote_addr
    }

    /// Returns the socket address of the local half of this TCP connection.
    pub fn local_addr(&self) -> SocketAddrV4 {
        self.local_addr
    }

    /// Shuts down the read, write, or both halves of this connection.
    pub fn shutdown(&self, how: Shutdown) -> Result<(), Error> {
        let how_val = match how {
            Shutdown::Read => socket_const::SHUT_RD,
            Shutdown::Write => socket_const::SHUT_WR,
            Shutdown::Both => socket_const::SHUT_RDWR,
        };
        let ret = crate::shutdown(self.fd, how_val);
        if ret < 0 {
            Err(Error::from_errno(-ret))
        } else {
            Ok(())
        }
    }

    /// Read data from the stream
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, Error> {
        let ret = crate::recv(self.fd, buf, 0);
        if ret < 0 {
            Err(Error::from_errno((-ret) as i32))
        } else {
            Ok(ret as usize)
        }
    }

    /// Write data to the stream
    pub fn write(&self, buf: &[u8]) -> Result<usize, Error> {
        let ret = crate::send(self.fd, buf, 0);
        if ret < 0 {
            Err(Error::from_errno((-ret) as i32))
        } else {
            Ok(ret as usize)
        }
    }

    /// Write all data to the stream
    pub fn write_all(&self, mut buf: &[u8]) -> Result<(), Error> {
        while !buf.is_empty() {
            match self.write(buf) {
                Ok(0) => return Err(Error::new(ErrorKind::WriteZero, "failed to write whole buffer")),
                Ok(n) => buf = &buf[n..],
                Err(e) if e.kind == ErrorKind::WouldBlock || e.kind == ErrorKind::TimedOut => {
                    // Kernel already blocks, so these are rare. Retry immediately.
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Read exact number of bytes
    pub fn read_exact(&self, buf: &mut [u8]) -> Result<(), Error> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.read(&mut buf[filled..]) {
                Ok(0) => return Err(Error::new(ErrorKind::UnexpectedEof, "failed to fill whole buffer")),
                Ok(n) => filled += n,
                Err(e) if e.kind == ErrorKind::WouldBlock || e.kind == ErrorKind::TimedOut => {
                    // Kernel already blocks, so these are rare. Retry immediately.
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Get the underlying file descriptor
    pub fn as_raw_fd(&self) -> i32 {
        self.fd
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        crate::close(self.fd);
    }
}

/// Possible values for shutdown
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutdown {
    Read,
    Write,
    Both,
}

// ============================================================================
// DNS Resolution
// ============================================================================

/// Resolve a hostname to IPv4 addresses
pub fn lookup_host(host: &str) -> Result<impl Iterator<Item = SocketAddrV4>, Error> {
    // Handle "host:port" format
    let (hostname, port) = if let Some(colon_pos) = host.rfind(':') {
        let h = &host[..colon_pos];
        let p = host[colon_pos + 1..].parse::<u16>().unwrap_or(0);
        (h, p)
    } else {
        (host, 0)
    };

    match crate::resolve_host(hostname) {
        Ok(ip) => Ok(core::iter::once(SocketAddrV4::new(ip, port))),
        Err(errno) => Err(Error::from_errno(errno)),
    }
}

/// Resolve a hostname to a single IPv4 address
pub fn resolve(hostname: &str) -> Result<[u8; 4], Error> {
    crate::resolve_host(hostname).map_err(Error::from_errno)
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Parse an address string like "127.0.0.1:8080" or "0.0.0.0:80"
fn parse_addr(addr: &str) -> Result<SocketAddrV4, Error> {
    SocketAddrV4::parse(addr)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "invalid address format"))
}

/// Format an IPv4 address as a string
pub fn format_ip(ip: [u8; 4]) -> String {
    use alloc::format;
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

/// Format a socket address as a string
pub fn format_addr(addr: &SocketAddrV4) -> String {
    use alloc::format;
    format!("{}.{}.{}.{}:{}", addr.ip[0], addr.ip[1], addr.ip[2], addr.ip[3], addr.port)
}
