# Step C: Integration with TCC and Sysroot

The final step is to replace the rudimentary stub libc with `musl` for all C applications compiled within Akuma.

## 1. Updating TCC Sysroot
The `tcc` compiler running inside Akuma currently stages its own sysroot.

**Action:**
- Update `userspace/tcc/build.rs` to stop embedding `lib/libc.c`.
- Embed the `libc.a` and headers from the `musl` build instead.
- Update the `install` logic in `userspace/tcc/src/main.rs` to copy `musl`'s `libc.a`, `crt1.o`, `crti.o`, and `crtn.o` to `/usr/lib/`.

## 2. Standardizing Headers
`musl` provides a full set of POSIX headers.

**Action:**
- Remove the custom headers in `userspace/tcc/include/`.
- Replace them with the comprehensive headers from `musl/include/`.

## 3. TCC Wrapper Adjustments
The `tcc` binary might need to be informed of the new default library paths and start files.

**Action:**
- Ensure `tcc` is configured to look for `crt1.o` and `libc.a` in `/usr/lib` by default.
- Verify that TCC's own internal `libtcc1.a` (which provides helper functions for math and atomics) remains compatible with `musl`.

## 4. Full System Verification
- Compile `hello_world` using the new TCC + Musl setup.
- Attempt to compile and run more complex C programs (e.g., a simple shell, basic utilities).
- Verify that `malloc`, `printf`, and file I/O work as expected across the entire POSIX spectrum provided by `musl`.
