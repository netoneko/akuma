//! The smallest ACPI a Linux guest needs to find one virtio-MMIO device while
//! keeping its legacy 8259/8254.
//!
//! A kernel built without `VIRTIO_MMIO_CMDLINE_DEVICES` (Firecracker's CI
//! kernels are) learns about a virtio-mmio device from exactly one place: an
//! ACPI namespace device with `_HID "LNRO0005"` whose `_CRS` is a 32-bit memory
//! window and an interrupt. This module builds that, as **hardware-reduced**
//! ACPI (FADT flag 20, ACPI 5.0+): no PM1/GPE register blocks, no SCI, no
//! firmware-controlled sleep — the namespace is the whole interface.
//!
//! **Not** hardware-reduced, deliberately. The first version set FADT flag 20,
//! and the guest's log showed the consequence: `nr_irqs: 8, preallocated irqs: 0`
//! where a non-ACPI boot says `24` and `16`. A hardware-reduced platform has no
//! legacy PIC or PIT by definition, Linux drops both, no IRQ 5 descriptor exists,
//! and the device's `_CRS` interrupt comes back disabled (`virtio-mmio
//! LNRO0005:00: error -ENXIO: IRQ index 0 not found`). So the FADT is a
//! full-hardware one with a PM1 block at [`PM1_EVT_PORT`] / [`PM1_CNT_PORT`] —
//! [`Pm1`] is the three registers behind it — and no PM timer, no GPE blocks, no
//! `SMI_CMD` (a zero `SMI_CMD` means ACPI mode is already enabled).
//!
//! Layout in one 4-KiB page, each table 16-byte aligned:
//!
//! | offset  | table |
//! |---------|-------|
//! | `0x000` | RSDP, revision 2 (36 bytes): points at the XSDT only |
//! | `0x040` | XSDT: the FADT and the MADT |
//! | `0x080` | FADT, revision 6 (276 bytes): `X_DSDT`, PM1 block, no timer/GPE |
//! | `0x1A0` | MADT, revision 5 (52 bytes): `PCAT_COMPAT` (dual 8259s), one local APIC |
//! | `0x200` | DSDT, revision 2: `Scope(\_SB) { Device(VIO0) { _HID, _UID, _CRS } }` |
//!
//! Sources: the ACPI specification (UEFI Forum), section 5.2.5 (RSDP), 5.2.6
//! (system description table header), 5.2.8 (XSDT), 5.2.9 (FADT, table 5-34),
//! 5.2.9.3 (`IAPC_BOOT_ARCH`), 4.8.3 (PM1 status/enable/control) and 20 (AML encoding: `DefScope`, `DefDevice`,
//! `DefName`, `PkgLength`, `DefBuffer`), 6.4.2.1 (IRQ descriptor), 6.4.3.4
//! (Memory32Fixed). `IAPC_BOOT_ARCH` keeps `LEGACY_DEVICES` set: without
//! it a guest assumes there is no PIC or PIT and installs null ones, and the
//! interrupt fabric this hypervisor emulates would never be used.

/// Page-relative offsets of the tables.
pub const RSDP_OFF: usize = 0x000;
/// See [`RSDP_OFF`].
pub const XSDT_OFF: usize = 0x040;
/// See [`RSDP_OFF`].
pub const FADT_OFF: usize = 0x080;
/// See [`RSDP_OFF`].
pub const MADT_OFF: usize = 0x1A0;
/// `sizeof(MADT)` as built: header, two words, one 8-byte local APIC entry.
pub const MADT_LEN: usize = madt_len(1);
/// Most processors the MADT page has room for (`MADT_OFF` to `DSDT_OFF`).
pub const MAX_CPUS: usize = (DSDT_OFF - MADT_OFF - 44) / 8;

/// The MADT's length with `cpus` processor-local-APIC entries of 8 bytes after
/// its 44-byte header.
#[must_use]
pub const fn madt_len(cpus: usize) -> usize {
    44 + 8 * cpus
}
/// See [`RSDP_OFF`].
pub const DSDT_OFF: usize = 0x200;
/// `sizeof(FADT)` for revision 6.
pub const FADT_LEN: usize = 276;
/// A page.
pub const PAGE: usize = 4096;
/// Bytes of the `_CRS` resource template.
const CRS_LEN: usize = 17;

/// I/O port of the PM1a event block (status word, then enable word).
pub const PM1_EVT_PORT: u16 = 0x600;
/// I/O port of the PM1a control block.
pub const PM1_CNT_PORT: u16 = 0x604;
/// First port past the PM1 block.
pub const PM1_END: u16 = 0x606;
/// The system control interrupt line the FADT names (never raised).
pub const SCI_IRQ: u16 = 9;

/// The PM1a registers behind [`PM1_EVT_PORT`]..[`PM1_END`].
///
/// Status (write 1 to clear), enable, and control. No event ever sets a status
/// bit; `SCI_EN` reads 1 (ACPI mode is always on) and a write of `SLP_EN` is
/// ignored — the guest cannot power the host off through it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pm1 {
    status: u16,
    enable: u16,
    control: u16,
}

/// `PM1_CNT.SCI_EN`.
const SCI_EN: u16 = 1;
/// Control bits a guest may set: `SCI_EN` is forced, `SLP_TYP`/`SLP_EN`
/// (bits 10-13) are dropped.
const CNT_WRITABLE: u16 = 0x0206; // BM_RLD, GBL_RLS, ... (bits 1, 2, 9)

impl Pm1 {
    /// Reset state: ACPI mode on, nothing pending or enabled.
    #[must_use]
    pub const fn new() -> Self {
        Self { status: 0, enable: 0, control: SCI_EN }
    }

    /// Whether `port` is one of ours.
    #[must_use]
    pub const fn owns(port: u16) -> bool {
        port >= PM1_EVT_PORT && port < PM1_END
    }

    fn word(&self, off: u16) -> u16 {
        match off {
            0 => self.status,
            2 => self.enable,
            4 => self.control,
            _ => 0,
        }
    }

    /// A read of `size` bytes (1, 2 or 4) at `port`.
    #[must_use]
    pub fn read(&self, port: u16, size: u8) -> u32 {
        let off = port - PM1_EVT_PORT;
        let mut v = 0u32;
        for k in 0..u16::from(size.min(4)) {
            let o = off + k;
            if o >= PM1_END - PM1_EVT_PORT {
                break;
            }
            let w = self.word(o & !1);
            let byte = if o & 1 == 0 { w as u8 } else { (w >> 8) as u8 };
            v |= u32::from(byte) << (8 * k);
        }
        v
    }

    /// A write of `size` bytes of `val` at `port`.
    pub fn write(&mut self, port: u16, size: u8, val: u32) {
        let off = port - PM1_EVT_PORT;
        for k in 0..u16::from(size.min(4)) {
            let o = off + k;
            if o >= PM1_END - PM1_EVT_PORT {
                break;
            }
            let byte = (val >> (8 * k)) as u8;
            let shift = 8 * (o & 1);
            let mask = 0xFFu16 << shift;
            let b16 = u16::from(byte) << shift;
            match o & !1 {
                // Write-1-to-clear.
                0 => self.status &= !b16,
                2 => self.enable = (self.enable & !mask) | b16,
                4 => self.control = (((self.control & !mask) | b16) & CNT_WRITABLE) | SCI_EN,
                _ => {}
            }
        }
    }
}

/// One virtio-MMIO device to declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioMmio {
    /// Guest-physical base of the register window.
    pub base: u32,
    /// Window length in bytes.
    pub len: u32,
    /// Interrupt line (ACPI global system interrupt = ISA IRQ with PICs only).
    pub irq: u32,
}

fn put32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

/// Fill in the 36-byte header of the table at `b[..len]` and its checksum.
fn header(b: &mut [u8], sig: &[u8; 4], len: usize, revision: u8) {
    b[..4].copy_from_slice(sig);
    put32(b, 4, len as u32);
    b[8] = revision;
    b[9] = 0;
    b[10..16].copy_from_slice(b"AKUMA ");
    b[16..24].copy_from_slice(b"KVMTEST ");
    put32(b, 24, 1);
    b[28..32].copy_from_slice(b"AKMA");
    put32(b, 32, 1);
    b[9] = checksum_byte(&b[..len]);
}

/// The byte that makes `bytes` (with its checksum field zero) sum to zero.
fn checksum_byte(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |a, &x| a.wrapping_add(x)))
}

/// A cursor over a fixed buffer, for the AML.
struct Out<'a> {
    b: &'a mut [u8],
    n: usize,
}

impl Out<'_> {
    fn put(&mut self, bytes: &[u8]) {
        self.b[self.n..self.n + bytes.len()].copy_from_slice(bytes);
        self.n += bytes.len();
    }

    /// `PkgLength` for a package whose contents (everything after the length
    /// field) are `content` bytes: the encoded length counts the field itself.
    fn pkg_length(&mut self, content: usize) {
        if content < 0x3F {
            self.put(&[(content + 1) as u8]);
        } else {
            // Two bytes: low nibble in bits 3:0 of the first, the rest in the second.
            let total = content + 2;
            self.put(&[0x40 | (total & 0xF) as u8, (total >> 4) as u8]);
        }
    }
}

/// The `_CRS` resource template for `dev`: `Memory32Fixed` then the small `IRQ`
/// descriptor (an ISA line, edge-triggered, active-high, exclusive — the shape
/// a legacy-PIC platform's devices use) and the end tag.
fn crs_resources(dev: &VirtioMmio) -> [u8; CRS_LEN] {
    let mut r = [0u8; CRS_LEN];
    // Memory32Fixed (large item 0x06): length 9, ReadWrite, base, length.
    r[0] = 0x86;
    r[1] = 9;
    r[2] = 0;
    r[3] = 1;
    r[4..8].copy_from_slice(&dev.base.to_le_bytes());
    r[8..12].copy_from_slice(&dev.len.to_le_bytes());
    // IRQ (small item 0x04, length 2): a 16-bit mask of the lines it may use.
    r[12] = 0x22;
    r[13..15].copy_from_slice(&(1u16 << (dev.irq & 15)).to_le_bytes());
    // End tag (small item 0x0F, length 1), checksum byte 0 = "ignore".
    r[15] = 0x79;
    r[16] = 0;
    r
}

/// Write the AML body of the DSDT at `out`; returns its length.
fn aml(out: &mut Out<'_>, dev: &VirtioMmio) {
    let res = crs_resources(dev);
    // Name(_HID, "LNRO0005") = 08 _HID 0D "LNRO0005" 00.
    const HID: usize = 1 + 4 + 1 + 8 + 1;
    // Name(_UID, 0) = 08 _UID 00 (ZeroOp).
    const UID: usize = 1 + 4 + 1;
    // Name(_CRS, Buffer(n){...}) = 08 _CRS 11 PkgLength 0A n <res>.
    let buf_content = 2 + res.len(); // ByteConst BufferSize + the data
    let buf_pkg = if buf_content < 0x3F { 1 } else { 2 };
    let crs = 1 + 4 + 1 + buf_pkg + buf_content;
    // Device(VIO0) = 5B 82 PkgLength "VIO0" ...
    let dev_content = 4 + HID + UID + crs;
    let dev_pkg = if dev_content < 0x3F { 1 } else { 2 };
    let device = 2 + dev_pkg + dev_content;
    // Scope(\_SB_) = 10 PkgLength 5C "_SB_" ...
    let scope_content = 5 + device;

    out.put(&[0x10]);
    out.pkg_length(scope_content);
    out.put(&[0x5C, b'_', b'S', b'B', b'_']);
    out.put(&[0x5B, 0x82]);
    out.pkg_length(dev_content);
    out.put(b"VIO0");
    out.put(&[0x08, b'_', b'H', b'I', b'D', 0x0D]);
    out.put(b"LNRO0005");
    out.put(&[0x00]);
    out.put(&[0x08, b'_', b'U', b'I', b'D', 0x00]);
    out.put(&[0x08, b'_', b'C', b'R', b'S', 0x11]);
    out.pkg_length(buf_content);
    out.put(&[0x0A, res.len() as u8]);
    out.put(&res);
}

/// Build the tables for one virtio-MMIO device into `page`.
///
/// `page` is one zeroed page that will live at guest-physical `page_gpa`.
/// Returns the RSDP's guest-physical address (the page's start), or `None` if
/// `page` is too small.
pub fn build(page: &mut [u8], page_gpa: u64, dev: &VirtioMmio) -> Option<u64> {
    build_smp(page, page_gpa, dev, 1)
}

/// [`build`] with `cpus` processors in the MADT: ACPI processor id and APIC id
/// both `0..cpus`, all enabled. `None` for zero or more than [`MAX_CPUS`].
pub fn build_smp(page: &mut [u8], page_gpa: u64, dev: &VirtioMmio, cpus: usize) -> Option<u64> {
    if page.len() < PAGE || cpus == 0 || cpus > MAX_CPUS {
        return None;
    }
    page[..PAGE].fill(0);

    // DSDT first: the FADT only needs its address.
    let aml_len = {
        let mut out = Out { b: &mut page[DSDT_OFF + 36..PAGE], n: 0 };
        aml(&mut out, dev);
        out.n
    };
    let dsdt_len = 36 + aml_len;
    header(&mut page[DSDT_OFF..DSDT_OFF + dsdt_len], b"DSDT", dsdt_len, 2);

    // FADT.
    {
        let f = &mut page[FADT_OFF..FADT_OFF + FADT_LEN];
        put32(f, 40, (page_gpa + DSDT_OFF as u64) as u32); // DSDT
        // SCI_INT (offset 46); never raised.
        f[46..48].copy_from_slice(&SCI_IRQ.to_le_bytes());
        // PM1a event and control blocks (offsets 56, 64) and their lengths
        // (88, 89): status+enable words, one control word.
        put32(f, 56, u32::from(PM1_EVT_PORT));
        put32(f, 64, u32::from(PM1_CNT_PORT));
        f[88] = 4;
        f[89] = 2;
        // IAPC_BOOT_ARCH (offset 109): LEGACY_DEVICES (bit 0); no 8042 (bit 1),
        // VGA not present (bit 2), no MSI (bit 3), PCIe ASPM not controllable
        // (bit 4), no CMOS RTC (bit 5) — there is a CMOS, but it reads zero.
        f[109] = 0b0011_1100 | 1;
        // Flags: WBINVD works (0), no power button (4), no sleep button (5),
        // no fixed-feature RTC (6). NOT HW_REDUCED_ACPI (20): see the module docs.
        put32(f, 112, 1 | (1 << 4) | (1 << 5) | (1 << 6));
        f[131] = 3; // FADT minor version (6.3)
        put64(f, 140, page_gpa + DSDT_OFF as u64); // X_DSDT
        header(f, b"FACP", FADT_LEN, 6);
    }

    // MADT: PCAT_COMPAT says two 8259s are present; each processor-local APIC
    // entry (type 0, length 8: ACPI id n, APIC id n, enabled) is a CPU.
    {
        let len = madt_len(cpus);
        let m = &mut page[MADT_OFF..MADT_OFF + len];
        put32(m, 36, 0xFEE0_0000); // local interrupt controller address
        put32(m, 40, 1); // flags: PCAT_COMPAT
        for n in 0..cpus {
            let e = 44 + 8 * n;
            m[e] = 0; // entry type: processor local APIC
            m[e + 1] = 8; // entry length
            m[e + 2] = n as u8; // ACPI processor id
            m[e + 3] = n as u8; // APIC id
            put32(m, e + 4, 1); // flags: enabled
        }
        header(m, b"APIC", len, 5);
    }

    // XSDT: header + two 64-bit pointers.
    {
        let x = &mut page[XSDT_OFF..XSDT_OFF + 36 + 16];
        put64(x, 36, page_gpa + FADT_OFF as u64);
        put64(x, 44, page_gpa + MADT_OFF as u64);
        header(x, b"XSDT", 36 + 16, 1);
    }

    // RSDP v2.
    {
        let r = &mut page[RSDP_OFF..RSDP_OFF + 36];
        r[..8].copy_from_slice(b"RSD PTR ");
        r[9..15].copy_from_slice(b"AKUMA ");
        r[15] = 2; // revision
        put32(r, 16, 0); // RsdtAddress: none
        put32(r, 20, 36); // Length
        put64(r, 24, page_gpa + XSDT_OFF as u64);
        r[8] = checksum_byte(&r[..20]);
        r[32] = checksum_byte(&r[..36]);
    }
    Some(page_gpa + RSDP_OFF as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GPA: u64 = 0x07FF_C000;
    const DEV: VirtioMmio = VirtioMmio { base: 0xD000_0000, len: 0x1000, irq: 5 };

    fn built() -> [u8; PAGE] {
        let mut p = [0u8; PAGE];
        assert_eq!(build(&mut p, GPA, &DEV), Some(GPA));
        p
    }

    fn sum(b: &[u8]) -> u8 {
        b.iter().fold(0u8, |a, &x| a.wrapping_add(x))
    }

    fn le32(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }

    fn le64(b: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }

    /// Walk RSDP -> XSDT -> FADT -> DSDT the way a consumer does, using only
    /// the pointers in the tables.
    #[test]
    #[allow(clippy::many_single_char_names)]
    fn the_chain_of_pointers_and_checksums_holds() {
        let p = built();
        let at = |gpa: u64| (gpa - GPA) as usize;
        // RSDP: signature, both checksums, revision 2, length 36.
        assert_eq!(&p[..8], b"RSD PTR ");
        assert_eq!(sum(&p[..20]), 0, "RSDP v1 checksum");
        assert_eq!(sum(&p[..36]), 0, "RSDP extended checksum");
        assert_eq!(p[15], 2);
        assert_eq!(le32(&p, 20), 36);
        assert_eq!(le32(&p, 16), 0, "no RSDT");
        // XSDT.
        let x = at(le64(&p, 24));
        assert_eq!(&p[x..x + 4], b"XSDT");
        let xlen = le32(&p, x + 4) as usize;
        assert_eq!(xlen, 52);
        assert_eq!(sum(&p[x..x + xlen]), 0);
        // MADT, second entry.
        let m = at(le64(&p, x + 44));
        assert_eq!(&p[m..m + 4], b"APIC");
        assert_eq!(le32(&p, m + 4) as usize, MADT_LEN);
        assert_eq!(sum(&p[m..m + MADT_LEN]), 0);
        assert_eq!(le32(&p, m + 40) & 1, 1, "PCAT_COMPAT");
        // FADT.
        let f = at(le64(&p, x + 36));
        assert_eq!(&p[f..f + 4], b"FACP");
        assert_eq!(le32(&p, f + 4) as usize, FADT_LEN);
        assert_eq!(p[f + 8], 6);
        assert_eq!(sum(&p[f..f + FADT_LEN]), 0);
        assert_eq!(le32(&p, f + 112) & (1 << 20), 0, "NOT hardware-reduced: that drops the PIC");
        assert_eq!(le32(&p, f + 56), u32::from(PM1_EVT_PORT));
        assert_eq!(le32(&p, f + 64), u32::from(PM1_CNT_PORT));
        assert_eq!((p[f + 88], p[f + 89]), (4, 2));
        assert_eq!(le32(&p, f + 48), 0, "no SMI_CMD: ACPI mode is already on");
        assert_eq!(p[f + 109] & 1, 1, "legacy devices present");
        // Both DSDT pointers agree and land on a DSDT.
        let d = at(le64(&p, f + 140));
        assert_eq!(u64::from(le32(&p, f + 40)), le64(&p, f + 140));
        assert_eq!(&p[d..d + 4], b"DSDT");
        let dlen = le32(&p, d + 4) as usize;
        assert_eq!(sum(&p[d..d + dlen]), 0);
        assert_eq!(p[d + 8], 2);
    }

    /// Decode a `PkgLength` the way the AML grammar says: top two bits of the
    /// first byte give the count of following bytes.
    fn pkg(b: &[u8]) -> (usize, usize) {
        let follow = usize::from(b[0] >> 6);
        if follow == 0 {
            return (usize::from(b[0] & 0x3F), 1);
        }
        let mut len = usize::from(b[0] & 0x0F);
        for i in 0..follow {
            len |= usize::from(b[1 + i]) << (4 + 8 * i);
        }
        (len, 1 + follow)
    }

    /// The AML: Scope(\_SB_) { Device(VIO0) { _HID, _UID, _CRS } }, each
    /// package's length field spanning exactly its contents.
    #[test]
    #[allow(clippy::many_single_char_names)]
    fn the_aml_nests_exactly_and_carries_the_device() {
        let p = built();
        let d = DSDT_OFF;
        let dlen = le32(&p, d + 4) as usize;
        let aml = &p[d + 36..d + dlen];
        // Scope.
        assert_eq!(aml[0], 0x10);
        let (slen, sf) = pkg(&aml[1..]);
        assert_eq!(1 + slen, aml.len(), "the scope fills the table");
        assert_eq!(&aml[1 + sf..1 + sf + 5], &[0x5C, b'_', b'S', b'B', b'_']);
        // Device.
        let dev = &aml[1 + sf + 5..];
        assert_eq!(&dev[..2], &[0x5B, 0x82]);
        let (devlen, df) = pkg(&dev[2..]);
        assert_eq!(2 + devlen, dev.len(), "the device fills the scope");
        let body = &dev[2 + df..];
        assert_eq!(&body[..4], b"VIO0");
        // _HID "LNRO0005".
        assert_eq!(&body[4..10], &[0x08, b'_', b'H', b'I', b'D', 0x0D]);
        assert_eq!(&body[10..19], b"LNRO0005\0");
        // _UID 0.
        assert_eq!(&body[19..25], &[0x08, b'_', b'U', b'I', b'D', 0x00]);
        // _CRS: a buffer whose package length covers its size and data.
        assert_eq!(&body[25..31], &[0x08, b'_', b'C', b'R', b'S', 0x11]);
        let (blen, bf) = pkg(&body[31..]);
        let buf = &body[31 + bf..];
        assert_eq!(blen - bf, buf.len(), "the buffer ends the device");
        assert_eq!(&buf[..2], &[0x0A, CRS_LEN as u8], "BufferSize = ByteConst");
        let res = &buf[2..];
        assert_eq!(res.len(), CRS_LEN);
        // Memory32Fixed: 86 09 00 | rw | base | length.
        assert_eq!(&res[..4], &[0x86, 9, 0, 1]);
        assert_eq!(le32(res, 4), 0xD000_0000);
        assert_eq!(le32(res, 8), 0x1000);
        // IRQ: 22 | mask of line 5.
        assert_eq!(&res[12..15], &[0x22, 0x20, 0x00]);
        // End tag.
        assert_eq!(&res[15..], &[0x79, 0]);
    }

    #[test]
    fn the_package_length_encoder_uses_one_byte_below_64_and_two_above() {
        let mut b = [0u8; 8];
        let mut o = Out { b: &mut b, n: 0 };
        o.pkg_length(10);
        assert_eq!(&o.b[..o.n], &[11]);
        o.n = 0;
        o.pkg_length(0x3E); // 0x3E + 1 = 0x3F still fits
        assert_eq!(&o.b[..o.n], &[0x3F]);
        o.n = 0;
        o.pkg_length(0x3F); // would need 0x40: two bytes, total counts both
        assert_eq!(o.n, 2);
        assert_eq!(pkg(&o.b[..2]).0, 0x3F + 2);
    }

    #[test]
    fn pm1_reports_acpi_mode_and_clears_status_by_writing_ones() {
        let mut pm = Pm1::new();
        // SCI_EN reads 1 from reset, as a word and as its low byte.
        assert_eq!(pm.read(PM1_CNT_PORT, 2), 1);
        assert_eq!(pm.read(PM1_CNT_PORT, 1), 1);
        // Enable is plain storage, byte- or word-wide.
        pm.write(PM1_EVT_PORT + 2, 2, 0x0521);
        assert_eq!(pm.read(PM1_EVT_PORT + 2, 2), 0x0521);
        assert_eq!(pm.read(PM1_EVT_PORT, 4), 0x0521_0000, "status 0, enable above it");
        pm.write(PM1_EVT_PORT + 3, 1, 0);
        assert_eq!(pm.read(PM1_EVT_PORT + 2, 2), 0x0021);
        // Status is write-1-to-clear (nothing sets it; poke via the private field).
        pm.status = 0x8101;
        pm.write(PM1_EVT_PORT, 2, 0x0100);
        assert_eq!(pm.read(PM1_EVT_PORT, 2), 0x8001);
        // Control: SCI_EN cannot be cleared and SLP_EN (bit 13) is dropped.
        pm.write(PM1_CNT_PORT, 2, 0x2400);
        assert_eq!(pm.read(PM1_CNT_PORT, 2), 1);
        pm.write(PM1_CNT_PORT, 2, 0x0004);
        assert_eq!(pm.read(PM1_CNT_PORT, 2), 5);
        // Ports outside the block are not ours.
        assert!(Pm1::owns(0x600) && Pm1::owns(0x605));
        assert!(!Pm1::owns(0x5FF) && !Pm1::owns(0x606));
    }

    #[test]
    fn the_madt_lists_every_cpu_with_its_apic_id() {
        let mut p = [0u8; PAGE];
        assert_eq!(build_smp(&mut p, GPA, &DEV, 3), Some(GPA));
        let m = MADT_OFF;
        assert_eq!(le32(&p, m + 4) as usize, madt_len(3));
        assert_eq!(sum(&p[m..m + madt_len(3)]), 0);
        for n in 0..3 {
            let e = m + 44 + 8 * n;
            assert_eq!(&p[e..e + 4], &[0, 8, n as u8, n as u8]);
            assert_eq!(le32(&p, e + 4), 1);
        }
        // The table ends where the length says: nothing of a longer table remains.
        assert_eq!(p[m + madt_len(3)], 0);
        // The DSDT after it is untouched by the longest MADT.
        assert_eq!(build_smp(&mut p, GPA, &DEV, MAX_CPUS), Some(GPA));
        assert_eq!(&p[DSDT_OFF..DSDT_OFF + 4], b"DSDT");
        assert_eq!(build_smp(&mut p, GPA, &DEV, 0), None);
        assert_eq!(build_smp(&mut p, GPA, &DEV, MAX_CPUS + 1), None);
    }

    #[test]
    fn a_short_page_is_refused() {
        let mut p = [0u8; PAGE - 1];
        assert_eq!(build(&mut p, GPA, &DEV), None);
    }
}
