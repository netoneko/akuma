//! Git object store
//!
//! Manages reading and writing of loose objects in .git/objects/

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libakuma::{close, mkdir, open, open_flags, read_fd, write_fd};

use crate::error::{Error, Result};
use crate::object::{Object, ObjectType};
use crate::sha1::{self, Sha1Hash};
use crate::zlib;

/// Git object store
pub struct ObjectStore {
    /// Path to .git directory
    git_dir: String,
}

impl ObjectStore {
    /// Create a new object store
    pub fn new(git_dir: &str) -> Self {
        Self {
            git_dir: String::from(git_dir),
        }
    }

    /// Get the path to an object file
    fn object_path(&self, sha: &Sha1Hash) -> String {
        let hex = sha1::to_hex(sha);
        format!("{}/objects/{}/{}", self.git_dir, &hex[..2], &hex[2..])
    }

    /// Get the directory path for an object
    fn object_dir(&self, sha: &Sha1Hash) -> String {
        let hex = sha1::to_hex(sha);
        format!("{}/objects/{}", self.git_dir, &hex[..2])
    }

    /// Check if an object exists
    pub fn exists(&self, sha: &Sha1Hash) -> bool {
        let path = self.object_path(sha);
        let fd = open(&path, open_flags::O_RDONLY);
        if fd >= 0 {
            close(fd);
            true
        } else {
            false
        }
    }

    /// Read a raw object (compressed) from disk
    pub fn read_raw_compressed(&self, sha: &Sha1Hash) -> Result<Vec<u8>> {
        let path = self.object_path(sha);
        let fd = open(&path, open_flags::O_RDONLY);
        if fd < 0 {
            return Err(Error::object_not_found());
        }

        // Read the compressed data
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        
        loop {
            let n = read_fd(fd, &mut buf);
            if n <= 0 {
                break;
            }
            data.extend_from_slice(&buf[..n as usize]);
        }
        
        close(fd);
        Ok(data)
    }

    /// Read and parse an object
    pub fn read(&self, sha: &Sha1Hash) -> Result<Object> {
        let compressed = self.read_raw_compressed(sha)?;
        let decompressed = zlib::decompress(&compressed)?;
        
        // Verify hash
        let actual_hash = sha1::hash(&decompressed);
        if actual_hash != *sha {
            return Err(Error::hash_mismatch());
        }
        
        Object::parse(&decompressed)
    }

    /// Read an object's type and size without decompressing the entire content.
    pub fn read_info(&self, sha: &Sha1Hash) -> Result<(ObjectType, usize)> {
        let path = self.object_path(sha);
        let fd = open(&path, open_flags::O_RDONLY);
        if fd < 0 {
            return Err(Error::object_not_found());
        }

        // Read just the first 1KB of compressed data - usually enough for the header
        let mut compressed = [0u8; 1024];
        let n = read_fd(fd, &mut compressed);
        close(fd);
        
        if n <= 0 {
            return Err(Error::io("failed to read object"));
        }

        // Decompress just the beginning
        let mut decompressed = [0u8; 128];
        let (_, written) = zlib::decompress_header(&compressed[..n as usize], &mut decompressed)?;

        // Find the null byte separating header from content
        let null_pos = decompressed[..written].iter().position(|&b| b == 0)
            .ok_or_else(|| Error::invalid_object("missing null byte in header"))?;

        let header = core::str::from_utf8(&decompressed[..null_pos])
            .map_err(|_| Error::invalid_object("invalid UTF-8 in header"))?;

        // Parse "{type} {size}"
        let mut parts = header.split(' ');
        let type_str = parts.next()
            .ok_or_else(|| Error::invalid_object("missing type in header"))?;
        let size_str = parts.next()
            .ok_or_else(|| Error::invalid_object("missing size in header"))?;

        let obj_type = ObjectType::from_str(type_str)
            .ok_or_else(|| Error::invalid_object("unknown object type"))?;

        let size: usize = size_str.parse()
            .map_err(|_| Error::invalid_object("invalid size in header"))?;

        Ok((obj_type, size))
    }

    /// Read an object's raw content (after decompression, without parsing)
    pub fn read_raw_content(&self, sha: &Sha1Hash) -> Result<(ObjectType, Vec<u8>)> {
        let compressed = self.read_raw_compressed(sha)?;
        let mut decompressed = zlib::decompress(&compressed)?;
        
        // Parse just the header to get type and size
        let null_pos = decompressed.iter().position(|&b| b == 0)
            .ok_or_else(|| Error::invalid_object("missing null byte"))?;
        
        let header = core::str::from_utf8(&decompressed[..null_pos])
            .map_err(|_| Error::invalid_object("invalid header"))?;
        
        let mut parts = header.split(' ');
        let type_str = parts.next()
            .ok_or_else(|| Error::invalid_object("missing type"))?;
        
        let obj_type = ObjectType::from_str(type_str)
            .ok_or_else(|| Error::invalid_object("unknown type"))?;
        
        // Move content to the beginning and truncate to avoid a new allocation (8MB+ save)
        let content_start = null_pos + 1;
        let content_len = decompressed.len() - content_start;
        decompressed.copy_within(content_start.., 0);
        decompressed.truncate(content_len);
        
        Ok((obj_type, decompressed))
    }

    /// Write an object to the store
    ///
    /// Returns the SHA-1 hash of the object
    pub fn write(&self, obj: &Object) -> Result<Sha1Hash> {
        let raw = obj.serialize();
        self.write_raw(&raw)
    }

    /// Write raw object data (with header)
    pub fn write_raw(&self, data: &[u8]) -> Result<Sha1Hash> {
        let sha = sha1::hash(data);
        
        // Always write the file even if it exists - a previous failed clone
        // may have left corrupt object files that need to be overwritten.
        // O_TRUNC ensures the file is truncated before writing.
        
        // Compress
        let compressed = zlib::compress(data);
        
        // Create directory
        let dir = self.object_dir(&sha);
        let _ = mkdir(&dir); // Ignore error if exists
        
        // Write file
        let path = self.object_path(&sha);
        let fd = open(&path, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
        if fd < 0 {
            return Err(Error::io("failed to create object file"));
        }
        
        let written = write_fd(fd, &compressed);
        close(fd);
        
        if written < 0 {
            return Err(Error::io("failed to write object file"));
        }
        if (written as usize) < compressed.len() {
            return Err(Error::io("short write to object file"));
        }
        
        Ok(sha)
    }

    /// Write object content with type (constructs header internally)
    pub fn write_content(&self, obj_type: ObjectType, content: &[u8]) -> Result<Sha1Hash> {
        let header = format!("{} {}\0", obj_type.as_str(), content.len());
        
        let mut data = Vec::with_capacity(header.len() + content.len());
        data.extend_from_slice(header.as_bytes());
        data.extend_from_slice(content);
        
        self.write_raw(&data)
    }

    /// Initialize object store directories
    pub fn init(&self) -> Result<()> {
        let objects_dir = format!("{}/objects", self.git_dir);
        if mkdir(&objects_dir) < 0 {
            // Might already exist, try to continue
        }
        
        // Create info and pack subdirectories
        let _ = mkdir(&format!("{}/objects/info", self.git_dir));
        let _ = mkdir(&format!("{}/objects/pack", self.git_dir));
        
        Ok(())
    }
}
