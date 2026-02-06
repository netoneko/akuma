# Plan: Enable Timestamp Support in `sqld`

This plan outlines the steps required to enable real-time timestamp support in the `sqld` library (SQLite on Akuma). Currently, `sqld` uses system uptime for its clock and has datetime functions disabled in the build.

## 1. Enable SQLite Datetime Functions

The current build configuration explicitly omits datetime functions to reduce binary size and OS dependencies.

**Action:** In `userspace/sqld/build.rs`, remove the following line:
```rust
.define("SQLITE_OMIT_DATETIME_FUNCS", None)
```

## 2. Update `libakuma` (Syscall Verification)

Ensure `libakuma` has the necessary syscall wrapper for `TIME`.

**Current Status:** `libakuma` already defines `syscall::TIME` (305) and provides:
```rust
pub fn time() -> u64 {
    syscall(syscall::TIME, 0, 0, 0, 0, 0, 0)
}
```
The kernel implements this syscall by reading from the PL031 RTC.

## 3. Implement C-compatible `time()` for SQLite

SQLite's C code occasionally calls the standard `time()` function. We need to provide a real implementation in our C stubs.

**Action A:** Update `userspace/sqld/sqlite3/time.h`:
Remove:
```c
#define time(x) ((time_t)0)
```
Add:
```c
time_t time(time_t *tloc);
```

**Action B:** Implement `time()` in `userspace/sqld/src/vfs.rs` as an exported C function:
```rust
#[no_mangle]
pub unsafe extern "C" fn time(tloc: *mut i64) -> i64 {
    let t = libakuma::time() as i64;
    if !tloc.is_null() {
        *tloc = t;
    }
    t
}
```

## 4. Update VFS to use Real Time

SQLite's VFS uses `xCurrentTime` (Julian days) to handle `datetime('now')` and other time-related queries. Currently, it uses `libakuma::uptime()`, which returns time since boot.

**Action:** Update `userspace/sqld/src/vfs.rs`:

1.  **Improve `akuma_vfs_current_time`**:
    Use `libakuma::time()` (Unix epoch) to calculate the Julian day.
    Julian Day for Unix Epoch (1970-01-01 00:00:00 UTC) is `2440587.5`.
    ```rust
    unsafe extern "C" fn akuma_vfs_current_time(_vfs: *mut sqlite3_vfs, p_time: *mut f64) -> c_int {
        let now_s = libakuma::time() as f64;
        let days = now_s / (24.0 * 60.0 * 60.0);
        *p_time = 2440587.5 + days;
        SQLITE_OK
    }
    ```

2.  **Implement `xCurrentTimeInt64`**:
    Modern SQLite prefers `xCurrentTimeInt64` for better precision (milliseconds).
    ```rust
    unsafe extern "C" fn akuma_vfs_current_time_int64(_vfs: *mut sqlite3_vfs, p_time: *mut i64) -> c_int {
        let now_s = libakuma::time() as i64;
        // Julian day * 86400000
        // Unix epoch in Julian milliseconds: 2440587.5 * 86400000 = 210866760000000
        *p_time = 210866760000000i64 + (now_s * 1000);
        SQLITE_OK
    }
    ```

3.  **Update `AKUMA_VFS` structure**:
    *   Set `iVersion` to 2.
    *   Set `xCurrentTimeInt64` field to `Some(akuma_vfs_current_time_int64)`.

## 5. Verification

After applying these changes:
1.  Rebuild `sqld`.
2.  Run a test query: `SELECT datetime('now');`
3.  Verify the output matches the current UTC time.
