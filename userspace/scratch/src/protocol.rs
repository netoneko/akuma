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

        // Update URL if redirected
        let client_url = self.client.url();
        // Check if host, scheme or path changed
        if client_url.host != self.url.host || client_url.https != self.url.https || client_url.path != path {
             // Try to extract base path from the info/refs URL
             if let Some(idx) = client_url.path.find("/info/refs") {
                 let new_base_path = &client_url.path[..idx];
                 let mut new_url = client_url.clone();
                 new_url.path = String::from(new_base_path);
                 
                 print("scratch: updating repo URL to ");
                 if new_url.https { print("https://"); } else { print("http://"); }
                 print(&new_url.host);
                 print(&new_url.path);
                 print("\n");
                 
                 self.url = new_url;
             }
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
                    self.in_pack_data = true;
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

        // Sideband enabled - need to demux pkt-line framed data
        self.buffer.extend_from_slice(data);
        let mut pack_data = Vec::new();

        // If we already switched to raw mode, just drain the buffer
        if self.in_pack_data {
            pack_data.extend_from_slice(&self.buffer);
            self.buffer.clear();
            return Ok(pack_data);
        }

        while self.buffer.len() >= 4 {
            // Try to parse pkt-line length (first 4 bytes must be ASCII hex)
            let len_hex = &self.buffer[..4];

            // All 4 bytes must be ASCII hex digits
            let valid_hex = len_hex.iter().all(|b| matches!(b,
                b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'));

            if !valid_hex {
                // Not a pkt-line. Scan for PACK magic to resynchronize.
                if let Some(pos) = find_pack_start(&self.buffer) {
                    // Found raw PACK data — switch to raw mode
                    pack_data.extend_from_slice(&self.buffer[pos..]);
                    self.buffer.clear();
                    self.in_pack_data = true;
                } else {
                    // No PACK magic yet. Drop bytes up to the last 3
                    // (PACK might be split across calls).
                    let keep = core::cmp::min(3, self.buffer.len());
                    let drain = self.buffer.len() - keep;
                    self.buffer = self.buffer[drain..].to_vec();
                }
                break;
            }

            let len_str = core::str::from_utf8(len_hex).unwrap_or("0000");
            let len = u16::from_str_radix(len_str, 16).unwrap_or(0) as usize;

            if len == 0 {
                // Flush packet — may signal end of sideband section
                self.buffer = self.buffer[4..].to_vec();
                continue;
            }

            if len < 4 || self.buffer.len() < len {
                // Need more data for this pkt-line
                break;
            }

            // Get packet content (after 4-byte length header)
            let content = &self.buffer[4..len];

            if !content.is_empty() {
                match content[0] {
                    1 => {
                        // Channel 1: pack data
                        pack_data.extend_from_slice(&content[1..]);
                    }
                    2 => {
                        // Channel 2: progress
                        if let Ok(msg) = core::str::from_utf8(&content[1..]) {
                            print("remote: ");
                            print(msg.trim());
                            print("\n");
                        }
                    }
                    3 => {
                        // Channel 3: error
                        if let Ok(msg) = core::str::from_utf8(&content[1..]) {
                            return Err(Error::protocol(msg.trim()));
                        }
                    }
                    _ => {
                        // NAK/ACK or other protocol lines — ignore
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

// ============================================================================
// Push Protocol (git-receive-pack)
// ============================================================================

/// Capabilities for receive-pack
#[derive(Debug, Default)]
pub struct ReceiveCapabilities {
    pub report_status: bool,
    pub delete_refs: bool,
    pub side_band_64k: bool,
    pub ofs_delta: bool,
}

impl ReceiveCapabilities {
    fn parse(caps_str: &str) -> Self {
        let mut caps = ReceiveCapabilities::default();
        
        for cap in caps_str.split(' ') {
            match cap {
                "report-status" => caps.report_status = true,
                "delete-refs" => caps.delete_refs = true,
                "side-band-64k" => caps.side_band_64k = true,
                "ofs-delta" => caps.ofs_delta = true,
                _ => {}
            }
        }
        
        caps
    }
}

impl ProtocolClient {
    /// Discover refs for push
    pub fn discover_refs_for_push(&mut self, auth: Option<&str>) -> Result<(Vec<RemoteRef>, ReceiveCapabilities)> {
        let path = self.url.info_refs_receive_url();
        
        print("scratch: fetching refs for push from ");
        print(&path);
        print("\n");

        let response = self.client.get_with_auth(&path, auth)?;

        if response.status == 401 {
            return Err(Error::protocol("authentication required"));
        }

        if response.status != 200 {
            return Err(Error::http(&format!("status {}", response.status)));
        }

        // Check content type
        let content_type = response.header("Content-Type").unwrap_or("");
        if !content_type.contains("x-git-receive-pack-advertisement") {
            return Err(Error::protocol("not a smart Git server (receive-pack)"));
        }

        parse_receive_ref_discovery(&response.body)
    }

    /// Push pack to remote
    ///
    /// # Arguments
    /// * `old_sha` - Current SHA of the ref on remote (zeros for new ref)
    /// * `new_sha` - New SHA to update the ref to
    /// * `ref_name` - Full ref name (e.g., "refs/heads/main")
    /// * `pack_data` - Pack file data containing objects
    /// * `caps` - Server capabilities
    /// * `auth` - Optional authentication header value
    pub fn push_pack(
        &mut self,
        old_sha: &Sha1Hash,
        new_sha: &Sha1Hash,
        ref_name: &str,
        pack_data: &[u8],
        caps: &ReceiveCapabilities,
        auth: Option<&str>,
    ) -> Result<()> {
        let path = self.url.receive_pack_url();
        
        print("scratch: pushing to ");
        print(&path);
        print("\n");

        // Build request body
        let body = build_receive_pack_request(old_sha, new_sha, ref_name, pack_data, caps);

        let response = self.client.post_with_auth(
            &path,
            "application/x-git-receive-pack-request",
            &body,
            auth,
        )?;

        if response.status == 401 {
            return Err(Error::protocol("authentication required"));
        }

        if response.status != 200 {
            return Err(Error::http(&format!("push failed with status {}", response.status)));
        }

        // Parse response for status
        parse_receive_pack_response(&response.body, caps)?;

        Ok(())
    }
}

/// Parse ref discovery response for receive-pack
fn parse_receive_ref_discovery(data: &[u8]) -> Result<(Vec<RemoteRef>, ReceiveCapabilities)> {
    let mut pos = 0;
    let mut refs = Vec::new();
    let mut capabilities = ReceiveCapabilities::default();
    let mut first_ref = true;

    // First line should be "# service=git-receive-pack\n"
    let (first_line, consumed) = pktline::read_pkt_line(data)?;
    pos += consumed;
    
    if let Some(line) = first_line {
        let line_str = core::str::from_utf8(line)
            .map_err(|_| Error::protocol("invalid service line"))?;
        if !line_str.contains("git-receive-pack") {
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
            // Handle zero-id for empty repo
            let sha = if sha_hex == "0000000000000000000000000000000000000000" {
                [0u8; 20]
            } else {
                sha1::from_hex(&sha_hex)
                    .ok_or_else(|| Error::protocol("invalid ref SHA"))?
            };

            refs.push(RemoteRef {
                sha,
                name: ref_name,
            });

            // First ref has capabilities
            if first_ref {
                if let Some(caps_str) = caps_opt {
                    capabilities = ReceiveCapabilities::parse(&caps_str);
                }
                first_ref = false;
            }
        }
    }

    Ok((refs, capabilities))
}

/// Build receive-pack request body
fn build_receive_pack_request(
    old_sha: &Sha1Hash,
    new_sha: &Sha1Hash,
    ref_name: &str,
    pack_data: &[u8],
    caps: &ReceiveCapabilities,
) -> Vec<u8> {
    let mut body = Vec::new();

    // Build capabilities string
    let mut caps_str = String::from("report-status");
    if caps.side_band_64k {
        caps_str.push_str(" side-band-64k");
    }
    caps_str.push_str(" agent=scratch/1.0");

    // Ref update line: "<old-sha> <new-sha> <ref-name>\0<capabilities>\n"
    let update_line = format!(
        "{} {} {}\0{}\n",
        sha1::to_hex(old_sha),
        sha1::to_hex(new_sha),
        ref_name,
        caps_str
    );
    body.extend_from_slice(&pktline::write_pkt_line(update_line.as_bytes()));

    // Flush packet to end ref updates
    body.extend_from_slice(&pktline::write_flush());

    // Pack data follows directly
    body.extend_from_slice(pack_data);

    body
}

/// Parse receive-pack response
fn parse_receive_pack_response(data: &[u8], caps: &ReceiveCapabilities) -> Result<()> {
    if data.is_empty() {
        // Some servers send empty response on success
        return Ok(());
    }

    let mut pos = 0;
    let mut had_error = false;
    let mut error_msg = String::new();

    while pos < data.len() {
        let (content, consumed) = match pktline::read_pkt_line(&data[pos..]) {
            Ok(r) => r,
            Err(_) => break,
        };
        pos += consumed;

        let line = match content {
            None => continue, // Flush packet
            Some(l) => l,
        };

        // Handle sideband
        if caps.side_band_64k && !line.is_empty() {
            match line[0] {
                1 => {
                    // Data channel - parse status
                    let payload = &line[1..];
                    if let Ok(status) = core::str::from_utf8(payload) {
                        if status.starts_with("unpack ok") {
                            // Good
                        } else if status.starts_with("unpack ") {
                            had_error = true;
                            error_msg = String::from(status);
                        } else if status.starts_with("ng ") {
                            had_error = true;
                            error_msg = String::from(status);
                        } else if status.starts_with("ok ") {
                            // Ref updated successfully
                        }
                    }
                }
                2 => {
                    // Progress
                    if let Ok(msg) = core::str::from_utf8(&line[1..]) {
                        print("remote: ");
                        print(msg.trim());
                        print("\n");
                    }
                }
                3 => {
                    // Error
                    if let Ok(msg) = core::str::from_utf8(&line[1..]) {
                        return Err(Error::protocol(msg.trim()));
                    }
                }
                _ => {}
            }
        } else {
            // No sideband - direct status
            if let Ok(status) = core::str::from_utf8(line) {
                let status = status.trim();
                if status.starts_with("unpack ok") {
                    // Good
                } else if status.starts_with("unpack ") {
                    had_error = true;
                    error_msg = String::from(status);
                } else if status.starts_with("ng ") {
                    had_error = true;
                    error_msg = String::from(status);
                } else if status.starts_with("ok ") {
                    // Ref updated successfully
                    print("scratch: ");
                    print(status);
                    print("\n");
                }
            }
        }
    }

    if had_error {
        return Err(Error::protocol(&error_msg));
    }

    Ok(())
}
