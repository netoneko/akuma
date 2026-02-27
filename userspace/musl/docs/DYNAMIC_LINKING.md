# Dynamic Linking for Akuma OS

## Goal

Enable Akuma to run dynamically linked aarch64-musl binaries. This would
allow using pre-built packages from Void Linux's `aarch64-musl` repository
instead of cross-compiling everything with `-static`.

## Current state

Akuma currently rejects dynamically linked binaries:

```
src/elf_loader.rs:139-144
```

```rust
// Reject dynamically-linked binaries that require a real interpreter.
for phdr in segments.iter() {
    if phdr.p_type == PT_INTERP && phdr.p_filesz > 1 {
        return Err(ElfError::DynamicallyLinked);
    }
}
```

However, much of the groundwork already exists:

- ELF loader already imports `PT_INTERP` and `PT_PHDR` types
- Auxiliary vector (auxv) is already set up with `AT_PHDR`, `AT_PHNUM`,
  `AT_PHENT`, `AT_PAGESZ`, `AT_ENTRY`, `AT_RANDOM` etc.
- `mmap` and `munmap` syscalls are implemented
- User address space isolation with MMU is working

## What needs to be built

### 1. mprotect syscall (~50 lines)

**Syscall 226** on aarch64 Linux.

The dynamic linker uses `mprotect` to change page permissions after loading
library segments (e.g., mark code pages as read-execute, data as read-write).

Akuma's MMU (`src/mmu.rs`) already tracks page mappings. The implementation
needs to walk the page tables and update permission bits:

```
mprotect(addr, len, prot) -> 0 on success
  PROT_READ  = 0x1
  PROT_WRITE = 0x2
  PROT_EXEC  = 0x4
```

The `user_flags` function in `src/mmu.rs` already maps these to aarch64 page
table bits. `mprotect` just needs to update existing mappings.

### 2. PT_INTERP handling in ELF loader (~150 lines)

Instead of rejecting binaries with `PT_INTERP`, the kernel should:

1. Read the interpreter path from the `PT_INTERP` segment
   (typically `/lib/ld-musl-aarch64.so.1`)
2. Load the interpreter ELF into the process address space at a
   non-conflicting base address (e.g., `0x7000_0000`)
3. Set `AT_BASE` in auxv to the interpreter's load address
4. Set `AT_ENTRY` to the *program's* entry point (not the interpreter's)
5. Start execution at the *interpreter's* entry point

The interpreter then uses AT_PHDR/AT_PHNUM to find the program's headers,
loads its shared libraries, performs relocations, and jumps to AT_ENTRY.

```
Current flow:  kernel loads ELF -> jump to ELF entry
New flow:      kernel loads ELF + interpreter -> jump to interpreter entry
                -> interpreter loads .so files -> jump to ELF entry
```

### 3. Cross-compile musl as a shared library (build step)

musl's build system can produce both `libc.a` (static) and `libc.so` (shared).
The shared library IS the dynamic linker — musl combines them into one file.

```bash
cd userspace/musl/musl
./configure --host=aarch64-linux-musl --prefix=/usr \
    --syslibdir=/lib --disable-static
make
# Produces: lib/ld-musl-aarch64.so.1 (this is also libc.so)
```

Install to the Akuma disk image:
```
/lib/ld-musl-aarch64.so.1    # dynamic linker + libc
/usr/lib/libc.so -> /lib/ld-musl-aarch64.so.1
```

### 4. Verify/fix mmap MAP_FIXED (~small)

The dynamic linker uses `MAP_FIXED` to place library segments at exact
addresses. Verify that Akuma's `sys_mmap` handles `MAP_FIXED` correctly
(places mapping at the exact requested address, potentially overwriting
existing mappings at that range).

### 5. Additional syscalls (incremental)

These will be needed as more dynamically linked programs are tested:

| Syscall | Number | Priority | Used by |
|---------|--------|----------|---------|
| mprotect | 226 | Required | Dynamic linker (page permissions) |
| readlinkat | 78 | High | musl init (`/proc/self/exe`) |
| futex | 98 | High | Thread synchronization, malloc |
| sigaltstack | 132 | Medium | Signal handling setup |
| prlimit64 | 261 | Medium | Stack size limits |
| set_robust_list | 99 | Low | Thread cleanup (can stub as 0) |
| getrlimit | 163 | Low | Resource limits (can stub) |

## Implementation order

```
Phase 1: Minimal dynamic linking
  1. Implement mprotect syscall
  2. Modify ELF loader to handle PT_INTERP
  3. Cross-compile musl as shared lib
  4. Test with a trivial dynamically linked hello world

Phase 2: Real-world packages
  5. Stub missing syscalls as they surface (readlinkat, futex, etc.)
  6. Test with busybox from Void Linux aarch64-musl
  7. Test with coreutils, grep, sed, etc.

Phase 3: Full compatibility
  8. Implement proper signal handling
  9. Implement futex for multi-threaded programs
  10. Test with complex packages (python, git, etc.)
```

## Effort estimate

| Phase | Work | Time |
|-------|------|------|
| Phase 1 | mprotect + PT_INTERP + musl build | 3-5 days |
| Phase 2 | Incremental syscall stubs | 1-2 weeks |
| Phase 3 | Proper signal/thread support | 2-4 weeks |

## Payoff

Once Phase 2 is complete, Akuma can use Void Linux's `aarch64-musl`
package repository (~12,000 packages) via xbps-install. No more
cross-compiling everything from source.

## References

- musl dynamic linker source: `userspace/musl/musl/ldso/dynlink.c` (~3000 lines)
- musl startup: `userspace/musl/musl/ldso/dlstart.c`
- Akuma ELF loader: `src/elf_loader.rs`
- Akuma MMU: `src/mmu.rs` (page table management, `user_flags`)
- Akuma process setup: `src/process.rs` (stack setup, auxv)
- Linux auxv spec: `userspace/musl/musl/include/sys/auxv.h`
