use crate::fdargs::*;
use crate::req::*;
use crate::slots::*;
use crate::state::*;

#[test]
fn request_words_roundtrip_and_decode_per_kind() {
    let r = Req::decode(0xC008_AE05);
    assert_eq!((r.dir, r.size, r.ty, r.nr), (3, 8, TYPE, 5));
    assert_eq!(r.encode(), 0xC008_AE05);
    assert_eq!(decode(0xC008_AE05, Kind::System), Some(Op::GetSupportedCpuid));
    assert_eq!(decode(0xC008_AE05, Kind::Vcpu), None);
    assert_eq!(decode(0x0000_AE80, Kind::Vcpu), Some(Op::Run));
    assert_eq!(decode(0x0000_AE03, Kind::Vm), Some(Op::CheckExtension));
    assert_eq!(decode(0x0000_AE03, Kind::Vcpu), None);
    assert_eq!(decode(0xdead_beef, Kind::System), None);
}

#[test]
fn table_codes_are_self_consistent() {
    for op in [Op::GetApiVersion, Op::CreateVm, Op::SetRegs, Op::Run, Op::Ioeventfd, Op::SetLapic] {
        let code = code_of(op);
        assert_eq!(Req::decode(code).ty, TYPE);
        assert_eq!(Req::decode(code).encode(), code);
    }
    // The sizes the VMM sent match the structures this crate parses.
    assert_eq!(Req::decode(code_of(Op::SetRegs)).size as usize, REGS_LEN);
    assert_eq!(Req::decode(code_of(Op::GetSregs)).size as usize, SREGS_LEN);
    assert_eq!(Req::decode(code_of(Op::SetUserMemoryRegion)).size as usize, ARG_LEN);
    assert_eq!(Req::decode(code_of(Op::Irqfd)).size as usize, IRQFD_LEN);
    assert_eq!(Req::decode(code_of(Op::Ioeventfd)).size as usize, IOEVENTFD_LEN);
}

#[test]
fn regs_from_the_first_run_dump() {
    // rbx = 0x6000, rip = 0x201000, rflags = 2: the PVH entry state the VMM set.
    let mut b = [0u8; REGS_LEN];
    b[8..16].copy_from_slice(&0x6000u64.to_le_bytes());
    b[128..136].copy_from_slice(&0x20_1000u64.to_le_bytes());
    b[136..144].copy_from_slice(&2u64.to_le_bytes());
    let r = Regs::parse(&b).unwrap();
    assert_eq!((r.rbx, r.rip, r.rflags, r.rax), (0x6000, 0x20_1000, 2, 0));
    let mut out = [0u8; REGS_LEN];
    r.encode(&mut out);
    assert_eq!(out, b);
    assert!(Regs::parse(&b[..100]).is_none());
}

#[test]
fn sregs_pvh_entry_state_roundtrips_and_maps_to_vmcb_attributes() {
    let flat_code = Segment { limit: 0xffff_ffff, selector: 8, ty: 11, present: 1, db: 1, s: 1, g: 1, ..Segment::default() };
    let flat_data = Segment { limit: 0xffff_ffff, selector: 0x10, ty: 3, present: 1, db: 1, s: 1, g: 1, ..Segment::default() };
    let s = Sregs {
        cs: flat_code,
        ds: flat_data,
        es: flat_data,
        fs: flat_data,
        gs: flat_data,
        ss: flat_data,
        tr: Segment { limit: 0x67, selector: 0x18, ty: 11, present: 1, ..Segment::default() },
        ldt: Segment { unusable: 1, ..Segment::default() },
        gdt: Dtable { base: 0x500, limit: 31 },
        idt: Dtable { base: 0x520, limit: 7 },
        cr0: 0x11,
        ..Sregs::default()
    };
    let mut b = [0u8; SREGS_LEN];
    s.encode(&mut b);
    assert_eq!(Sregs::parse(&b), Some(s));
    // type 11, S, P, D/B, G = 0xB | 0x10 | 0x80 | 0x400 | 0x800.
    assert_eq!(flat_code.vmcb_attrib(), 0xC9B);
    assert_eq!(flat_data.vmcb_attrib(), 0xC93);
    assert_eq!(s.ldt.vmcb_attrib(), 0);
    assert_eq!(Segment::from_vmcb(0, 0xffff_ffff, 8, 0xC9B).vmcb_attrib(), 0xC9B);
    assert!(Sregs::parse(&b[..300]).is_none());
}

#[test]
fn cpuid_entries_use_a_40_byte_stride_and_the_observed_leaf_zero() {
    // Entry 0 of the first SET_CPUID2: leaf 0, eax 0xd, "Auth" "cAMD" "enti".
    let e = CpuidEntry { function: 0, index: 0, flags: 0, eax: 0xd, ebx: 0x6874_7541, ecx: 0x444d_4163, edx: 0x6974_6e65 };
    let mut b = [0u8; CPUID_ENTRY_LEN];
    e.encode(&mut b);
    assert_eq!(&b[16..20], b"Auth");
    assert_eq!(&b[20..24], b"cAMD");
    assert_eq!(CpuidEntry::parse(&b), Some(e));
}

#[test]
fn msr_entries_and_list_bounds() {
    let m = MsrEntry { index: 0x1a0, data: 1 };
    let mut b = [0u8; MSR_ENTRY_LEN];
    m.encode(&mut b);
    assert_eq!(MsrEntry::parse(&b), Some(m));
    let mut hdr = [0u8; 8];
    hdr[0] = 11;
    assert_eq!(list_count(&hdr, MSR_ENTRY_LEN, LIST_HDR + 11 * MSR_ENTRY_LEN), Some(11));
    // A count larger than the buffer the VMM owns is refused, including overflowing ones.
    assert_eq!(list_count(&hdr, MSR_ENTRY_LEN, LIST_HDR + 10 * MSR_ENTRY_LEN), None);
    hdr = [0xff; 8];
    assert_eq!(list_count(&hdr, MSR_ENTRY_LEN, 4096), None);
}

fn region(slot: u32, gpa: u64, size: u64) -> Region {
    Region { slot, flags: 0, gpa, size, uaddr: 0x1_0003_c000 }
}

#[test]
fn memslot_observed_region_is_accepted_and_translated() {
    // From the census: slot 0, gpa 0, 128 MiB, uaddr 0x1_0003_c000.
    let mut raw = [0u8; ARG_LEN];
    raw[16..24].copy_from_slice(&0x0800_0000u64.to_le_bytes());
    raw[24..32].copy_from_slice(&0x1_0003_c000u64.to_le_bytes());
    let r = Region::parse(&raw).unwrap();
    assert_eq!((r.slot, r.gpa, r.size, r.uaddr), (0, 0, 0x0800_0000, 0x1_0003_c000));
    let mut t = Table::new();
    t.set(r, 0x40_0000).unwrap();
    assert_eq!(t.slice_offset(0), Some(0x40_0000));
    assert_eq!(t.slice_offset(0x1234), Some(0x40_1234));
    assert_eq!(t.slice_offset(0x0800_0000), None);
    assert_eq!(t.user_addr(0x1000), Some(0x1_0003_d000));
}

#[test]
fn memslot_refusals() {
    let t = Table::new();
    assert_eq!(t.check(&region(8, 0, LEAF)), Err(Refused::Slot));
    assert_eq!(t.check(&Region { flags: 1, ..region(0, 0, LEAF) }), Err(Refused::Flags));
    assert_eq!(t.check(&region(0, 0, 0)), Err(Refused::Geometry));
    assert_eq!(t.check(&region(0, 4096, LEAF)), Err(Refused::Geometry));
    assert_eq!(t.check(&region(0, 0, LEAF + 4096)), Err(Refused::Geometry));
    assert_eq!(t.check(&region(0, GPA_LIMIT, LEAF)), Err(Refused::Geometry));
    assert_eq!(t.check(&region(0, u64::MAX - LEAF + 1, LEAF)), Err(Refused::Geometry));
    assert_eq!(t.check(&Region { uaddr: 0x1001, ..region(0, 0, LEAF) }), Err(Refused::UserRange));
    assert_eq!(t.check(&Region { uaddr: USER_TOP - 4096, ..region(0, 0, LEAF) }), Err(Refused::UserRange));
    assert_eq!(t.check(&Region { uaddr: u64::MAX - 4095, ..region(0, 0, LEAF) }), Err(Refused::UserRange));
}

#[test]
fn memslot_overlap_and_replace() {
    let mut t = Table::new();
    t.set(region(0, 0, 4 * LEAF), 0).unwrap();
    assert_eq!(t.set(region(1, 2 * LEAF, 4 * LEAF), 4 * LEAF), Err(Refused::Overlap));
    t.set(region(1, 4 * LEAF, LEAF), 4 * LEAF).unwrap();
    // The same slot may be replaced (it does not overlap itself).
    t.set(region(0, 0, 2 * LEAF), 0).unwrap();
    assert_eq!(t.iter().count(), 2);
    assert_eq!(t.slice_offset(3 * LEAF), None);
}

#[test]
fn fd_arguments_from_the_dumps() {
    // IRQFD {fd 0x0e, gsi 1}
    let mut b = [0u8; IRQFD_LEN];
    b[0] = 0x0e;
    b[4] = 1;
    assert_eq!(Irqfd::parse(&b), Some(Irqfd { fd: 14, gsi: 1, flags: 0, resample_fd: 0 }));
    // IOEVENTFD {datamatch 0, addr 0xc0001050, len 4, fd 8, flags 1}
    let mut e = [0u8; IOEVENTFD_LEN];
    e[8..16].copy_from_slice(&0xc000_1050u64.to_le_bytes());
    e[16] = 4;
    e[20] = 8;
    e[24] = 1;
    let io = Ioeventfd::parse(&e).unwrap();
    assert_eq!((io.addr, io.len, io.fd, io.flags), (0xc000_1050, 4, 8, 1));
    assert!(io.matches(0xc000_1050, 4, 0));
    assert!(!io.matches(0xc000_1050, 4, 1), "datamatch is on");
    assert!(!io.matches(0xc000_1054, 4, 0));
    assert!(!io.matches(0xc000_1050, 2, 0));
}
