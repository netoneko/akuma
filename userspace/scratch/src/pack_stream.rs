//! Streaming pack file parser
//!
//! Parses pack files incrementally, writing objects to disk as they're decoded.
//! This avoids loading the entire pack file into memory.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use libakuma::{print, print_dec};

use crate::error::{Error, Result};
use crate::object::ObjectType;
use crate::sha1::Sha1Hash;
use crate::store::ObjectStore;
use crate::zlib;
use crate::pack::apply_delta;

const OBJ_COMMIT: u8 = 1;
const OBJ_TREE: u8 = 2;
const OBJ_BLOB: u8 = 3;
const OBJ_TAG: u8 = 4;
const OBJ_OFS_DELTA: u8 = 6;
const OBJ_REF_DELTA: u8 = 7;

/// Pack object type + SHA recorded after an object is written to the store,
/// keyed by its pack-stream byte offset.
type OffsetMap = BTreeMap<usize, (ObjectType, Sha1Hash)>;

/// Streaming pack parser
///
/// Accumulates data and processes complete objects as they become available.
pub struct StreamingPackParser {
    /// Raw receive buffer.
    buffer: Vec<u8>,
    /// How many bytes at the front of `buffer` have already been consumed.
    /// We compact lazily (every 64 KB) to avoid O(n) shifts on every object.
    drain_offset: usize,
    /// Current parsing state
    state: ParseState,
    /// Object store for writing objects
    store: ObjectStore,
    /// Number of objects in pack
    object_count: u32,
    /// Objects parsed so far
    objects_parsed: u32,
    /// Mapping from pack-stream offset → (type, sha) for OFS_DELTA lookup.
    /// BTreeMap gives O(log n) lookup vs the previous O(n) Vec scan.
    offset_map: OffsetMap,
    /// Current byte offset in the pack stream (for OFS_DELTA)
    current_offset: usize,
    /// Pending REF_DELTA objects that couldn't be resolved yet
    pending_deltas: Vec<PendingDelta>,
    /// Pack version
    version: u32,
}

#[derive(Debug, Clone)]
enum ParseState {
    Header,
    ObjectHeader { offset: usize },
    ObjectData {
        offset: usize,
        obj_type: u8,
        size: usize,
        header_len: usize,
    },
    OfsDeltaHeader {
        offset: usize,
        size: usize,
        header_len: usize,
    },
    OfsDeltaData {
        offset: usize,
        base_offset: usize,
        size: usize,
        header_len: usize,
        ofs_len: usize,
    },
    RefDeltaHeader {
        offset: usize,
        size: usize,
        header_len: usize,
    },
    RefDeltaData {
        offset: usize,
        base_sha: Sha1Hash,
        size: usize,
        header_len: usize,
    },
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
            drain_offset: 0,
            state: ParseState::Header,
            store: ObjectStore::new(git_dir),
            object_count: 0,
            objects_parsed: 0,
            offset_map: BTreeMap::new(),
            current_offset: 0,
            pending_deltas: Vec::new(),
            version: 0,
        }
    }

    /// View of unconsumed bytes in the receive buffer.
    #[inline]
    fn buf(&self) -> &[u8] {
        &self.buffer[self.drain_offset..]
    }

    /// Mark `n` bytes at the front of the unconsumed view as consumed.
    /// Compacts the underlying Vec when enough bytes have accumulated to
    /// make the shift worthwhile (≥64 KB), avoiding O(n²) total work.
    #[inline]
    fn consume(&mut self, n: usize) {
        self.drain_offset += n;
        if self.drain_offset >= 65536 {
            self.buffer.drain(..self.drain_offset);
            self.drain_offset = 0;
        }
    }

    fn lookup_offset(&self, offset: usize) -> Option<(ObjectType, Sha1Hash)> {
        self.offset_map.get(&offset).copied()
    }

    fn record_offset(&mut self, offset: usize, obj_type: ObjectType, sha: Sha1Hash) {
        self.offset_map.insert(offset, (obj_type, sha));
    }

    /// Feed data to the parser.
    /// Returns Ok(true) if parsing should continue, Ok(false) if done.
    pub fn feed(&mut self, data: &[u8]) -> Result<bool> {
        self.buffer.extend_from_slice(data);

        let buf_len = self.buf().len();
        if buf_len > 100_000 && buf_len % 50_000 < data.len() {
            print("scratch: buffer size ");
            print_dec(buf_len);
            print(" bytes, parsed ");
            print_dec(self.objects_parsed as usize);
            print("/");
            print_dec(self.object_count as usize);
            print("\n");
        }

        loop {
            match self.process_buffer()? {
                ProcessResult::NeedMore => return Ok(true),
                ProcessResult::Continue => continue,
                ProcessResult::Done => return Ok(false),
            }
        }
    }

    /// Finalize parsing and resolve any pending deltas.
    pub fn finish(&mut self) -> Result<u32> {
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
        // Read values into locals so the shared borrow ends before we consume.
        let (version, object_count) = {
            let buf = self.buf();
            if buf.len() < 12 {
                return Ok(ProcessResult::NeedMore);
            }
            if &buf[0..4] != b"PACK" {
                return Err(Error::invalid_pack("invalid pack signature"));
            }
            let version = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
            let count = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
            (version, count)
        };

        self.version = version;
        self.object_count = object_count;

        print("scratch: pack version ");
        print_dec(self.version as usize);
        print(", ");
        print_dec(self.object_count as usize);
        print(" objects\n");

        self.consume(12);
        self.current_offset = 12;

        self.state = if self.object_count == 0 {
            ParseState::Done
        } else {
            ParseState::ObjectHeader { offset: 12 }
        };

        Ok(ProcessResult::Continue)
    }

    fn process_object_header(&mut self) -> Result<ProcessResult> {
        if self.buf().is_empty() {
            return Ok(ProcessResult::NeedMore);
        }

        let (obj_type, size, header_len) = match self.parse_type_and_size() {
            Some(result) => result,
            None => return Ok(ProcessResult::NeedMore),
        };

        let offset = if let ParseState::ObjectHeader { offset } = self.state {
            offset
        } else {
            return Err(Error::other("invalid state"));
        };

        self.state = match obj_type {
            OBJ_COMMIT | OBJ_TREE | OBJ_BLOB | OBJ_TAG => ParseState::ObjectData {
                offset,
                obj_type,
                size,
                header_len,
            },
            OBJ_OFS_DELTA => ParseState::OfsDeltaHeader {
                offset,
                size,
                header_len,
            },
            OBJ_REF_DELTA => ParseState::RefDeltaHeader {
                offset,
                size,
                header_len,
            },
            _ => return Err(Error::invalid_pack("unknown object type")),
        };

        Ok(ProcessResult::Continue)
    }

    fn process_object_data(&mut self) -> Result<ProcessResult> {
        let (offset, obj_type, _size, header_len) = if let ParseState::ObjectData {
            offset, obj_type, size, header_len
        } = self.state {
            (offset, obj_type, size, header_len)
        } else {
            return Err(Error::other("invalid state"));
        };

        // Decompress inside a block so the shared borrow of self.buffer ends
        // before we call consume() (which takes &mut self).
        let decomp = {
            let buf = self.buf();
            if buf.len() <= header_len {
                return Ok(ProcessResult::NeedMore);
            }
            match zlib::decompress_with_consumed(&buf[header_len..]) {
                Ok(result) => result,
                Err(_) => return Ok(ProcessResult::NeedMore),
            }
        };
        let (decompressed, bytes_consumed) = decomp;

        let type_name = match obj_type {
            OBJ_COMMIT => ObjectType::Commit,
            OBJ_TREE => ObjectType::Tree,
            OBJ_BLOB => ObjectType::Blob,
            OBJ_TAG => ObjectType::Tag,
            _ => return Err(Error::invalid_pack("unknown type")),
        };

        let sha = self.store.write_content(type_name, &decompressed)?;
        self.record_offset(offset, type_name, sha);
        self.objects_parsed += 1;

        let total_consumed = header_len + bytes_consumed;
        self.consume(total_consumed);
        self.current_offset += total_consumed;

        self.advance_to_next_object();
        Ok(ProcessResult::Continue)
    }

    fn process_ofs_delta_header(&mut self) -> Result<ProcessResult> {
        let (offset, _size, header_len) = if let ParseState::OfsDeltaHeader {
            offset, size, header_len
        } = self.state {
            (offset, size, header_len)
        } else {
            return Err(Error::other("invalid state"));
        };

        let (base_offset_delta, ofs_len) = {
            let buf = self.buf();
            if buf.len() <= header_len {
                return Ok(ProcessResult::NeedMore);
            }
            let ofs_start = header_len;
            let mut ofs_len = 0usize;
            let mut base_offset_delta = 0usize;

            for i in 0..10 {
                if ofs_start + i >= buf.len() {
                    return Ok(ProcessResult::NeedMore);
                }
                let byte = buf[ofs_start + i];
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
            (base_offset_delta, ofs_len)
        };

        let base_offset = offset.saturating_sub(base_offset_delta);

        self.state = ParseState::OfsDeltaData {
            offset,
            base_offset,
            size: 0, // not used further
            header_len,
            ofs_len,
        };

        Ok(ProcessResult::Continue)
    }

    fn process_ofs_delta_data(&mut self) -> Result<ProcessResult> {
        let (offset, base_offset, _size, header_len, ofs_len) = if let ParseState::OfsDeltaData {
            offset, base_offset, size, header_len, ofs_len
        } = self.state {
            (offset, base_offset, size, header_len, ofs_len)
        } else {
            return Err(Error::other("invalid state"));
        };

        let data_start = header_len + ofs_len;

        let decomp = {
            let buf = self.buf();
            if buf.len() <= data_start {
                return Ok(ProcessResult::NeedMore);
            }
            match zlib::decompress_with_consumed(&buf[data_start..]) {
                Ok(result) => result,
                Err(_) => return Ok(ProcessResult::NeedMore),
            }
        };
        let (delta_data, bytes_consumed) = decomp;

        let (base_type, base_sha) = self.lookup_offset(base_offset)
            .ok_or_else(|| Error::invalid_pack("OFS_DELTA base not found"))?;

        let (_, base_content) = self.store.read_raw_content(&base_sha)?;
        let result = apply_delta(&base_content, &delta_data)?;
        let sha = self.store.write_content(base_type, &result)?;
        self.record_offset(offset, base_type, sha);
        self.objects_parsed += 1;

        let total_consumed = data_start + bytes_consumed;
        self.consume(total_consumed);
        self.current_offset += total_consumed;

        self.advance_to_next_object();
        Ok(ProcessResult::Continue)
    }

    fn process_ref_delta_header(&mut self) -> Result<ProcessResult> {
        let (offset, _size, header_len) = if let ParseState::RefDeltaHeader {
            offset, size, header_len
        } = self.state {
            (offset, size, header_len)
        } else {
            return Err(Error::other("invalid state"));
        };

        let base_sha = {
            let buf = self.buf();
            if buf.len() < header_len + 20 {
                return Ok(ProcessResult::NeedMore);
            }
            let mut sha = [0u8; 20];
            sha.copy_from_slice(&buf[header_len..header_len + 20]);
            sha
        };

        self.state = ParseState::RefDeltaData {
            offset,
            base_sha,
            size: 0,
            header_len,
        };

        Ok(ProcessResult::Continue)
    }

    fn process_ref_delta_data(&mut self) -> Result<ProcessResult> {
        // Copy all fields (all are Copy types) to avoid holding a borrow on self.state.
        let (offset, base_sha, header_len) = match &self.state {
            ParseState::RefDeltaData { offset, base_sha, header_len, .. } =>
                (*offset, *base_sha, *header_len),
            _ => return Err(Error::other("invalid state")),
        };

        let data_start = header_len + 20;

        let decomp = {
            let buf = self.buf();
            if buf.len() <= data_start {
                return Ok(ProcessResult::NeedMore);
            }
            match zlib::decompress_with_consumed(&buf[data_start..]) {
                Ok(result) => result,
                Err(_) => return Ok(ProcessResult::NeedMore),
            }
        };
        let (delta_data, bytes_consumed) = decomp;

        match self.resolve_ref_delta(&base_sha, &delta_data) {
            Ok(sha) => {
                if let Ok((base_type, _)) = self.store.read_raw_content(&base_sha) {
                    self.record_offset(offset, base_type, sha);
                }
            }
            Err(_) => {
                self.pending_deltas.push(PendingDelta {
                    base_sha,
                    delta_data,
                });
            }
        }

        self.objects_parsed += 1;

        let total_consumed = data_start + bytes_consumed;
        self.consume(total_consumed);
        self.current_offset += total_consumed;

        self.advance_to_next_object();
        Ok(ProcessResult::Continue)
    }

    fn resolve_ref_delta(&self, base_sha: &Sha1Hash, delta_data: &[u8]) -> Result<Sha1Hash> {
        let (base_type, base_content) = self.store.read_raw_content(base_sha)?;
        let result = apply_delta(&base_content, delta_data)?;
        self.store.write_content(base_type, &result)
    }

    fn advance_to_next_object(&mut self) {
        if self.objects_parsed >= self.object_count {
            self.state = ParseState::Done;
        } else {
            self.state = ParseState::ObjectHeader {
                offset: self.current_offset,
            };
        }

        if self.objects_parsed % 100 == 0 {
            print("scratch: parsed ");
            print_dec(self.objects_parsed as usize);
            print("/");
            print_dec(self.object_count as usize);
            print(" objects\r");
        }
    }

    fn parse_type_and_size(&self) -> Option<(u8, usize, usize)> {
        let buf = self.buf();
        if buf.is_empty() {
            return None;
        }

        let first = buf[0];
        let obj_type = (first >> 4) & 0x07;
        let mut size = (first & 0x0f) as usize;
        let mut shift = 4;
        let mut i = 1;

        if first & 0x80 != 0 {
            loop {
                if i >= buf.len() {
                    return None;
                }
                let byte = buf[i];
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

