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
| Memory | `memset`, `memcpy`, `memmove`, `memcmp` |
| String | `strlen`, `strcmp`, `strcpy`, `strcat`, `strdup` |
| Math | `sin`, `cos`, `tan`, `sqrt`, `pow`, `exp`, `log`, `floor`, `ceil` |
| I/O | `printf`, `snprintf`, `puts`, `putchar` |
| Time | `time`, `gettimeofday`, `localtime_r` |

### Memory Allocation

Memory is allocated via Rust's global allocator, exposed through FFI:

```rust
#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void { ... }

#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut c_void) { ... }

#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void { ... }
```

### Build Configuration

QuickJS is compiled with these flags for the `no_std` environment:

```
-nostdinc          # No standard include paths
-ffreestanding     # Freestanding environment
-fno-builtin       # No built-in functions

CONFIG_VERSION="2024-01-13"
CONFIG_BIGNUM      # Enable BigInt support
EMSCRIPTEN         # Minimal runtime mode
```

### File Reading

Scripts are read using libakuma file syscalls:

```rust
let fd = open(path, O_RDONLY);
let stat = fstat(fd)?;
let content = read_fd(fd, buffer);
close(fd);
```

## Binary Size

The compiled `qjs` binary is approximately 700KB, which includes:
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
├── build.rs            # QuickJS compilation script
├── src/
│   ├── main.rs         # CLI entry point
│   └── runtime.rs      # QuickJS FFI bindings
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
