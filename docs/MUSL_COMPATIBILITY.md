# Musl Compatibility and Linux ABI Realignment

This document details the changes made to the Akuma kernel and userspace to support `musl` libc and move closer to the standard Linux AArch64 ABI.

## 1. Syscall Number Realignment
Akuma originally used custom syscall numbers starting from 0. To support standard C libraries without extensive patching, the kernel now uses standard Linux AArch64 syscall numbers where possible.

| Syscall | Old Number | New Number (Linux) |
|---------|------------|---------------------|
| EXIT    | 0          | 93                  |
| READ    | 1          | 63                  |
| WRITE   | 2          | 64                  |
| BRK     | 3          | 214                 |
| GETRANDOM| 304       | 278                 |
| UPTIME  | 216        | 319 (Custom)        |

Other standard syscalls like `OPENAT` (56) and `CLOSE` (57) already matched and remain unchanged.

## 2. Linux ELF Stack Layout
Standard C library entry points (`crt1.o`) expect the kernel to initialize the user stack according to the System V ABI for AArch64.

### New Stack Structure (Top to Bottom):
1.  **Argument Strings**: The actual string data for CLI arguments.
2.  **Auxiliary Vector (AuxV)**: Information about the binary and system environment.
    - `AT_PHDR`, `AT_PHNUM`, `AT_PHENT`: Program header info for the dynamic linker/libc.
    - `AT_PAGESZ`: System page size (4096).
    - `AT_ENTRY`: Original entry point of the ELF.
    - `AT_NULL`: Terminator.
3.  **Environment Pointers**: Currently a single NULL pointer (empty environment).
4.  **Argument Pointers (argv)**: Pointers to the strings at the top of the stack, terminated by NULL.
5.  **Argument Count (argc)**: The number of arguments.

The User Stack Pointer (`sp_el0`) now points directly to `argc` at process start.

## 3. Thread-Local Storage (TLS) and Register Migration
`musl` and other modern C libraries use the `TPIDR_EL0` register for the Thread Pointer (TLS).

### The Conflict
Akuma previously used `TPIDR_EL0` to store the internal kernel Thread ID (tid) for scheduling. Using this register for TLS would have caused the kernel to lose track of which thread was running whenever userspace initialized TLS.

### The Solution: Register Shuffling
We have migrated the kernel's Thread ID tracking to `TPIDRRO_EL0`.

1.  **`TPIDRRO_EL0` (Thread ID)**:
    - Used by the kernel to store the `usize` Thread ID.
    - Accessible (Read/Write) from EL1 (Kernel).
    - Read-only from EL0 (User), which is safe as userspace shouldn't modify its own TID.
2.  **`TPIDR_EL0` (User TLS)**:
    - Now exclusively reserved for userspace TLS.
    - The kernel saves and restores this register during every context switch and exception entry/exit.
    - A new custom syscall `SET_TPIDR_EL0` (320) allows userspace to set this register.
3.  **`TPIDR_EL1` (Exception Stack)**:
    - Remains unchanged; used to store the top of the per-thread exception stack for the kernel.

## 4. Kernel Structural Changes
- **`src/syscall.rs`**: Updated `nr` constants and added `sys_set_tpidr_el0`.
- **`src/elf_loader.rs`**: Completely rewrote stack initialization logic to support strings and the Auxiliary Vector.
- **`src/threading.rs`**:
    - Updated `Context` and `UserContext` structs to include `user_tls`.
    - Updated `switch_context` assembly to use `TPIDRRO_EL0` for ID and preserve `TPIDR_EL0`.
    - Updated `setup_fake_irq_frame` to initialize the 304-byte frame layout.
- **`src/exceptions.rs`**:
    - Increased Trap Frame size to 304 bytes.
    - Updated `sync_el0_handler` and `irq_handler` to save/restore `TPIDR_EL0`.

## 5. Userspace Impact
- **`libakuma`**: All syscall constants updated. Existing Rust apps are transparently compatible.
- **`tcc`**: Stub libc updated to use the new Linux syscall numbers.
