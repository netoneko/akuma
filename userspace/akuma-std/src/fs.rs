//! File system operations for akuma

use alloc::string::String;
use alloc::vec::Vec;
use crate::io::{self, Read, Write, Error, ErrorKind, Result};
use crate::path::Path;

/// A reference to an open file on the filesystem
pub struct File {
    fd: i32,
}

impl File {
    /// Opens a file in read-only mode
    pub fn open<P: AsRef<Path>>(path: P) -> Result<File> {
        let path_str = path.as_ref().as_str();
        let fd = libakuma::open(path_str, libakuma::open_flags::O_RDONLY);
        if fd < 0 {
            Err(Error::from_raw_os_error(-fd))
        } else {
            Ok(File { fd })
        }
    }

    /// Opens a file in write-only mode, creating it if needed
    pub fn create<P: AsRef<Path>>(path: P) -> Result<File> {
        let path_str = path.as_ref().as_str();
        let flags = libakuma::open_flags::O_WRONLY 
            | libakuma::open_flags::O_CREAT 
            | libakuma::open_flags::O_TRUNC;
        let fd = libakuma::open(path_str, flags);
        if fd < 0 {
            Err(Error::from_raw_os_error(-fd))
        } else {
            Ok(File { fd })
        }
    }

    /// Returns file metadata
    pub fn metadata(&self) -> Result<Metadata> {
        match libakuma::fstat(self.fd) {
            Ok(stat) => Ok(Metadata { size: stat.st_size as u64 }),
            Err(e) => Err(Error::from_raw_os_error(e)),
        }
    }
}

impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = libakuma::read_fd(self.fd, buf);
        if n < 0 {
            Err(Error::from_raw_os_error((-n) as i32))
        } else {
            Ok(n as usize)
        }
    }
}

impl Write for File {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let n = libakuma::write_fd(self.fd, buf);
        if n < 0 {
            Err(Error::from_raw_os_error((-n) as i32))
        } else {
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        libakuma::close(self.fd);
    }
}

/// Metadata about a file
pub struct Metadata {
    size: u64,
}

impl Metadata {
    pub fn len(&self) -> u64 {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    pub fn is_file(&self) -> bool {
        true // Simplified
    }

    pub fn is_dir(&self) -> bool {
        false // Simplified
    }
}

/// Reads the entire contents of a file into a string
pub fn read_to_string<P: AsRef<Path>>(path: P) -> Result<String> {
    let mut file = File::open(path)?;
    let mut string = String::new();
    file.read_to_string(&mut string)?;
    Ok(string)
}

/// Reads the entire contents of a file into a bytes vector
pub fn read<P: AsRef<Path>>(path: P) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Writes a slice as the entire contents of a file
pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(contents.as_ref())?;
    Ok(())
}

/// Creates a new directory
pub fn create_dir<P: AsRef<Path>>(path: P) -> Result<()> {
    let result = libakuma::mkdir(path.as_ref().as_str());
    if result < 0 {
        Err(Error::from_raw_os_error(-result))
    } else {
        Ok(())
    }
}

/// Creates a directory and all parent directories
pub fn create_dir_all<P: AsRef<Path>>(path: P) -> Result<()> {
    if libakuma::mkdir_p(path.as_ref().as_str()) {
        Ok(())
    } else {
        Err(Error::new(ErrorKind::Other, "failed to create directory"))
    }
}

/// Returns an iterator over the entries in a directory
pub fn read_dir<P: AsRef<Path>>(path: P) -> Result<ReadDir> {
    match libakuma::read_dir(path.as_ref().as_str()) {
        Some(inner) => Ok(ReadDir { inner }),
        None => Err(Error::new(ErrorKind::NotFound, "directory not found")),
    }
}

/// Iterator over directory entries
pub struct ReadDir {
    inner: libakuma::ReadDir,
}

impl Iterator for ReadDir {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|entry| Ok(DirEntry { 
            name: entry.name,
            is_dir: entry.is_dir,
        }))
    }
}

/// Entry in a directory
pub struct DirEntry {
    name: String,
    is_dir: bool,
}

impl DirEntry {
    pub fn path(&self) -> crate::path::PathBuf {
        crate::path::PathBuf::from(self.name.clone())
    }

    pub fn file_name(&self) -> &str {
        &self.name
    }

    pub fn file_type(&self) -> Result<FileType> {
        Ok(FileType { is_dir: self.is_dir })
    }
}

/// File type (file vs directory)
pub struct FileType {
    is_dir: bool,
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_file(&self) -> bool {
        !self.is_dir
    }

    pub fn is_symlink(&self) -> bool {
        false
    }
}

/// Options for opening files
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    pub fn open<P: AsRef<Path>>(&self, path: P) -> Result<File> {
        let mut flags = 0u32;
        
        if self.read && self.write {
            flags |= libakuma::open_flags::O_RDWR;
        } else if self.write {
            flags |= libakuma::open_flags::O_WRONLY;
        } else {
            flags |= libakuma::open_flags::O_RDONLY;
        }

        if self.append {
            flags |= libakuma::open_flags::O_APPEND;
        }
        if self.truncate {
            flags |= libakuma::open_flags::O_TRUNC;
        }
        if self.create {
            flags |= libakuma::open_flags::O_CREAT;
        }

        let path_str = path.as_ref().as_str();
        let fd = libakuma::open(path_str, flags);
        if fd < 0 {
            Err(Error::from_raw_os_error(-fd))
        } else {
            Ok(File { fd })
        }
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}
