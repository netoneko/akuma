//! Decoding the one kind of instruction a guest uses to touch a device.
//!
//! A nested page fault on an unmapped device window tells the hypervisor the
//! guest-physical address and nothing else: not which register, not how wide,
//! not how long the instruction was. This module reads that off the faulting
//! instruction's bytes. It decodes exactly what MMIO accessors compile to — the
//! `MOV` family — and answers `None` for everything else (string moves,
//! read-modify-write, `LOCK`, SSE/AVX), so an instruction it does not know stops
//! the run instead of being emulated wrongly.
//!
//! One non-`MOV` family is covered because compilers produce it: a volatile load
//! whose only use is a comparison is folded into the comparison, so a driver's
//! `while read_volatile(x) != y` is `cmp r, [mem]` and never a load. `38/39/3A/3B`
//! (`CMP`), `80/81/83 /7` (`CMP` with an immediate) and `84/85`, `F6/F7 /0`
//! (`TEST`) only *read* the memory operand and write nothing but the arithmetic
//! flags, so the device sees exactly one load, as with a `MOV`; the flags are
//! computed here ([`sub_flags`], [`test_flags`]) and the caller merges them into
//! `RFLAGS`. The read-modify-write ALU ops (`ADD [mem], r` and friends) would be a
//! load *and* a store and stay refused.
//!
//! Covered: `88/89/8A/8B` (byte and word/dword/qword moves, either direction),
//! `C6/C7` (store immediate), `0F B6/B7/BE/BF` (`MOVZX`/`MOVSX`), `63` (`MOVSXD`),
//! with legacy prefixes (`66`, `67`, segment overrides, `F2/F3` ignored), REX,
//! ModRM/SIB/displacement lengths in 64-bit mode (including RIP-relative).
//!
//! The vectors in the tests were assembled by clang, not written by hand: the
//! decoder's author and its oracle are different programs.

/// How a loaded value reaches the destination register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ext {
    /// Plain `MOV`: the access width is the destination width.
    None,
    /// `MOVZX`.
    Zero,
    /// `MOVSX` / `MOVSXD`.
    Sign,
}

/// The `RFLAGS` bits `CMP`/`TEST` write: CF, PF, AF, ZF, SF, OF.
pub const ARITH_FLAGS: u64 = 0x8D5;

const fn width_mask(size: u8) -> u64 {
    if size >= 8 { u64::MAX } else { (1u64 << (size * 8)) - 1 }
}

const fn flags_of_result(result: u64, size: u8) -> u64 {
    let sign = 1u64 << (size * 8 - 1);
    let mut f = 0;
    if result & width_mask(size) == 0 {
        f |= 1 << 6; // ZF
    }
    if result & sign != 0 {
        f |= 1 << 7; // SF
    }
    if (result as u8).count_ones().is_multiple_of(2) {
        f |= 1 << 2; // PF: even parity of the low byte
    }
    f
}

/// The arithmetic flags of `a - b` at `size` bytes (what `CMP a, b` leaves).
#[must_use]
pub const fn sub_flags(lhs: u64, rhs: u64, size: u8) -> u64 {
    let mask = width_mask(size);
    let (lhs, rhs) = (lhs & mask, rhs & mask);
    let diff = lhs.wrapping_sub(rhs) & mask;
    let sign = 1u64 << (size * 8 - 1);
    let mut f = flags_of_result(diff, size);
    if lhs < rhs {
        f |= 1; // CF: borrow
    }
    if (lhs ^ rhs ^ diff) & 0x10 != 0 {
        f |= 1 << 4; // AF: borrow out of bit 3
    }
    if (lhs ^ rhs) & (lhs ^ diff) & sign != 0 {
        f |= 1 << 11; // OF: signed overflow
    }
    f
}

/// The arithmetic flags of `a & b` at `size` bytes (what `TEST a, b` leaves):
/// CF and OF clear, AF cleared (it is architecturally undefined).
#[must_use]
pub const fn test_flags(lhs: u64, rhs: u64, size: u8) -> u64 {
    flags_of_result((lhs & rhs) & width_mask(size), size)
}

/// Which flag-setting operation a compare is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    /// `CMP`: flags of `lhs - rhs`.
    Sub,
    /// `TEST`: flags of `lhs & rhs`.
    And,
}

/// The operand that is not the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Other {
    /// General register `reg` (0-15).
    Reg {
        /// Register number.
        reg: u8,
        /// Register is `AH/CH/DH/BH`.
        high8: bool,
    },
    /// An immediate (sign-extended to 64 bits, masked to the width on use).
    Imm(u64),
}

/// What the instruction does with the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Read from the device into general register `reg` (0-15).
    Load {
        /// Destination register number.
        reg: u8,
        /// Register is `AH/CH/DH/BH` (byte access, no REX, `reg` 4-7).
        high8: bool,
        /// Extension applied to the value read.
        ext: Ext,
        /// Destination operand width in bytes (1, 2, 4, 8).
        dst: u8,
    },
    /// Write general register `reg` to the device.
    Store {
        /// Source register number.
        reg: u8,
        /// Register is `AH/CH/DH/BH`.
        high8: bool,
    },
    /// Write an immediate (already sign-extended to 64 bits) to the device.
    StoreImm(u64),
    /// Read the device and compare it with `other`; write only the arithmetic
    /// flags. `device_first`: the device is the left operand (`cmp [mem], r`),
    /// else the right (`cmp r, [mem]`).
    Compare {
        /// `CMP` or `TEST`.
        op: CmpOp,
        /// Device is the left operand.
        device_first: bool,
        /// The register or immediate on the other side.
        other: Other,
    },
}

/// A decoded device access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// Instruction length in bytes: how far to advance `RIP`.
    pub len: u8,
    /// Width of the device access in bytes (1, 2, 4, 8).
    pub size: u8,
    /// Direction and operand.
    pub kind: Kind,
}

impl Access {
    /// The new value of the destination register after a load of `value`
    /// (`size` bytes, from the device), given its `old` contents. A 32-bit
    /// destination zeroes the upper half; 8- and 16-bit ones leave the rest.
    #[must_use]
    pub const fn load_result(&self, old: u64, value: u64) -> u64 {
        let Kind::Load { high8, ext, dst, .. } = self.kind else { return old };
        let mask = if self.size >= 8 { u64::MAX } else { (1u64 << (self.size * 8)) - 1 };
        let raw = value & mask;
        let v = match ext {
            Ext::Sign if self.size < 8 && raw >> (self.size * 8 - 1) & 1 == 1 => raw | !mask,
            _ => raw,
        };
        match dst {
            8 => v,
            4 => v & 0xFFFF_FFFF,
            2 => (old & !0xFFFF) | (v & 0xFFFF),
            _ if high8 => (old & !0xFF00) | ((v & 0xFF) << 8),
            _ => (old & !0xFF) | (v & 0xFF),
        }
    }

    /// For a [`Kind::Compare`]: the `RFLAGS` bits to merge (within
    /// [`ARITH_FLAGS`]), given the register operand's contents `reg_value` (ignored
    /// for an immediate) and the `device` value read. Zero for other kinds.
    #[must_use]
    pub const fn compare_flags(&self, reg_value: u64, device: u64) -> u64 {
        let Kind::Compare { op, device_first, other } = self.kind else { return 0 };
        let o = match other {
            Other::Imm(v) => v,
            Other::Reg { high8: true, .. } => (reg_value >> 8) & 0xFF,
            Other::Reg { .. } => reg_value,
        };
        let (l, r) = if device_first { (device, o) } else { (o, device) };
        match op {
            CmpOp::Sub => sub_flags(l, r, self.size),
            CmpOp::And => test_flags(l, r, self.size),
        }
    }

    /// The value a store writes, given the source register's contents
    /// (truncated to the access width).
    #[must_use]
    pub const fn store_value(&self, reg_value: u64) -> u64 {
        let mask = if self.size >= 8 { u64::MAX } else { (1u64 << (self.size * 8)) - 1 };
        match self.kind {
            Kind::StoreImm(v) => v & mask,
            Kind::Store { high8: true, .. } => (reg_value >> 8) & 0xFF,
            _ => reg_value & mask,
        }
    }
}

/// Decode the instruction at the start of `code`.
#[must_use]
pub fn decode(code: &[u8]) -> Option<Access> {
    let mut i = 0usize;
    let (mut opsize16, mut rex) = (false, 0u8);
    // Legacy prefixes.
    loop {
        match *code.get(i)? {
            0x66 => opsize16 = true,
            0x67 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 | 0xF2 | 0xF3 => {}
            // LOCK and the rest: not a plain device access.
            _ => break,
        }
        i += 1;
        if i > 4 {
            return None;
        }
    }
    if code[i] & 0xF0 == 0x40 {
        rex = code[i];
        i += 1;
    }
    let w = rex & 8 != 0;
    let (r_ext, _x, _b) = (u8::from(rex & 4 != 0) << 3, rex & 2, rex & 1);
    let op = *code.get(i)?;
    i += 1;
    // (width of the device access, kind builder) per opcode.
    enum Shape {
        Mov { size: u8, load: bool },
        Imm { size: u8 },
        Ext { src: u8, ext: Ext },
        Movsxd,
        /// `CMP`/`TEST` between the device and a register.
        CmpReg { size: u8, op: CmpOp, device_first: bool },
        /// `CMP`/`TEST` between the device and an immediate of `imm` bytes,
        /// valid only for ModRM `/ext`.
        CmpImm { size: u8, op: CmpOp, ext: u8, imm: u8 },
    }
    let opwidth = if w { 8 } else if opsize16 { 2 } else { 4 };
    let shape = match op {
        0x88 => Shape::Mov { size: 1, load: false },
        0x89 => Shape::Mov { size: opwidth, load: false },
        0x8A => Shape::Mov { size: 1, load: true },
        0x8B => Shape::Mov { size: opwidth, load: true },
        0xC6 => Shape::Imm { size: 1 },
        0xC7 => Shape::Imm { size: opwidth },
        0x63 if w => Shape::Movsxd,
        0x38 => Shape::CmpReg { size: 1, op: CmpOp::Sub, device_first: true },
        0x39 => Shape::CmpReg { size: opwidth, op: CmpOp::Sub, device_first: true },
        0x3A => Shape::CmpReg { size: 1, op: CmpOp::Sub, device_first: false },
        0x3B => Shape::CmpReg { size: opwidth, op: CmpOp::Sub, device_first: false },
        0x84 => Shape::CmpReg { size: 1, op: CmpOp::And, device_first: true },
        0x85 => Shape::CmpReg { size: opwidth, op: CmpOp::And, device_first: true },
        0x80 => Shape::CmpImm { size: 1, op: CmpOp::Sub, ext: 7, imm: 1 },
        0x81 => Shape::CmpImm { size: opwidth, op: CmpOp::Sub, ext: 7, imm: if opwidth == 2 { 2 } else { 4 } },
        0x83 => Shape::CmpImm { size: opwidth, op: CmpOp::Sub, ext: 7, imm: 1 },
        0xF6 => Shape::CmpImm { size: 1, op: CmpOp::And, ext: 0, imm: 1 },
        0xF7 => Shape::CmpImm { size: opwidth, op: CmpOp::And, ext: 0, imm: if opwidth == 2 { 2 } else { 4 } },
        0x0F => {
            let op2 = *code.get(i)?;
            i += 1;
            match op2 {
                0xB6 => Shape::Ext { src: 1, ext: Ext::Zero },
                0xB7 => Shape::Ext { src: 2, ext: Ext::Zero },
                0xBE => Shape::Ext { src: 1, ext: Ext::Sign },
                0xBF => Shape::Ext { src: 2, ext: Ext::Sign },
                _ => return None,
            }
        }
        _ => return None,
    };
    // ModRM, SIB, displacement.
    let modrm = *code.get(i)?;
    i += 1;
    let (md, reg, rm) = (modrm >> 6, (modrm >> 3) & 7, modrm & 7);
    if md == 3 {
        // Register-to-register: not a memory access.
        return None;
    }
    let mut disp = match md {
        1 => 1,
        2 => 4,
        _ => 0,
    };
    if rm == 4 {
        let sib = *code.get(i)?;
        i += 1;
        if md == 0 && sib & 7 == 5 {
            disp = 4;
        }
    } else if md == 0 && rm == 5 {
        disp = 4;
    }
    i += disp;
    let regno = reg | r_ext;
    let kind_size = match shape {
        Shape::Mov { size, load: true } => (
            size,
            Kind::Load { reg: regno, high8: size == 1 && rex == 0 && (4..8).contains(&reg), ext: Ext::None, dst: size },
        ),
        Shape::Mov { size, load: false } => {
            (size, Kind::Store { reg: regno, high8: size == 1 && rex == 0 && (4..8).contains(&reg) })
        }
        Shape::Ext { src, ext } => (src, Kind::Load { reg: regno, high8: false, ext, dst: opwidth }),
        Shape::Movsxd => (4, Kind::Load { reg: regno, high8: false, ext: Ext::Sign, dst: 8 }),
        Shape::CmpReg { size, op, device_first } => (
            size,
            Kind::Compare {
                op,
                device_first,
                other: Other::Reg { reg: regno, high8: size == 1 && rex == 0 && (4..8).contains(&reg) },
            },
        ),
        Shape::CmpImm { size, op, ext, imm } => {
            if reg != ext {
                return None;
            }
            let bytes = code.get(i..i + usize::from(imm))?;
            i += usize::from(imm);
            let mut raw = 0u64;
            for (k, b) in bytes.iter().enumerate() {
                raw |= u64::from(*b) << (8 * k);
            }
            // The immediate is sign-extended to the operand size (`83` always
            // from a byte; `81`/`F7` from their 16- or 32-bit field).
            let bits = u32::from(imm) * 8;
            if bits < 64 && raw >> (bits - 1) & 1 == 1 {
                raw |= !0u64 << bits;
            }
            // `F6`/`F7` TEST and `80` take the immediate at the operand width,
            // not sign-extended past it; masking to the width on use makes the
            // two spellings the same value.
            (size, Kind::Compare { op, device_first: true, other: Other::Imm(raw) })
        }
        Shape::Imm { size } => {
            // /0 only.
            if reg != 0 {
                return None;
            }
            let n = match size {
                1 => 1,
                2 => 2,
                _ => 4,
            };
            let bytes = code.get(i..i + n)?;
            i += n;
            let mut raw = 0u64;
            for (k, b) in bytes.iter().enumerate() {
                raw |= u64::from(*b) << (8 * k);
            }
            // A 64-bit store takes a sign-extended imm32; the narrower ones
            // are masked to their width by `store_value`.
            if size == 8 {
                raw = i64::from((raw as u32).cast_signed()) as u64;
            }
            (size, Kind::StoreImm(raw))
        }
    };
    if i > code.len() || i > 15 {
        return None;
    }
    Some(Access { len: i as u8, size: kind_size.0, kind: kind_size.1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (bytes as clang assembled them, kind letter, access width, register, extra)
    /// `extra` is the destination width for `Z`/`X`, the immediate for `I`,
    /// and 1 when the register is `AH/CH/DH/BH`.
    #[allow(clippy::type_complexity)]
    const VECTORS: &[(&[u8], char, u8, u8, u64)] = &[
        // movl (%rdi), %eax
        (&[0x8b, 0x07], 'L', 4, 0, 0x0),
        // movl 0x10(%rdi), %ecx
        (&[0x8b, 0x4f, 0x10], 'L', 4, 1, 0x0),
        // movl %eax, (%rdi)
        (&[0x89, 0x07], 'S', 4, 0, 0x0),
        // movl %edx, 0x100(%rbx)
        (&[0x89, 0x93, 0x00, 0x01, 0x00, 0x00], 'S', 4, 2, 0x0),
        // movl %r9d, 0x7fffffff(%rax)
        (&[0x44, 0x89, 0x88, 0xff, 0xff, 0xff, 0x7f], 'S', 4, 9, 0x0),
        // movq (%rdi), %rax
        (&[0x48, 0x8b, 0x07], 'L', 8, 0, 0x0),
        // movq %r15, 8(%rsi)
        (&[0x4c, 0x89, 0x7e, 0x08], 'S', 8, 15, 0x0),
        // movb (%rax), %al
        (&[0x8a, 0x00], 'L', 1, 0, 0x0),
        // movb %r9b, (%rdi)
        (&[0x44, 0x88, 0x0f], 'S', 1, 9, 0x0),
        // movb %sil, (%rdi)
        (&[0x40, 0x88, 0x37], 'S', 1, 6, 0x0),
        // movb %ah, (%rax)
        (&[0x88, 0x20], 'S', 1, 4, 0x1),
        // movb (%rax), %ch
        (&[0x8a, 0x28], 'L', 1, 5, 0x1),
        // movw %ax, (%rdi)
        (&[0x66, 0x89, 0x07], 'S', 2, 0, 0x0),
        // movw (%rdi), %bx
        (&[0x66, 0x8b, 0x1f], 'L', 2, 3, 0x0),
        // movzwl (%rdi), %eax
        (&[0x0f, 0xb7, 0x07], 'Z', 2, 0, 0x4),
        // movzbl (%rdi), %eax
        (&[0x0f, 0xb6, 0x07], 'Z', 1, 0, 0x4),
        // movzbl 4(%rdi,%rsi,2), %r8d
        (&[0x44, 0x0f, 0xb6, 0x44, 0x77, 0x04], 'Z', 1, 8, 0x4),
        // movzbq (%rdi), %rax
        (&[0x48, 0x0f, 0xb6, 0x07], 'Z', 1, 0, 0x8),
        // movsbl (%rdi), %eax
        (&[0x0f, 0xbe, 0x07], 'X', 1, 0, 0x4),
        // movswq (%rdi), %rax
        (&[0x48, 0x0f, 0xbf, 0x07], 'X', 2, 0, 0x8),
        // movslq (%rdi), %rax
        (&[0x48, 0x63, 0x07], 'X', 4, 0, 0x8),
        // movl $0x1234, (%rdi)
        (&[0xc7, 0x07, 0x34, 0x12, 0x00, 0x00], 'I', 4, 0, 0x1234),
        // movl $0x1234, 0x20(%rax,%rbx,4)
        (&[0xc7, 0x44, 0x98, 0x20, 0x34, 0x12, 0x00, 0x00], 'I', 4, 0, 0x1234),
        // movq $-1, (%rdi)
        (&[0x48, 0xc7, 0x07, 0xff, 0xff, 0xff, 0xff], 'I', 8, 0, 0xffffffffffffffff),
        // movb $5, 8(%rdi)
        (&[0xc6, 0x47, 0x08, 0x05], 'I', 1, 0, 0x5),
        // movw $3, (%rdi)
        (&[0x66, 0xc7, 0x07, 0x03, 0x00], 'I', 2, 0, 0x3),
        // movl (%r13), %eax
        (&[0x41, 0x8b, 0x45, 0x00], 'L', 4, 0, 0x0),
        // movl (%r12), %eax
        (&[0x41, 0x8b, 0x04, 0x24], 'L', 4, 0, 0x0),
        // movl (%rsp), %eax
        (&[0x8b, 0x04, 0x24], 'L', 4, 0, 0x0),
        // movl 16(%rip), %eax
        (&[0x8b, 0x05, 0x10, 0x00, 0x00, 0x00], 'L', 4, 0, 0x0),
        // movl %fs:(%rdi), %eax
        (&[0x64, 0x8b, 0x07], 'L', 4, 0, 0x0),
        // movl %eax, %gs:0x40(%rdi)
        (&[0x65, 0x89, 0x47, 0x40], 'S', 4, 0, 0x0),
        // movl (%edi), %eax
        (&[0x67, 0x8b, 0x07], 'L', 4, 0, 0x0),
        // movl 0x1000(,%rax,8), %edx
        (&[0x8b, 0x14, 0xc5, 0x00, 0x10, 0x00, 0x00], 'L', 4, 2, 0x0),
    ];

    #[test]
    fn decodes_what_clang_assembled() {
        for (bytes, kind, size, reg, extra) in VECTORS {
            let a = decode(bytes).unwrap_or_else(|| panic!("no decode for {bytes:02x?}"));
            assert_eq!(usize::from(a.len), bytes.len(), "length of {bytes:02x?}");
            assert_eq!(a.size, *size, "width of {bytes:02x?}");
            match (kind, a.kind) {
                ('L', Kind::Load { reg: r, high8, ext: Ext::None, dst }) => {
                    assert_eq!((r, high8, dst), (*reg, *extra == 1, *size), "{bytes:02x?}");
                }
                ('Z', Kind::Load { reg: r, ext: Ext::Zero, dst, .. })
                | ('X', Kind::Load { reg: r, ext: Ext::Sign, dst, .. }) => {
                    assert_eq!((r, u64::from(dst)), (*reg, *extra), "{bytes:02x?}");
                }
                ('S', Kind::Store { reg: r, high8 }) => {
                    assert_eq!((r, high8), (*reg, *extra == 1), "{bytes:02x?}");
                }
                ('I', Kind::StoreImm(v)) => {
                    assert_eq!(v, *extra, "{bytes:02x?}");
                }
                other => panic!("wrong shape {other:?} for {bytes:02x?}"),
            }
        }
    }

    /// (bytes as clang assembled them, access width, device is the left operand,
    /// `CMP` or `TEST`, the other operand)
    #[allow(clippy::type_complexity)]
    const COMPARES: &[(&[u8], u8, bool, CmpOp, Other)] = &[
        // cmpl (%rdi), %eax
        (&[0x3b, 0x07], 4, false, CmpOp::Sub, Other::Reg { reg: 0, high8: false }),
        // cmpl %eax, (%rdi)
        (&[0x39, 0x07], 4, true, CmpOp::Sub, Other::Reg { reg: 0, high8: false }),
        // cmpl 0xfc(%rax), %r9d — the instruction that stopped Akuma's virtio driver
        (&[0x44, 0x3b, 0x88, 0xfc, 0x00, 0x00, 0x00], 4, false, CmpOp::Sub, Other::Reg { reg: 9, high8: false }),
        // cmpq (%rdi), %rax
        (&[0x48, 0x3b, 0x07], 8, false, CmpOp::Sub, Other::Reg { reg: 0, high8: false }),
        // cmpq %rcx, 8(%rsi)
        (&[0x48, 0x39, 0x4e, 0x08], 8, true, CmpOp::Sub, Other::Reg { reg: 1, high8: false }),
        // cmpb (%rdi), %al
        (&[0x3a, 0x07], 1, false, CmpOp::Sub, Other::Reg { reg: 0, high8: false }),
        // cmpb %r9b, (%rdi)
        (&[0x44, 0x38, 0x0f], 1, true, CmpOp::Sub, Other::Reg { reg: 9, high8: false }),
        // cmpb %ah, (%rax)
        (&[0x38, 0x20], 1, true, CmpOp::Sub, Other::Reg { reg: 4, high8: true }),
        // cmpw (%rdi), %ax
        (&[0x66, 0x3b, 0x07], 2, false, CmpOp::Sub, Other::Reg { reg: 0, high8: false }),
        // cmpl $0x10, (%rdi)
        (&[0x83, 0x3f, 0x10], 4, true, CmpOp::Sub, Other::Imm(0x10)),
        // cmpl $0x12345, 4(%rdi)
        (&[0x81, 0x7f, 0x04, 0x45, 0x23, 0x01, 0x00], 4, true, CmpOp::Sub, Other::Imm(0x12345)),
        // cmpl $-1, (%rdi): imm8 sign-extended
        (&[0x83, 0x3f, 0xff], 4, true, CmpOp::Sub, Other::Imm(u64::MAX)),
        // cmpq $-2, (%rdi)
        (&[0x48, 0x83, 0x3f, 0xfe], 8, true, CmpOp::Sub, Other::Imm(u64::MAX - 1)),
        // cmpq $0x1234567, (%rdi)
        (&[0x48, 0x81, 0x3f, 0x67, 0x45, 0x23, 0x01], 8, true, CmpOp::Sub, Other::Imm(0x123_4567)),
        // cmpb $5, (%rdi)
        (&[0x80, 0x3f, 0x05], 1, true, CmpOp::Sub, Other::Imm(5)),
        // cmpw $3, (%rdi)
        (&[0x66, 0x83, 0x3f, 0x03], 2, true, CmpOp::Sub, Other::Imm(3)),
        // cmpw $0x1234, (%rdi)
        (&[0x66, 0x81, 0x3f, 0x34, 0x12], 2, true, CmpOp::Sub, Other::Imm(0x1234)),
        // testl %eax, (%rdi)
        (&[0x85, 0x07], 4, true, CmpOp::And, Other::Reg { reg: 0, high8: false }),
        // testb %al, (%rdi)
        (&[0x84, 0x07], 1, true, CmpOp::And, Other::Reg { reg: 0, high8: false }),
        // testq %rcx, (%rdi)
        (&[0x48, 0x85, 0x0f], 8, true, CmpOp::And, Other::Reg { reg: 1, high8: false }),
        // testl $0x80, (%rdi)
        (&[0xf7, 0x07, 0x80, 0x00, 0x00, 0x00], 4, true, CmpOp::And, Other::Imm(0x80)),
        // testb $1, 4(%rdi)
        (&[0xf6, 0x47, 0x04, 0x01], 1, true, CmpOp::And, Other::Imm(1)),
        // testq $0x100, (%rdi)
        (&[0x48, 0xf7, 0x07, 0x00, 0x01, 0x00, 0x00], 8, true, CmpOp::And, Other::Imm(0x100)),
    ];

    #[test]
    fn decodes_the_compares_clang_assembled() {
        for (bytes, size, device_first, op, other) in COMPARES {
            let a = decode(bytes).unwrap_or_else(|| panic!("no decode for {bytes:02x?}"));
            assert_eq!(usize::from(a.len), bytes.len(), "length of {bytes:02x?}");
            assert_eq!(a.size, *size, "width of {bytes:02x?}");
            assert_eq!(
                a.kind,
                Kind::Compare { op: *op, device_first: *device_first, other: *other },
                "{bytes:02x?}"
            );
        }
    }

    /// Real x86_64 `cmp` and `test`, executed (`scripts/kvm/flag_oracle.c`), against
    /// the flag arithmetic: 1600 operand pairs across all four widths.
    #[test]
    fn flag_arithmetic_matches_real_cmp_and_test() {
        const V: &[(u8, u64, u64, u64, u64)] = include!("mmio_flag_vectors.in");
        assert_eq!(V.len(), 1600);
        for &(size, a, b, cmp, test) in V {
            assert_eq!(sub_flags(a, b, size), cmp, "cmp {a:#x}, {b:#x} at {size}");
            assert_eq!(test_flags(a, b, size), test, "test {a:#x}, {b:#x} at {size}");
        }
    }

    #[test]
    fn compare_flags_pick_the_operand_order() {
        // cmpl %eax, (%rdi) is device - reg; cmpl (%rdi), %eax is reg - device.
        let dev_first = decode(&[0x39, 0x07]).unwrap();
        let reg_first = decode(&[0x3b, 0x07]).unwrap();
        // reg = 2, device = 1: 1 - 2 borrows (CF), 2 - 1 does not.
        assert_eq!(dev_first.compare_flags(2, 1), sub_flags(1, 2, 4));
        assert_eq!(reg_first.compare_flags(2, 1), sub_flags(2, 1, 4));
        assert_eq!(dev_first.compare_flags(2, 1) & 1, 1);
        assert_eq!(reg_first.compare_flags(2, 1) & 1, 0);
    }

    #[test]
    fn a_high_byte_register_compares_its_own_byte() {
        // cmpb %ah, (%rax): %ah of 0x1234 is 0x12.
        let a = decode(&[0x38, 0x20]).unwrap();
        assert_eq!(a.compare_flags(0x1234, 0x12), sub_flags(0x12, 0x12, 1));
        assert_eq!(a.compare_flags(0x1234, 0x12) & (1 << 6), 1 << 6, "equal: ZF");
    }

    #[test]
    fn other_kinds_have_no_compare_flags() {
        assert_eq!(decode(&[0x8b, 0x07]).unwrap().compare_flags(1, 2), 0);
    }

    #[test]
    fn read_modify_write_alu_stays_refused() {
        // add %eax,(%rdi); add (%rdi),%eax is a load into a register and could be
        // supported, but is not; sub $1,(%rdi); or %eax,(%rdi); cmpxchg; xchg.
        for code in [&[0x01u8, 0x07][..], &[0x03, 0x07], &[0x83, 0x2f, 0x01], &[0x09, 0x07], &[0x0f, 0xb1, 0x07], &[0x87, 0x07]] {
            assert!(decode(code).is_none(), "{code:02x?}");
        }
        // 0x83 with a /ext other than 7, and F7 other than /0.
        assert!(decode(&[0x83, 0x07, 0x01]).is_none());
        assert!(decode(&[0xf7, 0x17]).is_none());
    }

    #[test]
    fn trailing_bytes_after_the_instruction_are_ignored() {
        // `movl (%rdi), %eax` followed by the next instruction.
        let a = decode(&[0x8b, 0x07, 0x48, 0x89, 0xc3]).unwrap();
        assert_eq!(a.len, 2);
    }

    #[test]
    fn unknown_instructions_are_refused() {
        // add %eax, (%rdi); rep movsb; lock xadd; movaps; register-register mov.
        assert!(decode(&[0x01, 0x07]).is_none());
        assert!(decode(&[0xF3, 0xA4]).is_none());
        assert!(decode(&[0xF0, 0x0F, 0xC1, 0x07]).is_none());
        assert!(decode(&[0x0F, 0x28, 0x07]).is_none());
        assert!(decode(&[0x89, 0xC3]).is_none());
        // Truncated.
        assert!(decode(&[0x8B]).is_none());
        assert!(decode(&[0xC7, 0x07, 0x01]).is_none());
        assert!(decode(&[]).is_none());
    }

    #[test]
    fn loads_merge_into_the_register_the_way_the_cpu_does() {
        let old = 0x1122_3344_5566_7788u64;
        let mov = |size: u8, dst: u8, ext, high8| Access {
            len: 1,
            size,
            kind: Kind::Load { reg: 0, high8, ext, dst },
        };
        // 32-bit destination zeroes the upper half.
        assert_eq!(mov(4, 4, Ext::None, false).load_result(old, 0xAABB_CCDD), 0xAABB_CCDD);
        // 16-bit and 8-bit leave the rest alone.
        assert_eq!(mov(2, 2, Ext::None, false).load_result(old, 0xBEEF), 0x1122_3344_5566_BEEF);
        assert_eq!(mov(1, 1, Ext::None, false).load_result(old, 0xAB), 0x1122_3344_5566_77AB);
        assert_eq!(mov(1, 1, Ext::None, true).load_result(old, 0xAB), 0x1122_3344_5566_AB88);
        // Extensions.
        assert_eq!(mov(1, 4, Ext::Zero, false).load_result(old, 0x80), 0x80);
        assert_eq!(mov(1, 8, Ext::Sign, false).load_result(old, 0x80), 0xFFFF_FFFF_FFFF_FF80);
        assert_eq!(mov(4, 8, Ext::Sign, false).load_result(old, 0x8000_0000), 0xFFFF_FFFF_8000_0000);
        assert_eq!(mov(2, 4, Ext::Sign, false).load_result(old, 0x7FFF), 0x7FFF);
        // A device that returns more bits than the access is wide is truncated.
        assert_eq!(mov(1, 4, Ext::None, false).load_result(old, 0x1FF), 0xFF);
    }

    #[test]
    fn stores_take_the_access_width_of_the_register() {
        let st = |size, high8| Access { len: 1, size, kind: Kind::Store { reg: 0, high8 } };
        assert_eq!(st(4, false).store_value(0x1122_3344_5566_7788), 0x5566_7788);
        assert_eq!(st(1, false).store_value(0x1234), 0x34);
        assert_eq!(st(1, true).store_value(0x1234), 0x12);
        assert_eq!(st(8, false).store_value(u64::MAX), u64::MAX);
    }
}
