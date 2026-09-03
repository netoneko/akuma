//! Device discovery from the boot command line.
//!
//! This is the amd64 counterpart of `akuma-fdt`, and the machine gives it far
//! less to work with. x86_64 Firecracker passes **no device tree**, and it runs
//! with `pci=off` — there is no bus to enumerate. What there is, is a string:
//!
//! ```text
//!   Firecracker v1.16.1:  pci=off virtio_mmio.device=4K@0xc0001000:5
//!   QEMU microvm:         virtio_mmio.device=512@0xfeb00000:5   (we pass it)
//! ```
//!
//! Both measured, not quoted from documentation — the first by attaching a drive
//! and printing `hvm_start_info.cmdline_paddr`, the second from `info mtree`.
//!
//! The two differ in exactly the way
//! `docs/archive/AKUMA_FIRECRACKER_AMD64.md` records for the aarch64 machines:
//! Firecracker gives each device its own 4 KiB page, QEMU packs eight slots
//! 0x200 apart. `akuma-virtio` already took both at runtime, so nothing in the
//! driver had to learn about this.
//!
//! # Why the command line and not ACPI
//!
//! ACPI would also answer the question, and is where a general-purpose kernel
//! looks. It is further away than it appears here: `hvm_start_info.rsdp_paddr`
//! is **0 on both machines** (measured — see the archive doc §3.6), so the root
//! pointer would have to be found by scanning the BIOS area, and the tables then
//! parsed. The command line is one string, both VMMs write it, and it carries
//! the interrupt number too. ACPI becomes necessary for the IOAPIC, and that is
//! the stage where it should be paid for.
//!
//! # This parses attacker-adjacent input
//!
//! Every byte comes from the VMM. A malformed token is skipped, never guessed
//! at: a base address off by a nibble is a device mapping pointed at someone
//! else's memory, and `0` returned for a size would make the slot-array
//! arithmetic degenerate. Nothing here can panic on input, and nothing saturates
//! — overflow is a rejection.

use akuma_selftest::Suite;

/// The token Linux defined and both VMMs emit.
const TOKEN: &str = "virtio_mmio.device=";

/// Most devices this kernel will look at.
///
/// Matches `akuma_virtio::probe::MAX_SLOTS`, and for the same reason: it sizes a
/// fixed array so discovery allocates nothing. Firecracker's own limit is
/// higher; a machine with more devices than this gets the first eight.
pub const MAX_DEVICES: usize = 8;

/// One `virtio_mmio.device=<size>@<base>:<irq>` entry.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct MmioDevice {
    /// Physical base address of the transport's register file.
    pub base: u64,
    /// Size of that register file in bytes — the slot stride on this machine.
    pub len: u64,
    /// Interrupt line. Recorded but unused: there is no IOAPIC yet, so the block
    /// driver polls. Parsed anyway because dropping a field that is present is
    /// how the next stage discovers it has to re-parse.
    pub irq: u32,
}

/// The devices found on the command line, in the order they appeared.
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
fn parse_size(s: &str) -> Option<u64> {
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
    // Anything after a second colon is the device id.
    let irq = rest.split(':').next()?;

    let len = parse_size(size)?;
    let base = parse_u64(base)?;
    let irq = parse_u64(irq)?;

    // A device at physical 0 is the null page, not a transport. An IRQ that
    // does not fit a vector number is not one either.
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
pub fn parse_virtio_mmio(cmdline: &str) -> MmioDevices {
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

/// Parse the two command lines the two machines actually produce, plus the ways
/// a malformed one must not be believed.
///
/// The accepted cases are **measured strings**, pasted from a boot log and from
/// `info mtree`, not invented. That is what makes this a test of the parser
/// rather than a test of a guess about the format: if Firecracker changes its
/// spelling, this keeps passing and the boot stops finding a disk — so the real
/// discovery is also checked, live, by `blk::smoke_test`.
pub fn smoke_test(t: &mut Suite) {
    // Firecracker v1.16.1, one drive attached. Measured 2026-09-04.
    let fc = parse_virtio_mmio("pci=off virtio_mmio.device=4K@0xc0001000:5");
    t.check_eq("cmdline: firecracker device count", fc.len() as u64, 1);
    if let Some(d) = fc.as_slice().first() {
        t.check_eq("cmdline: firecracker base", d.base, 0xc000_1000);
        t.check_eq("cmdline: firecracker len (4K)", d.len, 4096);
        t.check_eq("cmdline: firecracker irq", u64::from(d.irq), 5);
    }

    // QEMU microvm, the line `amd64/run.sh` passes. Base measured via `info mtree`.
    let qemu = parse_virtio_mmio("virtio_mmio.device=512@0xfeb00000:5");
    t.check_eq("cmdline: qemu device count", qemu.len() as u64, 1);
    if let Some(d) = qemu.as_slice().first() {
        t.check_eq("cmdline: qemu base", d.base, 0xfeb0_0000);
        t.check_eq("cmdline: qemu len (512)", d.len, 512);
    }

    // Several devices, and the order must be preserved: slot index is position,
    // so a parser that reordered would map the right pages to the wrong slots.
    let many = parse_virtio_mmio(
        "virtio_mmio.device=4K@0xd0000000:5 virtio_mmio.device=4K@0xd0001000:6:2",
    );
    t.check_eq("cmdline: multiple devices", many.len() as u64, 2);
    t.check_eq(
        "cmdline: order preserved",
        many.as_slice().get(1).map_or(0, |d| d.base),
        0xd000_1000,
    );
    t.check_eq(
        "cmdline: trailing :id ignored, not rejected",
        many.as_slice().get(1).map_or(0, |d| u64::from(d.irq)),
        6,
    );

    // Suffixes are the same field spelled three ways.
    t.check_eq("cmdline: 1M suffix", parse_size("1M").unwrap_or(0), 1 << 20);
    t.check_eq("cmdline: 2G suffix", parse_size("2G").unwrap_or(0), 2 << 30);
    t.check_eq("cmdline: lowercase 4k", parse_size("4k").unwrap_or(0), 4096);

    // Rejections. Each is a value that would otherwise become a device mapping
    // pointed somewhere it should not be.
    //
    // Labels are spelled out rather than built from a loop variable: `Suite`
    // takes a `&str` and this kernel does not `format!` on a console path.
    let bad: [(&str, &str); 8] = [
        ("cmdline: rejects a token with no @", "virtio_mmio.device=4K"),
        ("cmdline: rejects a token with no irq", "virtio_mmio.device=4K@0xc0001000"),
        ("cmdline: rejects a device at physical 0", "virtio_mmio.device=4K@0x0:5"),
        ("cmdline: rejects a zero-length register file", "virtio_mmio.device=0@0xc0001000:5"),
        ("cmdline: rejects a non-numeric base", "virtio_mmio.device=4K@0xzzzz:5"),
        ("cmdline: rejects a bare 0x prefix", "virtio_mmio.device=4K@0x:5"),
        ("cmdline: rejects a base that overflows u64", "virtio_mmio.device=4K@99999999999999999999:5"),
        ("cmdline: rejects an empty value", "virtio_mmio.device="),
    ];
    for (label, line) in bad {
        t.check_eq(label, parse_virtio_mmio(line).len() as u64, 0);
    }

    // A command line with nothing on it is not an error, it is a machine with no
    // virtio devices — which is what `"drives": []` produces.
    t.check_eq(
        "cmdline: no devices is not a failure",
        parse_virtio_mmio("pci=off console=ttyS0").len() as u64,
        0,
    );
}
