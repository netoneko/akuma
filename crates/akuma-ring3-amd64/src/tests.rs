use super::*;

// ---- a synthetic static ELF64, built byte by byte so the tests do not lean
// on an outside toolchain ----

/// One program header, packed the way `Image::parse` reads it.
fn phdr(ty: u32, flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64) -> [u8; 56] {
    let mut p = [0u8; 56];
    p[0..4].copy_from_slice(&ty.to_le_bytes());
    p[4..8].copy_from_slice(&flags.to_le_bytes());
    p[8..16].copy_from_slice(&offset.to_le_bytes());
    p[16..24].copy_from_slice(&vaddr.to_le_bytes());
    p[32..40].copy_from_slice(&filesz.to_le_bytes());
    p[40..48].copy_from_slice(&memsz.to_le_bytes());
    p
}

fn elf64_header(phoff: u64, phnum: u16, entry: u64, etype: u16) -> [u8; 64] {
    let mut h = [0u8; 64];
    h[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    h[4] = 2; // ELF64
    h[5] = 1; // little-endian
    h[16..18].copy_from_slice(&etype.to_le_bytes());
    h[24..32].copy_from_slice(&entry.to_le_bytes());
    h[32..40].copy_from_slice(&phoff.to_le_bytes());
    h[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    h[56..58].copy_from_slice(&phnum.to_le_bytes());
    h
}

/// A two-segment image: code at 0x401000 (file offset 0x1000), data at
/// 0x402000 (file offset 0x2000) with 0x100 bytes of `.bss`. The phdr table
/// sits at file offset 0x40, file-backed by the first LOAD (offset 0), so
/// `AT_PHDR` computes to 0x400000 + 0x40 without a `PT_PHDR`.
fn sample_image() -> [u8; 0x3000] {
    let mut img = [0u8; 0x3000];
    img[0..64].copy_from_slice(&elf64_header(0x40, 3, 0x4010_a7, 2));
    img[0x40..0x40 + 56].copy_from_slice(&phdr(1, 5, 0x0, 0x400000, 0x1000, 0x1000)); // R+X
    img[0x40 + 56..0x40 + 112].copy_from_slice(&phdr(1, 6, 0x2000, 0x402000, 0x200, 0x300)); // RW+bss
    img[0x40 + 112..0x40 + 168].copy_from_slice(&phdr(6, 4, 0x40, 0x400040, 168, 168)); // PT_PHDR
    img[0x1000 + 7] = 0xEA; // a code byte, to check placement reads the right file bytes
    img[0x2000] = 0x42; // a data byte
    img
}

#[test]
fn parse_finds_segments_entry_and_phdr() {
    let image = sample_image();
    let img = Image::parse(&image).expect("sample parses");
    assert_eq!(img.entry(), 0x4010_a7);
    assert_eq!(img.phdr_va(), 0x400040, "PT_PHDR names its own link address");
    assert_eq!(img.phent(), 56);
    assert_eq!(img.phnum(), 3);
    let segs = img.segments();
    assert_eq!(segs.len(), 2);
    assert_eq!((segs[0].vaddr, segs[0].prot), (0x400000, Prot::Rx));
    assert_eq!((segs[1].vaddr, segs[1].prot), (0x402000, Prot::Rw));
    assert_eq!(segs[1].memsz, 0x300);
    assert_eq!(segs[1].filesz, 0x200);
    assert_eq!(img.segment_bytes(&segs[1])[0], 0x42);
    assert_eq!(img.image_end(), 0x402_300, "highest memsz byte, unrounded");
}

#[test]
fn parse_computes_phdr_when_no_pt_phdr() {
    let mut image = sample_image();
    // Blank the PT_PHDR: the table at file offset 0x40 is inside the first
    // LOAD (vaddr 0x400000, offset 0), so AT_PHDR = 0x400040 by arithmetic.
    image[0x40 + 112..0x40 + 168].fill(0);
    let img = Image::parse(&image).expect("parses");
    assert_eq!(img.phdr_va(), 0x400040);
}

#[test]
fn parse_rejects_bad_images() {
    let good = sample_image();
    assert!(matches!(Image::parse(&good[..32]), Err(LoadError::Magic)), "truncated before the header ends");
    let empty = [0u8; 4];
    assert!(matches!(Image::parse(&empty), Err(LoadError::Magic)));

    let mut bad = good;
    bad[4] = 1; // ELF32
    assert!(matches!(Image::parse(&bad), Err(LoadError::NotElf64Le)));

    bad[4] = 2;
    bad[16..18].copy_from_slice(&4u16.to_le_bytes()); // ET_CORE
    assert!(matches!(Image::parse(&bad), Err(LoadError::NotExec)));

    bad[16..18].copy_from_slice(&2u16.to_le_bytes());
    bad[32..40].copy_from_slice(&0u64.to_le_bytes()); // e_phoff = 0
    assert!(matches!(Image::parse(&bad), Err(LoadError::PhdrTable)));

    bad[32..40].copy_from_slice(&0x40u64.to_le_bytes());
    bad[56..58].copy_from_slice(&0xFFu16.to_le_bytes()); // phnum past the image
    assert!(matches!(Image::parse(&bad), Err(LoadError::PhdrTable)));

    bad[56..58].copy_from_slice(&3u16.to_le_bytes());
    bad[0x40..0x40 + 56].copy_from_slice(&phdr(1, 5, 0x2900, 0x400000, 0x1000, 0x1000)); // bytes past EOF
    assert!(matches!(Image::parse(&bad), Err(LoadError::SegmentBounds)));

    bad[0x40..0x40 + 56].copy_from_slice(&phdr(1, 5, 0x0, 0x400000, 0x1000, 0x10)); // memsz < filesz
    assert!(matches!(Image::parse(&bad), Err(LoadError::SegmentBounds)));
}

// ---- the stack builder: read the block back through the same mapping the
// kernel places it with ----

/// Read the u64 at stack VA `va` from a block whose end is `stack_top`.
fn word(buf: &[u8], stack_top: u64, va: u64) -> u64 {
    let i = buf.len() - (stack_top - va) as usize;
    u64::from_le_bytes(buf[i..i + 8].try_into().unwrap())
}

/// Read the NUL-terminated string at stack VA `va`.
fn string(buf: &[u8], stack_top: u64, va: u64) -> &str {
    let i = buf.len() - (stack_top - va) as usize;
    let end = buf[i..].iter().position(|&b| b == 0).unwrap() + i;
    core::str::from_utf8(&buf[i..end]).unwrap()
}

#[test]
fn stack_holds_the_linux_layout() {
    const TOP: u64 = 0x7000_0000_0000;
    let mut buf = [0u8; 512];
    let aux = [
        (AT_PHDR, 0x400040u64),
        (AT_PHENT, 56),
        (AT_PHNUM, 3),
        (AT_PAGESZ, 4096),
        (AT_ENTRY, 0x4010_a7),
        (AT_UID, 0),
        (AT_RANDOM, 0), // value supplied by the builder
        (AT_EXECFN, 0), // value supplied by the builder
    ];
    let rsp = build_stack(&mut buf, TOP, &["/bin/hello"], &[], &aux);

    assert_eq!(rsp % 16, 0, "the initial rsp is 16-byte aligned");
    assert_eq!(word(&buf, TOP, rsp), 1, "argc");
    let argv0 = word(&buf, TOP, rsp + 8);
    assert_eq!(string(&buf, TOP, argv0), "/bin/hello");
    assert_eq!(word(&buf, TOP, rsp + 16), 0, "argv NULL");
    assert_eq!(word(&buf, TOP, rsp + 24), 0, "envp NULL (no env)");

    // Walk the auxv: it ends with two AT_NULL words just above the envp NULL.
    let mut va = rsp + 32;
    let mut seen_random = None;
    let mut seen_execfn = None;
    let mut n_entries = 0;
    loop {
        let (k, v) = (word(&buf, TOP, va), word(&buf, TOP, va + 8));
        if k == AT_NULL {
            break;
        }
        match k {
            AT_RANDOM => seen_random = Some(v),
            AT_EXECFN => seen_execfn = Some(v),
            AT_PHDR => assert_eq!(v, 0x400040),
            AT_ENTRY => assert_eq!(v, 0x4010_a7),
            _ => {}
        }
        va += 16;
        n_entries += 1;
    }
    assert_eq!(n_entries, aux.len(), "the builder adds the AT_NULL pair itself");
    let random_va = seen_random.expect("AT_RANDOM present");
    assert_eq!(random_va % 16, 0, "canary block sits on the 16-boundary below the vectors");
    let i = buf.len() - (TOP - random_va) as usize;
    assert_eq!(buf[i], canary_byte(0), "canary bytes are where AT_RANDOM points");
    assert_eq!(seen_execfn, Some(argv0), "AT_EXECFN is argv[0]'s string");
}

#[test]
fn stack_without_at_random_has_no_canary() {
    const TOP: u64 = 0x7000_0000_0000;
    let mut buf = [0u8; 512];
    let aux = [(AT_PAGESZ, 4096u64)];
    let rsp = build_stack(&mut buf, TOP, &["x"], &[], &aux);
    assert_eq!(rsp % 16, 0);
    assert_eq!(word(&buf, TOP, rsp), 1);
}

// ---- the frames the two crossings use ----

#[test]
fn syscall_frame_offsets_match_the_stub_push_order() {
    // The stub pushes r11, rcx, rax, rdi, rsi, rdx, r10, r8, r9, rbx, rbp,
    // r12, r13, r14, r15 below an 8-byte pad, so [rsp] is r15 and the RAX
    // slot the result is stored to sits at +96.
    use core::mem::offset_of;
    assert_eq!(offset_of!(SyscallFrame, r15), 0);
    assert_eq!(offset_of!(SyscallFrame, rax), 96);
    assert_eq!(offset_of!(SyscallFrame, user_rip), 104);
    assert_eq!(offset_of!(SyscallFrame, user_rflags), 112);
}

#[test]
fn entry_frame_offsets_match_the_enter_asm() {
    // `enter_user` reads by hand: rax at +0x60, rip at +0x68 (sysretq's
    // target), rflags at +0x70, rsp at +0x78, and the frame-pointer spend
    // (r10) last at +0x30.
    use core::mem::offset_of;
    assert_eq!(offset_of!(EntryFrame, rax), 0x60);
    assert_eq!(offset_of!(EntryFrame, r10), 0x30);
    assert_eq!(offset_of!(EntryFrame, rip), 0x68);
    assert_eq!(offset_of!(EntryFrame, rflags), 0x70);
    assert_eq!(offset_of!(EntryFrame, rsp), 0x78);
}

#[test]
fn syscall_frame_accessors_name_the_right_registers() {
    let f = SyscallFrame {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        rbp: 0,
        rbx: 0,
        r10: 4,
        r8: 5,
        r9: 6,
        rdx: 3,
        rsi: 2,
        rdi: 1,
        rax: 60,
        user_rip: 0x4010_a7,
        user_rflags: 0x202,
    };
    assert_eq!((f.arg1(), f.arg2(), f.arg3()), (1, 2, 3));
    assert_eq!((f.arg4(), f.arg5(), f.arg6()), (4, 5, 6));
}
