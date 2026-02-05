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
#[derive(Debug, Clone)] // Clone is now cheap as it only copies metadata
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
    /// Offset of the compressed data in the original pack_data buffer
    compressed_data_offset: usize,
    /// Length of the compressed data (after header, before SHA)
    compressed_data_len: usize,
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
    /// This now does two passes: first pass to collect all entry metadata,
    /// second pass to resolve deltas and store. Only keeps necessary data in memory.
    pub fn parse_all(&mut self, store: &ObjectStore) -> Result<Vec<Sha1Hash>> {
        use libakuma::print;
        use alloc::collections::BTreeSet;
        
        print("scratch: parse_all starting\n");
        
        // First pass: collect all entry metadata and identify needed delta bases
        let mut entries: Vec<PackEntry> = Vec::with_capacity(self.object_count as usize);
        let mut offset_to_index: BTreeMap<usize, usize> = BTreeMap::new();
        // Sets for tracking which objects need their decompressed data kept in memory
        let mut needed_bases_offsets: BTreeSet<usize> = BTreeSet::new(); // For OFS_DELTA base offsets
        let mut needed_bases_shas: BTreeSet<Sha1Hash> = BTreeSet::new(); // For REF_DELTA base SHAs

        for i in 0..self.object_count {
            print("scratch: parsing object ");
            print_num(i as usize);
            print(" at pos ");
            print_num(self.pos);
            print("\n");
            
            let entry = self.parse_entry_metadata()?; // Parse only metadata
            
            // For logging, we need to temporarily decompress to get the size
            // We pass the compressed slice directly to decompress_with_consumed
            let compressed_slice = &self.data[entry.compressed_data_offset..entry.compressed_data_offset + entry.compressed_data_len];
            let (decompressed_preview, _) = zlib::decompress_with_consumed(compressed_slice)?;

            print("scratch: parsed object type=");
            print_num(entry.obj_type as u8 as usize);
            print(" size=");
            print_num(decompressed_preview.len());
            print("\n");

            // Identify potential delta bases
            match entry.obj_type {
                PackObjectType::OfsDelta => {
                    let base_offset = entry.offset - entry.ofs_delta_offset.unwrap();
                    needed_bases_offsets.insert(base_offset);
                }
                PackObjectType::RefDelta => {
                    // RefDelta bases refer to objects by SHA, which we don't know until it's stored.
                    // For now, assume all RefDelta refer to *some* object that might need its SHA.
                    // This is less efficient, but safer. Better would be to resolve base SHAs in a separate pass.
                    // However, we track `ref_delta_sha` for the actual lookup.
                    // For now, we only need to store the base data of objects that match `ref_delta_sha` later.
                }
                _ => {} // Non-delta objects might be bases for other deltas, so their actual SHA will be checked later.
            }

            offset_to_index.insert(entry.offset, i as usize);
            entries.push(entry);
        }
        
        // After first pass, we now know all OFS_DELTA base offsets that are needed.
        // For REF_DELTA bases, we need to know their SHA. Since we don't have all SHAs yet,
        // we'll rely on the `store.read_raw_content` for those.

        // Second pass: process non-delta objects and store them
        let mut shas = Vec::with_capacity(entries.len());
        // `resolved_bases` now only stores data for objects that are bases for *other* deltas *within this pack*.
        // Key is the offset of the object in the pack file.
        let mut resolved_bases: BTreeMap<usize, (ObjectType, Vec<u8>)> = BTreeMap::new();
        // Keep track of deltas that need resolving
        let mut remaining_deltas: Vec<usize> = Vec::new();

        for (i, entry) in entries.iter().enumerate() {
            if entry.obj_type == PackObjectType::OfsDelta || entry.obj_type == PackObjectType::RefDelta {
                remaining_deltas.push(i); // Add deltas to a list for later resolution
                continue;
            }

            // This is a non-delta object (Commit, Tree, Blob, Tag)
            let compressed_slice = &self.data[entry.compressed_data_offset..entry.compressed_data_offset + entry.compressed_data_len];
            let decompressed_data = Self::decompress_slice(compressed_slice)?;
            
            let obj_type = entry.obj_type.to_object_type().unwrap();
            let sha = store.write_content(obj_type, &decompressed_data)?;
            shas.push(sha);

            // If this non-delta object is needed as a base (by offset or by its SHA), store its data in `resolved_bases`
            if needed_bases_offsets.contains(&entry.offset) || needed_bases_shas.contains(&sha) {
                 resolved_bases.insert(entry.offset, (obj_type, decompressed_data));
            }
            // Decompressed data is dropped here if not inserted into `resolved_bases`
        }

        // Third pass: resolve deltas (may need multiple passes for chained deltas)
        let mut iteration_count = 0;
        let max_iterations = remaining_deltas.len() + 1; // Limit iterations in case of circular references or missing bases

        while !remaining_deltas.is_empty() && iteration_count < max_iterations {
            iteration_count += 1;
            let mut still_remaining = Vec::new();

            for &idx in &remaining_deltas {
                let entry = &entries[idx];
                
                let base: Option<(ObjectType, Vec<u8>)> = match entry.obj_type {
                    PackObjectType::OfsDelta => {
                        let base_offset = entry.offset - entry.ofs_delta_offset.unwrap();
                        resolved_bases.get(&base_offset).cloned()
                    }
                    PackObjectType::RefDelta => {
                        let base_sha = entry.ref_delta_sha.unwrap();
                        // Try resolved_bases first
                        let mut found = None;
                        for (&offset, (ot, d)) in resolved_bases.iter() {
                            let hash = sha1::hash_object(ot.as_str(), d); // This is expensive!
                            if hash == base_sha {
                                found = Some((*ot, d.clone()));
                                // If this object's data is needed by offset for a future delta, add its offset.
                                needed_bases_offsets.insert(offset);
                                break;
                            }
                        }
                        // If not found in resolved_bases, try to read from the ObjectStore
                        if found.is_none() {
                            if let Ok((obj_type, data)) = store.read_raw_content(&base_sha) {
                                found = Some((obj_type, data));
                                // Data from store is not automatically put into `resolved_bases`
                                // This assumes store has it, and doesn't need to be kept in `resolved_bases`.
                            }
                        }
                        found
                    }
                    _ => unreachable!(),
                };

                if let Some((base_type, base_data)) = base {
                    let compressed_slice = &self.data[entry.compressed_data_offset..entry.compressed_data_offset + entry.compressed_data_len];
                    let delta_data = Self::decompress_slice(compressed_slice)?;
                    
                    let result = apply_delta(&base_data, &delta_data)?;
                    let sha = store.write_content(base_type, &result)?;
                    shas.push(sha);

                    // If this newly resolved object is also needed as a base, store its data in `resolved_bases`
                    if needed_bases_offsets.contains(&entry.offset) || needed_bases_shas.contains(&sha) {
                        resolved_bases.insert(entry.offset, (base_type, result));
                    }
                    // `result` (Vec<u8>) is dropped here if not inserted into `resolved_bases`
                } else {
                    // Base object for this delta is not yet resolved, keep it for the next iteration
                    still_remaining.push(idx);
                }
            }

            if still_remaining.len() == remaining_deltas.len() && !remaining_deltas.is_empty() {
                // No progress was made in resolving deltas during this iteration,
                // indicating either a circular dependency or a missing base.
                return Err(Error::delta_base_not_found());
            }
            remaining_deltas = still_remaining;
        }

        if !remaining_deltas.is_empty() {
            return Err(Error::delta_base_not_found());
        }

        Ok(shas)
    }

    /// Parse a single pack entry's metadata, without decompressing its data.
    /// Updates self.pos past the compressed data.
    fn parse_entry_metadata(&mut self) -> Result<PackEntry> {
        let entry_start_pos = self.pos;
        
        // Parse type and size (variable-length encoding)
        let (obj_type, size, header_bytes_len) = self.parse_type_and_size()?;
        
        let _header_end_pos = self.pos; // Position after type/size and any delta headers

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

        // The start of the compressed data is at the current `self.pos`
        let compressed_data_start = self.pos;
        let compressed_data_slice = &self.data[compressed_data_start..];
        
        // We need to determine the length of the compressed data
        // by performing a dummy decompression to find how many input bytes are consumed.
        let (_, consumed_compressed_bytes) = zlib::decompress_with_consumed(compressed_data_slice)?;
        self.pos += consumed_compressed_bytes; // Advance parser position past the compressed data

        Ok(PackEntry {
            offset: entry_start_pos, // The offset of the entire entry in the pack file
            obj_type,
            size, // Decompressed size
            ofs_delta_offset,
            ref_delta_sha,
            compressed_data_offset: compressed_data_start,
            compressed_data_len: consumed_compressed_bytes,
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

    /// Decompress a specific zlib stream from a slice.
    /// Returns the decompressed data.
    fn decompress_slice(compressed_data: &[u8]) -> Result<Vec<u8>> {
        // Use streaming decompression that tracks exact bytes consumed
        let (decompressed, _consumed) = zlib::decompress_with_consumed(compressed_data)?;
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
