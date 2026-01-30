//! Streaming pack file parser
//!
//! Parses pack files incrementally, writing objects to disk as they're decoded.
//! This avoids loading the entire pack file into memory.

use alloc::vec::Vec;

use libakuma::print;

use crate::error::{Error, Result};
use crate::object::ObjectType;
use crate::sha1::Sha1Hash;
use crate::store::ObjectStore;
use crate::zlib;
use crate::pack::apply_delta;

/// Pack object types (from pack format)
const OBJ_COMMIT: u8 = 1;
const OBJ_TREE: u8 = 2;
const OBJ_BLOB: u8 = 3;
const OBJ_TAG: u8 = 4;
const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

/// Entry in the offset map: (offset, type, sha)
struct OffsetEntry {
    offset: usize,
    obj_type: ObjectType,
    sha: Sha1Hash,
}

/// Streaming pack parser
/// 
/// Accumulates data and processes complete objects as they become available.
pub struct StreamingPackParser {
    /// Internal buffer for accumulating data
    buffer: Vec<u8>,
    /// Current parsing state
    state: ParseState,
    /// Object store for writing objects
    store: ObjectStore,
    /// Number of objects in pack
    object_count: u32,
    /// Objects parsed so far
    objects_parsed: u32,
    /// Mapping from pack offset to (object_type, sha1) for delta resolution
    /// Uses Vec instead of BTreeMap to avoid deep recursion during insertion
    offset_entries: Vec<OffsetEntry>,
    /// Current offset in the pack stream (for OFS_DELTA)
    current_offset: usize,
    /// Pending REF_DELTA objects that couldn't be resolved yet
    pending_deltas: Vec<PendingDelta>,
    /// Pack version
    version: u32,
}

#[derive(Debug, Clone)]
enum ParseState {
    /// Waiting for pack header (12 bytes)
    Header,
    /// Waiting for object header
    ObjectHeader { offset: usize },
    /// Reading compressed object data
    ObjectData {
        offset: usize,
        obj_type: u8,
        size: usize,
        header_len: usize,
    },
    /// Reading OFS_DELTA header (variable length offset)
    OfsDeltaHeader {
        offset: usize,
        size: usize,
        header_len: usize,
    },
    /// Reading OFS_DELTA data
    OfsDeltaData {
        offset: usize,
        base_offset: usize,
        size: usize,
        header_len: usize,
        ofs_len: usize,
    },
    /// Reading REF_DELTA header (20 byte SHA-1)
    RefDeltaHeader {
        offset: usize,
        size: usize,
        header_len: usize,
    },
    /// Reading REF_DELTA data
    RefDeltaData {
        offset: usize,
        base_sha: Sha1Hash,
        size: usize,
        header_len: usize,
    },
    /// All objects parsed
    Done,
}

struct PendingDelta {
    base_sha: Sha1Hash,
    delta_data: Vec<u8>,
}

impl StreamingPackParser {
    pub fn new(git_dir: &str) -> Self {
        Self {
            buffer: Vec::new(),
            state: ParseState::Header,
            store: ObjectStore::new(git_dir),
            object_count: 0,
            objects_parsed: 0,
            offset_entries: Vec::new(),
            current_offset: 0,
            pending_deltas: Vec::new(),
            version: 0,
        }
    }
    
    /// Look up an object by its pack offset
    fn lookup_offset(&self, offset: usize) -> Option<(ObjectType, Sha1Hash)> {
        for entry in &self.offset_entries {
            if entry.offset == offset {
                return Some((entry.obj_type, entry.sha));
            }
        }
        None
    }
    
    /// Record an object's offset mapping
    fn record_offset(&mut self, offset: usize, obj_type: ObjectType, sha: Sha1Hash) {
        self.offset_entries.push(OffsetEntry { offset, obj_type, sha });
    }

    /// Feed data to the parser
    /// Returns Ok(true) if parsing should continue, Ok(false) if done
    pub fn feed(&mut self, data: &[u8]) -> Result<bool> {
        self.buffer.extend_from_slice(data);
        
        // Debug: track buffer growth
        if self.buffer.len() > 100000 && self.buffer.len() % 50000 < data.len() {
            print("scratch: buffer size ");
            print_num(self.buffer.len());
            print(" bytes, parsed ");
            print_num(self.objects_parsed as usize);
            print("/");
            print_num(self.object_count as usize);
            print("\n");
        }
        
        // Process as much as possible
        loop {
            match self.process_buffer()? {
                ProcessResult::NeedMore => return Ok(true),
                ProcessResult::Continue => continue,
                ProcessResult::Done => return Ok(false),
            }
        }
    }

    /// Finalize parsing and resolve any pending deltas
    pub fn finish(&mut self) -> Result<u32> {
        // Try to resolve pending REF_DELTA objects
        let pending = core::mem::take(&mut self.pending_deltas);
        for delta in pending {
            self.resolve_ref_delta(&delta.base_sha, &delta.delta_data)?;
        }
        
        Ok(self.objects_parsed)
    }

    fn process_buffer(&mut self) -> Result<ProcessResult> {
        match &self.state {
            ParseState::Header => self.process_header(),
            ParseState::ObjectHeader { .. } => self.process_object_header(),
            ParseState::ObjectData { .. } => self.process_object_data(),
            ParseState::OfsDeltaHeader { .. } => self.process_ofs_delta_header(),
            ParseState::OfsDeltaData { .. } => self.process_ofs_delta_data(),
            ParseState::RefDeltaHeader { .. } => self.process_ref_delta_header(),
            ParseState::RefDeltaData { .. } => self.process_ref_delta_data(),
            ParseState::Done => Ok(ProcessResult::Done),
        }
    }

    fn process_header(&mut self) -> Result<ProcessResult> {
        if self.buffer.len() < 12 {
            return Ok(ProcessResult::NeedMore);
        }

        // Verify magic
        if &self.buffer[0..4] != b"PACK" {
            return Err(Error::invalid_pack("invalid pack signature"));
        }

        self.version = u32::from_be_bytes([
            self.buffer[4], self.buffer[5], self.buffer[6], self.buffer[7]
        ]);

        self.object_count = u32::from_be_bytes([
            self.buffer[8], self.buffer[9], self.buffer[10], self.buffer[11]
        ]);

        print("scratch: pack version ");
        print_num(self.version as usize);
        print(", ");
        print_num(self.object_count as usize);
        print(" objects\n");

        // Remove header from buffer
        self.buffer = self.buffer[12..].to_vec();
        self.current_offset = 12;

        if self.object_count == 0 {
            self.state = ParseState::Done;
        } else {
            self.state = ParseState::ObjectHeader { offset: 12 };
        }

        Ok(ProcessResult::Continue)
    }

    fn process_object_header(&mut self) -> Result<ProcessResult> {
        // Object header is variable length, minimum 1 byte
        if self.buffer.is_empty() {
            return Ok(ProcessResult::NeedMore);
        }

        // Parse type and size from variable-length header
        let (obj_type, size, header_len) = match self.parse_type_and_size() {
            Some(result) => result,
            None => return Ok(ProcessResult::NeedMore),
        };

        let offset = if let ParseState::ObjectHeader { offset } = self.state {
            offset
        } else {
            return Err(Error::other("invalid state"));
        };

        match obj_type {
            OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => {
                self.state = ParseState::ObjectData {
                    offset,
                    obj_type,
                    size,
                    header_len,
                };
            }
            OBJ_OFS_DELTA => {
                self.state = ParseState::OfsDeltaHeader {
                    offset,
                    size,
                    header_len,
                };
            }
            OBJ_REF_DELTA => {
                self.state = ParseState::RefDeltaHeader {
                    offset,
                    size,
                    header_len,
                };
            }
            _ => return Err(Error::invalid_pack("unknown object type")),
        }

        Ok(ProcessResult::Continue)
    }

    fn process_object_data(&mut self) -> Result<ProcessResult> {
        let (offset, obj_type, size, header_len) = if let ParseState::ObjectData { 
            offset, obj_type, size, header_len 
        } = self.state {
            (offset, obj_type, size, header_len)
        } else {
            return Err(Error::other("invalid state"));
        };

        // Try to decompress - we need enough data
        // Remove header bytes first
        if self.buffer.len() <= header_len {
            return Ok(ProcessResult::NeedMore);
        }

        let compressed = &self.buffer[header_len..];
        
        // Try to decompress - use the version that tells us bytes consumed
        match zlib::decompress_with_consumed(compressed) {
            Ok((decompressed, bytes_consumed)) => {
                // Determine object type
                let type_name = match obj_type {
                    OBJ_COMMIT => ObjectType::Commit,
                    OBJ_TREE => ObjectType::Tree,
                    OBJ_BLOB => ObjectType::Blob,
                    OBJ_TAG => ObjectType::Tag,
                    _ => return Err(Error::invalid_pack("unknown type")),
                };

                // Write to store
                let sha = self.store.write_content(type_name, &decompressed)?;
                
                // Record in offset map
                self.record_offset(offset, type_name, sha);
                self.objects_parsed += 1;

                // Use exact bytes consumed from decompressor
                let total_consumed = header_len + bytes_consumed;
                
                self.buffer = self.buffer[total_consumed..].to_vec();
                self.current_offset += total_consumed;

                self.advance_to_next_object();
                Ok(ProcessResult::Continue)
            }
            Err(_) => {
                // Need more data
                Ok(ProcessResult::NeedMore)
            }
        }
    }

    fn process_ofs_delta_header(&mut self) -> Result<ProcessResult> {
        let (offset, size, header_len) = if let ParseState::OfsDeltaHeader { 
            offset, size, header_len 
        } = self.state {
            (offset, size, header_len)
        } else {
            return Err(Error::other("invalid state"));
        };

        // Parse negative offset (variable length)
        if self.buffer.len() <= header_len {
            return Ok(ProcessResult::NeedMore);
        }

        let ofs_start = header_len;
        let mut ofs_len = 0;
        let mut base_offset_delta: usize = 0;

        for i in 0..10 {
            if ofs_start + i >= self.buffer.len() {
                return Ok(ProcessResult::NeedMore);
            }
            
            let byte = self.buffer[ofs_start + i];
            ofs_len += 1;
            
            if i == 0 {
                base_offset_delta = (byte & 0x7f) as usize;
            } else {
                base_offset_delta = ((base_offset_delta + 1) << 7) | ((byte & 0x7f) as usize);
            }
            
            if byte & 0x80 == 0 {
                break;
            }
        }

        let base_offset = offset.saturating_sub(base_offset_delta);

        self.state = ParseState::OfsDeltaData {
            offset,
            base_offset,
            size,
            header_len,
            ofs_len,
        };

        Ok(ProcessResult::Continue)
    }

    fn process_ofs_delta_data(&mut self) -> Result<ProcessResult> {
        let (offset, base_offset, size, header_len, ofs_len) = if let ParseState::OfsDeltaData { 
            offset, base_offset, size, header_len, ofs_len
        } = self.state {
            (offset, base_offset, size, header_len, ofs_len)
        } else {
            return Err(Error::other("invalid state"));
        };

        let data_start = header_len + ofs_len;
        if self.buffer.len() <= data_start {
            return Ok(ProcessResult::NeedMore);
        }

        let compressed = &self.buffer[data_start..];
        
        match zlib::decompress_with_consumed(compressed) {
            Ok((delta_data, bytes_consumed)) => {
                // Look up base object
                if let Some((base_type, base_sha)) = self.lookup_offset(base_offset) {
                    // Read base object from store
                    let (_, base_content) = self.store.read_raw_content(&base_sha)?;
                    
                    // Apply delta
                    let result = apply_delta(&base_content, &delta_data)?;
                    
                    // Write result
                    let sha = self.store.write_content(base_type, &result)?;
                    self.record_offset(offset, base_type, sha);
                    self.objects_parsed += 1;

                    let total_consumed = data_start + bytes_consumed;
                    
                    self.buffer = self.buffer[total_consumed..].to_vec();
                    self.current_offset += total_consumed;

                    self.advance_to_next_object();
                    Ok(ProcessResult::Continue)
                } else {
                    // Base not found yet - this shouldn't happen for OFS_DELTA
                    // as the base should always come before
                    Err(Error::invalid_pack("OFS_DELTA base not found"))
                }
            }
            Err(_) => Ok(ProcessResult::NeedMore),
        }
    }

    fn process_ref_delta_header(&mut self) -> Result<ProcessResult> {
        let (offset, size, header_len) = if let ParseState::RefDeltaHeader { 
            offset, size, header_len 
        } = self.state {
            (offset, size, header_len)
        } else {
            return Err(Error::other("invalid state"));
        };

        // Need 20 bytes for SHA-1
        if self.buffer.len() < header_len + 20 {
            return Ok(ProcessResult::NeedMore);
        }

        let mut base_sha = [0u8; 20];
        base_sha.copy_from_slice(&self.buffer[header_len..header_len + 20]);

        self.state = ParseState::RefDeltaData {
            offset,
            base_sha,
            size,
            header_len,
        };

        Ok(ProcessResult::Continue)
    }

    fn process_ref_delta_data(&mut self) -> Result<ProcessResult> {
        let (offset, base_sha, size, header_len) = if let ParseState::RefDeltaData { 
            offset, base_sha, size, header_len 
        } = self.state.clone() {
            (offset, base_sha, size, header_len)
        } else {
            return Err(Error::other("invalid state"));
        };

        let data_start = header_len + 20;
        if self.buffer.len() <= data_start {
            return Ok(ProcessResult::NeedMore);
        }

        let compressed = &self.buffer[data_start..];
        
        match zlib::decompress_with_consumed(compressed) {
            Ok((delta_data, bytes_consumed)) => {
                // Try to resolve now or defer
                match self.resolve_ref_delta(&base_sha, &delta_data) {
                    Ok(sha) => {
                        // Find base type from store
                        if let Ok((base_type, _)) = self.store.read_raw_content(&base_sha) {
                            self.record_offset(offset, base_type, sha);
                        }
                    }
                    Err(_) => {
                        // Defer - base not available yet
                        self.pending_deltas.push(PendingDelta {
                            base_sha,
                            delta_data: delta_data.clone(),
                        });
                    }
                }
                
                self.objects_parsed += 1;

                let total_consumed = data_start + bytes_consumed;
                
                self.buffer = self.buffer[total_consumed..].to_vec();
                self.current_offset += total_consumed;

                self.advance_to_next_object();
                Ok(ProcessResult::Continue)
            }
            Err(_) => Ok(ProcessResult::NeedMore),
        }
    }

    fn resolve_ref_delta(&self, base_sha: &Sha1Hash, delta_data: &[u8]) -> Result<Sha1Hash> {
        // Read base from store
        let (base_type, base_content) = self.store.read_raw_content(base_sha)?;
        
        // Apply delta
        let result = apply_delta(&base_content, delta_data)?;
        
        // Write result
        self.store.write_content(base_type, &result)
    }

    fn advance_to_next_object(&mut self) {
        if self.objects_parsed >= self.object_count {
            self.state = ParseState::Done;
        } else {
            self.state = ParseState::ObjectHeader { 
                offset: self.current_offset 
            };
        }

        // Progress update
        if self.objects_parsed % 100 == 0 {
            print("scratch: parsed ");
            print_num(self.objects_parsed as usize);
            print("/");
            print_num(self.object_count as usize);
            print(" objects\r");
        }
    }

    fn parse_type_and_size(&self) -> Option<(u8, usize, usize)> {
        if self.buffer.is_empty() {
            return None;
        }

        let first = self.buffer[0];
        let obj_type = (first >> 4) & 0x07;
        let mut size = (first & 0x0f) as usize;
        let mut shift = 4;
        let mut i = 1;

        if first & 0x80 != 0 {
            loop {
                if i >= self.buffer.len() {
                    return None;
                }
                let byte = self.buffer[i];
                size |= ((byte & 0x7f) as usize) << shift;
                shift += 7;
                i += 1;
                if byte & 0x80 == 0 {
                    break;
                }
            }
        }

        Some((obj_type, size, i))
    }
}

enum ProcessResult {
    NeedMore,
    Continue,
    Done,
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
