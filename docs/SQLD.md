# sqld - SQLite Daemon for Akuma

`sqld` is a userspace SQLite server that provides a TCP interface for executing SQL queries.

## Architecture

```
┌─────────────────┐     TCP/4321      ┌─────────────────┐
│   sqld client   │ ◄───────────────► │   sqld server   │
│  (sqld run)     │   Binary Protocol │  (sqld <file>)  │
└─────────────────┘                   └────────┬────────┘
                                               │
                                               ▼
                                      ┌─────────────────┐
                                      │  SQLite VFS     │
                                      │  (libakuma I/O) │
                                      └────────┬────────┘
                                               │
                                               ▼
                                      ┌─────────────────┐
                                      │  Database File  │
                                      │ (local.sqlite)  │
                                      └─────────────────┘
```

## Usage

### Start the Server

```bash
sqld local.sqlite
```

This starts a TCP server on port 4321, opening (or creating) `local.sqlite`.

### Execute Queries

From another SSH session:

```bash
# Simple query
sqld run "SELECT 1"

# Query a table
sqld run "SELECT * FROM messages ORDER BY id DESC LIMIT 5"

# Insert data
sqld run "INSERT INTO messages (message) VALUES ('hello world')"

# Connect to a specific host
sqld run -h 10.0.2.15:4321 "SELECT 1"
```

### Check Database Status

```bash
sqld status local.sqlite
```

Lists all tables in the database (direct access, no server needed).

## Wire Protocol

Binary protocol over TCP:

### Request
```
[u32 length (big-endian)][SQL bytes]
```

### Response
```
[u32 length (big-endian)][u8 status][payload]
```

Status codes:
- `0x00` - OK with rows (SELECT result)
- `0x01` - OK with affected count (INSERT/UPDATE/DELETE)
- `0xFF` - Error

#### Status 0x00 (Rows) Payload
```
[u32 column_count]
[column names as null-terminated strings]
[u32 row_count]
[row values as null-terminated strings, column by column]
```

#### Status 0x01 (Affected) Payload
```
[u32 affected_row_count]
```

#### Status 0xFF (Error) Payload
```
[error message as null-terminated string]
```

## Implementation Details

### SQLite VFS

The custom VFS (`userspace/sqld/src/vfs.rs`) maps SQLite file operations to libakuma syscalls:

| SQLite VFS Method | libakuma Call |
|-------------------|---------------|
| xOpen | `open()` |
| xClose | `close()` |
| xRead | `lseek()` + `read_fd()` |
| xWrite | `lseek()` + `write_fd()` |
| xFileSize | `fstat()` |
| xSync | (no-op) |
| xLock/xUnlock | (no-op, single process) |

### Build Configuration

SQLite is compiled with these flags for the `no_std` environment:

```
SQLITE_OS_OTHER=1
SQLITE_THREADSAFE=0
SQLITE_OMIT_LOAD_EXTENSION
SQLITE_OMIT_LOCALTIME
SQLITE_OMIT_WAL
SQLITE_OMIT_SHARED_CACHE
SQLITE_OMIT_AUTOINIT
SQLITE_OMIT_DATETIME_FUNCS
SQLITE_TEMP_STORE=3
```

Custom C library stubs are provided in `userspace/sqld/sqlite3/` for functions like `memcpy`, `strlen`, `snprintf`, etc.

### Memory Allocation

SQLite's `malloc`/`free`/`realloc` are implemented in Rust using the global allocator, exposed via `#[no_mangle]` FFI functions in `vfs.rs`.

## Files

```
userspace/sqld/
├── Cargo.toml          # Package manifest
├── build.rs            # SQLite compilation script
├── src/
│   ├── main.rs         # CLI entry point
│   ├── server.rs       # TCP server implementation
│   ├── client.rs       # TCP client implementation
│   └── vfs.rs          # SQLite VFS + memory allocation
└── sqlite3/
    ├── sqlite3.c       # SQLite amalgamation
    ├── sqlite3.h       # SQLite headers
    ├── sqlite_stubs.c  # C library stubs
    └── *.h             # Minimal C headers (stdio, stdlib, etc.)
```

## Future Work

- [ ] Add localtime support (requires kernel RTC integration)
- [ ] Connection pooling / multiple clients
- [ ] Prepared statements over the wire
- [ ] Authentication
