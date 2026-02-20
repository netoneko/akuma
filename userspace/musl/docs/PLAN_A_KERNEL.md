# Step A: Kernel Preparation for Musl

To support `musl` libc, the Akuma kernel must move closer to the Linux AArch64 ABI. This plan outlines the required changes to the kernel's syscall interface and process loading mechanism.

## 1. Linux-Compatible Syscall Numbers
Update `src/syscall.rs` to use standard Linux AArch64 syscall numbers. This prevents us from having to patch every syscall site in `musl`.

| Syscall | Akuma (Current) | Linux AArch64 |
|---------|-----------------|---------------|
| EXIT    | 0               | 93            |
| READ    | 1               | 63            |
| WRITE   | 2               | 64            |
| BRK     | 3               | 214           |
| OPENAT  | 56              | 56 (Match!)   |
| CLOSE   | 57              | 57 (Match!)   |
| LSEEK   | 62              | 62 (Match!)   |

**Action:**
- Refactor `src/syscall.rs` constants.
- Ensure all existing userspace apps (like `tcc` and `libakuma`) are updated to use these new numbers.

## 2. Linux ELF Stack Layout
`musl`'s entry point (`crt1.o`) expects the stack to be initialized by the kernel according to the System V ABI for AArch64.

**Required Stack State at Entry:**
- `[sp]` : `argc` (number of arguments)
- `[sp+8]` : `argv[0]` pointer
- `...`
- `[sp+8*argc]` : `argv[argc]` (NULL)
- `[sp+8*argc+8]` : `envp[0]` pointer
- `...`
- `NULL` (end of environment)
- **Auxiliary Vector (AuxV):** A list of `(type, value)` pairs terminated by `AT_NULL`.

**Action:**
- Modify `src/process.rs` (or the ELF loader) to properly push these values onto the user stack before starting a process.
- At a minimum, provide `AT_PAGESZ` (4096) and `AT_PHDR` / `AT_PHENT` / `AT_PHNUM` if we want to support dynamic linking later.

## 3. Thread-Local Storage (TLS) Support
`musl` relies on TLS. On AArch64, this involves the `TPIDR_EL0` register.

**Action:**
- Ensure the kernel handles `set_tid_address` (syscall 96) or at least stubs it.
- Ensure `TPIDR_EL0` is saved and restored during context switches in `src/threading.rs`.
- Implement `set_tpidr_el0` (usually via `msr tpidr_el0, x0`) for the current process.

## 4. Basic Memory Management
- **`brk` improvements:** Ensure `sys_brk` correctly handles memory growth.
- **`mmap` flags:** `musl` uses various `PROT_*` and `MAP_*` flags. Ensure the kernel validates or gracefully ignores flags it doesn't yet support.
