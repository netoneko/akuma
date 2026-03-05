//! TLS RNG Adapter
//!
//! Wraps the `VirtIO` RNG for use with embedded-tls.
//! Implements `rand_core::RngCore` and `CryptoRng` traits required by TLS.

use rand_core::{CryptoRng, RngCore};

use crate::runtime::runtime;

/// RNG adapter for TLS operations
///
/// Dispatches to the kernel's RNG via registered runtime function pointers.
pub struct TlsRng;

impl TlsRng {
    /// Create a new TLS RNG adapter
    #[must_use] 
    pub const fn new() -> Self {
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
        (runtime().rng_fill)(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        // rand_core 0.6 doesn't have Error::new, just return Ok or panic
        self.fill_bytes(dest);
        Ok(())
    }
}

// Mark as cryptographically secure
impl CryptoRng for TlsRng {}
