//! SQLite VFS implementation for Akuma
//!
//! This module implements SQLite's Virtual File System interface using
//! libakuma syscalls for file I/O.

#![allow(non_snake_case)]
#![allow(dead_code)]

use alloc::alloc::{alloc, dealloc, realloc as rust_realloc, Layout};
use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use libakuma::{close, fstat, lseek, open, open_flags, read_fd, seek_mode, write_fd};

const PRINT_DEBUG: bool = false;

fn debug(msg: &str) {
    if PRINT_DEBUG {
        libakuma::print(msg);
    }
}

// ============================================================================
// C Library Memory Functions (for SQLite)
// ============================================================================

/// Allocate memory - called by SQLite
#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    if size == 0 {
        return ptr::null_mut();
    }
    let layout = match Layout::from_size_align(size + 8, 8) {
        Ok(l) => l,
        Err(_) => return ptr::null_mut(),
    };
    let ptr = alloc(layout);
    if ptr.is_null() {
        return ptr::null_mut();
    }
    // Store size at the beginning for later deallocation
    *(ptr as *mut usize) = size;
    ptr.add(8) as *mut c_void
}

/// Free memory - called by SQLite
#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let real_ptr = (ptr as *mut u8).sub(8);
    let size = *(real_ptr as *const usize);
    let layout = match Layout::from_size_align(size + 8, 8) {
        Ok(l) => l,
        Err(_) => return,
    };
    dealloc(real_ptr, layout);
}

/// Reallocate memory - called by SQLite
#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, new_size: usize) -> *mut c_void {
    if ptr.is_null() {
        return malloc(new_size);
    }
    if new_size == 0 {
        free(ptr);
        return ptr::null_mut();
    }
    
    let real_ptr = (ptr as *mut u8).sub(8);
    let old_size = *(real_ptr as *const usize);
    
    let old_layout = match Layout::from_size_align(old_size + 8, 8) {
        Ok(l) => l,
        Err(_) => return ptr::null_mut(),
    };
    
    let new_ptr = rust_realloc(real_ptr, old_layout, new_size + 8);
    if new_ptr.is_null() {
        return ptr::null_mut();
    }
    
    // Update stored size
    *(new_ptr as *mut usize) = new_size;
    new_ptr.add(8) as *mut c_void
}

// SQLite constants
pub const SQLITE_OK: c_int = 0;
pub const SQLITE_ERROR: c_int = 1;
pub const SQLITE_IOERR: c_int = 10;
pub const SQLITE_IOERR_READ: c_int = 266;   // SQLITE_IOERR | (1<<8)
pub const SQLITE_IOERR_SHORT_READ: c_int = 522; // SQLITE_IOERR | (2<<8)
pub const SQLITE_IOERR_WRITE: c_int = 778;  // SQLITE_IOERR | (3<<8)
pub const SQLITE_IOERR_FSYNC: c_int = 1034; // SQLITE_IOERR | (4<<8)
pub const SQLITE_CANTOPEN: c_int = 14;
pub const SQLITE_NOTFOUND: c_int = 12;
pub const SQLITE_ROW: c_int = 100;
pub const SQLITE_DONE: c_int = 101;

// Open flags
pub const SQLITE_OPEN_READONLY: c_int = 0x00000001;
pub const SQLITE_OPEN_READWRITE: c_int = 0x00000002;
pub const SQLITE_OPEN_CREATE: c_int = 0x00000004;
pub const SQLITE_OPEN_MAIN_DB: c_int = 0x00000100;

// Lock levels (we ignore these - single process)
pub const SQLITE_LOCK_NONE: c_int = 0;
pub const SQLITE_LOCK_SHARED: c_int = 1;
pub const SQLITE_LOCK_RESERVED: c_int = 2;
pub const SQLITE_LOCK_PENDING: c_int = 3;
pub const SQLITE_LOCK_EXCLUSIVE: c_int = 4;

/// Opaque SQLite types
#[repr(C)]
pub struct sqlite3 {
    _private: [u8; 0],
}

#[repr(C)]
pub struct sqlite3_stmt {
    _private: [u8; 0],
}

#[repr(C)]
pub struct sqlite3_vfs {
    pub iVersion: c_int,
    pub szOsFile: c_int,
    pub mxPathname: c_int,
    pub pNext: *mut sqlite3_vfs,
    pub zName: *const c_char,
    pub pAppData: *mut c_void,
    pub xOpen: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, *mut sqlite3_file, c_int, *mut c_int) -> c_int>,
    pub xDelete: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, c_int) -> c_int>,
    pub xAccess: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, c_int, *mut c_int) -> c_int>,
    pub xFullPathname: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, c_int, *mut c_char) -> c_int>,
    pub xDlOpen: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char) -> *mut c_void>,
    pub xDlError: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int, *mut c_char)>,
    pub xDlSym: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut c_void, *const c_char) -> Option<unsafe extern "C" fn()>>,
    pub xDlClose: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut c_void)>,
    pub xRandomness: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int, *mut c_char) -> c_int>,
    pub xSleep: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int) -> c_int>,
    pub xCurrentTime: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut f64) -> c_int>,
    pub xGetLastError: Option<unsafe extern "C" fn(*mut sqlite3_vfs, c_int, *mut c_char) -> c_int>,
    // Version 2+
    pub xCurrentTimeInt64: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *mut i64) -> c_int>,
    // Version 3+
    pub xSetSystemCall: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char, Option<unsafe extern "C" fn()>) -> c_int>,
    pub xGetSystemCall: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char) -> Option<unsafe extern "C" fn()>>,
    pub xNextSystemCall: Option<unsafe extern "C" fn(*mut sqlite3_vfs, *const c_char) -> *const c_char>,
}

#[repr(C)]
pub struct sqlite3_io_methods {
    pub iVersion: c_int,
    pub xClose: Option<unsafe extern "C" fn(*mut sqlite3_file) -> c_int>,
    pub xRead: Option<unsafe extern "C" fn(*mut sqlite3_file, *mut c_void, c_int, i64) -> c_int>,
    pub xWrite: Option<unsafe extern "C" fn(*mut sqlite3_file, *const c_void, c_int, i64) -> c_int>,
    pub xTruncate: Option<unsafe extern "C" fn(*mut sqlite3_file, i64) -> c_int>,
    pub xSync: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    pub xFileSize: Option<unsafe extern "C" fn(*mut sqlite3_file, *mut i64) -> c_int>,
    pub xLock: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    pub xUnlock: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    pub xCheckReservedLock: Option<unsafe extern "C" fn(*mut sqlite3_file, *mut c_int) -> c_int>,
    pub xFileControl: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int, *mut c_void) -> c_int>,
    pub xSectorSize: Option<unsafe extern "C" fn(*mut sqlite3_file) -> c_int>,
    pub xDeviceCharacteristics: Option<unsafe extern "C" fn(*mut sqlite3_file) -> c_int>,
    // Version 2+
    pub xShmMap: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int, c_int, c_int, *mut *mut c_void) -> c_int>,
    pub xShmLock: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int, c_int, c_int) -> c_int>,
    pub xShmBarrier: Option<unsafe extern "C" fn(*mut sqlite3_file)>,
    pub xShmUnmap: Option<unsafe extern "C" fn(*mut sqlite3_file, c_int) -> c_int>,
    // Version 3+
    pub xFetch: Option<unsafe extern "C" fn(*mut sqlite3_file, i64, c_int, *mut *mut c_void) -> c_int>,
    pub xUnfetch: Option<unsafe extern "C" fn(*mut sqlite3_file, i64, *mut c_void) -> c_int>,
}

#[repr(C)]
pub struct sqlite3_file {
    pub pMethods: *const sqlite3_io_methods,
}

/// Our custom file structure
#[repr(C)]
pub struct AkumaFile {
    pub base: sqlite3_file,
    pub fd: i32,
}

// Static VFS name
static VFS_NAME: &[u8] = b"akuma\0";

// Static IO methods
static AKUMA_IO_METHODS: sqlite3_io_methods = sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(akuma_close),
    xRead: Some(akuma_read),
    xWrite: Some(akuma_write),
    xTruncate: Some(akuma_truncate),
    xSync: Some(akuma_sync),
    xFileSize: Some(akuma_file_size),
    xLock: Some(akuma_lock),
    xUnlock: Some(akuma_unlock),
    xCheckReservedLock: Some(akuma_check_reserved_lock),
    xFileControl: Some(akuma_file_control),
    xSectorSize: Some(akuma_sector_size),
    xDeviceCharacteristics: Some(akuma_device_characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};

// Static VFS instance
static mut AKUMA_VFS: sqlite3_vfs = sqlite3_vfs {
    iVersion: 1,
    szOsFile: core::mem::size_of::<AkumaFile>() as c_int,
    mxPathname: 512,
    pNext: ptr::null_mut(),
    zName: VFS_NAME.as_ptr() as *const c_char,
    pAppData: ptr::null_mut(),
    xOpen: Some(akuma_vfs_open),
    xDelete: Some(akuma_vfs_delete),
    xAccess: Some(akuma_vfs_access),
    xFullPathname: Some(akuma_vfs_full_pathname),
    xDlOpen: None,
    xDlError: None,
    xDlSym: None,
    xDlClose: None,
    xRandomness: Some(akuma_vfs_randomness),
    xSleep: Some(akuma_vfs_sleep),
    xCurrentTime: Some(akuma_vfs_current_time),
    xGetLastError: Some(akuma_vfs_get_last_error),
    xCurrentTimeInt64: None,
    xSetSystemCall: None,
    xGetSystemCall: None,
    xNextSystemCall: None,
};

// ============================================================================
// VFS Methods
// ============================================================================

unsafe extern "C" fn akuma_vfs_open(
    _vfs: *mut sqlite3_vfs,
    z_name: *const c_char,
    file: *mut sqlite3_file,
    flags: c_int,
    _p_out_flags: *mut c_int,
) -> c_int {
    debug ("sqld: VFS xOpen called\n");
    let akuma_file = file as *mut AkumaFile;

    // Convert flags
    let mut open_mode = open_flags::O_RDONLY;
    if (flags & SQLITE_OPEN_READWRITE) != 0 {
        open_mode = open_flags::O_RDWR;
    }
    if (flags & SQLITE_OPEN_CREATE) != 0 {
        open_mode |= open_flags::O_CREAT;
    }

    // Get path as str
    if z_name.is_null() {
        debug ("sqld: VFS xOpen: null path\n");
        return SQLITE_CANTOPEN;
    }
    
    let path = cstr_to_str(z_name);
    if path.is_empty() {
        debug ("sqld: VFS xOpen: empty path\n");
        return SQLITE_CANTOPEN;
    }

    debug ("sqld: VFS xOpen: ");
    debug(path);
    debug("\n");

    // Open the file
    let fd = open(path, open_mode);
    if fd < 0 {
        debug ("sqld: VFS xOpen: open failed\n");
        return SQLITE_CANTOPEN;
    }

    debug ("sqld: VFS xOpen: success\n");
    (*akuma_file).base.pMethods = &AKUMA_IO_METHODS;
    (*akuma_file).fd = fd;

    SQLITE_OK
}

unsafe extern "C" fn akuma_vfs_delete(
    _vfs: *mut sqlite3_vfs,
    _z_name: *const c_char,
    _sync_dir: c_int,
) -> c_int {
    // TODO: Implement file deletion when kernel supports it
    SQLITE_OK
}

unsafe extern "C" fn akuma_vfs_access(
    _vfs: *mut sqlite3_vfs,
    z_name: *const c_char,
    _flags: c_int,
    p_res_out: *mut c_int,
) -> c_int {
    // Try to open the file to check if it exists
    let path = cstr_to_str(z_name);
    if path.is_empty() {
        *p_res_out = 0;
        return SQLITE_OK;
    }

    let fd = open(path, open_flags::O_RDONLY);
    if fd >= 0 {
        close(fd);
        *p_res_out = 1;
    } else {
        *p_res_out = 0;
    }
    SQLITE_OK
}

unsafe extern "C" fn akuma_vfs_full_pathname(
    _vfs: *mut sqlite3_vfs,
    z_name: *const c_char,
    n_out: c_int,
    z_out: *mut c_char,
) -> c_int {
    // Just copy the path as-is (we don't have a working directory concept)
    let mut i = 0;
    while i < n_out - 1 {
        let c = *z_name.add(i as usize);
        if c == 0 {
            break;
        }
        *z_out.add(i as usize) = c;
        i += 1;
    }
    *z_out.add(i as usize) = 0;
    SQLITE_OK
}

unsafe extern "C" fn akuma_vfs_randomness(
    _vfs: *mut sqlite3_vfs,
    n_byte: c_int,
    z_out: *mut c_char,
) -> c_int {
    // Fill with pseudo-random data based on uptime
    let mut seed = libakuma::uptime();
    for i in 0..n_byte as usize {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        *z_out.add(i) = (seed >> 16) as c_char;
    }
    n_byte
}

unsafe extern "C" fn akuma_vfs_sleep(_vfs: *mut sqlite3_vfs, microseconds: c_int) -> c_int {
    let ms = (microseconds / 1000).max(1) as u64;
    libakuma::sleep_ms(ms);
    microseconds
}

unsafe extern "C" fn akuma_vfs_current_time(_vfs: *mut sqlite3_vfs, p_time: *mut f64) -> c_int {
    // Return uptime as Julian day offset from a fixed point
    // Julian day for Unix epoch (1970-01-01) is 2440587.5
    let uptime_us = libakuma::uptime();
    let days = uptime_us as f64 / (24.0 * 60.0 * 60.0 * 1_000_000.0);
    *p_time = 2440587.5 + days;
    SQLITE_OK
}

unsafe extern "C" fn akuma_vfs_get_last_error(
    _vfs: *mut sqlite3_vfs,
    _n_buf: c_int,
    _z_buf: *mut c_char,
) -> c_int {
    0
}

// ============================================================================
// IO Methods
// ============================================================================

unsafe extern "C" fn akuma_close(file: *mut sqlite3_file) -> c_int {
    let akuma_file = file as *mut AkumaFile;
    close((*akuma_file).fd);
    SQLITE_OK
}

unsafe extern "C" fn akuma_read(
    file: *mut sqlite3_file,
    buf: *mut c_void,
    amt: c_int,
    offset: i64,
) -> c_int {
    let akuma_file = file as *mut AkumaFile;
    let fd = (*akuma_file).fd;

    // Seek to position
    let pos = lseek(fd, offset, seek_mode::SEEK_SET);
    if pos < 0 {
        debug ("sqld: VFS xRead: seek failed\n");
        return SQLITE_IOERR_READ;
    }

    // Read data
    let buf_slice = core::slice::from_raw_parts_mut(buf as *mut u8, amt as usize);
    let n = read_fd(fd, buf_slice);
    
    if n < 0 {
        debug ("sqld: VFS xRead: read failed\n");
        return SQLITE_IOERR_READ;
    }
    
    // Debug: show first few bytes if reading from offset 0
    if offset == 0 && n >= 16 {
        debug ("sqld: VFS xRead header: ");
        for i in 0..16 {
            let b = buf_slice[i];
            if b >= 0x20 && b < 0x7f {
                libakuma::write(libakuma::fd::STDOUT, &[b]);
            } else {
                libakuma::print(".");
            }
        }
        libakuma::print("\n");
    }
    
    if (n as c_int) < amt {
        // Zero-fill the rest (short read)
        for i in n as usize..amt as usize {
            buf_slice[i] = 0;
        }
        return SQLITE_IOERR_SHORT_READ;
    }
    
    SQLITE_OK
}

unsafe extern "C" fn akuma_write(
    file: *mut sqlite3_file,
    buf: *const c_void,
    amt: c_int,
    offset: i64,
) -> c_int {
    let akuma_file = file as *mut AkumaFile;
    let fd = (*akuma_file).fd;

    debug ("sqld: VFS xWrite fd=");
    debug_print_num(fd);
    debug(" offset=");
    debug_print_num(offset as i32);
    debug(" amt=");
    debug_print_num(amt);
    debug("\n");
    
    if PRINT_DEBUG {
        // Debug: show first few bytes if writing to offset 0
        if offset == 0 {
            let buf_slice = core::slice::from_raw_parts(buf as *const u8, amt as usize);
            debug ("sqld: VFS xWrite header: ");
            for i in 0..16.min(amt as usize) {
                let b = buf_slice[i];
                if b >= 0x20 && b < 0x7f {
                    libakuma::write(libakuma::fd::STDOUT, &[b]);
                } else {
                    libakuma::print(".");
                }
            }
            libakuma::print("\n");
        }
    }

    // Seek to position
    let pos = lseek(fd, offset, seek_mode::SEEK_SET);
    if pos < 0 {
        debug("sqld: VFS xWrite: seek failed\n");
        return SQLITE_IOERR_WRITE;
    }

    // Write data
    let buf_slice = core::slice::from_raw_parts(buf as *const u8, amt as usize);
    let n = write_fd(fd, buf_slice);
    
    if n < 0 || (n as c_int) != amt {
        libakuma::print("sqld: VFS xWrite: write failed n=");
        print_num(n as i32);
        libakuma::print("\n");
        return SQLITE_IOERR_WRITE;
    }
    
    SQLITE_OK
}

fn debug_print_num(n: i32) {
    if PRINT_DEBUG {
        print_num(n);
    }
}

fn print_num(n: i32) {
    if n < 0 {
        libakuma::print("-");
        print_num(-n);
        return;
    }
    if n == 0 {
        libakuma::print("0");
        return;
    }
    let mut buf = [0u8; 12];
    let mut i = 0;
    let mut num = n as u32;
    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        libakuma::write(libakuma::fd::STDOUT, &buf[i..i+1]);
    }
}

unsafe extern "C" fn akuma_truncate(_file: *mut sqlite3_file, _size: i64) -> c_int {
    // TODO: Implement truncate when kernel supports it
    SQLITE_OK
}

unsafe extern "C" fn akuma_sync(_file: *mut sqlite3_file, _flags: c_int) -> c_int {
    // No-op - we don't have fsync
    SQLITE_OK
}

unsafe extern "C" fn akuma_file_size(file: *mut sqlite3_file, p_size: *mut i64) -> c_int {
    let akuma_file = file as *mut AkumaFile;
    let fd = (*akuma_file).fd;

    match fstat(fd) {
        Ok(stat) => {
            *p_size = stat.st_size;
            SQLITE_OK
        }
        Err(_) => SQLITE_IOERR,
    }
}

unsafe extern "C" fn akuma_lock(_file: *mut sqlite3_file, _lock: c_int) -> c_int {
    // No-op - single process
    SQLITE_OK
}

unsafe extern "C" fn akuma_unlock(_file: *mut sqlite3_file, _lock: c_int) -> c_int {
    // No-op - single process
    SQLITE_OK
}

unsafe extern "C" fn akuma_check_reserved_lock(
    _file: *mut sqlite3_file,
    p_res_out: *mut c_int,
) -> c_int {
    *p_res_out = 0;
    SQLITE_OK
}

unsafe extern "C" fn akuma_file_control(
    _file: *mut sqlite3_file,
    _op: c_int,
    _p_arg: *mut c_void,
) -> c_int {
    SQLITE_NOTFOUND
}

unsafe extern "C" fn akuma_sector_size(_file: *mut sqlite3_file) -> c_int {
    4096
}

unsafe extern "C" fn akuma_device_characteristics(_file: *mut sqlite3_file) -> c_int {
    0
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Convert a C string to a Rust &str
/// Safety: The caller must ensure the pointer is valid and null-terminated
unsafe fn cstr_to_str<'a>(s: *const c_char) -> &'a str {
    if s.is_null() {
        return "";
    }
    let mut len = 0;
    while *s.add(len) != 0 {
        len += 1;
    }
    let bytes = core::slice::from_raw_parts(s as *const u8, len);
    core::str::from_utf8_unchecked(bytes)
}

// ============================================================================
// SQLite OS Interface (required for SQLITE_OS_OTHER)
// ============================================================================

/// Called by SQLite during initialization
#[no_mangle]
pub unsafe extern "C" fn sqlite3_os_init() -> c_int {
    // Register our VFS as the default
    let rc = sqlite3_vfs_register(&raw mut AKUMA_VFS, 1);
    rc
}

/// Called by SQLite during shutdown
#[no_mangle]
pub unsafe extern "C" fn sqlite3_os_end() -> c_int {
    SQLITE_OK
}

// ============================================================================
// SQLite FFI
// ============================================================================

extern "C" {
    pub fn sqlite3_vfs_register(vfs: *mut sqlite3_vfs, make_default: c_int) -> c_int;
    pub fn sqlite3_initialize() -> c_int;
    pub fn sqlite3_open_v2(
        filename: *const c_char,
        ppDb: *mut *mut sqlite3,
        flags: c_int,
        zVfs: *const c_char,
    ) -> c_int;
    pub fn sqlite3_close(db: *mut sqlite3) -> c_int;
    pub fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        zSql: *const c_char,
        nByte: c_int,
        ppStmt: *mut *mut sqlite3_stmt,
        pzTail: *mut *const c_char,
    ) -> c_int;
    pub fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_column_text(stmt: *mut sqlite3_stmt, col: c_int) -> *const c_char;
    pub fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
}

// ============================================================================
// Public API
// ============================================================================

/// Initialize the Akuma VFS and register it with SQLite
pub fn init() -> Result<(), &'static str> {
    unsafe {
        // Initialize SQLite (this will call sqlite3_os_init which registers our VFS)
        let rc = sqlite3_initialize();
        if rc != SQLITE_OK {
            libakuma::print("sqld: sqlite3_initialize failed with code ");
            print_rc(rc);
            libakuma::print("\n");
            return Err("Failed to initialize SQLite");
        }

        Ok(())
    }
}

fn print_rc(rc: c_int) {
    let mut buf = [0u8; 12];
    let mut i = 0;
    let mut num = if rc < 0 { (-rc) as u32 } else { rc as u32 };
    if num == 0 {
        libakuma::print("0");
        return;
    }
    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }
    if rc < 0 {
        libakuma::print("-");
    }
    while i > 0 {
        i -= 1;
        libakuma::write(libakuma::fd::STDOUT, &buf[i..i+1]);
    }
}

/// Open a SQLite database
pub fn open_db(path: &str) -> Result<*mut sqlite3, &'static str> {
    unsafe {
        let mut db: *mut sqlite3 = ptr::null_mut();
        
        // Create null-terminated path
        let mut path_buf = [0u8; 512];
        let path_bytes = path.as_bytes();
        if path_bytes.len() >= path_buf.len() {
            return Err("Path too long");
        }
        path_buf[..path_bytes.len()].copy_from_slice(path_bytes);
        
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE;
        debug("sqld: Calling sqlite3_open_v2...\n");
        let rc = sqlite3_open_v2(
            path_buf.as_ptr() as *const c_char,
            &mut db,
            flags,
            VFS_NAME.as_ptr() as *const c_char,
        );
        
        if rc != SQLITE_OK {
            libakuma::print("sqld: sqlite3_open_v2 failed with code ");
            print_rc(rc);
            libakuma::print("\n");
            return Err("Failed to open database");
        }
        
        debug("sqld: sqlite3_open_v2 succeeded\n");
        
        // Disable journaling - our VFS doesn't support file deletion
        // which causes journal cleanup to fail and corrupt the database
        let pragma = b"PRAGMA journal_mode=OFF\0";
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        let rc = sqlite3_prepare_v2(
            db,
            pragma.as_ptr() as *const c_char,
            -1,
            &mut stmt,
            ptr::null_mut(),
        );
        if rc == SQLITE_OK {
            sqlite3_step(stmt);
            sqlite3_finalize(stmt);
            debug("sqld: Journal mode disabled\n");
        }
        
        Ok(db)
    }
}

/// Close a SQLite database
pub fn close_db(db: *mut sqlite3) {
    unsafe {
        sqlite3_close(db);
    }
}

/// Get list of tables in the database
pub fn list_tables(db: *mut sqlite3) -> Result<alloc::vec::Vec<alloc::string::String>, &'static str> {
    use alloc::string::String;
    use alloc::vec::Vec;
    
    unsafe {
        let sql = b"SELECT name FROM sqlite_master WHERE type='table' ORDER BY name\0";
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        
        libakuma::print("sqld: Preparing SQL statement...\n");
        let rc = sqlite3_prepare_v2(
            db,
            sql.as_ptr() as *const c_char,
            -1,
            &mut stmt,
            ptr::null_mut(),
        );
        
        if rc != SQLITE_OK {
            libakuma::print("sqld: sqlite3_prepare_v2 failed with code ");
            print_rc(rc);
            libakuma::print("\n");
            return Err("Failed to prepare statement");
        }
        libakuma::print("sqld: Statement prepared, stepping...\n");
        
        let mut tables = Vec::new();
        
        loop {
            let rc = sqlite3_step(stmt);
            if rc == SQLITE_ROW {
                let text = sqlite3_column_text(stmt, 0);
                if !text.is_null() {
                    let name = cstr_to_str(text);
                    libakuma::print("sqld: Found table: ");
                    libakuma::print(name);
                    libakuma::print("\n");
                    tables.push(String::from(name));
                }
            } else if rc == SQLITE_DONE {
                libakuma::print("sqld: Query complete\n");
                break;
            } else {
                libakuma::print("sqld: sqlite3_step failed with code ");
                print_rc(rc);
                libakuma::print("\n");
                sqlite3_finalize(stmt);
                return Err("Error stepping statement");
            }
        }
        
        sqlite3_finalize(stmt);
        Ok(tables)
    }
}

// Additional FFI for column names and last insert rowid
extern "C" {
    pub fn sqlite3_column_name(stmt: *mut sqlite3_stmt, col: c_int) -> *const c_char;
    pub fn sqlite3_changes(db: *mut sqlite3) -> c_int;
    pub fn sqlite3_last_insert_rowid(db: *mut sqlite3) -> i64;
}

/// Get the rowid of the last inserted row
pub fn last_insert_rowid(db: *mut sqlite3) -> i64 {
    unsafe { sqlite3_last_insert_rowid(db) }
}

/// Query result with column names and rows
pub struct QueryResult {
    pub columns: alloc::vec::Vec<alloc::string::String>,
    pub rows: alloc::vec::Vec<alloc::vec::Vec<alloc::string::String>>,
    pub changes: u32,
}

/// Execute SQL and return results
pub fn execute_sql(db: *mut sqlite3, sql: &str) -> Result<QueryResult, alloc::string::String> {
    use alloc::string::String;
    use alloc::vec::Vec;
    
    unsafe {
        // Create null-terminated SQL
        let mut sql_buf = alloc::vec![0u8; sql.len() + 1];
        sql_buf[..sql.len()].copy_from_slice(sql.as_bytes());
        
        let mut stmt: *mut sqlite3_stmt = ptr::null_mut();
        
        let rc = sqlite3_prepare_v2(
            db,
            sql_buf.as_ptr() as *const c_char,
            -1,
            &mut stmt,
            ptr::null_mut(),
        );
        
        if rc != SQLITE_OK {
            let err = sqlite3_errmsg(db);
            let msg = if !err.is_null() {
                String::from(cstr_to_str(err))
            } else {
                String::from("Failed to prepare statement")
            };
            return Err(msg);
        }
        
        // Get column names
        let col_count = sqlite3_column_count(stmt);
        let mut columns = Vec::new();
        for i in 0..col_count {
            let name = sqlite3_column_name(stmt, i);
            if !name.is_null() {
                columns.push(String::from(cstr_to_str(name)));
            } else {
                columns.push(String::from("?"));
            }
        }
        
        // Execute and collect rows
        let mut rows = Vec::new();
        
        loop {
            let rc = sqlite3_step(stmt);
            if rc == SQLITE_ROW {
                let mut row = Vec::new();
                for i in 0..col_count {
                    let text = sqlite3_column_text(stmt, i);
                    if !text.is_null() {
                        row.push(String::from(cstr_to_str(text)));
                    } else {
                        row.push(String::from("NULL"));
                    }
                }
                rows.push(row);
            } else if rc == SQLITE_DONE {
                break;
            } else {
                let err = sqlite3_errmsg(db);
                let msg = if !err.is_null() {
                    String::from(cstr_to_str(err))
                } else {
                    String::from("Error executing statement")
                };
                sqlite3_finalize(stmt);
                return Err(msg);
            }
        }
        
        let changes = sqlite3_changes(db) as u32;
        sqlite3_finalize(stmt);
        
        Ok(QueryResult { columns, rows, changes })
    }
}
