//! The initial user stack: argc, argv, envp and the auxiliary vector.
//!
//! Laid out the way Linux builds it and musl's `__init_libc` walks it. Architecture-neutral:
//! moved from `akuma-ring3-amd64` (which re-exports it) for the aarch64 tokio
//! kernel (B21 A5.6). Its tests stay in `akuma-ring3-amd64`, through the re-export.

/// `AT_NULL` — ends the auxiliary vector.
pub const AT_NULL: u64 = 0;
/// `AT_PHDR` — the program header table's link address.
pub const AT_PHDR: u64 = 3;
/// `AT_PHENT` — size of one program header entry.
pub const AT_PHENT: u64 = 4;
/// `AT_PHNUM` — number of program headers.
pub const AT_PHNUM: u64 = 5;
/// `AT_PAGESZ` — page size.
pub const AT_PAGESZ: u64 = 6;
/// `AT_ENTRY` — the image entry point.
pub const AT_ENTRY: u64 = 9;
/// `AT_UID`/`AT_EUID`/`AT_GID`/`AT_EGID` — the credentials musl caches.
pub const AT_UID: u64 = 11;
/// `AT_EUID`.
pub const AT_EUID: u64 = 12;
/// `AT_GID`.
pub const AT_GID: u64 = 13;
/// `AT_EGID`.
pub const AT_EGID: u64 = 14;
/// `AT_HWCAP` — the ELF capability word (none: no HWCAP features offered).
pub const AT_HWCAP: u64 = 16;
/// `AT_CLKTCK` — `times(2)` resolution.
pub const AT_CLKTCK: u64 = 17;
/// `AT_SECURE` — whether the kernel elevated the process (no).
pub const AT_SECURE: u64 = 23;
/// `AT_RANDOM` — 16 bytes for the stack canary. The builder materialises them
/// inside the stack block and writes their address into the auxv itself.
pub const AT_RANDOM: u64 = 25;
/// `AT_EXECFN` — pointer to the program name string.
pub const AT_EXECFN: u64 = 31;
/// `AT_BASE` — the interpreter's base (none: static).
pub const AT_BASE: u64 = 7;

/// Bytes [`build_stack`] writes for these strings and `n_aux` auxv pairs.
///
/// An upper bound with the alignment slack included. A caller sizes its buffer
/// from this (the block is as large as the arguments make it; there is no
/// fixed entry cap), and `build_stack` never writes below
/// `stack_top - stack_block_len(..)`.
#[must_use]
pub fn stack_block_len(argv: &[&str], env: &[&str], n_aux: usize) -> usize {
    let strings: usize = env.iter().chain(argv.iter()).map(|s| s.len() + 1).sum();
    // canary 16 + two 16-byte alignments, then the words: auxv pairs plus
    // AT_NULL, envp + NULL, argv + NULL, argc.
    let words = 2 * (n_aux + 1) + (env.len() + 1) + (argv.len() + 1) + 1;
    strings + 16 + 32 + words * 8
}

/// Lay out the initial user stack into `buf`, whose **last** byte is the last
/// byte of the stack page at `stack_top` (i.e. `buf` is the tail of the stack,
/// placed so that `buf.as_ptr() + buf.len() == stack_top`).
///
/// Returns the initial user `RSP`, 16-byte aligned, pointing at `argc`.
///
/// The layout, built from `stack_top` down: the strings (env, then argv, then
/// 16 canary bytes), padding to 16, the auxv (`AT_NULL`-terminated), a NULL,
/// envp pointers with a NULL, argv pointers with a NULL, and `argc` last — the
/// exact order Linux builds and musl's `__init_libc` walks. `AT_RANDOM` and
/// `AT_EXECFN` values are supplied by the builder itself (`AT_RANDOM` gets the
/// in-block canary address, `AT_EXECFN` the `argv[0]` string address), so the
/// caller states them as `(AT_RANDOM, 0)` / `(AT_EXECFN, 0)` placeholders.
///
/// `buf` must hold [`stack_block_len`] bytes. No string address is stored:
/// string `i` (env first, then argv) sits at `stack_top` less the lengths of
/// strings `0..=i`, so the pointer vectors are recomputed from the lengths,
/// and the builder needs no table — and so no cap on the count.
pub fn build_stack(buf: &mut [u8], stack_top: u64, argv: &[&str], env: &[&str], aux: &[(u64, u64)]) -> u64 {
    assert!(
        buf.len() >= stack_block_len(argv, env, aux.len()),
        "the argument block is smaller than stack_block_len"
    );
    let mut top = stack_top;
    for s in env.iter().chain(argv.iter()) {
        let bytes = s.as_bytes();
        top -= bytes.len() as u64 + 1; // NUL
        let i = buf.len() - (stack_top - top) as usize;
        buf[i..i + bytes.len()].copy_from_slice(bytes);
        buf[i + bytes.len()] = 0;
    }
    // Where the env strings end and argv's begin; argv[0] is right below.
    let env_bytes: u64 = env.iter().map(|s| s.len() as u64 + 1).sum();
    let argv_top = stack_top - env_bytes;
    let execfn = argv.first().map_or(0, |a0| argv_top - (a0.len() as u64 + 1));

    // The 16 canary bytes `AT_RANDOM` points at, then 16-byte alignment for
    // the pointer area (always applied: the vectors are u64s but the ABI
    // promises the initial rsp itself is 16-aligned).
    let mut random_va = 0u64;
    if aux.iter().any(|(k, _)| *k == AT_RANDOM) {
        top = (top - 16) & !0xF;
        random_va = top;
        let i = buf.len() - (stack_top - top) as usize;
        for (j, b) in buf[i..i + 16].iter_mut().enumerate() {
            *b = canary_byte(j as u64);
        }
    }
    top &= !0xF;
    // The words below are the auxv pairs (even), the two vectors with their
    // NULLs, and argc: an odd count when argc + envc is even, and then one
    // pad word keeps the final rsp on the 16-boundary (Linux pads the same way).
    if (argv.len() + env.len()).is_multiple_of(2) {
        top -= 8;
    }

    // Everything from here down is 8-byte words, written back-to-front so the
    // final `rsp` lands on `argc`.
    let mut push = |v: u64| {
        top -= 8;
        let i = buf.len() - (stack_top - top) as usize;
        buf[i..i + 8].copy_from_slice(&v.to_le_bytes());
    };
    // The `AT_NULL` terminator is pushed first so it lands highest — musl
    // walks the vector upward and stops at a zero key — then the pairs in
    // reverse, so entry 0 ends up lowest, first above the envp NULL.
    push(AT_NULL);
    push(0);
    let mut i_aux = aux.len();
    while i_aux > 0 {
        i_aux -= 1;
        let (k, v) = aux[i_aux];
        let value = match k {
            AT_RANDOM => random_va,
            AT_EXECFN => execfn,
            _ => v,
        };
        push(value);
        push(k);
    }

    // Pushed last-first, so each vector walks its strings from the lowest
    // address up: string `j`'s address is the top of its region less the
    // lengths of strings `0..=j`.
    push(0); // envp NULL
    let mut end = argv_top;
    for s in env.iter().rev() {
        push(end);
        end += s.len() as u64 + 1;
    }
    push(0); // argv NULL
    let mut end = stack_top - env_bytes - argv.iter().map(|s| s.len() as u64 + 1).sum::<u64>();
    for s in argv.iter().rev() {
        push(end);
        end += s.len() as u64 + 1;
    }
    push(argv.len() as u64); // argc
    top
}

/// Deterministic canary byte. There is no entropy source behind this kernel;
/// a fixed canary defeats naive overflow chains and costs nothing, and is
/// stated here rather than disguised as randomness.
#[must_use]
pub fn canary_byte(i: u64) -> u8 {
    0xA5_u8.rotate_left((i % 8) as u32 * 3) ^ (i as u8).wrapping_mul(0x5A)
}

