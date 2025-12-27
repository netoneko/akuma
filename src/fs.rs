//! Synchronous Filesystem API
//!
//! Provides a synchronous FAT32 filesystem API built on top of the embedded-sdmmc crate
//! and VirtIO block device driver.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use embedded_sdmmc::{Mode, ShortFileName, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use spinning_top::Spinlock;

use crate::block;
use crate::console;

// ============================================================================
// Time Source
// ============================================================================

/// Simple time source that returns a fixed timestamp
struct AkumaTimeSource;

impl TimeSource for AkumaTimeSource {
    fn get_timestamp(&self) -> Timestamp {
        let secs = crate::timer::utc_time_us()
            .map(|us| us / 1_000_000)
            .unwrap_or(0);
        // FAT epoch is 1980-01-01, Unix epoch is 1970-01-01
        let fat_secs = if secs > 315532800 {
            secs - 315532800
        } else {
            0
        };
        let years_since_1980 = fat_secs / (365 * 24 * 3600);
        let year = 1980 + years_since_1980 as u16;

        Timestamp {
            year_since_1970: (year - 1970) as u8,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

// ============================================================================
// Error Types
// ============================================================================

/// Filesystem error type
#[derive(Debug, Clone, Copy)]
pub enum FsError {
    BlockDeviceNotInitialized,
    NotInitialized,
    NotFound,
    PermissionDenied,
    AlreadyExists,
    NotADirectory,
    NotAFile,
    DirectoryNotEmpty,
    IoError,
    InvalidPath,
    NoSpace,
    TooManyOpenFiles,
    InvalidHandle,
    Corrupt,
    EndOfFile,
    NoFilesystem,
    Internal,
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FsError::BlockDeviceNotInitialized => write!(f, "Block device not initialized"),
            FsError::NotInitialized => write!(f, "Filesystem not initialized"),
            FsError::NotFound => write!(f, "Not found"),
            FsError::PermissionDenied => write!(f, "Permission denied"),
            FsError::AlreadyExists => write!(f, "Already exists"),
            FsError::NotADirectory => write!(f, "Not a directory"),
            FsError::NotAFile => write!(f, "Not a file"),
            FsError::DirectoryNotEmpty => write!(f, "Directory not empty"),
            FsError::IoError => write!(f, "I/O error"),
            FsError::InvalidPath => write!(f, "Invalid path"),
            FsError::NoSpace => write!(f, "No space left"),
            FsError::TooManyOpenFiles => write!(f, "Too many open files"),
            FsError::InvalidHandle => write!(f, "Invalid file handle"),
            FsError::Corrupt => write!(f, "Filesystem corrupt"),
            FsError::EndOfFile => write!(f, "End of file"),
            FsError::NoFilesystem => write!(f, "No filesystem found"),
            FsError::Internal => write!(f, "Internal error"),
        }
    }
}

/// Convert embedded-sdmmc error to our error type
fn convert_error<E: core::fmt::Debug>(err: embedded_sdmmc::Error<E>) -> FsError {
    match err {
        embedded_sdmmc::Error::DeviceError(_) => FsError::IoError,
        embedded_sdmmc::Error::FormatError(_) => FsError::Corrupt,
        embedded_sdmmc::Error::NoSuchVolume => FsError::NoFilesystem,
        embedded_sdmmc::Error::FilenameError(_) => FsError::InvalidPath,
        embedded_sdmmc::Error::TooManyOpenVolumes => FsError::TooManyOpenFiles,
        embedded_sdmmc::Error::TooManyOpenDirs => FsError::TooManyOpenFiles,
        embedded_sdmmc::Error::TooManyOpenFiles => FsError::TooManyOpenFiles,
        embedded_sdmmc::Error::NotFound => FsError::NotFound,
        embedded_sdmmc::Error::FileAlreadyOpen => FsError::PermissionDenied,
        embedded_sdmmc::Error::DirAlreadyOpen => FsError::PermissionDenied,
        embedded_sdmmc::Error::OpenedDirAsFile => FsError::NotAFile,
        embedded_sdmmc::Error::OpenedFileAsDir => FsError::NotADirectory,
        embedded_sdmmc::Error::DeleteDirAsFile => FsError::NotAFile,
        embedded_sdmmc::Error::VolumeStillInUse => FsError::PermissionDenied,
        embedded_sdmmc::Error::VolumeAlreadyOpen => FsError::PermissionDenied,
        embedded_sdmmc::Error::Unsupported => FsError::Internal,
        embedded_sdmmc::Error::EndOfFile => FsError::EndOfFile,
        embedded_sdmmc::Error::BadCluster => FsError::Corrupt,
        embedded_sdmmc::Error::ConversionError => FsError::InvalidPath,
        embedded_sdmmc::Error::NotEnoughSpace => FsError::NoSpace,
        embedded_sdmmc::Error::AllocationError => FsError::NoSpace,
        embedded_sdmmc::Error::ReadOnly => FsError::PermissionDenied,
        embedded_sdmmc::Error::FileAlreadyExists => FsError::AlreadyExists,
        embedded_sdmmc::Error::BadBlockSize(_) => FsError::Corrupt,
        embedded_sdmmc::Error::InvalidOffset => FsError::InvalidPath,
        _ => FsError::Internal,
    }
}

// ============================================================================
// Open Mode
// ============================================================================

/// File open mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    Read,
    Write,
    Append,
    ReadWrite,
}

// ============================================================================
// Directory Entry
// ============================================================================

/// Directory entry information
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

// ============================================================================
// Filesystem State
// ============================================================================

static FS_INITIALIZED: Spinlock<bool> = Spinlock::new(false);

// ============================================================================
// Public API
// ============================================================================

/// Initialize the filesystem
pub fn init() -> Result<(), FsError> {
    log("[FS] Initializing filesystem...\n");

    if !block::is_initialized() {
        log("[FS] Error: Block device not initialized\n");
        return Err(FsError::BlockDeviceNotInitialized);
    }

    let result = block::with_device(|device| {
        let mut volume_mgr = VolumeManager::new(device, AkumaTimeSource);

        // Try to open volume 0
        let mut volume = match volume_mgr.open_volume(VolumeIdx(0)) {
            Ok(v) => v,
            Err(e) => {
                log("[FS] Failed to open volume: ");
                console::print(&alloc::format!("{:?}\n", e));
                return Err(convert_error(e));
            }
        };

        log("[FS] FAT volume opened successfully\n");

        // Try to open root directory to verify
        let mut root_dir = match volume.open_root_dir() {
            Ok(d) => d,
            Err(e) => {
                log("[FS] Failed to open root directory: ");
                console::print(&alloc::format!("{:?}\n", e));
                return Err(convert_error(e));
            }
        };

        log("[FS] Root directory accessible\n");

        // Count files in root
        let mut count = 0;
        root_dir
            .iterate_dir(|_entry| {
                count += 1;
            })
            .ok();

        log("[FS] Files in root: ");
        console::print(&alloc::format!("{}\n", count));

        Ok(())
    });

    match result {
        Some(Ok(())) => {
            *FS_INITIALIZED.lock() = true;
            log("[FS] Filesystem initialized\n");
            Ok(())
        }
        Some(Err(e)) => Err(e),
        None => Err(FsError::BlockDeviceNotInitialized),
    }
}

/// Check if filesystem is initialized
pub fn is_initialized() -> bool {
    *FS_INITIALIZED.lock()
}

/// List directory contents
pub fn list_dir(path: &str) -> Result<Vec<DirEntry>, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }

    block::with_device(|device| {
        let mut volume_mgr = VolumeManager::new(device, AkumaTimeSource);
        let mut volume = volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(convert_error)?;
        let mut root_dir = volume.open_root_dir().map_err(convert_error)?;

        let mut entries = Vec::new();

        if path.is_empty() || path == "/" {
            // List root directory
            root_dir
                .iterate_dir(|entry| {
                    entries.push(DirEntry {
                        name: entry.name.to_string(),
                        is_dir: entry.attributes.is_directory(),
                        size: entry.size as u64,
                    });
                })
                .map_err(convert_error)?;
        } else {
            // Navigate to subdirectory
            let path = path.trim_start_matches('/');
            let sfn = ShortFileName::create_from_str(path).map_err(|_| FsError::InvalidPath)?;
            let mut dir = root_dir.open_dir(sfn).map_err(convert_error)?;
            dir.iterate_dir(|entry| {
                entries.push(DirEntry {
                    name: entry.name.to_string(),
                    is_dir: entry.attributes.is_directory(),
                    size: entry.size as u64,
                });
            })
            .map_err(convert_error)?;
        }

        Ok(entries)
    })
    .ok_or(FsError::BlockDeviceNotInitialized)?
}

/// Read entire file contents as bytes
pub fn read_file(path: &str) -> Result<Vec<u8>, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }

    block::with_device(|device| {
        let mut volume_mgr = VolumeManager::new(device, AkumaTimeSource);
        let mut volume = volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(convert_error)?;
        let mut root_dir = volume.open_root_dir().map_err(convert_error)?;

        let path = path.trim_start_matches('/');
        let sfn = ShortFileName::create_from_str(path).map_err(|_| FsError::InvalidPath)?;

        let mut file = root_dir
            .open_file_in_dir(sfn, Mode::ReadOnly)
            .map_err(convert_error)?;

        let size = file.length() as usize;
        let mut buf = alloc::vec![0u8; size];

        let bytes_read = file.read(&mut buf).map_err(convert_error)?;
        buf.truncate(bytes_read);

        Ok(buf)
    })
    .ok_or(FsError::BlockDeviceNotInitialized)?
}

/// Read file contents as a string
pub fn read_to_string(path: &str) -> Result<String, FsError> {
    let bytes = read_file(path)?;
    String::from_utf8(bytes).map_err(|_| FsError::IoError)
}

/// Write data to a file (creates or truncates)
pub fn write_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }

    block::with_device(|device| {
        let mut volume_mgr = VolumeManager::new(device, AkumaTimeSource);
        let mut volume = volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(convert_error)?;
        let mut root_dir = volume.open_root_dir().map_err(convert_error)?;

        let path = path.trim_start_matches('/');
        let sfn = ShortFileName::create_from_str(path).map_err(|_| FsError::InvalidPath)?;

        let mut file = root_dir
            .open_file_in_dir(sfn.clone(), Mode::ReadWriteCreateOrTruncate)
            .map_err(convert_error)?;

        file.write(data).map_err(convert_error)?;

        // Close the file explicitly to ensure data is flushed
        drop(file);
        drop(root_dir);
        drop(volume);

        Ok(())
    })
    .ok_or(FsError::BlockDeviceNotInitialized)?
}

/// Append data to a file
pub fn append_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }

    block::with_device(|device| {
        let mut volume_mgr = VolumeManager::new(device, AkumaTimeSource);
        let mut volume = volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(convert_error)?;
        let mut root_dir = volume.open_root_dir().map_err(convert_error)?;

        let path = path.trim_start_matches('/');
        let sfn = ShortFileName::create_from_str(path).map_err(|_| FsError::InvalidPath)?;

        let mut file = root_dir
            .open_file_in_dir(sfn.clone(), Mode::ReadWriteCreateOrAppend)
            .map_err(convert_error)?;

        file.write(data).map_err(convert_error)?;

        // Close the file explicitly to ensure data is flushed
        drop(file);
        drop(root_dir);
        drop(volume);

        Ok(())
    })
    .ok_or(FsError::BlockDeviceNotInitialized)?
}

/// Create a directory
pub fn create_dir(path: &str) -> Result<(), FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }

    block::with_device(|device| {
        let mut volume_mgr = VolumeManager::new(device, AkumaTimeSource);
        let mut volume = volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(convert_error)?;
        let mut root_dir = volume.open_root_dir().map_err(convert_error)?;

        let path = path.trim_start_matches('/');
        let sfn = ShortFileName::create_from_str(path).map_err(|_| FsError::InvalidPath)?;

        root_dir.make_dir_in_dir(sfn).map_err(convert_error)?;

        Ok(())
    })
    .ok_or(FsError::BlockDeviceNotInitialized)?
}

/// Remove a file
pub fn remove_file(path: &str) -> Result<(), FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }

    block::with_device(|device| {
        let mut volume_mgr = VolumeManager::new(device, AkumaTimeSource);
        let mut volume = volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(convert_error)?;
        let mut root_dir = volume.open_root_dir().map_err(convert_error)?;

        let path = path.trim_start_matches('/');
        let sfn = ShortFileName::create_from_str(path).map_err(|_| FsError::InvalidPath)?;

        root_dir.delete_file_in_dir(sfn).map_err(convert_error)?;

        Ok(())
    })
    .ok_or(FsError::BlockDeviceNotInitialized)?
}

/// Remove a directory
pub fn remove_dir(path: &str) -> Result<(), FsError> {
    remove_file(path)
}

/// Check if a file or directory exists
pub fn exists(path: &str) -> bool {
    if !is_initialized() {
        return false;
    }

    block::with_device(|device| {
        let mut volume_mgr = VolumeManager::new(device, AkumaTimeSource);
        let mut volume = match volume_mgr.open_volume(VolumeIdx(0)) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let mut root_dir = match volume.open_root_dir() {
            Ok(d) => d,
            Err(_) => return false,
        };

        let path = path.trim_start_matches('/');
        if path.is_empty() {
            return true;
        }

        let sfn = match ShortFileName::create_from_str(path) {
            Ok(s) => s,
            Err(_) => return false,
        };

        let mut found = false;
        let _ = root_dir.iterate_dir(|entry| {
            if entry.name == sfn {
                found = true;
            }
        });

        found
    })
    .unwrap_or(false)
}

/// Get file size
pub fn file_size(path: &str) -> Result<u64, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }

    block::with_device(|device| {
        let mut volume_mgr = VolumeManager::new(device, AkumaTimeSource);
        let mut volume = volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(convert_error)?;
        let mut root_dir = volume.open_root_dir().map_err(convert_error)?;

        let path = path.trim_start_matches('/');
        let sfn = ShortFileName::create_from_str(path).map_err(|_| FsError::InvalidPath)?;

        let file = root_dir
            .open_file_in_dir(sfn, Mode::ReadOnly)
            .map_err(convert_error)?;

        Ok(file.length() as u64)
    })
    .ok_or(FsError::BlockDeviceNotInitialized)?
}

/// Filesystem statistics
#[derive(Debug, Clone)]
pub struct FsStats {
    pub cluster_size: u32,
    pub total_clusters: u32,
    pub free_clusters: u32,
}

impl FsStats {
    pub fn total_bytes(&self) -> u64 {
        self.total_clusters as u64 * self.cluster_size as u64
    }

    pub fn free_bytes(&self) -> u64 {
        self.free_clusters as u64 * self.cluster_size as u64
    }

    pub fn used_bytes(&self) -> u64 {
        self.total_bytes() - self.free_bytes()
    }
}

/// Get filesystem statistics
pub fn stats() -> Result<FsStats, FsError> {
    if !is_initialized() {
        return Err(FsError::NotInitialized);
    }

    let capacity = block::capacity().ok_or(FsError::BlockDeviceNotInitialized)?;
    let cluster_size = 512u32; // Sector size for basic estimation
    let total_clusters = (capacity / cluster_size as u64) as u32;
    let free_clusters = total_clusters * 9 / 10; // Rough estimate

    Ok(FsStats {
        cluster_size,
        total_clusters,
        free_clusters,
    })
}

fn log(msg: &str) {
    console::print(msg);
}
