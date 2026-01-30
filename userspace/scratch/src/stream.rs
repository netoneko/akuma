//! Streaming HTTP and pack file handling
//!
//! Provides streaming reads to avoid loading entire responses into memory.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::vec;

use libakuma::net::{resolve, TcpStream};
use libakuma::print;
use libakuma_tls::transport::TcpTransport;
use libakuma_tls::{TlsStream, TLS_RECORD_SIZE};

use crate::error::{Error, Result};
use crate::http::Url;
use crate::sha1::{Sha1Hash, to_hex};
use crate::store::ObjectStore;
use crate::zlib;

/// Buffer size for streaming reads
const STREAM_BUFFER_SIZE: usize = 8192;

/// Streaming HTTP connection that can read body in chunks
pub struct StreamingConnection {
    // TLS stream and its owned buffers
    tls: Option<TlsStreamOwned>,
    // Parsed response info
    pub status: u16,
    pub headers: Vec<(String, String)>,
    // Chunked encoding state
    is_chunked: bool,
    current_chunk_remaining: usize,
    chunk_ended: bool,
    // Internal read buffer for leftover data after headers
    leftover: Vec<u8>,
    leftover_pos: usize,
    // Stats
    bytes_read: usize,
}

/// Owned TLS stream with its buffers
struct TlsStreamOwned {
    // We need to store the buffers alongside the stream
    // since TlsStream borrows them
    transport: TcpTransport,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
}

impl StreamingConnection {
    /// Create a new streaming HTTPS connection and send a POST request
    pub fn post_streaming(
        url: &Url,
        resolved_ip: [u8; 4],
        path: &str,
        content_type: &str,
        body: &[u8],
    ) -> Result<Self> {
        let addr = format!("{}.{}.{}.{}:{}", 
            resolved_ip[0], resolved_ip[1], resolved_ip[2], resolved_ip[3], url.port);

        // Connect
        let stream = TcpStream::connect(&addr)
            .map_err(|_| Error::network("connection failed"))?;

        let transport = TcpTransport::new(stream);
        let mut read_buf = vec![0u8; TLS_RECORD_SIZE];
        let mut write_buf = vec![0u8; TLS_RECORD_SIZE];

        // TLS handshake - we need to work around the borrow checker here
        // by using unsafe to extend the lifetime temporarily
        let mut tls = TlsStream::connect(
            transport,
            &url.host,
            &mut read_buf,
            &mut write_buf,
        ).map_err(|_| Error::network("TLS handshake failed"))?;

        // Build and send request
        let request = format!(
            "POST {} HTTP/1.1\r\n\
             Host: {}\r\n\
             User-Agent: scratch/1.0\r\n\
             Content-Type: {}\r\n\
             Content-Length: {}\r\n\
             Accept: application/x-git-upload-pack-result\r\n\
             Connection: close\r\n\
             \r\n",
            path, url.host, content_type, body.len()
        );

        tls.write_all(request.as_bytes())
            .map_err(|_| Error::network("failed to send request"))?;
        
        if !body.is_empty() {
            tls.write_all(body)
                .map_err(|_| Error::network("failed to send body"))?;
        }
        
        tls.flush().map_err(|_| Error::network("failed to flush"))?;

        // Read headers
        let mut header_buf = Vec::new();
        let mut temp = [0u8; 1024];
        
        loop {
            match tls.read(&mut temp) {
                Ok(0) => return Err(Error::http("connection closed before headers")),
                Ok(n) => {
                    header_buf.extend_from_slice(&temp[..n]);
                    // Check if we have complete headers
                    if let Some(end) = find_header_end(&header_buf) {
                        // Parse headers
                        let (status, headers, is_chunked) = parse_headers(&header_buf[..end])?;
                        
                        // Save leftover data (body start)
                        let leftover = header_buf[end..].to_vec();
                        
                        // Close TLS properly since we can't store it with borrows
                        let _ = tls.close();
                        
                        return Ok(Self {
                            tls: None, // Can't store TLS stream due to lifetime issues
                            status,
                            headers,
                            is_chunked,
                            current_chunk_remaining: 0,
                            chunk_ended: false,
                            leftover,
                            leftover_pos: 0,
                            bytes_read: 0,
                        });
                    }
                }
                Err(_) => return Err(Error::http("failed to read headers")),
            }
        }
    }

    /// Read the next chunk of body data
    /// Returns Ok(0) when done
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        // First, drain any leftover data
        if self.leftover_pos < self.leftover.len() {
            let available = self.leftover.len() - self.leftover_pos;
            let to_copy = core::cmp::min(available, buf.len());
            buf[..to_copy].copy_from_slice(&self.leftover[self.leftover_pos..self.leftover_pos + to_copy]);
            self.leftover_pos += to_copy;
            self.bytes_read += to_copy;
            return Ok(to_copy);
        }
        
        // TODO: Read from TLS stream
        // For now, we can only use the leftover data
        // The full streaming implementation requires more complex lifetime handling
        Ok(0)
    }

    /// Get a header value
    pub fn header(&self, name: &str) -> Option<&str> {
        let name_lower = name.to_ascii_lowercase();
        for (k, v) in &self.headers {
            if k.to_ascii_lowercase() == name_lower {
                return Some(v);
            }
        }
        None
    }
    
    /// Get total bytes read so far
    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }
    
    /// Consume all remaining data into a buffer (fallback for small responses)
    pub fn read_all_remaining(&mut self) -> Result<Vec<u8>> {
        let mut result = Vec::new();
        
        // Drain leftover
        if self.leftover_pos < self.leftover.len() {
            result.extend_from_slice(&self.leftover[self.leftover_pos..]);
            self.leftover_pos = self.leftover.len();
        }
        
        // Decode chunked if needed
        if self.is_chunked {
            decode_chunked_inplace(&mut result)?;
        }
        
        Ok(result)
    }
}

/// Alternative approach: Stream pack directly during download
/// This creates a connection, sends request, and provides streaming access
pub struct PackStreamConnection {
    transport: Option<TcpTransport>,
    status: u16,
    is_chunked: bool,
    // Buffer for reading
    buffer: Vec<u8>,
    buffer_pos: usize,
    buffer_len: usize,
    // Chunked state
    chunk_remaining: usize,
    done: bool,
    // Stats  
    total_read: usize,
}

impl PackStreamConnection {
    /// Connect and send a POST request, returning streaming access to body
    pub fn connect(
        url: &Url,
        resolved_ip: [u8; 4],
        path: &str,
        content_type: &str,
        request_body: &[u8],
    ) -> Result<Self> {
        let addr = format!("{}.{}.{}.{}:{}", 
            resolved_ip[0], resolved_ip[1], resolved_ip[2], resolved_ip[3], url.port);

        let stream = TcpStream::connect(&addr)
            .map_err(|_| Error::network("connection failed"))?;

        // For HTTPS, we need a different approach - use blocking full read
        // but process in chunks. See process_pack_streaming below.
        
        if url.https {
            return Err(Error::other("use process_pack_streaming for HTTPS"));
        }

        let transport = TcpTransport::new(stream);
        
        Ok(Self {
            transport: Some(transport),
            status: 0,
            is_chunked: false,
            buffer: vec![0u8; STREAM_BUFFER_SIZE],
            buffer_pos: 0,
            buffer_len: 0,
            chunk_remaining: 0,
            done: false,
            total_read: 0,
        })
    }
}

/// Process a pack file in streaming fashion over HTTPS
/// This is the main entry point for streaming clone/fetch
pub fn process_pack_streaming<F>(
    url: &Url,
    resolved_ip: [u8; 4],
    path: &str,
    content_type: &str,
    request_body: &[u8],
    mut processor: F,
) -> Result<()>
where
    F: FnMut(&[u8]) -> Result<bool>, // Returns true to continue, false to stop
{
    let addr = format!("{}.{}.{}.{}:{}", 
        resolved_ip[0], resolved_ip[1], resolved_ip[2], resolved_ip[3], url.port);

    let stream = TcpStream::connect(&addr)
        .map_err(|_| Error::network("connection failed"))?;

    if !url.https {
        return Err(Error::other("HTTP not implemented for streaming"));
    }

    let transport = TcpTransport::new(stream);
    let mut read_buf = vec![0u8; TLS_RECORD_SIZE];
    let mut write_buf = vec![0u8; TLS_RECORD_SIZE];

    let mut tls = TlsStream::connect(
        transport,
        &url.host,
        &mut read_buf,
        &mut write_buf,
    ).map_err(|_| Error::network("TLS handshake failed"))?;

    // Send request
    let request = format!(
        "POST {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: scratch/1.0\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Accept: application/x-git-upload-pack-result\r\n\
         Connection: close\r\n\
         \r\n",
        path, url.host, content_type, request_body.len()
    );

    tls.write_all(request.as_bytes())
        .map_err(|_| Error::network("failed to send request"))?;
    
    if !request_body.is_empty() {
        tls.write_all(request_body)
            .map_err(|_| Error::network("failed to send body"))?;
    }
    
    tls.flush().map_err(|_| Error::network("failed to flush"))?;

    // Read and process response in chunks
    let mut header_buf = Vec::new();
    let mut temp = [0u8; 4096];
    let mut headers_parsed = false;
    let mut is_chunked = false;
    let mut chunk_state = ChunkedState::new();
    let mut total_bytes = 0usize;
    let mut last_progress = 0usize;
    
    loop {
        match tls.read(&mut temp) {
            Ok(0) => break,
            Ok(n) => {
                if !headers_parsed {
                    header_buf.extend_from_slice(&temp[..n]);
                    if let Some(end) = find_header_end(&header_buf) {
                        let (status, headers, chunked) = parse_headers(&header_buf[..end])?;
                        
                        if status != 200 {
                            return Err(Error::http(&format!("status {}", status)));
                        }
                        
                        is_chunked = chunked;
                        headers_parsed = true;
                        
                        // Process any body data that came with headers
                        let body_start = &header_buf[end..];
                        if !body_start.is_empty() {
                            if is_chunked {
                                let decoded = chunk_state.process(body_start)?;
                                if !decoded.is_empty() {
                                    total_bytes += decoded.len();
                                    if !processor(&decoded)? {
                                        break;
                                    }
                                }
                            } else {
                                total_bytes += body_start.len();
                                if !processor(body_start)? {
                                    break;
                                }
                            }
                        }
                    }
                } else {
                    // Body data
                    if is_chunked {
                        let decoded = chunk_state.process(&temp[..n])?;
                        if !decoded.is_empty() {
                            total_bytes += decoded.len();
                            if !processor(&decoded)? {
                                break;
                            }
                        }
                        if chunk_state.done {
                            print("\nscratch: chunked transfer complete\n");
                            break;
                        }
                    } else {
                        total_bytes += n;
                        if !processor(&temp[..n])? {
                            break;
                        }
                    }
                }
                
                // Progress reporting
                if total_bytes - last_progress >= 65536 {
                    print("scratch: received ");
                    print_size(total_bytes);
                    print("\r");
                    last_progress = total_bytes;
                }
            }
            Err(_) => {
                print("\nscratch: TLS read error, stopping\n");
                break;
            }
        }
    }
    
    print("scratch: download finished, total ");
    print_size(total_bytes);
    print("\n");
    
    let _ = tls.close();
    Ok(())
}

/// State machine for chunked transfer decoding
struct ChunkedState {
    buffer: Vec<u8>,
    chunk_remaining: usize,
    state: ChunkParseState,
    done: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum ChunkParseState {
    ExpectingSize,
    ReadingData,
    ExpectingCrlf,
}

impl ChunkedState {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            chunk_remaining: 0,
            state: ChunkParseState::ExpectingSize,
            done: false,
        }
    }
    
    /// Process incoming data and return decoded chunk data
    fn process(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        self.buffer.extend_from_slice(data);
        let mut result = Vec::new();
        
        while !self.buffer.is_empty() && !self.done {
            match self.state {
                ChunkParseState::ExpectingSize => {
                    // Looking for chunk size line
                    if let Some(crlf_pos) = find_crlf_slice(&self.buffer) {
                        let size_line = &self.buffer[..crlf_pos];
                        let size_str = core::str::from_utf8(size_line)
                            .map_err(|_| Error::http("invalid chunk size"))?;
                        let size_part = size_str.split(';').next().unwrap_or(size_str).trim();
                        
                        self.chunk_remaining = match usize::from_str_radix(size_part, 16) {
                            Ok(s) => s,
                            Err(_) => {
                                // Debug: show what we got
                                print("scratch: bad chunk size '");
                                print(size_part);
                                print("' buffer starts: ");
                                let preview = core::cmp::min(30, self.buffer.len());
                                if let Ok(p) = core::str::from_utf8(&self.buffer[..preview]) {
                                    print(p);
                                }
                                print("\n");
                                return Err(Error::http("invalid chunk size hex"));
                            }
                        };
                        
                        // Remove size line from buffer
                        self.buffer = self.buffer[crlf_pos + 2..].to_vec();
                        
                        if self.chunk_remaining == 0 {
                            self.done = true;
                            break;
                        }
                        
                        self.state = ChunkParseState::ReadingData;
                    } else {
                        // Need more data for size line
                        break;
                    }
                }
                ChunkParseState::ReadingData => {
                    // Reading chunk data
                    let available = core::cmp::min(self.chunk_remaining, self.buffer.len());
                    result.extend_from_slice(&self.buffer[..available]);
                    self.buffer = self.buffer[available..].to_vec();
                    self.chunk_remaining -= available;
                    
                    if self.chunk_remaining == 0 {
                        self.state = ChunkParseState::ExpectingCrlf;
                    } else {
                        // Need more data for current chunk
                        break;
                    }
                }
                ChunkParseState::ExpectingCrlf => {
                    // Need to consume trailing CRLF
                    if self.buffer.len() >= 2 {
                        if &self.buffer[..2] == b"\r\n" {
                            self.buffer = self.buffer[2..].to_vec();
                        }
                        // Even if it's not CRLF, move on (corrupted but try to continue)
                        self.state = ChunkParseState::ExpectingSize;
                    } else {
                        // Need more data for trailing CRLF
                        break;
                    }
                }
            }
        }
        
        Ok(result)
    }
}

fn find_crlf_slice(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(1) {
        if data[i] == b'\r' && data[i + 1] == b'\n' {
            return Some(i);
        }
    }
    None
}

fn find_header_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

fn parse_headers(data: &[u8]) -> Result<(u16, Vec<(String, String)>, bool)> {
    let header_str = core::str::from_utf8(data)
        .map_err(|_| Error::http("invalid headers"))?;
    
    let mut lines = header_str.lines();
    
    // Parse status line
    let status_line = lines.next()
        .ok_or_else(|| Error::http("missing status line"))?;
    
    let status = parse_status_line(status_line)?;
    
    let mut headers = Vec::new();
    let mut is_chunked = false;
    
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some(colon_pos) = line.find(':') {
            let name = line[..colon_pos].trim();
            let value = line[colon_pos + 1..].trim();
            
            if name.eq_ignore_ascii_case("Transfer-Encoding") && value.contains("chunked") {
                is_chunked = true;
            }
            
            headers.push((String::from(name), String::from(value)));
        }
    }
    
    Ok((status, headers, is_chunked))
}

fn parse_status_line(line: &str) -> Result<u16> {
    let mut parts = line.split_whitespace();
    let _version = parts.next()
        .ok_or_else(|| Error::http("missing HTTP version"))?;
    let status_str = parts.next()
        .ok_or_else(|| Error::http("missing status code"))?;
    
    status_str.parse::<u16>()
        .map_err(|_| Error::http("invalid status code"))
}

fn decode_chunked_inplace(data: &mut Vec<u8>) -> Result<()> {
    let original = core::mem::take(data);
    let mut pos = 0;
    
    while pos < original.len() {
        if let Some(crlf) = find_crlf_slice(&original[pos..]) {
            let size_str = core::str::from_utf8(&original[pos..pos + crlf])
                .map_err(|_| Error::http("invalid chunk size"))?;
            let size = usize::from_str_radix(size_str.trim(), 16)
                .map_err(|_| Error::http("invalid chunk hex"))?;
            
            pos += crlf + 2;
            
            if size == 0 {
                break;
            }
            
            let end = core::cmp::min(pos + size, original.len());
            data.extend_from_slice(&original[pos..end]);
            pos = end + 2; // Skip trailing CRLF
        } else {
            break;
        }
    }
    
    Ok(())
}

fn print_size(bytes: usize) {
    if bytes >= 1024 * 1024 {
        let mb = bytes / (1024 * 1024);
        print_num(mb);
        print(" MB");
    } else if bytes >= 1024 {
        let kb = bytes / 1024;
        print_num(kb);
        print(" KB");
    } else {
        print_num(bytes);
        print(" bytes");
    }
}

fn print_num(n: usize) {
    if n == 0 {
        print("0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    let mut val = n;
    while val > 0 {
        buf[i] = b'0' + (val % 10) as u8;
        val /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        let s = core::str::from_utf8(&buf[i..i+1]).unwrap();
        print(s);
    }
}
