# Step B: Musl Porting Experiment - Technical Record (Final)

This document records the final successful steps taken to run `musl` on Akuma OS.

## 1. Accomplishments
- **Musl Successfully Executed**: The first Musl-linked binary (`hello_musl.bin`) successfully initialized and printed to the console.
- **Syscall Integration**: 
    - `writev` (66) implemented to support Musl's buffered I/O.
    - `exit_group` (94) added for standard process termination.
    - `set_tid_address` (96) and `rt_sigprocmask` (135) stubbed to satisfy Musl initialization.
- **Stack Alignment**: Verified and implemented strict 16-byte stack alignment for `argc`, `argv`, and `AuxV`.
- **TLS Support**: Verified `TPIDR_EL0` is correctly handled by the kernel and can be set via Musl's `__set_thread_area`.

## 2. Compilation and Linking Details
Refer to `STEP_B_RECORD.md` for the specific `clang` and `rust-lld` commands used to generate the test binary.

## 3. Key Findings
- Musl relies heavily on `AuxV` for hardware detection (`AT_HWCAP`) and initialization.
- The `writev` syscall is the primary path for `printf` and `puts` in Musl.
- Moving internal kernel thread tracking to `TPIDRRO_EL0` was critical to prevent corruption when Musl sets up its own Thread-Local Storage.
