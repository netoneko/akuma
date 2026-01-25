//! Foreign function interface types for akuma
//!
//! Since akuma is pure Rust, OsStr/OsString are just wrappers around str/String

use alloc::borrow::{Borrow, ToOwned};
use alloc::string::String;
use core::ops::Deref;

/// Platform-specific string slice
#[repr(transparent)]
pub struct OsStr {
    inner: str,
}

impl OsStr {
    /// Create from str
    pub fn new<S: AsRef<str> + ?Sized>(s: &S) -> &OsStr {
        unsafe { &*(s.as_ref() as *const str as *const OsStr) }
    }

    /// Convert to string slice (always succeeds on akuma)
    pub fn to_str(&self) -> Option<&str> {
        Some(&self.inner)
    }

    /// Convert to lossy string (no-op on akuma)
    pub fn to_string_lossy(&self) -> alloc::borrow::Cow<'_, str> {
        alloc::borrow::Cow::Borrowed(&self.inner)
    }

    /// Convert to owned OsString
    pub fn to_os_string(&self) -> OsString {
        OsString { inner: self.inner.to_owned() }
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Get length
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Get bytes
    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_bytes()
    }
}

impl AsRef<OsStr> for OsStr {
    fn as_ref(&self) -> &OsStr {
        self
    }
}

impl AsRef<OsStr> for str {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(self)
    }
}

impl AsRef<OsStr> for String {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(self)
    }
}

impl ToOwned for OsStr {
    type Owned = OsString;

    fn to_owned(&self) -> OsString {
        self.to_os_string()
    }
}

impl core::fmt::Debug for OsStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", &self.inner)
    }
}

impl core::fmt::Display for OsStr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", &self.inner)
    }
}

impl PartialEq for OsStr {
    fn eq(&self, other: &OsStr) -> bool {
        self.inner == other.inner
    }
}

impl Eq for OsStr {}

impl PartialEq<str> for OsStr {
    fn eq(&self, other: &str) -> bool {
        &self.inner == other
    }
}

/// Platform-specific owned string
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct OsString {
    inner: String,
}

impl OsString {
    /// Create empty OsString
    pub fn new() -> OsString {
        OsString { inner: String::new() }
    }

    /// Get as OsStr
    pub fn as_os_str(&self) -> &OsStr {
        OsStr::new(&self.inner)
    }

    /// Convert to String (always succeeds on akuma)
    pub fn into_string(self) -> Result<String, OsString> {
        Ok(self.inner)
    }

    /// Push a string
    pub fn push<T: AsRef<OsStr>>(&mut self, s: T) {
        self.inner.push_str(&s.as_ref().inner);
    }

    /// Get capacity
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Reserve capacity
    pub fn reserve(&mut self, additional: usize) {
        self.inner.reserve(additional);
    }

    /// Reserve exact capacity
    pub fn reserve_exact(&mut self, additional: usize) {
        self.inner.reserve_exact(additional);
    }

    /// Shrink to fit
    pub fn shrink_to_fit(&mut self) {
        self.inner.shrink_to_fit();
    }

    /// Clear
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Get length
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

impl Deref for OsString {
    type Target = OsStr;

    fn deref(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl AsRef<OsStr> for OsString {
    fn as_ref(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl From<String> for OsString {
    fn from(s: String) -> OsString {
        OsString { inner: s }
    }
}

impl From<&str> for OsString {
    fn from(s: &str) -> OsString {
        OsString { inner: s.to_owned() }
    }
}

impl From<&OsStr> for OsString {
    fn from(s: &OsStr) -> OsString {
        s.to_os_string()
    }
}

impl core::fmt::Debug for OsString {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", &self.inner)
    }
}

impl core::fmt::Display for OsString {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", &self.inner)
    }
}

impl Borrow<OsStr> for OsString {
    fn borrow(&self) -> &OsStr {
        self.as_os_str()
    }
}

/// C string slice (null-terminated)
#[repr(transparent)]
pub struct CStr {
    inner: [u8],
}

impl CStr {
    /// Create from bytes with nul terminator
    pub unsafe fn from_ptr<'a>(ptr: *const i8) -> &'a CStr {
        let mut len = 0;
        while *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = core::slice::from_raw_parts(ptr as *const u8, len + 1);
        &*(slice as *const [u8] as *const CStr)
    }

    /// Convert to str
    pub fn to_str(&self) -> Result<&str, core::str::Utf8Error> {
        core::str::from_utf8(self.to_bytes())
    }

    /// Get bytes without nul
    pub fn to_bytes(&self) -> &[u8] {
        &self.inner[..self.inner.len() - 1]
    }

    /// Get bytes with nul
    pub fn to_bytes_with_nul(&self) -> &[u8] {
        &self.inner
    }
}

/// Owned C string
pub struct CString {
    inner: alloc::vec::Vec<u8>,
}

impl CString {
    /// Create new CString
    pub fn new<T: Into<alloc::vec::Vec<u8>>>(t: T) -> Result<CString, NulError> {
        let mut bytes = t.into();
        if bytes.contains(&0) {
            return Err(NulError(()));
        }
        bytes.push(0);
        Ok(CString { inner: bytes })
    }

    /// Get as CStr
    pub fn as_c_str(&self) -> &CStr {
        unsafe { &*(&self.inner[..] as *const [u8] as *const CStr) }
    }

    /// Get pointer
    pub fn as_ptr(&self) -> *const i8 {
        self.inner.as_ptr() as *const i8
    }

    /// Into bytes with nul
    pub fn into_bytes_with_nul(self) -> alloc::vec::Vec<u8> {
        self.inner
    }
}

impl Deref for CString {
    type Target = CStr;

    fn deref(&self) -> &CStr {
        self.as_c_str()
    }
}

/// Error type for CString creation
#[derive(Debug)]
pub struct NulError(());

impl core::fmt::Display for NulError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "nul byte found in data")
    }
}

// C type aliases (for compatibility)
#[allow(non_camel_case_types)]
pub type c_char = i8;
#[allow(non_camel_case_types)]
pub type c_int = i32;
#[allow(non_camel_case_types)]
pub type c_uint = u32;
#[allow(non_camel_case_types)]
pub type c_long = i64;
#[allow(non_camel_case_types)]
pub type c_ulong = u64;
#[allow(non_camel_case_types)]
pub type c_void = core::ffi::c_void;
