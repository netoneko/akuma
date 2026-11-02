use super::*;

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
    // slot the result is stored to sits at +96. Pushes go DOWN in memory:
    // the last of the three argument pushes (r9) is the one at +48.
    use core::mem::offset_of;
    assert_eq!(offset_of!(SyscallFrame, r15), 0);
    assert_eq!(offset_of!(SyscallFrame, r14), 8);
    assert_eq!(offset_of!(SyscallFrame, r13), 16);
    assert_eq!(offset_of!(SyscallFrame, r12), 24);
    assert_eq!(offset_of!(SyscallFrame, rbp), 32);
    assert_eq!(offset_of!(SyscallFrame, rbx), 40);
    assert_eq!(offset_of!(SyscallFrame, r9), 48);
    assert_eq!(offset_of!(SyscallFrame, r8), 56);
    assert_eq!(offset_of!(SyscallFrame, r10), 64);
    assert_eq!(offset_of!(SyscallFrame, rdx), 72);
    assert_eq!(offset_of!(SyscallFrame, rsi), 80);
    assert_eq!(offset_of!(SyscallFrame, rdi), 88);
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
