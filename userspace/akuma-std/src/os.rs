//! OS-specific extensions for akuma

pub mod unix {
    //! Unix-like extensions (akuma is Unix-like)
    
    pub mod ffi {
        pub use crate::ffi::*;
    }
    
    pub mod fs {
        use crate::fs::File;
        use crate::io;
        
        /// Extension trait for raw file descriptors
        pub trait FileExt {
            fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
            fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize>;
        }
        
        // Stub implementation
        impl FileExt for File {
            fn read_at(&self, _buf: &mut [u8], _offset: u64) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::Other, "not implemented"))
            }
            
            fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::Other, "not implemented"))
            }
        }
    }
    
    pub mod io {
        /// Raw file descriptor
        pub type RawFd = i32;
        
        /// Types that have a raw file descriptor
        pub trait AsRawFd {
            fn as_raw_fd(&self) -> RawFd;
        }
        
        /// Types that can be created from a raw file descriptor
        pub trait FromRawFd {
            unsafe fn from_raw_fd(fd: RawFd) -> Self;
        }
        
        /// Types that consume a raw file descriptor
        pub trait IntoRawFd {
            fn into_raw_fd(self) -> RawFd;
        }
    }
}

// Re-export as akuma-specific
pub mod akuma {
    pub use super::unix::*;
}
