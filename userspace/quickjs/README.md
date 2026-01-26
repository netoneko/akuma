# QuickJS - JavaScript Runtime for Akuma

A minimal JavaScript runtime using Bellard's QuickJS engine.

## Quick Start

```bash
# Run a JavaScript file
qjs script.js

# Execute inline code
qjs -e "console.log('Hello, World!')"

# Print expression result
qjs -e "1 + 2 * 3"
```

## Features

- Full ES2020 JavaScript support via QuickJS
- `console.log`, `console.info`, `console.warn`, `console.error`
- Global `print()` function
- BigInt support
- Regular expressions
- JSON parsing/stringification
- Promises
- TypedArrays
- Map/Set

## Example Scripts

```javascript
// hello.js
console.log("Hello from QuickJS!");

// Calculate factorial
function factorial(n) {
    if (n <= 1) return 1n;
    return BigInt(n) * factorial(n - 1);
}
console.log("20! =", factorial(20).toString());

// Array operations
const nums = [1, 2, 3, 4, 5];
console.log("Sum:", nums.reduce((a, b) => a + b));
```

## Implementation

QuickJS is compiled for bare-metal aarch64 with custom C library stubs:

| Component | Implementation |
|-----------|---------------|
| Memory | Rust allocator via FFI malloc/free |
| Console | libakuma stdout |
| File I/O | libakuma open/read/close |
| Time | libakuma uptime |

## Build

```bash
cd userspace
./build.sh
```

The `qjs` binary is copied to `bootstrap/bin/`.

## Limitations

- No file system APIs exposed to JavaScript yet
- No networking APIs yet
- No REPL mode (file or -e execution only)
- Time/Date functions return uptime-based values
