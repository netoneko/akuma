# qjs - QuickJS JavaScript Runtime for Akuma

`qjs` is a userspace JavaScript runtime using Bellard's QuickJS engine, providing ES2020 JavaScript support for the Akuma kernel.

## Quick Start

```bash
# Run a JavaScript file
qjs hello.js

# Execute inline code
qjs -e "console.log('Hello, World!')"

# Evaluate an expression
qjs -e "1 + 2 * 3"
```

## Architecture

```
┌─────────────────┐                    ┌─────────────────┐
│   qjs CLI       │                    │  JavaScript     │
│  (qjs <file>)   │───────────────────►│   Script        │
└────────┬────────┘                    └─────────────────┘
         │
         ▼
┌─────────────────┐
│  QuickJS Engine │
│  (quickjs.c)    │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│   C Stubs       │
│  (stubs.c)      │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│   libakuma      │
│  (syscalls)     │
└─────────────────┘
```

## Usage

### Run a Script File

```bash
qjs script.js
```

Executes the JavaScript file and exits.

### Execute Inline Code

```bash
qjs -e "console.log('hello')"
```

Evaluates the JavaScript code provided on the command line.

### Example Scripts

```javascript
// hello.js
console.log("Hello from QuickJS!");

// Factorial with BigInt
function factorial(n) {
    if (n <= 1) return 1n;
    return BigInt(n) * factorial(n - 1);
}
console.log("20! =", factorial(20).toString());

// Array operations
const nums = [1, 2, 3, 4, 5];
console.log("Sum:", nums.reduce((a, b) => a + b));
console.log("Squares:", nums.map(x => x * x));
```

## JavaScript Features

QuickJS provides full ES2020 support:

- `let`, `const`, arrow functions, classes
- `async`/`await`, Promises
- BigInt for arbitrary precision integers
- Regular expressions
- JSON parsing/stringification
- Map, Set, WeakMap, WeakSet
- TypedArrays (Uint8Array, etc.)
- Destructuring, spread operator
- Template literals
- Modules (import/export) - file loading required

### Console API

```javascript
console.log("message");   // Print to stdout
console.info("info");     // Alias for log
console.warn("warning");  // Alias for log
console.error("error");   // Alias for log
print("message");         // Global print function
```

## Implementation Details

### C Library Stubs

The QuickJS engine requires C library functions. These are provided via `stubs.c`:

| Category | Functions |
|----------|-----------|
| Memory | `memset`, `memcpy`, `memmove`, `memcmp`, `memchr` |
| String | `strlen`, `strcmp`, `strcpy`, `strcat`, `strdup`, `strndup` |
| Math | `sin`, `cos`, `tan`, `sqrt`, `pow`, `exp`, `log`, `floor`, `ceil`, `round`, `trunc` |
| I/O | `printf`, `snprintf`, `vsnprintf`, `puts`, `putchar` |
| Time | `time`, `gettimeofday`, `localtime_r`, `gmtime_r` |
| Conversion | `strtol`, `strtoll`, `strtod`, `atoi` |

### Memory Allocation

Memory is allocated via Rust's global allocator, exposed through FFI:

```rust
#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    // Allocate size + 8 bytes, store size in header
    let layout = Layout::from_size_align(size + 8, 8)?;
    let ptr = alloc(layout);
    *(ptr as *mut usize) = size;
    ptr.add(8) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    // Retrieve size from header, deallocate
    let real_ptr = (ptr as *mut u8).sub(8);
    let size = *(real_ptr as *const usize);
    dealloc(real_ptr, Layout::from_size_align(size + 8, 8)?);
}
```

### JSValue Reference Counting

QuickJS uses reference counting for heap-allocated values. The Rust runtime implements
proper ref-count handling to avoid double-frees:

```rust
pub fn free_value(&self, val: JSValue) {
    const JS_TAG_FIRST: i64 = -11;
    // Only heap values (negative tags) have ref counts
    if (val.tag as u64) >= (JS_TAG_FIRST as u64) {
        let header = val.u.ptr as *mut JSRefCountHeader;
        (*header).ref_count -= 1;
        if (*header).ref_count <= 0 {
            __JS_FreeValue(ctx, val);
        }
    }
}
```

### Build Configuration

QuickJS is compiled with these flags for the `no_std` environment:

```
-nostdinc          # No standard include paths
-ffreestanding     # Freestanding environment
-fno-builtin       # No built-in functions

CONFIG_VERSION="2024-01-13"
CONFIG_BIGNUM      # Enable BigInt support
EMSCRIPTEN         # Minimal runtime mode (disables atomics/threads)
```

### File Reading

Scripts are read using libakuma file syscalls:

```rust
let fd = open(path, O_RDONLY);
let stat = fstat(fd)?;
let mut content = vec![0u8; stat.st_size];
read_fd(fd, &mut content);
close(fd);
```

## Key Implementation Notes

### Avoiding Double Initialization

`JS_NewContext()` internally calls all `JS_AddIntrinsic*` functions. Calling them
again after context creation causes crashes. The solution is to use `JS_NewContext()`
directly without manual intrinsic setup.

### FFI Symbol Linking

QuickJS exports `__JS_FreeValue` as the actual implementation, while `JS_FreeValue`
is a static inline wrapper. Rust FFI uses `#[link_name = "__JS_FreeValue"]` to
bind to the correct symbol.

### Math Function Macros

`isnan`, `isinf`, `isfinite` are implemented using `__builtin_*` compiler intrinsics
to avoid circular macro definitions.

## Binary Size

The compiled `qjs` binary is approximately 700KB:
- QuickJS engine (~600KB)
- C library stubs (~40KB)
- Rust runtime and libakuma (~60KB)

## Comparison with sqld

| Aspect | sqld | qjs |
|--------|------|-----|
| C Library | SQLite (1.8MB) | QuickJS (600KB) |
| Purpose | SQL database | JS runtime |
| Interface | TCP protocol | CLI script runner |
| VFS Layer | Custom SQLite VFS | None needed |
| Memory | FFI malloc/free | FFI malloc/free |
| Build | cc crate, nostdinc | cc crate, nostdinc |

## Files

```
userspace/quickjs/
├── Cargo.toml          # Package manifest
├── README.md           # This file
├── build.rs            # QuickJS compilation script
├── src/
│   ├── main.rs         # CLI entry point, console setup
│   └── runtime.rs      # QuickJS FFI bindings, memory
└── quickjs/
    ├── quickjs.c       # QuickJS engine
    ├── quickjs.h       # QuickJS headers
    ├── cutils.c        # Utility functions
    ├── libbf.c         # BigNum support
    ├── libregexp.c     # Regular expressions
    ├── libunicode.c    # Unicode tables
    ├── stubs.c         # C library stubs
    └── *.h             # Minimal C headers
```

## Limitations

- No file system APIs exposed to JavaScript (yet)
- No networking APIs (yet)
- No REPL mode (file or `-e` execution only)
- `Date` functions return uptime-based values (no RTC)
- No `require()` or dynamic module loading

## Future Work

- [ ] Add `Deno.readFile` / `Deno.writeFile` APIs
- [ ] Add networking APIs for HTTP
- [ ] Implement REPL mode
- [ ] Add RTC support for proper Date
- [ ] Module loading from filesystem
