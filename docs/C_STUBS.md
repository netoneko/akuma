# C Library Stubs - Future Refactoring

## Current State

Both `sqld` and `qjs` maintain their own C library stub implementations:

- `userspace/sqld/sqlite3/sqlite_stubs.c` (~500 lines)
- `userspace/quickjs/quickjs/stubs.c` (~1300 lines)

These files contain overlapping implementations of standard C library functions needed for bare-metal operation.

## Duplicated Functions

| Function Category | sqld | qjs |
|-------------------|------|-----|
| `memset`, `memcpy`, `memmove`, `memcmp` | ✓ | ✓ |
| `strlen`, `strcmp`, `strcpy`, `strcat` | ✓ | ✓ |
| `strtol`, `strtod`, `atoi` | ✓ | ✓ |
| `printf`, `snprintf`, `vsnprintf` | ✓ | ✓ |
| `malloc`, `free`, `realloc` | Rust FFI | Rust FFI |
| Math functions (`sin`, `cos`, `sqrt`, etc.) | partial | ✓ |
| Time functions (`time`, `gettimeofday`) | ✓ | ✓ |

## Proposed Refactoring

Extract common C stubs into a shared library:

```
userspace/
├── libcstubs/              # NEW: Shared C library stubs
│   ├── Cargo.toml
│   ├── build.rs
│   ├── src/
│   │   └── lib.rs          # Rust FFI exports (malloc, free, etc.)
│   └── c/
│       ├── string.c        # String functions
│       ├── memory.c        # memset, memcpy, etc.
│       ├── stdio.c         # printf family
│       ├── stdlib.c        # strtol, strtod, etc.
│       ├── math.c          # Math functions
│       ├── time.c          # Time functions
│       └── headers/        # Minimal C headers
│           ├── stddef.h
│           ├── stdint.h
│           ├── string.h
│           └── ...
├── sqld/
│   └── (uses libcstubs)
├── quickjs/
│   └── (uses libcstubs)
└── libakuma/
```

## Benefits

1. **DRY**: Single implementation of each function
2. **Easier maintenance**: Bug fixes apply to all users
3. **Smaller binaries**: Shared code linked once
4. **Easier porting**: New C libraries just depend on libcstubs
5. **Better testing**: Test stubs in isolation

## Implementation Notes

### Rust Memory Functions

The `malloc`/`free`/`realloc` implementations are in Rust and should move to `libcstubs`:

```rust
// libcstubs/src/lib.rs
#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void { ... }
#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut c_void) { ... }
#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void { ... }
```

### Build Integration

Each userspace program would depend on `libcstubs`:

```toml
# sqld/Cargo.toml
[dependencies]
libcstubs = { path = "../libcstubs" }

# quickjs/Cargo.toml  
[dependencies]
libcstubs = { path = "../libcstubs" }
```

The `build.rs` for each program would include `libcstubs` headers:

```rust
// quickjs/build.rs
cc::Build::new()
    .file("quickjs/quickjs.c")
    .include("../libcstubs/c/headers")  // Use shared headers
    .compile("quickjs");
```

### Header Compatibility

Some libraries may need slightly different header configurations. Options:
1. Use `#define` flags to customize behavior
2. Allow per-library header overrides
3. Keep library-specific stubs minimal, delegate to libcstubs

## Potential Issues

- **Linking complexity**: May need careful ordering of static libraries
- **Header conflicts**: Different libraries may expect different header layouts
- **Customization**: Some stubs may need library-specific behavior

## Priority

Low priority - current duplication works fine. Consider when:
- Adding a third C library port
- Finding bugs that need fixing in multiple places
- Binary size becomes a concern
