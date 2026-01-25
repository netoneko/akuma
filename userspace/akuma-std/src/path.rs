//! Path handling for akuma

use alloc::borrow::{Borrow, ToOwned};
use alloc::string::String;
use core::ops::Deref;

/// A slice of a path (borrowed)
#[repr(transparent)]
pub struct Path {
    inner: str,
}

impl Path {
    /// Create a Path from a string slice
    pub fn new<S: AsRef<str> + ?Sized>(s: &S) -> &Path {
        unsafe { &*(s.as_ref() as *const str as *const Path) }
    }

    /// Returns the path as a string slice
    pub fn as_str(&self) -> &str {
        &self.inner
    }

    /// Returns the path as an OS string
    pub fn as_os_str(&self) -> &crate::ffi::OsStr {
        crate::ffi::OsStr::new(&self.inner)
    }

    /// Convert to owned PathBuf
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(self.inner.to_owned())
    }

    /// Check if path is absolute
    pub fn is_absolute(&self) -> bool {
        self.inner.starts_with('/')
    }

    /// Check if path is relative
    pub fn is_relative(&self) -> bool {
        !self.is_absolute()
    }

    /// Get the parent path
    pub fn parent(&self) -> Option<&Path> {
        if self.inner.is_empty() || &self.inner == "/" {
            None
        } else {
            let trimmed = self.inner.trim_end_matches('/');
            match trimmed.rfind('/') {
                Some(idx) if idx == 0 => Some(Path::new("/")),
                Some(idx) => Some(Path::new(&trimmed[..idx])),
                None => Some(Path::new("")),
            }
        }
    }

    /// Get the file name component
    pub fn file_name(&self) -> Option<&str> {
        let trimmed = self.inner.trim_end_matches('/');
        if trimmed.is_empty() {
            None
        } else {
            match trimmed.rfind('/') {
                Some(idx) => Some(&trimmed[idx + 1..]),
                None => Some(trimmed),
            }
        }
    }

    /// Get file stem (file name without extension)
    pub fn file_stem(&self) -> Option<&str> {
        self.file_name().map(|name| {
            match name.rfind('.') {
                Some(idx) if idx > 0 => &name[..idx],
                _ => name,
            }
        })
    }

    /// Get extension
    pub fn extension(&self) -> Option<&str> {
        self.file_name().and_then(|name| {
            match name.rfind('.') {
                Some(idx) if idx > 0 && idx < name.len() - 1 => Some(&name[idx + 1..]),
                _ => None,
            }
        })
    }

    /// Check if path starts with another path
    pub fn starts_with<P: AsRef<Path>>(&self, base: P) -> bool {
        self.inner.starts_with(base.as_ref().as_str())
    }

    /// Check if path ends with another path
    pub fn ends_with<P: AsRef<Path>>(&self, child: P) -> bool {
        self.inner.ends_with(child.as_ref().as_str())
    }

    /// Join with another path
    pub fn join<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        let other = path.as_ref();
        if other.is_absolute() {
            other.to_path_buf()
        } else {
            let mut buf = self.to_path_buf();
            buf.push(path);
            buf
        }
    }

    /// Returns path with extension replaced
    pub fn with_extension<S: AsRef<str>>(&self, extension: S) -> PathBuf {
        let mut buf = self.to_path_buf();
        buf.set_extension(extension);
        buf
    }

    /// Returns an iterator over path components
    pub fn components(&self) -> Components<'_> {
        Components { path: &self.inner, pos: 0 }
    }

    /// Check if file exists
    pub fn exists(&self) -> bool {
        crate::fs::File::open(self).is_ok()
    }

    /// Check if path is a file
    pub fn is_file(&self) -> bool {
        crate::fs::File::open(self)
            .and_then(|f| f.metadata())
            .map(|m| m.is_file())
            .unwrap_or(false)
    }

    /// Check if path is a directory
    pub fn is_dir(&self) -> bool {
        crate::fs::read_dir(self).is_ok()
    }

    /// Display the path
    pub fn display(&self) -> Display<'_> {
        Display { path: self }
    }
}

impl AsRef<Path> for Path {
    fn as_ref(&self) -> &Path {
        self
    }
}

impl AsRef<Path> for str {
    fn as_ref(&self) -> &Path {
        Path::new(self)
    }
}

impl AsRef<Path> for String {
    fn as_ref(&self) -> &Path {
        Path::new(self)
    }
}

impl ToOwned for Path {
    type Owned = PathBuf;

    fn to_owned(&self) -> PathBuf {
        self.to_path_buf()
    }
}

impl core::fmt::Debug for Path {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", &self.inner)
    }
}

impl core::fmt::Display for Path {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", &self.inner)
    }
}

/// Path display helper
pub struct Display<'a> {
    path: &'a Path,
}

impl core::fmt::Display for Display<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.path.as_str())
    }
}

/// An owned, mutable path
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PathBuf {
    inner: String,
}

impl PathBuf {
    /// Create an empty PathBuf
    pub fn new() -> PathBuf {
        PathBuf { inner: String::new() }
    }

    /// Create PathBuf with capacity
    pub fn with_capacity(capacity: usize) -> PathBuf {
        PathBuf { inner: String::with_capacity(capacity) }
    }

    /// Get as Path
    pub fn as_path(&self) -> &Path {
        Path::new(&self.inner)
    }

    /// Push a path onto this one
    pub fn push<P: AsRef<Path>>(&mut self, path: P) {
        let path = path.as_ref();
        if path.is_absolute() {
            self.inner = path.as_str().to_owned();
        } else {
            if !self.inner.is_empty() && !self.inner.ends_with('/') {
                self.inner.push('/');
            }
            self.inner.push_str(path.as_str());
        }
    }

    /// Pop the last component
    pub fn pop(&mut self) -> bool {
        match self.parent().map(|p| p.as_str().len()) {
            Some(len) => {
                self.inner.truncate(len);
                true
            }
            None => false,
        }
    }

    /// Set the file name
    pub fn set_file_name<S: AsRef<str>>(&mut self, file_name: S) {
        if self.file_name().is_some() {
            self.pop();
        }
        self.push(file_name.as_ref());
    }

    /// Set the extension
    pub fn set_extension<S: AsRef<str>>(&mut self, extension: S) -> bool {
        let ext = extension.as_ref();
        
        if let Some(file_stem) = self.file_stem().map(|s| s.to_owned()) {
            let parent_len = self.parent()
                .map(|p| p.as_str().len() + 1)
                .unwrap_or(0);
            
            self.inner.truncate(parent_len);
            self.inner.push_str(&file_stem);
            
            if !ext.is_empty() {
                self.inner.push('.');
                self.inner.push_str(ext);
            }
            true
        } else {
            false
        }
    }

    /// Convert to OS string
    pub fn into_os_string(self) -> crate::ffi::OsString {
        crate::ffi::OsString::from(self.inner)
    }

    /// Clear the path
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Reserve capacity
    pub fn reserve(&mut self, additional: usize) {
        self.inner.reserve(additional);
    }

    /// Get capacity
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }
}

impl Deref for PathBuf {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.as_path()
    }
}

impl AsRef<Path> for PathBuf {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl From<String> for PathBuf {
    fn from(s: String) -> PathBuf {
        PathBuf { inner: s }
    }
}

impl From<&str> for PathBuf {
    fn from(s: &str) -> PathBuf {
        PathBuf { inner: s.to_owned() }
    }
}

impl From<&Path> for PathBuf {
    fn from(p: &Path) -> PathBuf {
        p.to_path_buf()
    }
}

impl core::fmt::Debug for PathBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", &self.inner)
    }
}

impl core::fmt::Display for PathBuf {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", &self.inner)
    }
}

/// Iterator over path components
pub struct Components<'a> {
    path: &'a str,
    pos: usize,
}

impl<'a> Iterator for Components<'a> {
    type Item = Component<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.path.len() {
            return None;
        }

        // Skip leading slashes
        while self.pos < self.path.len() && self.path.as_bytes()[self.pos] == b'/' {
            if self.pos == 0 {
                self.pos = 1;
                return Some(Component::RootDir);
            }
            self.pos += 1;
        }

        if self.pos >= self.path.len() {
            return None;
        }

        // Find end of component
        let start = self.pos;
        while self.pos < self.path.len() && self.path.as_bytes()[self.pos] != b'/' {
            self.pos += 1;
        }

        let component = &self.path[start..self.pos];
        match component {
            "." => Some(Component::CurDir),
            ".." => Some(Component::ParentDir),
            _ => Some(Component::Normal(component)),
        }
    }
}

/// A component of a path
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Component<'a> {
    /// Root directory
    RootDir,
    /// Current directory (.)
    CurDir,
    /// Parent directory (..)
    ParentDir,
    /// Normal component
    Normal(&'a str),
}

impl<'a> Component<'a> {
    /// Extract the underlying str
    pub fn as_os_str(&self) -> &crate::ffi::OsStr {
        match self {
            Component::RootDir => crate::ffi::OsStr::new("/"),
            Component::CurDir => crate::ffi::OsStr::new("."),
            Component::ParentDir => crate::ffi::OsStr::new(".."),
            Component::Normal(s) => crate::ffi::OsStr::new(s),
        }
    }
}

impl Borrow<Path> for PathBuf {
    fn borrow(&self) -> &Path {
        self.as_path()
    }
}
