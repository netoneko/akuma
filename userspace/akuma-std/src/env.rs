//! Environment access for akuma

use alloc::string::String;
use alloc::vec::Vec;

/// Returns the arguments that this program was started with
pub fn args() -> Args {
    Args { current: 0 }
}

/// Iterator over command line arguments
pub struct Args {
    current: u32,
}

impl Iterator for Args {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= libakuma::argc() {
            None
        } else {
            let arg = libakuma::arg(self.current).map(String::from);
            self.current += 1;
            arg
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (libakuma::argc() - self.current) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Args {}

/// Returns an iterator over the arguments
pub fn args_os() -> ArgsOs {
    ArgsOs { inner: args() }
}

/// OS string arguments iterator
pub struct ArgsOs {
    inner: Args,
}

impl Iterator for ArgsOs {
    type Item = crate::ffi::OsString;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(crate::ffi::OsString::from)
    }
}

/// Gets an environment variable (stub - akuma doesn't have env vars yet)
pub fn var(key: &str) -> Result<String, VarError> {
    let _ = key;
    Err(VarError::NotPresent)
}

/// Gets an environment variable as OsString
pub fn var_os(key: &str) -> Option<crate::ffi::OsString> {
    let _ = key;
    None
}

/// Error for environment variable access
#[derive(Debug)]
pub enum VarError {
    NotPresent,
    NotUnicode,
}

impl core::fmt::Display for VarError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VarError::NotPresent => write!(f, "environment variable not found"),
            VarError::NotUnicode => write!(f, "environment variable was not unicode"),
        }
    }
}

/// Returns the current working directory (stub)
pub fn current_dir() -> crate::io::Result<crate::path::PathBuf> {
    Ok(crate::path::PathBuf::from("/"))
}

/// Sets the current working directory (stub)
pub fn set_current_dir<P: AsRef<crate::path::Path>>(path: P) -> crate::io::Result<()> {
    let _ = path;
    Ok(())
}

/// Returns an iterator over all environment variables (empty for now)
pub fn vars() -> Vars {
    Vars { done: true }
}

/// Iterator over environment variables
pub struct Vars {
    done: bool,
}

impl Iterator for Vars {
    type Item = (String, String);

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

/// OS string environment variable iterator
pub fn vars_os() -> VarsOs {
    VarsOs { inner: vars() }
}

/// OS string vars iterator
pub struct VarsOs {
    inner: Vars,
}

impl Iterator for VarsOs {
    type Item = (crate::ffi::OsString, crate::ffi::OsString);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, v)| {
            (crate::ffi::OsString::from(k), crate::ffi::OsString::from(v))
        })
    }
}
