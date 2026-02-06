# High-Precision Timestamp Support in `sqld`

The `sqld` library now supports high-precision real-time timestamps, allowing use of SQLite's `datetime('now')` and related functions.

## Implementation Details

### 1. Kernel and Syscall Level
- The `TIME` syscall (305) has been updated to return **microseconds** since the Unix Epoch (1970-01-01 00:00:00 UTC).
- It uses the PL031 RTC combined with the ARM generic timer for high precision.

### 2. `libakuma` Level
- `libakuma::time()` returns a `u64` containing microseconds.

### 3. `sqld` Level
- **Build:** `SQLITE_OMIT_DATETIME_FUNCS` was removed from `build.rs`, enabling SQLite's built-in date and time processing logic.
- **C Stubs:** A proper `time_t time(time_t *tloc)` function is exported to C. It converts Akuma's microseconds to seconds as expected by standard C callers.
- **VFS (Julian Time):**
    - `xCurrentTime`: Calculates Julian Day from microseconds with `f64` precision.
    - `xCurrentTimeInt64`: Implements the modern SQLite interface returning Julian Milliseconds.
    - `AKUMA_VFS` is updated to version 2 to support these precision features.

## Testing via CLI

You can test timestamp support using the `sqld` CLI tool within the Akuma shell.

### Step 1: Start the SQL Server
Start the server on a database file (e.g., `test.db`):
```bash
sqld test.db &
```

### Step 2: Execute Time Queries
Use the `run` command to query the local server:

**Check current UTC time:**
```bash
sqld run "SELECT datetime('now');"
```

**Check high-precision Julian milliseconds:**
```bash
sqld run "SELECT strftime('%f', 'now');"
```

**Verify high-precision increments:**
Running this repeatedly should show changing millisecond/microsecond values:
```bash
sqld run "SELECT (julianday('now') - 2440587.5) * 86400.0;"
```

## Internal Constants
- **Unix Epoch Julian Day:** `2440587.5`
- **Unix Epoch Julian Milliseconds:** `210866760000000`
