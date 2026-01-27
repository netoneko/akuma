# CGI Support in Akuma

Akuma's httpd server supports the Common Gateway Interface (CGI) for executing dynamic scripts and binaries.

## Overview

CGI scripts are placed in `/public/cgi-bin/` and executed when accessed via HTTP requests to `/cgi-bin/*` paths.

## Features

- **GET and POST support** - Query strings and POST body data
- **Interpreter mapping** - Automatic interpreter selection based on file extension
- **Header parsing** - Scripts can set Content-Type and other headers
- **stdin/stdout** - POST data passed via stdin, response read from stdout

## Configuration

Constants in `userspace/httpd/src/main.rs`:

```rust
/// Maximum size of CGI response in bytes (64KB default)
const CGI_MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// CGI process timeout in milliseconds (5 seconds)
const CGI_TIMEOUT_MS: u32 = 5000;
```

## Interpreter Mapping

File extensions are mapped to interpreters:

| Extension | Interpreter | Example |
|-----------|-------------|---------|
| `.js` | `/bin/qjs` | JavaScript via QuickJS |
| (none) | Direct execution | ELF binaries |

To add more interpreters, modify `get_interpreter()` in httpd.

## Writing CGI Scripts

### JavaScript Example

```javascript
// /public/cgi-bin/hello.js

// Read POST data (if any)
var postData = readStdin();

// Output CGI headers
console.log("Content-Type: text/html");
console.log("");  // Blank line separates headers from body

// Output body
console.log("<html>");
console.log("<body>");
console.log("<h1>Hello from CGI!</h1>");
if (postData.length > 0) {
    console.log("<p>POST data: " + postData + "</p>");
}
console.log("</body>");
console.log("</html>");
```

### CGI Header Format

Scripts output headers followed by a blank line, then the body:

```
Content-Type: text/html

<html>...</html>
```

Supported headers:
- `Content-Type` - MIME type of response (default: `text/plain`)

### Command-Line Arguments

CGI scripts receive request information as arguments:

For interpreted scripts (e.g., JavaScript):
```
/bin/qjs /public/cgi-bin/script.js <METHOD> <QUERY_STRING>
```

For ELF binaries:
```
/public/cgi-bin/binary <METHOD> <QUERY_STRING>
```

Where:
- `METHOD` - HTTP method (GET, POST)
- `QUERY_STRING` - URL query string (e.g., `name=value&foo=bar`)

### Reading POST Data

For POST requests, the request body is passed via stdin:

**JavaScript:**
```javascript
var postData = readStdin();
```

**ELF binaries:**
Read from file descriptor 0 (stdin).

## Testing

```bash
# GET request
curl http://localhost:8080/cgi-bin/hello.js

# GET with query string
curl "http://localhost:8080/cgi-bin/hello.js?name=world"

# POST request
curl -X POST -d "data=test" http://localhost:8080/cgi-bin/hello.js
```

## Limitations

- No environment variables (use command-line arguments instead)
- Maximum response size limited by `CGI_MAX_RESPONSE_BYTES`
- Process timeout after `CGI_TIMEOUT_MS` milliseconds
- Single-threaded request handling

## Implementation Details

### Request Flow

1. httpd receives HTTP request
2. Detects `/cgi-bin/` path prefix
3. Parses query string from URL
4. For POST: extracts body from request
5. Determines interpreter from file extension
6. Spawns process with `spawn_with_stdin()`
7. Reads stdout until process exits
8. Parses CGI headers from output
9. Sends HTTP response with body

### Kernel Support

The CGI implementation uses:
- `SPAWN` syscall with stdin support
- `WAITPID` syscall for process completion
- `ChildStdout` file descriptor for reading output

### Files

- `userspace/httpd/src/main.rs` - CGI handler implementation
- `userspace/libakuma/src/lib.rs` - `spawn_with_stdin()` function
- `src/syscall.rs` - `sys_spawn()` with stdin support
