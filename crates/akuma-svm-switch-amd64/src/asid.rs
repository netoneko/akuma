//! Address space identifiers (APM Vol 2 §15.16): the tag every TLB entry a guest makes carries.
//!
//! Two live guests that share an ASID share TLB entries, so one can reach memory the other's
//! nested page table maps. An [`Asid`] is therefore an owned value: [`Asid::alloc`] or
//! [`Asid::claim`] takes one out of a global bitmap and dropping it puts it back, so no two live
//! values hold the same number. ASID 0 is the host's and is never handed out.
//!
//! Reuse after a drop is safe without a flush here: a [`crate::Guest`] flushes its ASID's
//! entries on its first entry on each core (`guest.rs`), which is the only way an entry the
//! previous holder left behind could be reached.

use core::arch::x86_64::__cpuid;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Bitmap words: ASIDs `0..WORDS * 64`. Real parts report a few hundred to a few thousand
/// (`CPUID Fn8000_000A_EBX`); 512 live guests is far past anything this hypervisor runs.
const WORDS: usize = 8;
const CAP: u32 = (WORDS * 64) as u32;

static USED: [AtomicU64; WORDS] = [const { AtomicU64::new(0) }; WORDS];
/// `min(NASID, CAP)`, read once; `u32::MAX` until then.
static LIMIT: AtomicU32 = AtomicU32::new(u32::MAX);

/// How many ASIDs this core implements (`CPUID Fn8000_000A_EBX`, §15.16), capped at the bitmap.
/// 0 when the leaf is absent (no SVM): nothing can then be allocated.
fn limit() -> u32 {
    let l = LIMIT.load(Ordering::Relaxed);
    if l != u32::MAX {
        return l;
    }
    let l = if __cpuid(0x8000_0000).eax >= 0x8000_000A { __cpuid(0x8000_000A).ebx.min(CAP) } else { 0 };
    LIMIT.store(l, Ordering::Relaxed);
    l
}

fn try_take(n: u32) -> bool {
    let (w, b) = ((n / 64) as usize, n % 64);
    USED[w].fetch_or(1 << b, Ordering::AcqRel) & (1 << b) == 0
}

/// One ASID, held alone. Not `Clone`; dropping it frees the number.
#[derive(Debug)]
pub struct Asid(u32);

impl Asid {
    /// The lowest free ASID.
    #[must_use]
    pub fn alloc() -> Option<Self> {
        (1..limit()).find(|&n| try_take(n)).map(Self)
    }

    /// ASID `n` exactly, if it is free and implemented (the rehearsal ladder names its rows'
    /// ASIDs; a reused number is part of what a row tests).
    #[must_use]
    pub fn claim(n: u32) -> Option<Self> {
        (n != 0 && n < limit() && try_take(n)).then_some(Self(n))
    }

    /// The number, for logs.
    #[must_use]
    pub const fn get(&self) -> u32 {
        self.0
    }
}

impl Drop for Asid {
    fn drop(&mut self) {
        let (w, b) = ((self.0 / 64) as usize, self.0 % 64);
        USED[w].fetch_and(!(1 << b), Ordering::Release);
    }
}
