use super::*;

// ---- decoding helpers: the inverse of the packing, so the tests do not just
// restate the formula ----

fn dpl(d: u64) -> u64 {
    d >> 45 & 3
}
fn present(d: u64) -> bool {
    d >> 47 & 1 == 1
}
fn is_code(d: u64) -> bool {
    d >> 43 & 1 == 1
}
/// The `L` bit: 64-bit code.
fn long_mode(d: u64) -> bool {
    d >> 53 & 1 == 1
}
fn tss_base(t: [u64; 2]) -> u64 {
    (t[0] >> 16 & 0xFF_FFFF) | (t[0] >> 56 & 0xFF) << 24 | t[1] << 32
}
fn tss_limit(t: [u64; 2]) -> u64 {
    t[0] & 0xFFFF | (t[0] >> 48 & 0xF) << 16
}

#[test]
fn the_selectors_are_the_ones_the_hardware_documents() {
    assert_eq!(KERNEL_CODE, 0x08);
    assert_eq!(KERNEL_DATA, 0x10);
    assert_eq!(TSS_SELECTOR, 0x28);
    assert_eq!(SYSRET_BASE, 0x13);
    assert_eq!(USER_CS, 0x23);
    assert_eq!(USER_SS, 0x1b);
}

/// What `sysret` computes must land on the rows the table has, with RPL 3 on
/// *both* — the `#GP(0x18)` that only real AMD hardware raised.
#[test]
fn sysret_lands_on_the_user_rows_with_rpl_3_on_both_selectors() {
    let t = table(0);
    let row = |sel: u16| t[usize::from(sel >> 3)];
    assert_eq!(USER_CS & 3, 3);
    assert_eq!(USER_SS & 3, 3, "SS.RPL must equal CS.RPL or iretq faults");
    assert!(is_code(row(USER_CS)) && dpl(row(USER_CS)) == 3 && long_mode(row(USER_CS)));
    assert!(!is_code(row(USER_SS)) && dpl(row(USER_SS)) == 3);
}

#[test]
fn syscall_lands_on_the_kernel_rows() {
    let t = table(0);
    let row = |sel: u16| t[usize::from(sel >> 3)];
    // CS = STAR[47:32], SS = that + 8.
    let cs = (star() >> 32 & 0xFFFF) as u16;
    let ss = cs + 8;
    assert_eq!(cs, KERNEL_CODE);
    assert!(is_code(row(cs)) && dpl(row(cs)) == 0 && long_mode(row(cs)));
    assert!(!is_code(row(ss)) && dpl(row(ss)) == 0);
}

#[test]
fn star_packs_the_two_bases_where_the_msr_wants_them() {
    assert_eq!(star() >> 48, u64::from(SYSRET_BASE));
    assert_eq!(star() >> 32 & 0xFFFF, u64::from(KERNEL_CODE));
    assert_eq!(star() & 0xFFFF_FFFF, 0, "the legacy EIP field is unused in long mode");
}

#[test]
fn every_segment_row_is_present_and_flat() {
    for d in [D_KERNEL_CODE, D_KERNEL_DATA, D_USER_CODE, D_USER_DATA] {
        assert!(present(d));
        assert_eq!(d & 0xFFFF, 0xFFFF, "limit low");
        assert_eq!(d >> 16 & 0xFF_FFFF, 0, "base 0");
    }
    assert_eq!([dpl(D_KERNEL_CODE), dpl(D_KERNEL_DATA)], [0, 0]);
    assert_eq!([dpl(D_USER_CODE), dpl(D_USER_DATA)], [3, 3]);
}

/// `boot.s` and the AP trampoline install these two rows; the GDT here keeps
/// them byte-identical so `CS` stays valid across `lgdt` with no far return.
#[test]
fn the_kernel_rows_are_the_bytes_boot_s_installs() {
    assert_eq!(D_KERNEL_CODE, 0x00AF_9A00_0000_FFFF);
    assert_eq!(D_KERNEL_DATA, 0x00CF_9200_0000_FFFF);
    let t = table(0x1234);
    assert_eq!(t[0], 0, "row 0 is the null descriptor");
    assert_eq!(t[1], D_KERNEL_CODE);
    assert_eq!(t[2], D_KERNEL_DATA);
}

#[test]
fn the_tss_descriptor_round_trips_the_base_whatever_it_is() {
    for base in [
        0u64,
        0x1000,
        0x00FF_FFFF,
        0x0100_0000,
        0xFFFF_FFFF,
        0x1_0000_0000,
        0xFFFF_8000_1234_5678,
        u64::MAX,
    ] {
        let d = tss_descriptor(base);
        assert_eq!(tss_base(d), base, "base {base:#x}");
        assert_eq!(tss_limit(d), (TSS_SIZE - 1) as u64);
        assert_eq!(d[0] >> 40 & 0xF, 0x9, "available 64-bit TSS");
        assert_eq!(d[0] >> 44 & 1, 0, "S=0: a system descriptor");
        assert!(present(d[0]));
        assert_eq!(dpl(d[0]), 0);
    }
}

#[test]
fn the_tss_descriptor_occupies_the_last_two_rows_and_selector_points_at_the_first() {
    let t = table(0xFFFF_8000_0000_1000);
    assert_eq!(usize::from(TSS_SELECTOR >> 3), SLOTS - 2);
    assert_eq!(tss_base([t[SLOTS - 2], t[SLOTS - 1]]), 0xFFFF_8000_0000_1000);
    assert_eq!(GDT_LIMIT, 55);
}

#[test]
fn the_tss_is_104_bytes_with_the_io_map_past_the_end() {
    assert_eq!(core::mem::size_of::<Tss>(), 104);
    let t = Tss::new();
    // iomap_base >= limit+1 means no I/O permission bitmap: ring 3 gets no ports.
    let iomap = t.iomap_base;
    assert!(usize::from(iomap) > TSS_SIZE - 1);
}

#[test]
fn rsp0_is_settable_and_nothing_else_moves() {
    let mut t = Tss::new();
    assert_eq!(t.rsp0(), 0);
    t.set_rsp0(0xFFFF_8000_DEAD_B000);
    assert_eq!(t.rsp0(), 0xFFFF_8000_DEAD_B000);
    let (r1, r2, iomap) = (t.rsp1, t.rsp2, t.iomap_base);
    assert_eq!((r1, r2, iomap), (0, 0, TSS_SIZE as u16));
}
