//! VirtIO Sound Output Driver
//!
//! Wraps `virtio_drivers`' `VirtIOSound` device to expose a single PCM **output**
//! stream to userspace via `/dev/dsp` (OSS-style). Modeled structurally on
//! `src/block.rs`: probe the shared 8-slot virtio-mmio scan for device id 25,
//! build an `MmioTransport`, and drive the crate.
//!
//! Memory model: DMA stays bounded. `pcm_xfer` shares the caller's PCM buffer in
//! place (no megabyte-sized device buffer), and we pick small period/buffer sizes
//! so the driver fits the 4 MB `extreme` budget if the `sound` feature is opted
//! back in there. The userspace player (`wavplay`) streams the file through a
//! small fixed buffer, so end-to-end RAM is independent of song length.
//!
//! I/O model: polling (the crate's `pcm_xfer` is blocking), matching block/rng.
//! The whole module is gated behind the `sound` Cargo feature; when off, the
//! public API degrades to "not available" stubs so the syscall layer compiles
//! and `/dev/dsp` simply does not open.

// ============================================================================
// Audio error (shared by both feature states)
// ============================================================================

/// Audio device error type
#[derive(Debug, Clone, Copy)]
// With the `sound` feature off, the stub API only ever yields `NotInitialized`;
// the other variants describe real-driver failures and would be dead code.
#[cfg_attr(not(feature = "sound"), allow(dead_code))]
pub enum AudioError {
    /// Device not found on the bus
    NotFound,
    /// Device not initialized (feature off, or init failed/absent)
    NotInitialized,
    /// Underlying virtio I/O error
    IoError,
    /// Unsupported / invalid parameter (format, rate, channels)
    InvalidParam,
}

impl core::fmt::Display for AudioError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound => write!(f, "Sound device not found"),
            Self::NotInitialized => write!(f, "Sound device not initialized"),
            Self::IoError => write!(f, "Sound I/O error"),
            Self::InvalidParam => write!(f, "Invalid audio parameter"),
        }
    }
}

// ============================================================================
// OSS ioctl constants + format codes (shared; userspace mirrors these)
// ============================================================================

/// `SNDCTL_DSP_SPEED` — set/return sample rate (Hz). Arg: *mut i32.
pub const SNDCTL_DSP_SPEED: u32 = 0xC0045002;
/// `SNDCTL_DSP_SETFMT` — set/return sample format. Arg: *mut i32 (AFMT_*).
pub const SNDCTL_DSP_SETFMT: u32 = 0xC0045005;
/// `SNDCTL_DSP_CHANNELS` — set/return channel count. Arg: *mut i32.
pub const SNDCTL_DSP_CHANNELS: u32 = 0xC0045006;

/// OSS format: signed 16-bit little-endian PCM.
// Consumed only by the real driver (sound on); part of the documented ABI.
#[cfg_attr(not(feature = "sound"), allow(dead_code))]
pub const AFMT_S16_LE: i32 = 0x00000010;
/// OSS format: unsigned 8-bit PCM.
#[cfg_attr(not(feature = "sound"), allow(dead_code))]
pub const AFMT_U8: i32 = 0x00000008;

// ============================================================================
// Feature ON: real driver
// ============================================================================

#[cfg(feature = "sound")]
mod imp {
    use super::{AudioError, AFMT_S16_LE, AFMT_U8};
    use core::cell::UnsafeCell;

    use spinning_top::Spinlock;
    use virtio_drivers::device::sound::{PcmFeatures, PcmFormat, PcmRate, VirtIOSound};
    use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

    use crate::console;
    use crate::virtio_hal::VirtioHal;

    /// QEMU virt machine virtio MMIO addresses (remapped via L0[1]); same scan
    /// table as block.rs / rng.rs.
    const VIRTIO_MMIO_ADDRS: [usize; 8] = [
        akuma_exec::mmu::DEV_VIRTIO_VA,
        akuma_exec::mmu::DEV_VIRTIO_VA + 0x200,
        akuma_exec::mmu::DEV_VIRTIO_VA + 0x400,
        akuma_exec::mmu::DEV_VIRTIO_VA + 0x600,
        akuma_exec::mmu::DEV_VIRTIO_VA + 0x800,
        akuma_exec::mmu::DEV_VIRTIO_VA + 0xa00,
        akuma_exec::mmu::DEV_VIRTIO_VA + 0xc00,
        akuma_exec::mmu::DEV_VIRTIO_VA + 0xe00,
    ];

    /// VirtIO device ID for sound devices.
    const VIRTIO_DEVICE_ID_SOUND: u32 = 25;

    /// Bytes per PCM period. Small + bounded so total in-flight DMA stays well
    /// under ~128 KB even with the crate's 32-entry tx queue. This is the kernel's
    /// contract for `wavplay`'s read-buffer size.
    const PERIOD_BYTES: u32 = 8 * 1024;
    /// Ring buffer size advertised to the device: a few periods.
    const BUFFER_BYTES: u32 = PERIOD_BYTES * 4;

    /// Cached PCM parameters set via ioctl, applied lazily on first `play`.
    #[derive(Clone, Copy)]
    struct Params {
        channels: u8,
        format: PcmFormat,
        rate: PcmRate,
    }

    impl Default for Params {
        fn default() -> Self {
            Self { channels: 2, format: PcmFormat::S16, rate: PcmRate::Rate44100 }
        }
    }

    /// VirtIO sound device wrapper with interior mutability.
    ///
    /// Like `VirtioBlockDevice`, all access is serialized through the global
    /// `SOUND_DEVICE` Spinlock, so the `UnsafeCell`s are sound.
    pub struct VirtioSoundDevice {
        inner: UnsafeCell<VirtIOSound<VirtioHal, MmioTransport>>,
        out_stream: u32,
        params: UnsafeCell<Params>,
        /// True once set_params+prepare+start have run for the current params.
        prepared: UnsafeCell<bool>,
    }

    // SAFETY: only accessed under the SOUND_DEVICE Spinlock (exclusive access).
    unsafe impl Sync for VirtioSoundDevice {}

    impl VirtioSoundDevice {
        #[inline]
        #[allow(clippy::mut_from_ref)]
        fn inner_mut(&self) -> &mut VirtIOSound<VirtioHal, MmioTransport> {
            unsafe { &mut *self.inner.get() }
        }

        /// Update one cached PCM field; invalidates the prepared stream so the
        /// next `play` re-applies the full parameter set. OSS apps set format,
        /// channels, and rate via independent ioctls, so each is applied alone
        /// against the retained defaults.
        fn update_params<F: FnOnce(&mut Params)>(&self, f: F) {
            unsafe {
                f(&mut *self.params.get());
                *self.prepared.get() = false;
            }
        }

        /// Ensure the stream is configured + started for the current params.
        fn ensure_prepared(&self) -> Result<(), AudioError> {
            if unsafe { *self.prepared.get() } {
                return Ok(());
            }
            let p = unsafe { *self.params.get() };
            let dev = self.inner_mut();
            dev.pcm_set_params(
                self.out_stream,
                BUFFER_BYTES,
                PERIOD_BYTES,
                PcmFeatures::empty(),
                p.channels,
                p.format,
                p.rate,
            )
            .map_err(|_| AudioError::InvalidParam)?;
            dev.pcm_prepare(self.out_stream).map_err(|_| AudioError::IoError)?;
            dev.pcm_start(self.out_stream).map_err(|_| AudioError::IoError)?;
            unsafe {
                *self.prepared.get() = true;
            }
            Ok(())
        }

        /// Play (blocking) a buffer of PCM frames matching the current params.
        fn play(&self, frames: &[u8]) -> Result<usize, AudioError> {
            if frames.is_empty() {
                return Ok(0);
            }
            self.ensure_prepared()?;
            self.inner_mut()
                .pcm_xfer(self.out_stream, frames)
                .map_err(|_| AudioError::IoError)?;
            Ok(frames.len())
        }

        /// Stop + release the stream so the next file starts clean.
        fn stop(&self) {
            if unsafe { *self.prepared.get() } {
                let dev = self.inner_mut();
                let _ = dev.pcm_stop(self.out_stream);
                let _ = dev.pcm_release(self.out_stream);
                unsafe {
                    *self.prepared.get() = false;
                }
            }
        }
    }

    static SOUND_DEVICE: Spinlock<Option<VirtioSoundDevice>> = Spinlock::new(None);

    /// Initialize the sound driver: scan for virtio-snd (id 25), pick the first
    /// output stream. Non-fatal: returns Err if no device / no output stream.
    pub fn init() -> Result<(), AudioError> {
        for (i, &addr) in VIRTIO_MMIO_ADDRS.iter().enumerate() {
            // SAFETY: reading MMIO device-id register at a known QEMU address.
            let device_id = unsafe { core::ptr::read_volatile((addr + 0x008) as *const u32) };
            if device_id != VIRTIO_DEVICE_ID_SOUND {
                continue;
            }

            crate::safe_print!(48, "[SND] Found virtio-snd at slot {}\n", i);

            let header_ptr = match core::ptr::NonNull::new(addr as *mut VirtIOHeader) {
                Some(p) => p,
                None => continue,
            };

            // SAFETY: building MmioTransport over a verified virtio device header.
            let transport = if let Ok(t) = unsafe { MmioTransport::new(header_ptr) } { t } else {
                console::print("[SND] Failed to create transport\n");
                continue;
            };

            let mut snd = if let Ok(s) = VirtIOSound::<VirtioHal, MmioTransport>::new(transport) { s } else {
                console::print("[SND] Failed to init virtio-snd device\n");
                continue;
            };

            let outputs = if let Ok(v) = snd.output_streams() { v } else {
                console::print("[SND] Failed to query output streams\n");
                continue;
            };

            let out_stream = if let Some(&s) = outputs.first() { s } else {
                console::print("[SND] No output streams advertised\n");
                continue;
            };

            crate::safe_print!(
                96,
                "[SND] jacks={} streams={} chmaps={} output_stream={}\n",
                snd.jacks(),
                snd.streams(),
                snd.chmaps(),
                out_stream
            );

            let device = VirtioSoundDevice {
                inner: UnsafeCell::new(snd),
                out_stream,
                params: UnsafeCell::new(Params::default()),
                prepared: UnsafeCell::new(false),
            };
            *SOUND_DEVICE.lock() = Some(device);
            return Ok(());
        }

        Err(AudioError::NotFound)
    }

    /// True once a sound device has been found and initialized.
    pub fn is_available() -> bool {
        SOUND_DEVICE.lock().is_some()
    }

    /// OSS `SNDCTL_DSP_SETFMT`: set sample format from an AFMT_* code.
    pub fn set_format_oss(fmt: i32) -> Result<(), AudioError> {
        let format = match fmt {
            AFMT_S16_LE => PcmFormat::S16,
            AFMT_U8 => PcmFormat::U8,
            _ => return Err(AudioError::InvalidParam),
        };
        let guard = SOUND_DEVICE.lock();
        let dev = guard.as_ref().ok_or(AudioError::NotInitialized)?;
        dev.update_params(|p| p.format = format);
        Ok(())
    }

    /// OSS `SNDCTL_DSP_CHANNELS`: set channel count.
    pub fn set_channels(channels: i32) -> Result<(), AudioError> {
        if !(1..=8).contains(&channels) {
            return Err(AudioError::InvalidParam);
        }
        let guard = SOUND_DEVICE.lock();
        let dev = guard.as_ref().ok_or(AudioError::NotInitialized)?;
        dev.update_params(|p| p.channels = channels as u8);
        Ok(())
    }

    /// OSS `SNDCTL_DSP_SPEED`: set sample rate in Hz.
    pub fn set_rate(rate_hz: i32) -> Result<(), AudioError> {
        let rate = match rate_hz {
            8000 => PcmRate::Rate8000,
            11025 => PcmRate::Rate11025,
            16000 => PcmRate::Rate16000,
            22050 => PcmRate::Rate22050,
            32000 => PcmRate::Rate32000,
            44100 => PcmRate::Rate44100,
            48000 => PcmRate::Rate48000,
            _ => return Err(AudioError::InvalidParam),
        };
        let guard = SOUND_DEVICE.lock();
        let dev = guard.as_ref().ok_or(AudioError::NotInitialized)?;
        dev.update_params(|p| p.rate = rate);
        Ok(())
    }

    /// Play (blocking) a buffer of PCM frames matching the current params.
    pub fn play(frames: &[u8]) -> Result<usize, AudioError> {
        let guard = SOUND_DEVICE.lock();
        let dev = guard.as_ref().ok_or(AudioError::NotInitialized)?;
        dev.play(frames)
    }

    /// Stop + release the current stream (called on `/dev/dsp` close).
    pub fn stop() {
        let guard = SOUND_DEVICE.lock();
        if let Some(dev) = guard.as_ref() {
            dev.stop();
        }
    }
}

// ============================================================================
// Feature OFF: stubs so the syscall layer still compiles
// ============================================================================

#[cfg(not(feature = "sound"))]
mod imp {
    use super::AudioError;

    pub fn init() -> Result<(), AudioError> {
        Err(AudioError::NotInitialized)
    }
    pub fn is_available() -> bool {
        false
    }
    pub fn set_format_oss(_fmt: i32) -> Result<(), AudioError> {
        Err(AudioError::NotInitialized)
    }
    pub fn set_channels(_channels: i32) -> Result<(), AudioError> {
        Err(AudioError::NotInitialized)
    }
    pub fn set_rate(_rate_hz: i32) -> Result<(), AudioError> {
        Err(AudioError::NotInitialized)
    }
    pub fn play(_frames: &[u8]) -> Result<usize, AudioError> {
        Err(AudioError::NotInitialized)
    }
    pub fn stop() {}
}

pub use imp::{init, is_available, play, set_channels, set_format_oss, set_rate, stop};
