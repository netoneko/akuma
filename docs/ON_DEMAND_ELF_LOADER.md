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
