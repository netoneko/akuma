//! Git pack file parsing
//!
//! Pack files are how Git efficiently stores and transfers objects.
//! Format:
//!   - Header: "PACK" + version (4 bytes) + object count (4 bytes)
//!   - Objects: variable-length encoded entries
//!   - Trailer: SHA-1 checksum (20 bytes)

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::object::ObjectType;
use crate::sha1::{self, Sha1Hash};
use crate::store::ObjectStore;
use crate::zlib;

/// Pack object types (including delta types)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackObjectType {
    Commit,
    Tree,
    Blob,
    Tag,
    /// Delta with offset to base object
    OfsDelta,
    /// Delta with SHA reference to base object
    RefDelta,
}

impl PackObjectType {
    pub fn from_type_bits(bits: u8) -> Option<Self> {
        match bits {
            1 => Some(PackObjectType::Commit),
            2 => Some(PackObjectType::Tree),
            3 => Some(PackObjectType::Blob),
            4 => Some(PackObjectType::Tag),
            6 => Some(PackObjectType::OfsDelta),
            7 => Some(PackObjectType::RefDelta),
            _ => None,
        }
    }

    pub fn to_object_type(&self) -> Option<ObjectType> {
        match self {
            PackObjectType::Commit => Some(ObjectType::Commit),
            PackObjectType::Tree => Some(ObjectType::Tree),
            PackObjectType::Blob => Some(ObjectType::Blob),
            PackObjectType::Tag => Some(ObjectType::Tag),
            _ => None,
        }
    }
}

/// A parsed pack entry (before delta resolution)
#[derive(Debug)]
struct PackEntry {
    /// Offset in pack file
    offset: usize,
    /// Object type
    obj_type: PackObjectType,
    /// Decompressed size
    size: usize,
    /// For OFS_DELTA: negative offset to base
    ofs_delta_offset: Option<usize>,
    /// For REF_DELTA: base object SHA
    ref_delta_sha: Option<Sha1Hash>,
    /// Decompressed data (or delta instructions)
    data: Vec<u8>,
}

/// Pack file parser
pub struct PackParser<'a> {
    data: &'a [u8],
    pos: usize,
    object_count: u32,
    version: u32,
}

impl<'a> PackParser<'a> {
    /// Create a new pack parser
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.len() < 12 {
            return Err(Error::invalid_pack("pack too small for header"));
        }

        // Check magic
        if &data[0..4] != b"PACK" {
            return Err(Error::invalid_pack("invalid pack magic"));
        }

        // Parse version (big-endian)
        let version = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if version != 2 && version != 3 {
            return Err(Error::invalid_pack("unsupported pack version"));
        }

        // Parse object count
        let object_count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

        Ok(Self {
            data,
            pos: 12, // Skip header
            object_count,
            version,
        })
    }

    /// Get the number of objects in the pack
    pub fn object_count(&self) -> u32 {
        self.object_count
    }

    /// Parse all objects and store them
    pub fn parse_all(&mut self, store: &ObjectStore) -> Result<Vec<Sha1Hash>> {
        // First pass: parse all entries (including deltas)
        let mut entries: Vec<PackEntry> = Vec::with_capacity(self.object_count as usize);
        let mut offset_to_index: BTreeMap<usize, usize> = BTreeMap::new();

        for i in 0..self.object_count {
            let entry = self.parse_entry()?;
            offset_to_index.insert(entry.offset, i as usize);
            entries.push(entry);
        }

        // Second pass: resolve deltas and store objects
        let mut shas = Vec::with_capacity(entries.len());
        let mut resolved: BTreeMap<usize, (ObjectType, Vec<u8>)> = BTreeMap::new();

        // Process non-delta objects first
        for (i, entry) in entries.iter().enumerate() {
            if let Some(obj_type) = entry.obj_type.to_object_type() {
                resolved.insert(entry.offset, (obj_type, entry.data.clone()));
                let sha = store.write_content(obj_type, &entry.data)?;
                shas.push(sha);
            }
        }

        // Now resolve deltas (may need multiple passes for chained deltas)
        let mut remaining: Vec<usize> = entries.iter()
            .enumerate()
            .filter(|(_, e)| e.obj_type == PackObjectType::OfsDelta || e.obj_type == PackObjectType::RefDelta)
            .map(|(i, _)| i)
            .collect();

        let max_iterations = remaining.len() + 1;
        for _ in 0..max_iterations {
            if remaining.is_empty() {
                break;
            }

            let mut still_remaining = Vec::new();

            for &idx in &remaining {
                let entry = &entries[idx];
                
                // Find base object
                let base = match entry.obj_type {
                    PackObjectType::OfsDelta => {
                        let base_offset = entry.offset - entry.ofs_delta_offset.unwrap();
                        resolved.get(&base_offset).cloned()
                    }
                    PackObjectType::RefDelta => {
                        let base_sha = entry.ref_delta_sha.as_ref().unwrap();
                        // Try resolved objects first, then the store
                        let mut found = None;
                        for (ot, d) in resolved.values() {
                            let hash = sha1::hash_object(ot.as_str(), d);
                            if &hash == base_sha {
                                found = Some((*ot, d.clone()));
                                break;
                            }
                        }
                        if found.is_none() {
                            if let Ok((obj_type, data)) = store.read_raw_content(base_sha) {
                                found = Some((obj_type, data));
                            }
                        }
                        found
                    }
                    _ => unreachable!(),
                };

                if let Some((base_type, base_data)) = base {
                    // Apply delta
                    let result = apply_delta(&base_data, &entry.data)?;
                    resolved.insert(entry.offset, (base_type, result.clone()));
                    let sha = store.write_content(base_type, &result)?;
                    shas.push(sha);
                } else {
                    // Base not yet resolved
                    still_remaining.push(idx);
                }
            }

            if still_remaining.len() == remaining.len() {
                // No progress made
                return Err(Error::delta_base_not_found());
            }
            remaining = still_remaining;
        }

        if !remaining.is_empty() {
            return Err(Error::delta_base_not_found());
        }

        Ok(shas)
    }

    /// Parse a single pack entry
    fn parse_entry(&mut self) -> Result<PackEntry> {
        let entry_offset = self.pos;
        
        // Parse type and size (variable-length encoding)
        let (obj_type, size, header_bytes) = self.parse_type_and_size()?;

        // For delta types, read additional header
        let (ofs_delta_offset, ref_delta_sha) = match obj_type {
            PackObjectType::OfsDelta => {
                let offset = self.parse_ofs_delta_offset()?;
                (Some(offset), None)
            }
            PackObjectType::RefDelta => {
                if self.pos + 20 > self.data.len() {
                    return Err(Error::invalid_pack("truncated REF_DELTA header"));
                }
                let mut sha = [0u8; 20];
                sha.copy_from_slice(&self.data[self.pos..self.pos + 20]);
                self.pos += 20;
                (None, Some(sha))
            }
            _ => (None, None),
        };

        // Decompress the data
        let data = self.decompress_next()?;

        Ok(PackEntry {
            offset: entry_offset,
            obj_type,
            size,
            ofs_delta_offset,
            ref_delta_sha,
            data,
        })
    }

    /// Parse type and size from variable-length header
    fn parse_type_and_size(&mut self) -> Result<(PackObjectType, usize, usize)> {
        if self.pos >= self.data.len() {
            return Err(Error::invalid_pack("unexpected end of pack"));
        }

        let mut byte = self.data[self.pos];
        self.pos += 1;
        let mut header_bytes = 1;

        // Type is bits 4-6 of first byte
        let type_bits = (byte >> 4) & 0x7;
        let obj_type = PackObjectType::from_type_bits(type_bits)
            .ok_or_else(|| Error::invalid_pack("invalid object type"))?;

        // Size starts with bits 0-3 of first byte
        let mut size = (byte & 0x0f) as usize;
        let mut shift = 4;

        // Continue while MSB is set
        while byte & 0x80 != 0 {
            if self.pos >= self.data.len() {
                return Err(Error::invalid_pack("truncated size"));
            }
            byte = self.data[self.pos];
            self.pos += 1;
            header_bytes += 1;
            
            size |= ((byte & 0x7f) as usize) << shift;
            shift += 7;
        }

        Ok((obj_type, size, header_bytes))
    }

    /// Parse OFS_DELTA offset (variable-length, different encoding)
    fn parse_ofs_delta_offset(&mut self) -> Result<usize> {
        if self.pos >= self.data.len() {
            return Err(Error::invalid_pack("truncated OFS_DELTA offset"));
        }

        let mut byte = self.data[self.pos];
        self.pos += 1;
        let mut offset = (byte & 0x7f) as usize;

        while byte & 0x80 != 0 {
            if self.pos >= self.data.len() {
                return Err(Error::invalid_pack("truncated OFS_DELTA offset"));
            }
            byte = self.data[self.pos];
            self.pos += 1;
            
            offset = ((offset + 1) << 7) | ((byte & 0x7f) as usize);
        }

        Ok(offset)
    }

    /// Decompress the next zlib stream
    fn decompress_next(&mut self) -> Result<Vec<u8>> {
        // Find the end of the zlib stream by trial decompression
        // This is necessary because zlib streams don't have a length prefix in pack files
        
        let remaining = &self.data[self.pos..];
        
        // Try decompressing with increasing amounts of data
        for len in 2..remaining.len() {
            if let Ok(decompressed) = zlib::decompress(&remaining[..len]) {
                self.pos += len;
                return Ok(decompressed);
            }
        }
        
        // Try the whole remaining data
        let decompressed = zlib::decompress(remaining)?;
        self.pos = self.data.len();
        Ok(decompressed)
    }
}

/// Apply a delta to a base object
pub fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0;

    // Parse base size (variable-length)
    let (base_size, consumed) = parse_delta_size(&delta[pos..])?;
    pos += consumed;
    
    if base_size != base.len() {
        return Err(Error::invalid_pack("delta base size mismatch"));
    }

    // Parse result size (variable-length)
    let (result_size, consumed) = parse_delta_size(&delta[pos..])?;
    pos += consumed;

    let mut result = Vec::with_capacity(result_size);

    // Apply delta instructions
    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;

        if cmd & 0x80 != 0 {
            // Copy instruction
            let (copy_offset, copy_size, consumed) = parse_copy_instruction(cmd, &delta[pos..])?;
            pos += consumed;
            
            if copy_offset + copy_size > base.len() {
                return Err(Error::invalid_pack("copy out of bounds"));
            }
            
            result.extend_from_slice(&base[copy_offset..copy_offset + copy_size]);
        } else if cmd != 0 {
            // Insert instruction: cmd is the number of bytes to insert
            let insert_size = cmd as usize;
            if pos + insert_size > delta.len() {
                return Err(Error::invalid_pack("insert truncated"));
            }
            result.extend_from_slice(&delta[pos..pos + insert_size]);
            pos += insert_size;
        } else {
            // Reserved
            return Err(Error::invalid_pack("reserved delta instruction"));
        }
    }

    if result.len() != result_size {
        return Err(Error::invalid_pack("delta result size mismatch"));
    }

    Ok(result)
}

/// Parse variable-length size in delta header
fn parse_delta_size(data: &[u8]) -> Result<(usize, usize)> {
    let mut size = 0usize;
    let mut shift = 0;
    let mut pos = 0;

    loop {
        if pos >= data.len() {
            return Err(Error::invalid_pack("truncated delta size"));
        }
        let byte = data[pos];
        pos += 1;
        
        size |= ((byte & 0x7f) as usize) << shift;
        shift += 7;
        
        if byte & 0x80 == 0 {
            break;
        }
    }

    Ok((size, pos))
}

/// Parse copy instruction
fn parse_copy_instruction(cmd: u8, data: &[u8]) -> Result<(usize, usize, usize)> {
    let mut offset = 0usize;
    let mut size = 0usize;
    let mut pos = 0;

    // Offset bytes (little-endian)
    if cmd & 0x01 != 0 {
        offset |= (data[pos] as usize) << 0;
        pos += 1;
    }
    if cmd & 0x02 != 0 {
        offset |= (data[pos] as usize) << 8;
        pos += 1;
    }
    if cmd & 0x04 != 0 {
        offset |= (data[pos] as usize) << 16;
        pos += 1;
    }
    if cmd & 0x08 != 0 {
        offset |= (data[pos] as usize) << 24;
        pos += 1;
    }

    // Size bytes (little-endian)
    if cmd & 0x10 != 0 {
        size |= (data[pos] as usize) << 0;
        pos += 1;
    }
    if cmd & 0x20 != 0 {
        size |= (data[pos] as usize) << 8;
        pos += 1;
    }
    if cmd & 0x40 != 0 {
        size |= (data[pos] as usize) << 16;
        pos += 1;
    }

    // Size of 0 means 0x10000
    if size == 0 {
        size = 0x10000;
    }

    Ok((offset, size, pos))
}
