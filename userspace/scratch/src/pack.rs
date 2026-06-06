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
use crate::sha1::Sha1Hash;
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

/// Lightweight metadata from the scan phase (no decompressed data kept)
struct EntryMeta {
    /// Offset of this entry in the pack file
    offset: usize,
    /// Object type
    obj_type: PackObjectType,
    /// Start of compressed data in pack buffer
    compressed_offset: usize,
    /// Length of compressed data
    compressed_len: usize,
    /// For OFS_DELTA: negative offset to base
    ofs_delta_offset: Option<usize>,
    /// For REF_DELTA: base object SHA
    ref_delta_sha: Option<Sha1Hash>,
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

    /// Parse all objects and store them.
    ///
    /// Two-phase approach to keep memory bounded:
    ///  - Phase 1 (scan): parse headers and decompress to find boundaries.
    ///    Builds lightweight metadata and identifies which offsets are needed
    ///    as delta bases.  Decompressed data is immediately discarded.
    ///  - Phase 2 (process): decompress each object again and store it.
    ///    Only objects that serve as delta bases are retained in memory.
    pub fn parse_all(&mut self, store: &ObjectStore) -> Result<Vec<Sha1Hash>> {
        use alloc::collections::BTreeSet;
        use libakuma::print;

        let total = self.object_count as usize;

        // ---- Phase 1: scan metadata, identify needed delta bases ----
        let mut entries: Vec<EntryMeta> = Vec::with_capacity(total);
        let mut needed_base_offsets: BTreeSet<usize> = BTreeSet::new();

        for _ in 0..self.object_count {
            let entry = self.scan_entry()?;

            // Record which offsets are referenced as delta bases
            if let Some(ofs) = entry.ofs_delta_offset {
                needed_base_offsets.insert(entry.offset - ofs);
            }
            // (RefDelta bases are looked up by SHA later — they may come
            //  from outside the pack, so we can't pre-mark offsets for them.)

            entries.push(entry);
        }

        // ---- Phase 2: decompress, store, retain only needed bases ----
        let mut shas = Vec::with_capacity(total);
        // Only keeps decompressed data for objects that are delta bases
        let mut resolved_bases: BTreeMap<usize, (ObjectType, Vec<u8>)> = BTreeMap::new();
        // SHA → pack offset for O(log n) RefDelta lookup
        let mut sha_to_offset: BTreeMap<Sha1Hash, usize> = BTreeMap::new();
        // Delta entries whose base isn't resolved yet (indices into `entries`)
        let mut remaining_deltas: Vec<usize> = Vec::new();

        for (i, entry) in entries.iter().enumerate() {
            if i % 500 == 0 {
                print("scratch: processing ");
                print_num(i);
                print("/");
                print_num(total);
                print("\n");
            }

            let is_delta = entry.obj_type == PackObjectType::OfsDelta
                || entry.obj_type == PackObjectType::RefDelta;

            if is_delta {
                remaining_deltas.push(i);
                continue;
            }

            // Non-delta object — decompress and store
            let decompressed = Self::decompress_entry(self.data, entry)?;
            let obj_type = entry.obj_type.to_object_type().unwrap();
            let sha = store.write_content(obj_type, &decompressed)?;
            sha_to_offset.insert(sha, entry.offset);
            shas.push(sha);

            // Only retain data if this object is a base for some delta
            if needed_base_offsets.contains(&entry.offset) {
                resolved_bases.insert(entry.offset, (obj_type, decompressed));
            }
            // Otherwise `decompressed` is dropped here, freeing memory
        }

        // ---- Phase 3: resolve deltas (may need multiple iterations for chains) ----
        let max_iterations = remaining_deltas.len() + 1;
        let mut iteration = 0;

        while !remaining_deltas.is_empty() && iteration < max_iterations {
            iteration += 1;
            let prev_count = remaining_deltas.len();
            let mut still_remaining: Vec<usize> = Vec::new();

            for &idx in &remaining_deltas {
                let entry = &entries[idx];

                let base: Option<(ObjectType, &[u8])> = match entry.obj_type {
                    PackObjectType::OfsDelta => {
                        let base_offset = entry.offset - entry.ofs_delta_offset.unwrap();
                        resolved_bases.get(&base_offset).map(|(t, d)| (*t, d.as_slice()))
                    }
                    PackObjectType::RefDelta => {
                        let base_sha = entry.ref_delta_sha.unwrap();
                        sha_to_offset.get(&base_sha)
                            .and_then(|off| resolved_bases.get(off))
                            .map(|(t, d)| (*t, d.as_slice()))
                    }
                    _ => unreachable!(),
                };

                if let Some((base_type, base_data)) = base {
                    let delta_data = Self::decompress_entry(self.data, entry)?;
                    let result = apply_delta(base_data, &delta_data)?;
                    let sha = store.write_content(base_type, &result)?;
                    sha_to_offset.insert(sha, entry.offset);
                    shas.push(sha);

                    if needed_base_offsets.contains(&entry.offset) {
                        resolved_bases.insert(entry.offset, (base_type, result));
                    }
                } else if entry.obj_type == PackObjectType::RefDelta {
                    // Try reading base from the on-disk object store
                    let base_sha = entry.ref_delta_sha.unwrap();
                    if let Ok((base_type, base_data)) = store.read_raw_content(&base_sha) {
                        let delta_data = Self::decompress_entry(self.data, entry)?;
                        let result = apply_delta(&base_data, &delta_data)?;
                        let sha = store.write_content(base_type, &result)?;
                        sha_to_offset.insert(sha, entry.offset);
                        shas.push(sha);

                        if needed_base_offsets.contains(&entry.offset) {
                            resolved_bases.insert(entry.offset, (base_type, result));
                        }
                    } else {
                        still_remaining.push(idx);
                    }
                } else {
                    still_remaining.push(idx);
                }
            }

            if still_remaining.len() == prev_count {
                return Err(Error::delta_base_not_found());
            }
            remaining_deltas = still_remaining;
        }

        if !remaining_deltas.is_empty() {
            return Err(Error::delta_base_not_found());
        }

        print("scratch: resolved ");
        print_num(shas.len());
        print(" objects\n");

        Ok(shas)
    }

    /// Phase 1 helper: parse entry header and skip over compressed data.
    /// Returns lightweight metadata; decompressed data is discarded.
    fn scan_entry(&mut self) -> Result<EntryMeta> {
        let entry_offset = self.pos;

        let (obj_type, _size, _) = self.parse_type_and_size()?;

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

        let compressed_offset = self.pos;
        // Must decompress to find where the zlib stream ends
        let (_decompressed, consumed) = zlib::decompress_with_consumed(&self.data[compressed_offset..])?;
        self.pos += consumed;

        Ok(EntryMeta {
            offset: entry_offset,
            obj_type,
            compressed_offset,
            compressed_len: consumed,
            ofs_delta_offset,
            ref_delta_sha,
        })
    }

    /// Phase 2 helper: decompress an entry's data from the pack buffer.
    fn decompress_entry(pack_data: &[u8], entry: &EntryMeta) -> Result<Vec<u8>> {
        let slice = &pack_data[entry.compressed_offset..entry.compressed_offset + entry.compressed_len];
        let (decompressed, _) = zlib::decompress_with_consumed(slice)?;
        Ok(decompressed)
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

fn print_num(n: usize) {
    use libakuma::print;
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
