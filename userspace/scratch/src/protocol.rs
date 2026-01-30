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
    pub fn discover_refs(&self) -> Result<(Vec<RemoteRef>, Capabilities)> {
        let path = self.url.info_refs_url();
        
        print("scratch: fetching refs from ");
        print(&path);
        print("\n");

        let response = self.client.get(&path)?;

        if response.status != 200 {
            return Err(Error::http(&format!("status {}", response.status)));
        }

        // Check content type
        let content_type = response.header("Content-Type")
            .unwrap_or("");
        if !content_type.contains("x-git-upload-pack-advertisement") {
            // Might be a dumb server or error page
            return Err(Error::protocol("not a smart Git server"));
        }

        parse_ref_discovery(&response.body)
    }

    /// Fetch a pack file with the given wants
    ///
    /// Returns the raw pack data
    pub fn fetch_pack(&self, wants: &[Sha1Hash], haves: &[Sha1Hash], caps: &Capabilities) -> Result<Vec<u8>> {
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
