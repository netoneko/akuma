//! Git Smart HTTP Protocol
//!
//! Implements the Git smart HTTP protocol for clone and fetch.
//!
//! Protocol flow:
//! 1. GET /info/refs?service=git-upload-pack - Discover refs
//! 2. POST /git-upload-pack - Request pack with wanted objects

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::print;

use crate::error::{Error, Result};
use crate::pack_stream::StreamingPackParser;
use crate::stream::process_pack_streaming;

fn print_status(status: u16) {
    let mut buf = [0u8; 5];
    let mut n = status;
    let mut i = 4;
    loop {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 || i == 0 {
            break;
        }
        i -= 1;
    }
    if let Ok(s) = core::str::from_utf8(&buf[i..]) {
        print(s);
    }
}
use crate::http::{HttpClient, Url};
use crate::pktline;
use crate::sha1::{self, Sha1Hash};

/// A remote reference
#[derive(Debug, Clone)]
pub struct RemoteRef {
    pub sha: Sha1Hash,
    pub name: String,
}

/// Server capabilities
#[derive(Debug, Default)]
pub struct Capabilities {
    pub multi_ack: bool,
    pub thin_pack: bool,
    pub side_band: bool,
    pub side_band_64k: bool,
    pub ofs_delta: bool,
    pub shallow: bool,
    pub no_progress: bool,
    pub include_tag: bool,
}

impl Capabilities {
    fn parse(caps_str: &str) -> Self {
        let mut caps = Capabilities::default();
        
        for cap in caps_str.split(' ') {
            match cap {
                "multi_ack" => caps.multi_ack = true,
                "thin-pack" => caps.thin_pack = true,
                "side-band" => caps.side_band = true,
                "side-band-64k" => caps.side_band_64k = true,
                "ofs-delta" => caps.ofs_delta = true,
                "shallow" => caps.shallow = true,
                "no-progress" => caps.no_progress = true,
                "include-tag" => caps.include_tag = true,
                _ => {}
            }
        }
        
        caps
    }
}

/// Git protocol client
pub struct ProtocolClient {
    client: HttpClient,
    url: Url,
}

impl ProtocolClient {
    pub fn new(url: Url) -> Self {
        Self {
            client: HttpClient::new(url.clone()),
            url,
        }
    }

    /// Discover refs from the remote
    pub fn discover_refs(&mut self) -> Result<(Vec<RemoteRef>, Capabilities)> {
        let path = self.url.info_refs_url();
        
        print("scratch: fetching refs from ");
        print(&path);
        print("\n");

        let response = self.client.get(&path)?;

        print("scratch: got status ");
        print_status(response.status);
        print("\n");

        if response.status != 200 {
            // Print response body for debugging
            if let Ok(body_str) = core::str::from_utf8(&response.body) {
                let preview: &str = if body_str.len() > 200 { &body_str[..200] } else { body_str };
                print("scratch: response: ");
                print(preview);
                print("\n");
            }
            return Err(Error::http(&format!("status {}", response.status)));
        }

        // Check content type
        let content_type = response.header("Content-Type")
            .unwrap_or("");
        print("scratch: content-type: ");
        print(content_type);
        print("\n");
        
        if !content_type.contains("x-git-upload-pack-advertisement") {
            // Print response body for debugging
            if let Ok(body_str) = core::str::from_utf8(&response.body) {
                let preview: &str = if body_str.len() > 200 { &body_str[..200] } else { body_str };
                print("scratch: unexpected response: ");
                print(preview);
                print("\n");
            }
            return Err(Error::protocol("not a smart Git server"));
        }

        // Debug: show first bytes of body
        print("scratch: body starts with: ");
        let preview_len = core::cmp::min(40, response.body.len());
        if let Ok(s) = core::str::from_utf8(&response.body[..preview_len]) {
            print(s);
        } else {
            print("<binary>");
        }
        print("\n");

        parse_ref_discovery(&response.body)
    }

    /// Fetch a pack file with the given wants
    ///
    /// Returns the raw pack data
    pub fn fetch_pack(&mut self, wants: &[Sha1Hash], haves: &[Sha1Hash], caps: &Capabilities) -> Result<Vec<u8>> {
        let path = self.url.upload_pack_url();
        
        print("scratch: requesting pack from ");
        print(&path);
        print("\n");

        // Build the request body
        let body = build_upload_pack_request(wants, haves, caps);

        let response = self.client.post(
            &path,
            "application/x-git-upload-pack-request",
            &body,
        )?;

        if response.status != 200 {
            return Err(Error::http(&format!("status {}", response.status)));
        }

        // Parse the response
        // It's either:
        // 1. Side-band multiplexed data (if we requested side-band)
        // 2. NAK + pack data directly
        parse_upload_pack_response(&response.body, caps)
    }

    /// Fetch pack with streaming - downloads to temp file, then parses
    /// Returns the number of objects parsed
    pub fn fetch_pack_streaming(
        &mut self,
        wants: &[Sha1Hash],
        haves: &[Sha1Hash],
        caps: &Capabilities,
        git_dir: &str,
    ) -> Result<u32> {
        let path = self.url.upload_pack_url();
        
        print("scratch: requesting pack from ");
        print(&path);
        print("\n");

        // Build the request body
        let request_body = build_upload_pack_request(wants, haves, caps);

        // Get resolved IP from client
        let resolved_ip = self.client.get_ip()?;

        // State for sideband demultiplexing
        let use_sideband = caps.side_band || caps.side_band_64k;
        let mut sideband_state = SidebandState::new(use_sideband);
        
        // Temporary file path for pack
        let pack_path = format!("{}/objects/pack/tmp.pack", git_dir);
        
        // Open temp file for writing
        let pack_fd = libakuma::open(
            &pack_path,
            libakuma::open_flags::O_WRONLY | libakuma::open_flags::O_CREAT | libakuma::open_flags::O_TRUNC
        );
        if pack_fd < 0 {
            return Err(Error::io("failed to create temp pack file"));
        }
        
        let mut pack_size = 0usize;

        // Download pack to file
        let download_result = process_pack_streaming(
            &self.url,
            resolved_ip,
            &path,
            "application/x-git-upload-pack-request",
            &request_body,
            |chunk| {
                // Process through sideband demuxer
                let pack_data = sideband_state.process(chunk)?;
                
                if !pack_data.is_empty() {
                    // Write to temp file instead of parsing
                    let written = libakuma::write_fd(pack_fd, &pack_data);
                    if written < 0 {
                        return Err(Error::io("failed to write pack data"));
                    }
                    pack_size += pack_data.len();
                }
                
                Ok(true) // Continue
            },
        );
        
        libakuma::close(pack_fd);
        
        download_result?;
        
        print("scratch: downloaded ");
        print_num(pack_size);
        print(" bytes\n");
        
        // Now parse the pack file from disk using the original parser
        print("scratch: parsing pack file...\n");
        
        let count = parse_pack_from_file(&pack_path, git_dir)?;
        
        print("scratch: stored ");
        print_num(count as usize);
        print(" objects\n");
        
        // Clean up temp file (best effort)
        // Note: libakuma doesn't have unlink, so file stays around
        
        Ok(count)
    }
}

/// Parse pack file from disk using small batches
fn parse_pack_from_file(pack_path: &str, git_dir: &str) -> Result<u32> {
    use crate::pack::PackParser;
    use crate::store::ObjectStore;
    
    // Read pack file in chunks
    let fd = libakuma::open(pack_path, libakuma::open_flags::O_RDONLY);
    if fd < 0 {
        return Err(Error::io("failed to open pack file"));
    }
    
    // Read entire pack (we've already saved it to disk, memory is freed)
    let mut pack_data = Vec::new();
    let mut buf = [0u8; 4096];
    
    loop {
        let n = libakuma::read_fd(fd, &mut buf);
        if n <= 0 {
            break;
        }
        pack_data.extend_from_slice(&buf[..n as usize]);
    }
    libakuma::close(fd);
    
    // Now parse with the original parser
    let store = ObjectStore::new(git_dir);
    let mut parser = PackParser::new(&pack_data)?;
    
    print("scratch: pack contains ");
    print_num(parser.object_count() as usize);
    print(" objects\n");
    
    let shas = parser.parse_all(&store)?;
    
    Ok(shas.len() as u32)
}

/// State machine for sideband demultiplexing during streaming
struct SidebandState {
    enabled: bool,
    buffer: Vec<u8>,
    in_pack_data: bool,
}

impl SidebandState {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            buffer: Vec::new(),
            in_pack_data: false,
        }
    }

    /// Process incoming data and extract pack data
    fn process(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if !self.enabled {
            // Skip NAK line if present at start
            if !self.in_pack_data {
                if data.starts_with(b"NAK") || data.starts_with(b"0008NAK") {
                    // Skip NAK packet
                    self.in_pack_data = true;
                    // Find pack data start
                    if let Some(pos) = find_pack_start(data) {
                        return Ok(data[pos..].to_vec());
                    }
                    return Ok(Vec::new());
                }
                if data.starts_with(b"PACK") {
                    self.in_pack_data = true;
                    return Ok(data.to_vec());
                }
            }
            return Ok(data.to_vec());
        }

        // Sideband enabled - need to demux
        self.buffer.extend_from_slice(data);
        
        // Debug: show sideband buffer growth
        if self.buffer.len() > 50000 && !self.in_pack_data {
            print("scratch: sideband buffer ");
            print_num(self.buffer.len());
            print(" bytes, first bytes: ");
            let preview_len = core::cmp::min(20, self.buffer.len());
            if let Ok(s) = core::str::from_utf8(&self.buffer[..preview_len]) {
                print(s);
            } else {
                print("<binary>");
            }
            print("\n");
        }
        let mut pack_data = Vec::new();

        while self.buffer.len() >= 4 {
            // Parse pkt-line length
            let len_hex = &self.buffer[..4];
            let len_str = core::str::from_utf8(len_hex).unwrap_or("0000");
            
            let len = match u16::from_str_radix(len_str, 16) {
                Ok(l) => l as usize,
                Err(_) => {
                    // Not a valid pkt-line, might be raw pack data
                    if self.buffer.starts_with(b"PACK") {
                        pack_data.extend_from_slice(&self.buffer);
                        self.buffer.clear();
                    }
                    break;
                }
            };

            if len == 0 {
                // Flush packet
                self.buffer = self.buffer[4..].to_vec();
                continue;
            }

            if len < 4 || self.buffer.len() < len {
                // Need more data
                break;
            }

            // Get packet content
            let content = &self.buffer[4..len];
            
            if !content.is_empty() {
                match content[0] {
                    1 => {
                        // Pack data
                        pack_data.extend_from_slice(&content[1..]);
                    }
                    2 => {
                        // Progress - print it
                        if let Ok(msg) = core::str::from_utf8(&content[1..]) {
                            print("remote: ");
                            print(msg.trim());
                            print("\n");
                        }
                    }
                    3 => {
                        // Error
                        if let Ok(msg) = core::str::from_utf8(&content[1..]) {
                            return Err(Error::protocol(msg.trim()));
                        }
                    }
                    _ => {
                        // Unknown, might be NAK/ACK
                        let line = core::str::from_utf8(content).unwrap_or("");
                        if !line.starts_with("NAK") && !line.starts_with("ACK") {
                            // Unknown content
                        }
                    }
                }
            }

            self.buffer = self.buffer[len..].to_vec();
        }

        Ok(pack_data)
    }
}

fn find_pack_start(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(4) {
        if &data[i..i+4] == b"PACK" {
            return Some(i);
        }
    }
    None
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
        if let Ok(s) = core::str::from_utf8(&buf[i..i+1]) {
            print(s);
        }
    }
}

/// Parse ref discovery response
fn parse_ref_discovery(data: &[u8]) -> Result<(Vec<RemoteRef>, Capabilities)> {
    let mut pos = 0;
    let mut refs = Vec::new();
    let mut capabilities = Capabilities::default();
    let mut first_ref = true;

    // First line should be "# service=git-upload-pack\n"
    let (first_line, consumed) = pktline::read_pkt_line(data)?;
    pos += consumed;
    
    if let Some(line) = first_line {
        let line_str = core::str::from_utf8(line)
            .map_err(|_| Error::protocol("invalid service line"))?;
        if !line_str.contains("git-upload-pack") {
            return Err(Error::protocol("unexpected service"));
        }
    }

    // Skip flush packet after service line
    let (_, consumed) = pktline::read_pkt_line(&data[pos..])?;
    pos += consumed;

    // Parse refs
    while pos < data.len() {
        let (content, consumed) = pktline::read_pkt_line(&data[pos..])?;
        pos += consumed;

        let line = match content {
            None => break, // Flush packet
            Some(l) => l,
        };

        // Parse ref line
        if let Some((sha_hex, ref_name, caps_opt)) = pktline::parse_ref_line(line) {
            // Parse SHA
            let sha = sha1::from_hex(&sha_hex)
                .ok_or_else(|| Error::protocol("invalid ref SHA"))?;

            refs.push(RemoteRef {
                sha,
                name: ref_name,
            });

            // First ref has capabilities
            if first_ref {
                if let Some(caps_str) = caps_opt {
                    capabilities = Capabilities::parse(&caps_str);
                }
                first_ref = false;
            }
        }
    }

    Ok((refs, capabilities))
}

/// Build upload-pack request body
fn build_upload_pack_request(wants: &[Sha1Hash], haves: &[Sha1Hash], caps: &Capabilities) -> Vec<u8> {
    let mut body = Vec::new();

    // Build capabilities string for first want line
    let mut caps_str = String::from("multi_ack");
    if caps.side_band_64k {
        caps_str.push_str(" side-band-64k");
    } else if caps.side_band {
        caps_str.push_str(" side-band");
    }
    if caps.ofs_delta {
        caps_str.push_str(" ofs-delta");
    }
    caps_str.push_str(" agent=scratch/1.0");

    // Want lines
    for (i, want) in wants.iter().enumerate() {
        let line = if i == 0 {
            format!("want {} {}\n", sha1::to_hex(want), caps_str)
        } else {
            format!("want {}\n", sha1::to_hex(want))
        };
        body.extend_from_slice(&pktline::write_pkt_line(line.as_bytes()));
    }

    // Flush after wants
    body.extend_from_slice(&pktline::write_flush());

    // Have lines (for incremental fetch)
    for have in haves {
        let line = format!("have {}\n", sha1::to_hex(have));
        body.extend_from_slice(&pktline::write_pkt_line(line.as_bytes()));
    }

    // Done
    body.extend_from_slice(&pktline::write_pkt_line(b"done\n"));

    body
}

/// Parse upload-pack response
fn parse_upload_pack_response(data: &[u8], caps: &Capabilities) -> Result<Vec<u8>> {
    let mut pos = 0;

    // First, read any NAK/ACK lines
    while pos < data.len() {
        let (content, consumed) = pktline::read_pkt_line(&data[pos..])?;
        pos += consumed;

        match content {
            None => {
                // Flush packet - pack data follows
                break;
            }
            Some(line) => {
                let line_str = pktline::line_to_str(line).unwrap_or("");
                if line_str == "NAK" {
                    // Continue to read pack
                    continue;
                }
                if line_str.starts_with("ACK") {
                    // Continue with multi_ack
                    continue;
                }
                // Might be start of pack data
                if line.starts_with(b"PACK") || (line.len() > 0 && line[0] <= 3) {
                    // This is already pack data (or sideband)
                    pos -= consumed; // Rewind
                    break;
                }
            }
        }
    }

    let remaining = &data[pos..];

    // Check if it's sideband multiplexed
    if caps.side_band || caps.side_band_64k {
        let (pack_data, messages) = pktline::demux_sideband(remaining)?;
        
        // Print progress messages
        for msg in messages {
            print("remote: ");
            print(&msg);
            print("\n");
        }
        
        Ok(pack_data)
    } else {
        // Raw pack data
        Ok(remaining.to_vec())
    }
}
