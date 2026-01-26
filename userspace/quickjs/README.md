# QuickJS - JavaScript Runtime for Akuma

A minimal JavaScript runtime using Bellard's QuickJS engine, running on the Akuma kernel.

## Quick Start

```bash
# Run a JavaScript file
qjs script.js

# Execute inline code
qjs -e "console.log('Hello, World!')"

# Evaluate an expression
qjs -e "1 + 2 * 3"
```

## Features

- Full ES2020 JavaScript support via QuickJS
- `console.log`, `console.info`, `console.warn`, `console.error`
- Global `print()` function
- BigInt support for arbitrary precision integers
- Regular expressions
- JSON parsing/stringification
- Promises and async/await
- TypedArrays (Uint8Array, etc.)
- Map, Set, WeakMap, WeakSet
- Classes, arrow functions, destructuring
- Template literals

## Example Scripts

```javascript
// hello.js
console.log("Hello from QuickJS!");

// Calculate factorial with BigInt
function factorial(n) {
    if (n <= 1) return 1n;
    return BigInt(n) * factorial(n - 1);
}
console.log("20! =", factorial(20).toString());

// Array operations
const nums = [1, 2, 3, 4, 5];
console.log("Sum:", nums.reduce((a, b) => a + b));
console.log("Squares:", nums.map(x => x * x));

// Object destructuring
const { name, age } = { name: "Alice", age: 30 };
console.log(`${name} is ${age} years old`);
```

## Architecture

```
┌─────────────────┐
│   qjs CLI       │  Rust entry point (main.rs)
│  (qjs <file>)   │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│  Runtime        │  Rust FFI bindings (runtime.rs)
│  (JSValue etc.) │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│  QuickJS Engine │  C code (quickjs.c, libbf.c, etc.)
│  (55k lines)    │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│   C Stubs       │  C library replacements (stubs.c)
│  (stubs.c)      │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│   libakuma      │  Kernel syscall wrappers
│  (syscalls)     │
└─────────────────┘
```

## Implementation Details

### Memory Management

Memory is allocated via Rust's global allocator, exposed through FFI:

```rust
#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void { ... }
#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut c_void) { ... }
#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void { ... }
```

A size header is stored before each allocation to support `free` and `realloc`.

### JSValue Reference Counting

QuickJS uses reference counting for heap-allocated values (objects, strings, etc.). 
The Rust runtime implements proper ref-count management:

```rust
pub fn free_value(&self, val: JSValue) {
    if value_has_ref_count(val) {
        let header = val.u.ptr as *mut JSRefCountHeader;
        (*header).ref_count -= 1;
        if (*header).ref_count <= 0 {
            __JS_FreeValue(ctx, val);
        }
    }
}
```

### Console API

The `console` object is set up with native Rust callbacks:

```javascript
console.log("message");   // Print to stdout
console.info("info");     // Alias for log
console.warn("warning");  // Alias for log  
console.error("error");   // Alias for log
print("message");         // Global print function
```

### Build Configuration

QuickJS is compiled with these flags for the `no_std` environment:

```
-nostdinc          # No standard include paths
-ffreestanding     # Freestanding environment
-fno-builtin       # No built-in functions

CONFIG_BIGNUM      # Enable BigInt support
EMSCRIPTEN         # Minimal runtime mode (disables atomics)
```

## Build

```bash
cd userspace
./build.sh
```

The `qjs` binary is copied to `bootstrap/bin/`.

## File Structure

```
userspace/quickjs/
├── Cargo.toml          # Package manifest
├── build.rs            # QuickJS compilation script
├── src/
│   ├── main.rs         # CLI entry point, console setup
│   └── runtime.rs      # QuickJS FFI bindings, memory functions
└── quickjs/
    ├── quickjs.c       # QuickJS engine (55k lines)
    ├── quickjs.h       # QuickJS public headers
    ├── cutils.c        # Utility functions
    ├── libbf.c         # BigNum/BigFloat support
    ├── libregexp.c     # Regular expression engine
    ├── libunicode.c    # Unicode tables
    ├── stubs.c         # C library stub implementations
    └── *.h             # Minimal C header shims
```

## Limitations

- No file system APIs exposed to JavaScript (yet)
- No networking APIs (yet)
- No REPL mode (file or `-e` execution only)
- `Date` functions return uptime-based values (no RTC)
- No `require()` or dynamic module loading

## Future Work

- Add file system APIs (`readFile`, `writeFile`)
- Add networking/HTTP APIs
- Implement REPL mode
- Add proper RTC support for Date
- Module loading from filesystem
