# Dynamic Linking for Akuma OS

## Status: Working

Akuma can run dynamically linked aarch64-musl binaries. The dynamic linker
(`ld-musl-aarch64.so.1`) is loaded by the kernel and handles symbol
resolution, relocations, and transfer to the program's entry point.

## How it works

When the kernel loads an ELF binary with a `PT_INTERP` segment:

1. The kernel loads the main binary's `PT_LOAD` segments as usual
2. Reads the interpreter path from `PT_INTERP` (e.g. `/lib/ld-musl-aarch64.so.1`)
3. Loads the interpreter ELF from the filesystem at base address `0x3000_0000`
4. Applies relocations to the interpreter (RELATIVE, GLOB_DAT, JUMP_SLOT, ABS64)
5. Sets up the auxiliary vector with `AT_BASE` pointing to the interpreter
6. Starts execution at the interpreter's entry point (not the program's)
7. The interpreter reads `AT_PHDR`/`AT_ENTRY` from auxv, resolves symbols,
   and jumps to the program's entry point

```
Kernel loads ELF + interpreter
  → Jump to interpreter entry (0x3000_0000 + e_entry)
    → Interpreter loads .so files via AT_PHDR
      → Perform relocations
        → Jump to AT_ENTRY (program's main)
```

## Memory layout (dynamically linked binary)

```
0x0000_0000  (unmapped)
0x0040_0000  ET_EXEC code (traditional static binaries)
0x1000_0000  PIE binary base (ET_DYN main binaries)
  ...        heap (brk grows up, ~256MB gap)
0x2000_0000  mmap region (grows up)
0x3000_0000  interpreter base (ld-musl-aarch64.so.1, ~700KB)
0x3fec_0000  guard page
0x3ffc_0000  stack (grows down)
0x4000_0000  kernel
```

## Kernel changes

### ELF loader (`src/elf_loader.rs`)

- `PT_INTERP` handling: reads interpreter path, calls `load_interpreter()`
- `load_interpreter()`: loads interpreter segments at `INTERP_BASE` (0x3000_0000),
  applies RELA relocations (RELATIVE, GLOB_DAT, JUMP_SLOT, ABS64) using the
  interpreter's own dynamic symbol table
- `LoadedElf` struct extended with `interp: Option<InterpInfo>` containing
  the interpreter's entry point and base address
- `load_elf_with_stack()` sets `AT_BASE` in auxv and starts execution at the
  interpreter's entry point when an interpreter is present

### mprotect syscall (`src/syscall.rs`)

Syscall 226. Walks the page tables for `[addr, addr+len)` and updates
permission bits via `update_page_flags()`. Used by the dynamic linker to
change page permissions after loading library segments.

### mmap improvements (`src/syscall.rs`)

- **prot flags honored**: `from_prot()` in `src/mmu.rs` converts Linux
  `PROT_READ`/`PROT_WRITE`/`PROT_EXEC` to AArch64 page table bits.
  File-backed mmaps are initially mapped RW for data copy, then permissions
  are applied afterward.
- **MAP_FIXED**: when `flags & 0x10`, the provided address is used directly
  instead of allocating from the mmap bump allocator. Existing pages at that
  range are unmapped first.

### MMU (`src/mmu.rs`)

- `user_flags::from_prot(prot: u32) -> u64`: converts Linux prot bitmask to
  AArch64 page table descriptor bits
- `UserAddressSpace::update_page_flags()`: modifies permission bits of an
  existing L3 page table entry without reallocating the page

### Additional syscall stubs (`src/syscall.rs`)

- **futex** (98): basic WAIT/WAKE with yield-based scheduling
- **prlimit64** (261): returns stack size and fd limits
- **getrlimit** (163): delegates to prlimit64
- **sigaltstack** (132): stub returning 0
- **set_robust_list** (99): stub returning 0

## Getting the dynamic linker

The dynamic linker comes from Alpine Linux's `musl` package. Install it
on a running Akuma system:

```
apk add musl
```

This places `/lib/ld-musl-aarch64.so.1` on the filesystem. The Alpine
build (GCC-based) includes `libgcc` builtins for 128-bit float operations
that aarch64 `long double` requires.

Building musl as a shared library from source with clang on macOS does NOT
work because Homebrew's LLVM only ships compiler-rt for darwin targets, not
for `aarch64-linux`. The resulting `.so` would be missing symbols like
`__floatunsitf`, `__getf2`, `__addtf3`, etc.

## Building dynamically linked binaries

Use the `aarch64-linux-musl-gcc` cross-compiler without `-static`:

```bash
aarch64-linux-musl-gcc hello.c -o hello
# produces ET_EXEC with PT_INTERP = /lib/ld-musl-aarch64.so.1
```

The resulting binary's `PT_INTERP` points to `/lib/ld-musl-aarch64.so.1`.
The kernel reads this path and loads the interpreter from the Akuma
filesystem.

## Static linking is unaffected

The TCC toolchain and all existing userspace binaries use `musl/dist/lib/libc.a`
(static library) built by `userspace/musl/build.rs` with `--disable-shared`.
The dynamic linker is a separate runtime artifact and does not affect static
builds.

## Verified working

- `hello_dynamic.bin`: trivial `printf("Hello")` compiled without `-static`
- musl 1.2.5 dynamic linker from Alpine Linux (723 KB)
- Kernel applies 21 relocations to interpreter, boots in ~10ms
- Clean exit code 0
