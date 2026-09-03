//! virtio-MMIO device discovery from the boot command line.
//!
//! ```text
//!   Firecracker v1.16.1:  pci=off virtio_mmio.device=4K@0xc0001000:5
//!   QEMU microvm:         virtio_mmio.device=512@0xfeb00000:5
//! ```
//!
//! Both measured, not quoted from documentation — the first by attaching a drive
//! and printing `hvm_start_info.cmdline_paddr` on the Ryzen host, the second from
//! `info mtree`.
//!
//! This is the whole of device discovery on this machine for anything on a
//! virtio transport. Firecracker runs with `pci=off` and there is no bus to
//! enumerate; there is no device tree; and while ACPI *is* present (see
//! [`super::acpi`]) it describes the interrupt controllers and not the virtio
//! transports. The string is the only place the transport's address appears.
//!
//! # This parses attacker-adjacent input
//!
//! Every byte comes from the VMM. A malformed token is skipped, never guessed
//! at: a base address off by a nibble is a device mapping pointed at someone
//! else's memory. Nothing here allocates, panics, or saturates — an overflow is
//! a rejection.

/// The token Linux defined and both VMMs emit.
const TOKEN: &str = "virtio_mmio.device=";

/// Most devices this will report.
///
/// Matches `akuma_virtio::probe::MAX_SLOTS`, and for the same reason: it sizes a
/// fixed array so discovery allocates nothing.
pub const MAX_DEVICES: usize = 8;

/// One `virtio_mmio.device=<size>@<base>:<irq>` entry.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MmioDevice {
    /// Physical base address of the transport's register file.
    pub base: u64,
    /// Size of that register file in bytes — the slot stride on this machine.
    pub len: u64,
    /// Interrupt line (a GSI, to be routed through the IOAPIC).
    pub irq: u32,
}

/// The devices found on the command line, in the order they appeared.
#[derive(Copy, Clone)]
pub struct MmioDevices {
    devs: [MmioDevice; MAX_DEVICES],
    len: usize,
}

impl MmioDevices {
    const EMPTY: MmioDevice = MmioDevice { base: 0, len: 0, irq: 0 };

    #[must_use]
    pub const fn new() -> Self {
        Self { devs: [Self::EMPTY; MAX_DEVICES], len: 0 }
    }

    #[must_use]
    pub fn as_slice(&self) -> &[MmioDevice] {
        &self.devs[..self.len]
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The slot geometry these devices describe: `(base, stride, count)`.
    ///
    /// Drivers address virtio slots as `base + i * stride`, so a machine whose
    /// transports are not evenly spaced cannot be described that way. Both
    /// machines *are* — Firecracker 0x1000 apart, microvm 0x200 — but that is
    /// measured, not guaranteed, so it is checked here and the count falls back
    /// to 1. Believing a stride that does not hold would point slot 1 at nothing
    /// and hand a driver a page of zeroes.
    #[must_use]
    pub fn geometry(&self) -> Option<(u64, u64, usize)> {
        let first = self.as_slice().first()?;
        let stride = first.len;
        let dense = self
            .as_slice()
            .iter()
            .enumerate()
            .all(|(i, d)| d.base == first.base + (i as u64) * stride && d.len == stride);
        Some((first.base, stride, if dense { self.len } else { 1 }))
    }

    fn push(&mut self, dev: MmioDevice) {
        if self.len < MAX_DEVICES {
            self.devs[self.len] = dev;
            self.len += 1;
        }
    }
}

impl Default for MmioDevices {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse an unsigned integer, hex if it carries an `0x` prefix.
///
/// `checked_*` throughout: the string is VMM-supplied, and a 40-digit number
/// must be a rejection rather than a wrap. Rejects an empty body, so `0x` alone
/// is not read as zero.
fn parse_u64(s: &str) -> Option<u64> {
    let (digits, radix) = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(rest) => (rest, 16),
        None => (s, 10),
    };
    if digits.is_empty() {
        return None;
    }
    let mut acc: u64 = 0;
    for b in digits.bytes() {
        let d = match b {
            b'0'..=b'9' => u64::from(b - b'0'),
            b'a'..=b'f' => u64::from(b - b'a') + 10,
            b'A'..=b'F' => u64::from(b - b'A') + 10,
            _ => return None,
        };
        if d >= radix {
            return None;
        }
        acc = acc.checked_mul(radix)?.checked_add(d)?;
    }
    Some(acc)
}

/// Parse a size with an optional `K`/`M`/`G` suffix — Linux `memparse` form.
///
/// Firecracker writes `4K`; QEMU takes a plain `512`. Both spellings are the
/// same field, which is why this is not two parsers.
pub(crate) fn parse_size(s: &str) -> Option<u64> {
    let (body, shift) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 10),
        b'M' | b'm' => (&s[..s.len() - 1], 20),
        b'G' | b'g' => (&s[..s.len() - 1], 30),
        _ => (s, 0),
    };
    let n = parse_u64(body)?;
    let size = n.checked_mul(1u64.checked_shl(shift)?)?;
    if size == 0 { None } else { Some(size) }
}

/// Parse one `<size>@<base>:<irq>[:<id>]` value.
///
/// The optional trailing `:<id>` is Linux's device-id field. Accepted and
/// ignored rather than rejected — a token this kernel does not use is not a
/// malformed token.
fn parse_device(value: &str) -> Option<MmioDevice> {
    let (size, rest) = value.split_once('@')?;
    let (base, rest) = rest.split_once(':')?;
    let irq = rest.split(':').next()?;

    let len = parse_size(size)?;
    let base = parse_u64(base)?;
    let irq = parse_u64(irq)?;

    // A device at physical 0 is the null page, not a transport. An IRQ that does
    // not fit a GSI is not one either.
    if base == 0 || irq > u64::from(u32::MAX) {
        return None;
    }
    // The register file must not wrap the address space.
    base.checked_add(len)?;

    Some(MmioDevice { base, len, irq: irq as u32 })
}

/// Find every `virtio_mmio.device=` entry on `cmdline`.
///
/// Tokens are whitespace-separated; unrecognised ones (`pci=off`, `console=…`)
/// are skipped, and a malformed `virtio_mmio.device=` is skipped rather than
/// guessed at.
#[must_use]
pub fn parse(cmdline: &str) -> MmioDevices {
    let mut out = MmioDevices::new();
    for token in cmdline.split_ascii_whitespace() {
        if let Some(value) = token.strip_prefix(TOKEN)
            && let Some(dev) = parse_device(value)
        {
            out.push(dev);
        }
    }
    out
}
