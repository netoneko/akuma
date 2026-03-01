# On-Demand ELF Loader for Large Binaries

## Problem

The kernel's `read_file()` path buffers the entire ELF binary into kernel heap
before parsing. The ext2 filesystem enforces a 16MB safety limit in
`read_inode_data()` to prevent OOM, and the kernel heap itself is only 32MB.

This means any binary larger than 16MB fails with:
```
Error: Failed to read /bin/bun: Internal error
```

The bun runtime binary is ~93MB, making it impossible to load through the
existing path.

## Solution

Added an on-demand ELF loader that reads segment data page-by-page from disk
using `vfs::read_at()`, never buffering more than one 4KB page at a time.

### New functions

**`src/elf_loader.rs`:**

- `load_elf_from_path(path, file_size)` — Manually parses the 64-byte ELF64
  header and program header table from a small buffer read via `read_at()`.
  Then for each PT_LOAD segment, reads file data one page at a time and copies
  it directly into the user address space. Peak kernel heap usage is ~4KB
  regardless of binary size.

- `load_elf_with_stack_from_path(path, file_size, args, env, stack_size)` —
  Wrapper that calls `load_elf_from_path` and sets up the user stack, auxiliary
  vector, and pre-allocated heap pages (same as `load_elf_with_stack`).

**`src/process.rs`:**

- `Process::from_elf_path(name, path, file_size, args, env)` — Creates a new
  process using the on-demand loader.

- `Process::replace_image_from_path(path, file_size, args, env)` — Replaces the
  current process image using on-demand loading (for execve of large binaries).

### Fallback logic

Both `spawn_process_with_channel_ext()` and `do_execve()` now try `read_file()`
first. If it fails (file exceeds 16MB limit), they fall back to the on-demand
path:

```
read_file(path) → success → from_elf(elf_data)
               → FsError  → file_size(path) → from_elf_path(path, size)
```

Small binaries (<16MB) still use the original buffered path with the `elf` crate
parser. Large binaries use the manual header parser + `read_at()`.

### Manual ELF parsing

The on-demand loader parses ELF64 headers directly instead of using the `elf`
crate's `ElfBytes::minimal_parse()`, which requires the full file buffer. The
manual parser reads:

- ELF64 header (64 bytes): magic, class, endianness, type, machine, entry point,
  program header offset/count/size
- Program headers (56 bytes each): type, flags, offset, vaddr, filesz, memsz

### Limitations

- **No kernel-side relocations for large ET_EXEC binaries.** The `elf` crate is
  needed to parse SHT_RELA sections (at the end of the file), which the on-demand
  loader skips. This only affects non-PIE ET_EXEC binaries. All modern large
  binaries (bun, node, etc.) are ET_DYN (PIE) and self-relocate at startup.

- **Interpreter loading still uses `read_file()`.** The dynamic linker
  (ld-musl-aarch64.so.1) is typically <1MB, well within the 16MB limit.

## Memory impact

| Binary size | Old path (buffered) | New path (on-demand) |
|-------------|--------------------|--------------------|
| 1 MB        | 1 MB heap          | 1 MB heap (uses old path) |
| 93 MB       | FAILS (>16MB)      | ~4 KB heap + user pages |

The on-demand loader allocates user-space pages through the PMM (physical memory
manager), which draws from the user pages pool — separate from the kernel heap.

---

## Dynamic Virtual Address Space

### Problem

The original kernel used a fixed 1GB user address space (STACK_TOP = 0x40000000).
Large binaries like bun (93MB code) with a dynamic linker need significantly more
VA space:

- ELF segments span 0x200000–0x5C6D418 (~92MB)
- Interpreter loaded at 0x30000000
- bun's arena allocator reserves 1GB+ contiguous VA regions via mmap

### Solution: `compute_stack_top()`

`src/elf_loader.rs` now dynamically computes `STACK_TOP` based on binary layout:

- **Small static binaries** (code < 64MB, no interpreter): keep default 1GB layout
- **Large or dynamic binaries**: expand to provide ~2GB mmap space, with
  `STACK_TOP` up to 3GB (0xC0000000)

The mmap region sits between the interpreter end (0x30100000) and the stack
bottom, giving large binaries ~2.3GB of mmap space.

### User page table fix (L1 BLOCK descriptor bug)

`add_kernel_mappings()` in `src/mmu.rs` created a 1GB BLOCK descriptor at
L1\[2\] mapping VA 2–3GB → PA 0x80000000. With only 256MB RAM (PA
0x40000000–0x4FFFFFFF), PA 0x80000000 does not exist.

When `map_page` tried to map the user stack at VA ~0xBFFC0000 (in the 2–3GB
range), `get_or_create_table` saw L1\[2\] had `VALID` set and treated the
BLOCK descriptor as a TABLE descriptor — extracting PA 0x80000000 as a page
table pointer. Accessing `phys_to_virt(0x80000000)` = 0x80000000 (identity-
mapped but no RAM) caused a synchronous external abort (EC=0x25, ISS=0x10,
FAR=0x80000FF8).

**Fix:** Removed the bogus L1\[2\] block mapping and hardened
`get_or_create_table` to distinguish BLOCK (bit\[1\]=0) from TABLE
(bit\[1\]=1) descriptors. If a BLOCK is encountered where a TABLE is needed,
it is replaced with a fresh zeroed page table.

## Pointer Validation Fix

### Problem

`validate_user_ptr()` and `copy_from_user_str()` in `src/syscall.rs` rejected
any user pointer above 0x40000000 (the old 1GB limit). With the dynamic address
space, bun's stack lives at 0xBFFC0000–0xC0000000. Every syscall the dynamic
linker made — `openat` with a filename on the stack, `writev` for error
messages — was silently rejected with EFAULT. The linker couldn't search for
libraries or print diagnostics, so it called `_exit(127)`.

### Fix

Both functions now call `user_va_limit()` which reads `stack_top` from the
current process's `ProcessMemory`, falling back to 0x40000000 when no process
is active. This makes the validation limit match the actual address space layout
of each process.
