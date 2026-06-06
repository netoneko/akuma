//! Error types for scratch

use alloc::string::String;

/// Error type for Git operations
#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// I/O error (file not found, read/write failed)
    Io,
    /// Invalid Git object format
    InvalidObject,
    /// Invalid pack file format
    InvalidPack,
    /// Zlib decompression failed
    DecompressFailed,
    /// SHA-1 mismatch
    HashMismatch,
    /// Network error
    Network,
    /// HTTP protocol error
    Http,
    /// Git protocol error
    Protocol,
    /// Reference not found
    RefNotFound,
    /// Repository not found or not initialized
    NotARepository,
    /// Invalid URL format
    InvalidUrl,
    /// Object not found in store
    ObjectNotFound,
    /// Delta base not found
    DeltaBaseNotFound,
    /// Other error
    Other,
}

impl Error {
    pub fn new(kind: ErrorKind, message: &str) -> Self {
        Self {
            kind,
            message: String::from(message),
        }
    }

    pub fn io(message: &str) -> Self {
        Self::new(ErrorKind::Io, message)
    }

    pub fn invalid_object(message: &str) -> Self {
        Self::new(ErrorKind::InvalidObject, message)
    }

    pub fn invalid_pack(message: &str) -> Self {
        Self::new(ErrorKind::InvalidPack, message)
    }

    pub fn decompress() -> Self {
        Self::new(ErrorKind::DecompressFailed, "zlib decompression failed")
    }

    pub fn hash_mismatch() -> Self {
        Self::new(ErrorKind::HashMismatch, "SHA-1 hash mismatch")
    }

    pub fn network(message: &str) -> Self {
        Self::new(ErrorKind::Network, message)
    }

    pub fn http(message: &str) -> Self {
        Self::new(ErrorKind::Http, message)
    }

    pub fn protocol(message: &str) -> Self {
        Self::new(ErrorKind::Protocol, message)
    }

    pub fn ref_not_found(name: &str) -> Self {
        Self::new(ErrorKind::RefNotFound, name)
    }

    pub fn not_a_repository() -> Self {
        Self::new(ErrorKind::NotARepository, "not a git repository")
    }

    pub fn invalid_url() -> Self {
        Self::new(ErrorKind::InvalidUrl, "invalid URL format")
    }

    pub fn object_not_found() -> Self {
        Self::new(ErrorKind::ObjectNotFound, "object not found")
    }

    pub fn other(message: &str) -> Self {
        Self::new(ErrorKind::Other, message)
    }

    pub fn delta_base_not_found() -> Self {
        Self::new(ErrorKind::DeltaBaseNotFound, "delta base not found")
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

pub type Result<T> = core::result::Result<T, Error>;
