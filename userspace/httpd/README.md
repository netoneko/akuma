# httpd - HTTP Server for Akuma

A simple HTTP/1.0 server that serves static files and supports CGI scripts.

## Features

- Static file serving from `/public`
- CGI script execution from `/public/cgi-bin/`
- GET, HEAD, and POST methods
- MIME type detection
- Directory traversal protection

## Configuration

Edit constants in `src/main.rs`:

```rust
const HTTP_PORT: u16 = 8080;
const CGI_MAX_RESPONSE_BYTES: usize = 64 * 1024;  // 64KB
const CGI_TIMEOUT_MS: u32 = 5000;  // 5 seconds
```

## Usage

The server starts automatically and listens on port 8080.

```bash
# Fetch static file
curl http://localhost:8080/index.html

# Fetch CGI script
curl http://localhost:8080/cgi-bin/hello.js

# POST to CGI script
curl -X POST -d "data=test" http://localhost:8080/cgi-bin/hello.js
```

## Static Files

Place files in `/public/`:

```
/public/
├── index.html      -> http://localhost:8080/
├── style.css       -> http://localhost:8080/style.css
└── images/
    └── logo.png    -> http://localhost:8080/images/logo.png
```

Supported MIME types:
- `.html`, `.htm` - text/html
- `.css` - text/css
- `.js` - application/javascript
- `.json` - application/json
- `.txt` - text/plain
- `.png`, `.jpg`, `.gif`, `.svg`, `.ico` - image/*

## CGI Scripts

Place scripts in `/public/cgi-bin/`:

```
/public/cgi-bin/
├── hello.js        -> http://localhost:8080/cgi-bin/hello.js
└── api             -> http://localhost:8080/cgi-bin/api (ELF binary)
```

### JavaScript CGI Example

```javascript
// Read POST data
var postData = readStdin();

// Output headers
console.log("Content-Type: text/html");
console.log("");

// Output body
console.log("<h1>Hello!</h1>");
```

See [docs/CGI.md](../../docs/CGI.md) for complete CGI documentation.

## Architecture

```
Request -> handle_connection()
              |
              ├── Static file? -> read_file() -> send_file()
              |
              └── CGI request? -> handle_cgi_request()
                                      |
                                      ├── Parse query string
                                      ├── Get interpreter
                                      ├── spawn_with_stdin()
                                      ├── Read stdout
                                      ├── Parse CGI headers
                                      └── send_cgi_response()
```

## Dependencies

- `libakuma` - Syscall wrappers and networking
