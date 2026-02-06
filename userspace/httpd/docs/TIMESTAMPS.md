# High-Precision Timestamp Support in `httpd`

The `httpd` server has been updated to utilize Akuma's high-precision timestamp support for both logging and protocol compliance.

## Key Changes

### 1. RFC 1123 Date Formatting
- Implemented `format_time_rfc1123(us: u64)` to convert Akuma's microsecond-precision Unix timestamps into the standard format required by the HTTP protocol (e.g., `Fri, 06 Feb 2026 14:30:00 GMT`).
- Added logic to handle leap years and day-of-week calculations since the Unix epoch.

### 2. HTTP Protocol Compliance
- Added the `Date` header to all HTTP responses. This includes:
    - Standard file responses (`200 OK`).
    - Error responses (e.g., `404 Not Found`, `500 Internal Server Error`).
    - CGI script responses.

### 3. High-Precision Logging
- Updated the request logger to prefix every incoming request with a high-precision timestamp.
- Logs now follow the format: `[Date] METHOD PATH`.
- This allows for precise performance monitoring and request ordering.

### 4. Robust Networking
- By virtue of updating `libakuma`, `httpd` now more reliably handles `WouldBlock` conditions in its networking stack, ensuring that the short-timeout high-precision syscalls do not cause premature connection failures.

## Verification

When running `httpd`, you will now see log output similar to:
```text
[Fri, 06 Feb 2026 14:35:22 GMT] GET /index.html
[Fri, 06 Feb 2026 14:35:25 GMT] CGI GET /cgi-bin/hello.js
```

Inspecting the response with a tool like `curl -v` will show the new header:
```text
< HTTP/1.0 200 OK
< Date: Fri, 06 Feb 2026 14:35:22 GMT
< Content-Type: text/html; charset=utf-8
< Content-Length: 123
...
```
