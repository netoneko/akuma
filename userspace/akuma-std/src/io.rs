//! I/O traits and types for akuma

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// The error type for I/O operations
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
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
    pub fn new(kind: ErrorKind, _error: &str) -> Self {
        Self { kind }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn from_raw_os_error(code: i32) -> Self {
        let kind = match code {
            2 => ErrorKind::NotFound,      // ENOENT
            13 => ErrorKind::PermissionDenied, // EACCES
            _ => ErrorKind::Other,
        };
        Self { kind }
    }

    pub fn raw_os_error(&self) -> Option<i32> {
        None
    }

    pub fn last_os_error() -> Self {
        Self { kind: ErrorKind::Other }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.kind)
    }
}

/// A specialized Result type for I/O operations
pub type Result<T> = core::result::Result<T, Error>;

/// The Read trait for reading bytes
pub trait Read {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> Result<usize> {
        let mut total = 0;
        let mut tmp = [0u8; 1024];
        loop {
            match self.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    total += n;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    fn read_to_string(&mut self, buf: &mut String) -> Result<usize> {
        let mut bytes = Vec::new();
        let len = self.read_to_end(&mut bytes)?;
        match core::str::from_utf8(&bytes) {
            Ok(s) => {
                buf.push_str(s);
                Ok(len)
            }
            Err(_) => Err(Error::new(ErrorKind::InvalidData, "invalid UTF-8")),
        }
    }

    fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<()> {
        while !buf.is_empty() {
            match self.read(buf) {
                Ok(0) => return Err(Error::new(ErrorKind::UnexpectedEof, "unexpected eof")),
                Ok(n) => buf = &mut buf[n..],
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn bytes(self) -> Bytes<Self> where Self: Sized {
        Bytes { inner: self }
    }
}

/// The Write trait for writing bytes
pub trait Write {
    fn write(&mut self, buf: &[u8]) -> Result<usize>;
    fn flush(&mut self) -> Result<()>;

    fn write_all(&mut self, mut buf: &[u8]) -> Result<()> {
        while !buf.is_empty() {
            match self.write(buf) {
                Ok(0) => return Err(Error::new(ErrorKind::WriteZero, "write zero")),
                Ok(n) => buf = &buf[n..],
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn write_fmt(&mut self, fmt: fmt::Arguments<'_>) -> Result<()> {
        // Create a shim writer
        struct Adapter<'a, T: ?Sized + 'a> {
            inner: &'a mut T,
            error: Result<()>,
        }

        impl<T: Write + ?Sized> fmt::Write for Adapter<'_, T> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                match self.inner.write_all(s.as_bytes()) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        self.error = Err(e);
                        Err(fmt::Error)
                    }
                }
            }
        }

        let mut adapter = Adapter {
            inner: self,
            error: Ok(()),
        };
        match fmt::write(&mut adapter, fmt) {
            Ok(()) => Ok(()),
            Err(_) => {
                if adapter.error.is_err() {
                    adapter.error
                } else {
                    Err(Error::new(ErrorKind::Other, "formatter error"))
                }
            }
        }
    }
}

/// Iterator over bytes
pub struct Bytes<R> {
    inner: R,
}

impl<R: Read> Iterator for Bytes<R> {
    type Item = Result<u8>;

    fn next(&mut self) -> Option<Result<u8>> {
        let mut byte = 0;
        loop {
            return match self.inner.read(core::slice::from_mut(&mut byte)) {
                Ok(0) => None,
                Ok(..) => Some(Ok(byte)),
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => Some(Err(e)),
            };
        }
    }
}

/// Buffered reader wrapper
pub struct BufReader<R> {
    inner: R,
    buf: Vec<u8>,
    pos: usize,
    cap: usize,
}

impl<R: Read> BufReader<R> {
    pub fn new(inner: R) -> Self {
        Self::with_capacity(8192, inner)
    }

    pub fn with_capacity(capacity: usize, inner: R) -> Self {
        let mut buf = Vec::with_capacity(capacity);
        buf.resize(capacity, 0);
        Self { inner, buf, pos: 0, cap: 0 }
    }

    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> Read for BufReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        // If buffer is empty, fill it
        if self.pos >= self.cap {
            self.cap = self.inner.read(&mut self.buf)?;
            self.pos = 0;
        }

        // Copy from buffer
        let available = self.cap - self.pos;
        let to_copy = buf.len().min(available);
        buf[..to_copy].copy_from_slice(&self.buf[self.pos..self.pos + to_copy]);
        self.pos += to_copy;
        Ok(to_copy)
    }
}

/// BufRead trait for buffered reading
pub trait BufRead: Read {
    fn fill_buf(&mut self) -> Result<&[u8]>;
    fn consume(&mut self, amt: usize);

    fn read_line(&mut self, buf: &mut String) -> Result<usize> {
        let mut total = 0;
        loop {
            let available = match self.fill_buf() {
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            
            if available.is_empty() {
                break;
            }

            // Find newline
            let (done, used) = match available.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    if let Ok(s) = core::str::from_utf8(&available[..=i]) {
                        buf.push_str(s);
                    }
                    (true, i + 1)
                }
                None => {
                    if let Ok(s) = core::str::from_utf8(available) {
                        buf.push_str(s);
                    }
                    (false, available.len())
                }
            };
            
            self.consume(used);
            total += used;
            
            if done {
                break;
            }
        }
        Ok(total)
    }

    fn lines(self) -> Lines<Self> where Self: Sized {
        Lines { buf: self }
    }
}

/// Lines iterator
pub struct Lines<B> {
    buf: B,
}

impl<B: BufRead> Iterator for Lines<B> {
    type Item = Result<String>;

    fn next(&mut self) -> Option<Result<String>> {
        let mut buf = String::new();
        match self.buf.read_line(&mut buf) {
            Ok(0) => None,
            Ok(_) => {
                if buf.ends_with('\n') {
                    buf.pop();
                    if buf.ends_with('\r') {
                        buf.pop();
                    }
                }
                Some(Ok(buf))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

impl<R: Read> BufRead for BufReader<R> {
    fn fill_buf(&mut self) -> Result<&[u8]> {
        if self.pos >= self.cap {
            self.cap = self.inner.read(&mut self.buf)?;
            self.pos = 0;
        }
        Ok(&self.buf[self.pos..self.cap])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.cap);
    }
}

/// Standard output handle
pub struct Stdout;

impl Stdout {
    pub fn lock(&self) -> StdoutLock<'_> {
        StdoutLock { _marker: core::marker::PhantomData }
    }
}

pub struct StdoutLock<'a> {
    _marker: core::marker::PhantomData<&'a ()>,
}

impl Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let written = libakuma::write(libakuma::fd::STDOUT, buf);
        if written < 0 {
            Err(Error::from_raw_os_error((-written) as i32))
        } else {
            Ok(written as usize)
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Write for StdoutLock<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        Stdout.write(buf)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Standard error handle
pub struct Stderr;

impl Stderr {
    pub fn lock(&self) -> StderrLock<'_> {
        StderrLock { _marker: core::marker::PhantomData }
    }
}

pub struct StderrLock<'a> {
    _marker: core::marker::PhantomData<&'a ()>,
}

impl Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let written = libakuma::write(libakuma::fd::STDERR, buf);
        if written < 0 {
            Err(Error::from_raw_os_error((-written) as i32))
        } else {
            Ok(written as usize)
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Write for StderrLock<'_> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        Stderr.write(buf)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Standard input handle
pub struct Stdin;

impl Stdin {
    pub fn lock(&self) -> StdinLock<'_> {
        StdinLock { _marker: core::marker::PhantomData }
    }

    pub fn read_line(&self, buf: &mut String) -> Result<usize> {
        // Simple implementation - read byte by byte until newline
        let mut bytes_read = 0;
        let mut byte = [0u8; 1];
        loop {
            let n = libakuma::read(libakuma::fd::STDIN, &mut byte);
            if n <= 0 {
                break;
            }
            bytes_read += 1;
            if byte[0] == b'\n' {
                buf.push('\n');
                break;
            }
            if let Ok(c) = core::str::from_utf8(&byte) {
                buf.push_str(c);
            }
        }
        Ok(bytes_read)
    }
}

pub struct StdinLock<'a> {
    _marker: core::marker::PhantomData<&'a ()>,
}

impl Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = libakuma::read(libakuma::fd::STDIN, buf);
        if n < 0 {
            Err(Error::from_raw_os_error((-n) as i32))
        } else {
            Ok(n as usize)
        }
    }
}

impl Read for StdinLock<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        Stdin.read(buf)
    }
}

/// Get stdout handle
pub fn stdout() -> Stdout {
    Stdout
}

/// Get stderr handle
pub fn stderr() -> Stderr {
    Stderr
}

/// Get stdin handle
pub fn stdin() -> Stdin {
    Stdin
}

/// Copy from reader to writer
pub fn copy<R: Read, W: Write>(reader: &mut R, writer: &mut W) -> Result<u64> {
    let mut buf = [0u8; 8192];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        total += n as u64;
    }
    Ok(total)
}

/// Cursor for in-memory I/O
pub struct Cursor<T> {
    inner: T,
    pos: u64,
}

impl<T> Cursor<T> {
    pub fn new(inner: T) -> Self {
        Self { inner, pos: 0 }
    }

    pub fn into_inner(self) -> T {
        self.inner
    }

    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn set_position(&mut self, pos: u64) {
        self.pos = pos;
    }
}

impl<T: AsRef<[u8]>> Read for Cursor<T> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let data = self.inner.as_ref();
        let pos = self.pos as usize;
        if pos >= data.len() {
            return Ok(0);
        }
        let available = &data[pos..];
        let to_read = buf.len().min(available.len());
        buf[..to_read].copy_from_slice(&available[..to_read]);
        self.pos += to_read as u64;
        Ok(to_read)
    }
}

impl Write for Cursor<Vec<u8>> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let pos = self.pos as usize;
        let len = self.inner.len();
        
        // Extend if necessary
        if pos + buf.len() > len {
            self.inner.resize(pos + buf.len(), 0);
        }
        
        self.inner[pos..pos + buf.len()].copy_from_slice(buf);
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Write for Cursor<&mut Vec<u8>> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let pos = self.pos as usize;
        let len = self.inner.len();
        
        if pos + buf.len() > len {
            self.inner.resize(pos + buf.len(), 0);
        }
        
        self.inner[pos..pos + buf.len()].copy_from_slice(buf);
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
