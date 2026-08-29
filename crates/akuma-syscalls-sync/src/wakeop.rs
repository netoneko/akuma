//! `FUTEX_WAKE_OP`: the `val3` opcode.
//!
//! One 32-bit word encodes a read-modify-write to perform on the *second* futex
//! word and a comparison against its old value that decides whether a second
//! wake happens. Decode and arithmetic are here; the read, the write and the
//! wakes are the kernel's.

use akuma_syscalls_linux::flags::futex::wake_op as w;

/// A decoded `val3`: `{ shift[31], op[30:28], cmp[27:24], oparg[23:12], cmparg[11:0] }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeOp {
    pub op: u32,
    pub cmp: u32,
    /// Already resolved through `FUTEX_OP_OPARG_SHIFT`, so callers never repeat
    /// that step — the shift bit shares the op nibble and is easy to leave in.
    pub oparg: u32,
    pub cmparg: u32,
}

impl WakeOp {
    /// Extract the fields, matching Linux's `futex_atomic_op_inuser`.
    ///
    /// The two 12-bit fields are extracted by shifting left and back rather
    /// than by masking, which is how Linux spells it and is worth copying: it
    /// makes `cmparg` a *signed* 12-bit quantity where the comparisons want one.
    #[must_use]
    pub const fn decode(val3: u32) -> Self {
        let op = (val3 >> 28) & 0x7;
        let cmp = (val3 >> 24) & 0xf;
        let mut oparg = (val3 << 8) >> 20;
        let cmparg = (val3 << 20) >> 20;
        if (val3 & (w::FUTEX_OP_OPARG_SHIFT << 28)) != 0 {
            oparg = 1u32 << oparg;
        }
        Self { op, cmp, oparg, cmparg }
    }

    /// The new value for the second futex word, or `None` if the op field is
    /// not one of the five defined ones (the caller reports `ENOSYS`).
    #[must_use]
    pub const fn apply(self, oldval: u32) -> Option<u32> {
        Some(match self.op {
            w::FUTEX_OP_SET => self.oparg,
            w::FUTEX_OP_ADD => oldval.wrapping_add(self.oparg),
            w::FUTEX_OP_OR => oldval | self.oparg,
            w::FUTEX_OP_ANDN => oldval & !self.oparg,
            w::FUTEX_OP_XOR => oldval ^ self.oparg,
            _ => return None,
        })
    }

    /// Whether the conditional second wake fires. The comparison is **signed**,
    /// as in Linux — an unsigned `<` here would make every comparison against a
    /// negative `cmparg` come out backwards.
    #[must_use]
    pub const fn compare(self, oldval: u32) -> bool {
        match self.cmp {
            w::FUTEX_OP_CMP_EQ => oldval == self.cmparg,
            w::FUTEX_OP_CMP_NE => oldval != self.cmparg,
            w::FUTEX_OP_CMP_LT => oldval.cast_signed() < self.cmparg.cast_signed(),
            w::FUTEX_OP_CMP_LE => oldval.cast_signed() <= self.cmparg.cast_signed(),
            w::FUTEX_OP_CMP_GT => oldval.cast_signed() > self.cmparg.cast_signed(),
            w::FUTEX_OP_CMP_GE => oldval.cast_signed() >= self.cmparg.cast_signed(),
            _ => false,
        }
    }
}
