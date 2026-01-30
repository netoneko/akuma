//! RNG Adapter for TLS
//!
//! Implements `rand_core::RngCore` and `CryptoRng` traits using the kernel's
//! GETRANDOM syscall to provide cryptographically secure random bytes for TLS.

use rand_core::{CryptoRng, RngCore};

/// RNG adapter that uses the kernel's VirtIO RNG via syscall
///
/// Implements the traits required by embedded-tls for cryptographic operations.
pub struct TlsRng;

impl TlsRng {
    /// Create a new TLS RNG adapter
    pub fn new() -> Self {
        Self
    }
}

impl Default for TlsRng {
    fn default() -> Self {
        Self::new()
    }
}

impl RngCore for TlsRng {
    fn next_u32(&mut self) -> u32 {
        let mut buf = [0u8; 4];
        self.fill_bytes(&mut buf);
        u32::from_le_bytes(buf)
    }

    fn next_u64(&mut self) -> u64 {
        let mut buf = [0u8; 8];
        self.fill_bytes(&mut buf);
        u64::from_le_bytes(buf)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        // Use kernel's getrandom syscall (max 256 bytes per call)
        let mut offset = 0;
        while offset < dest.len() {
            let chunk_size = (dest.len() - offset).min(256);
            match libakuma::getrandom(&mut dest[offset..offset + chunk_size]) {
                Ok(n) => {
                    offset += n;
                    if n == 0 {
                        // RNG returned no data - this shouldn't happen
                        panic!("TLS RNG: kernel returned 0 bytes");
                    }
                }
                Err(_) => {
                    panic!("TLS RNG: getrandom syscall failed");
                }
            }
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

// Mark as cryptographically secure
impl CryptoRng for TlsRng {}
