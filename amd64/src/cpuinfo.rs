//! `/proc/cpuinfo`: one block per logical CPU, from `cpuid`.
//!
//! The file, its offset handling and the core count are `akuma-vfs-glue`'s
//! (`proc.rs::cpuinfo_read`); this is the block renderer it calls. Before
//! 2026-10-09 neither kernel had the file, and Chromium logged `Failed to
//! initialize cpuinfo` on every start.
//!
//! **Every block describes the CPU `cpuid` runs on**, not core `n`: the machines
//! this boots on (one Ryzen laptop, QEMU/Firecracker guests, an HP desktop) are
//! homogeneous, and executing `cpuid` on a chosen core would need a cross-core
//! call for no information gained. A hybrid-core part would be reported wrongly;
//! that is the stated cost.
//!
//! **What is left out is deliberate:** `cpu MHz`, `cache size`, `physical id`,
//! `core id`, `cpu cores` and `bogomips`. The kernel does not know the first
//! four (no topology enumeration, no frequency source) and a plausible made-up
//! number is worse than a missing key. `flags` is what `cpuid` *advertises*, not
//! what the kernel has enabled (a guest's hypervisor may mask more at
//! `XCR0`); userspace that matters (Chromium, V8) reads `cpuid` itself anyway.
//!
//! No allocation: the block is formatted straight into the caller's buffer.

use core::arch::x86_64::{__cpuid, __cpuid_count};
use core::fmt::Write;

use akuma_primitives::console::FmtBuf;

/// `(bit, Linux's name)` tables, one per register, in the order Linux prints.
const LEAF1_EDX: &[(u32, &str)] = &[
    (0, "fpu"), (1, "vme"), (2, "de"), (3, "pse"), (4, "tsc"), (5, "msr"), (6, "pae"),
    (7, "mce"), (8, "cx8"), (9, "apic"), (11, "sep"), (12, "mtrr"), (13, "pge"),
    (14, "mca"), (15, "cmov"), (16, "pat"), (17, "pse36"), (19, "clflush"), (23, "mmx"),
    (24, "fxsr"), (25, "sse"), (26, "sse2"), (28, "ht"),
];
const LEAF1_ECX: &[(u32, &str)] = &[
    (0, "pni"), (1, "pclmulqdq"), (9, "ssse3"), (12, "fma"), (13, "cx16"), (19, "sse4_1"),
    (20, "sse4_2"), (22, "movbe"), (23, "popcnt"), (25, "aes"), (26, "xsave"),
    (27, "osxsave"), (28, "avx"), (29, "f16c"), (30, "rdrand"), (31, "hypervisor"),
];
const LEAF7_EBX: &[(u32, &str)] = &[
    (0, "fsgsbase"), (3, "bmi1"), (5, "avx2"), (7, "smep"), (8, "bmi2"), (9, "erms"),
    (16, "avx512f"), (18, "rdseed"), (19, "adx"), (20, "smap"), (29, "sha_ni"),
];
const EXT1_ECX: &[(u32, &str)] = &[(0, "lahf_lm"), (5, "abm")];
const EXT1_EDX: &[(u32, &str)] = &[(11, "syscall"), (20, "nx"), (26, "pdpe1gb"), (27, "rdtscp"), (29, "lm")];

fn flags(w: &mut impl Write, reg: u32, table: &[(u32, &str)]) {
    for &(bit, name) in table {
        if reg & (1 << bit) != 0 {
            let _ = write!(w, " {name}");
        }
    }
}

/// The 48-byte brand string (leaves `0x8000_0002..=4`) into `out`, or empty when
/// the CPU has no such leaves. Returns the trimmed length.
fn brand(out: &mut [u8; 48]) -> usize {
    if __cpuid(0x8000_0000).eax < 0x8000_0004 {
        return 0;
    }
    for (i, leaf) in (0x8000_0002u32..=0x8000_0004).enumerate() {
        let r = __cpuid(leaf);
        for (j, reg) in [r.eax, r.ebx, r.ecx, r.edx].into_iter().enumerate() {
            out[i * 16 + j * 4..i * 16 + j * 4 + 4].copy_from_slice(&reg.to_le_bytes());
        }
    }
    let end = out.iter().position(|&b| b == 0).unwrap_or(48);
    let start = out[..end].iter().position(|&b| b != b' ').unwrap_or(end);
    out.copy_within(start..end, 0);
    end - start
}

/// One block for logical CPU `core`, ending in the blank line that separates
/// blocks. Fills `buf`, returns the bytes written; a short buffer truncates.
pub fn render(core: usize, buf: &mut [u8]) -> usize {
    let l0 = __cpuid(0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&l0.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&l0.edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&l0.ecx.to_le_bytes());
    let l1 = __cpuid(1);
    // Family/model as Linux derives them: the extended fields only count for
    // family 0xf (and the extended model for 0x6 as well).
    let base_family = (l1.eax >> 8) & 0xf;
    let family = if base_family == 0xf { base_family + ((l1.eax >> 20) & 0xff) } else { base_family };
    let base_model = (l1.eax >> 4) & 0xf;
    let model = if base_family == 0x6 || base_family == 0xf {
        base_model | (((l1.eax >> 16) & 0xf) << 4)
    } else {
        base_model
    };
    let l7 = if l0.eax >= 7 { __cpuid_count(7, 0) } else { __cpuid(0) };
    let ext1 = if __cpuid(0x8000_0000).eax >= 0x8000_0001 { __cpuid(0x8000_0001) } else { __cpuid(0) };
    let mut name = [0u8; 48];
    let nlen = brand(&mut name);

    let mut pos = 0usize;
    let mut w = FmtBuf { buf, pos: &mut pos };
    let _ = writeln!(w, "processor\t: {core}");
    let _ = writeln!(w, "vendor_id\t: {}", core::str::from_utf8(&vendor).unwrap_or("?"));
    let _ = writeln!(w, "cpu family\t: {family}");
    let _ = writeln!(w, "model\t\t: {model}");
    let _ = writeln!(w, "model name\t: {}", core::str::from_utf8(&name[..nlen]).unwrap_or("?"));
    let _ = writeln!(w, "stepping\t: {}", l1.eax & 0xf);
    let _ = write!(w, "flags\t\t:");
    flags(&mut w, l1.edx, LEAF1_EDX);
    flags(&mut w, l1.ecx, LEAF1_ECX);
    flags(&mut w, l7.ebx, LEAF7_EBX);
    flags(&mut w, ext1.ecx, EXT1_ECX);
    flags(&mut w, ext1.edx, EXT1_EDX);
    let _ = writeln!(w, "\n");
    pos
}
