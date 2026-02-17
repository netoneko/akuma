#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

use alloc::alloc::Layout;
use core::ffi::{c_char, c_void, c_int};
use core::ptr;
use libakuma::{
    close as akuma_close, exit as akuma_exit,
    open as akuma_open, open_flags,
    read_fd, seek_mode, write_fd,
    unlink as akuma_unlink,
    rename as akuma_rename, mkdir as akuma_mkdir,
    getcwd as akuma_getcwd, Stat,
    mmap as akuma_mmap, munmap as akuma_munmap,
};

// ============================================================================
// C Types and Globals
// ============================================================================

#[repr(C)]
pub struct timeval {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
pub struct FILE {
    fd: i32,
    error: i32,
    eof: i32,
    ungot: i32, // For ungetc, -1 if empty
}

#[no_mangle]
pub static mut stdin: *mut FILE = ptr::null_mut();

#[no_mangle]
pub static mut stdout: *mut FILE = ptr::null_mut();

#[no_mangle]
pub static mut stderr: *mut FILE = ptr::null_mut();

type TimeT = i64;

// Static buffers for standard streams
static mut STDIN_FILE: FILE = FILE { fd: 0, error: 0, eof: 0, ungot: -1 };
static mut STDOUT_FILE: FILE = FILE { fd: 1, error: 0, eof: 0, ungot: -1 };
static mut STDERR_FILE: FILE = FILE { fd: 2, error: 0, eof: 0, ungot: -1 };

// ============================================================================
// Entry Point
// ============================================================================

extern "C" {
    fn tcc_main(argc: c_int, argv: *const *const c_char) -> c_int;
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    unsafe {
        // Initialize standard streams
        stdin = &raw mut STDIN_FILE;
        stdout = &raw mut STDOUT_FILE;
        stderr = &raw mut STDERR_FILE;

        // Get args from libakuma
        let args_iter = libakuma::args();
        let mut argv_ptrs: alloc::vec::Vec<*const c_char> = alloc::vec::Vec::new();
        // Keep strings alive
        let mut argv_strings: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();

        for arg in args_iter {
            let s = alloc::string::String::from(arg) + "\0";
            argv_ptrs.push(s.as_ptr() as *const c_char);
            argv_strings.push(s);
        }
        argv_ptrs.push(ptr::null());

        let argc = (argv_ptrs.len() - 1) as c_int;
        let ret = tcc_main(argc, argv_ptrs.as_ptr());
        exit(ret);
    }
}

// ============================================================================
// Memory Allocation
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    if size == 0 {
        return ptr::null_mut();
    }
    let layout = match Layout::from_size_align(size + 8, 8) {
        Ok(l) => l,
        Err(_) => return ptr::null_mut(),
    };
    let ptr = alloc::alloc::alloc(layout);
    if ptr.is_null() {
        return ptr::null_mut();
    }
    *(ptr as *mut usize) = size;
    ptr.add(8) as *mut c_void
}

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
    alloc::alloc::dealloc(real_ptr, layout);
}

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
    
    let new_ptr = alloc::alloc::realloc(real_ptr, old_layout, new_size + 8);
    if new_ptr.is_null() {
        return ptr::null_mut();
    }
    
    *(new_ptr as *mut usize) = new_size;
    new_ptr.add(8) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn calloc(nmemb: usize, size: usize) -> *mut c_void {
    let total = nmemb.checked_mul(size).unwrap_or(0);
    let ptr = malloc(total);
    if !ptr.is_null() {
        ptr::write_bytes(ptr, 0, total);
    }
    ptr
}

// ============================================================================
// File I/O
// ============================================================================

#[no_mangle]
pub unsafe extern "C" fn open(pathname: *const c_char, flags: c_int, _mode: c_int) -> c_int {
    let path = cstr_to_str(pathname);
    // Ignore mode for now, libakuma uses default
    akuma_open(path, flags as u32)
}

#[no_mangle]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize {
    let buf_slice = core::slice::from_raw_parts_mut(buf as *mut u8, count);
    libakuma::read_fd(fd, buf_slice)
}

#[no_mangle]
pub unsafe extern "C" fn write(fd: c_int, buf: *const c_void, count: usize) -> isize {
    let buf_slice = core::slice::from_raw_parts(buf as *const u8, count);
    libakuma::write_fd(fd, buf_slice)
}

#[no_mangle]
pub unsafe extern "C" fn lseek(fd: c_int, offset: i64, whence: c_int) -> i64 {
    libakuma::lseek(fd, offset, whence)
}

#[no_mangle]
pub unsafe extern "C" fn getcwd(buf: *mut c_char, size: usize) -> *mut c_char {
    let cwd = akuma_getcwd();
    let len = cwd.len();
    if len + 1 > size {
        return ptr::null_mut(); // ERANGE
    }
    ptr::copy_nonoverlapping(cwd.as_ptr(), buf as *mut u8, len);
    *buf.add(len) = 0;
    buf
}

#[no_mangle]
pub unsafe extern "C" fn mmap(addr: *mut c_void, length: usize, prot: c_int, flags: c_int, _fd: c_int, _offset: i64) -> *mut c_void {
    // libakuma::mmap(addr, len, prot, flags)
    let ret = akuma_mmap(addr as usize, length, prot as u32, flags as u32);
    if ret == usize::MAX {
        return -1isize as *mut c_void; // MAP_FAILED
    }
    ret as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn munmap(addr: *mut c_void, length: usize) -> c_int {
    akuma_munmap(addr as usize, length) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn mprotect(_addr: *mut c_void, _len: usize, _prot: c_int) -> c_int {
    // Stub: assume success (or kernel doesn't support changing protections yet)
    0
}

#[no_mangle]
pub unsafe extern "C" fn fcntl(_fd: c_int, _cmd: c_int, _arg: c_int) -> c_int {
    // Stub
    0
}

#[no_mangle]
pub unsafe extern "C" fn fopen(filename: *const c_char, mode: *const c_char) -> *mut FILE {
    let _filename_str = cstr_to_str(filename);
    let mode_str = cstr_to_str(mode);
    
    let mut flags = 0;
    if mode_str.contains("r+") || mode_str.contains("w+") || mode_str.contains("a+") {
        flags = open_flags::O_RDWR;
    } else if mode_str.contains("r") {
        flags = open_flags::O_RDONLY;
    } else if mode_str.contains("w") || mode_str.contains("a") {
        flags = open_flags::O_WRONLY;
    }
    
    if mode_str.contains("w") {
        flags |= open_flags::O_CREAT | open_flags::O_TRUNC;
    }
    if mode_str.contains("a") {
        flags |= open_flags::O_CREAT | open_flags::O_APPEND;
    }

    let fd = open(filename, flags as c_int, 0);
    if fd < 0 {
        return ptr::null_mut();
    }

    let file = malloc(core::mem::size_of::<FILE>()) as *mut FILE;
    if file.is_null() {
        close(fd);
        return ptr::null_mut();
    }
    
    (*file).fd = fd;
    (*file).error = 0;
    (*file).eof = 0;
    (*file).ungot = -1;
    
    file
}

#[no_mangle]
pub unsafe extern "C" fn fdopen(fd: i32, _mode: *const c_char) -> *mut FILE {
    let file = malloc(core::mem::size_of::<FILE>()) as *mut FILE;
    if file.is_null() {
        return ptr::null_mut();
    }
    (*file).fd = fd;
    (*file).error = 0;
    (*file).eof = 0;
    (*file).ungot = -1;
    file
}

#[no_mangle]
pub unsafe extern "C" fn fclose(stream: *mut FILE) -> c_int {
    if stream.is_null() {
        return -1;
    }
    // Don't close stdin/stdout/stderr if they are static
    if stream == stdin || stream == stdout || stream == stderr {
        return 0;
    }
    
    let fd = (*stream).fd;
    close(fd);
    free(stream as *mut c_void);
    0
}

#[no_mangle]
pub unsafe extern "C" fn fread(ptr: *mut c_void, size: usize, nmemb: usize, stream: *mut FILE) -> usize {
    if stream.is_null() {
        return 0;
    }
    
    let mut total_bytes = size * nmemb;
    let mut bytes_read = 0;
    let mut buf_ptr = ptr as *mut u8;

    // Handle ungot char
    if (*stream).ungot != -1 {
        *buf_ptr = (*stream).ungot as u8;
        buf_ptr = buf_ptr.add(1);
        bytes_read += 1;
        total_bytes -= 1;
        (*stream).ungot = -1;
    }

    if total_bytes > 0 {
        let buf = core::slice::from_raw_parts_mut(buf_ptr, total_bytes);
        let n = read_fd((*stream).fd, buf);
        if n < 0 {
            (*stream).error = 1;
        } else if n == 0 {
            (*stream).eof = 1;
        } else {
            bytes_read += n as usize;
        }
    }
    
    bytes_read / size
}

#[no_mangle]
pub unsafe extern "C" fn fwrite(ptr: *const c_void, size: usize, nmemb: usize, stream: *mut FILE) -> usize {
    if stream.is_null() {
        return 0;
    }
    let total_bytes = size * nmemb;
    let buf = core::slice::from_raw_parts(ptr as *const u8, total_bytes);
    let n = write_fd((*stream).fd, buf);
    
    if n < 0 {
        (*stream).error = 1;
        0
    } else {
        (n as usize) / size
    }
}

#[no_mangle]
pub unsafe extern "C" fn fputc(c: c_int, stream: *mut FILE) -> c_int {
    let buf = [c as u8];
    if fwrite(buf.as_ptr() as *const c_void, 1, 1, stream) == 1 {
        c
    } else {
        -1 // EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn fgetc(stream: *mut FILE) -> c_int {
    let mut c = 0u8;
    if fread(&mut c as *mut u8 as *mut c_void, 1, 1, stream) == 1 {
        c as c_int
    } else {
        -1 // EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn ungetc(c: c_int, stream: *mut FILE) -> c_int {
    if stream.is_null() || c == -1 {
        return -1;
    }
    (*stream).ungot = c;
    c
}

#[no_mangle]
pub unsafe extern "C" fn getc(stream: *mut FILE) -> c_int {
    fgetc(stream)
}

#[no_mangle]
pub unsafe extern "C" fn putc(c: c_int, stream: *mut FILE) -> c_int {
    fputc(c, stream)
}

#[no_mangle]
pub unsafe extern "C" fn putchar(c: c_int) -> c_int {
    fputc(c, stdout)
}

#[no_mangle]
pub unsafe extern "C" fn fputs(s: *const c_char, stream: *mut FILE) -> c_int {
    let mut len = 0;
    while *s.add(len) != 0 {
        len += 1;
    }
    if fwrite(s as *const c_void, 1, len, stream) == len {
        0
    } else {
        -1
    }
}

#[no_mangle]
pub unsafe extern "C" fn fflush(_stream: *mut FILE) -> c_int {
    0 // No buffering yet
}

#[no_mangle]
pub unsafe extern "C" fn fseek(stream: *mut FILE, offset: i64, whence: c_int) -> c_int {
    if stream.is_null() {
        return -1;
    }
    (*stream).ungot = -1; // Clear ungot char
    let res = lseek((*stream).fd, offset, whence);
    if res < 0 { -1 } else { 0 }
}

#[no_mangle]
pub unsafe extern "C" fn ftell(stream: *mut FILE) -> i64 {
    if stream.is_null() {
        return -1;
    }
    lseek((*stream).fd, 0, seek_mode::SEEK_CUR)
}

#[no_mangle]
pub unsafe extern "C" fn rewind(stream: *mut FILE) {
    fseek(stream, 0, seek_mode::SEEK_SET);
}

#[no_mangle]
pub unsafe extern "C" fn ferror(stream: *mut FILE) -> c_int {
    if stream.is_null() { 1 } else { (*stream).error }
}

#[no_mangle]
pub unsafe extern "C" fn feof(stream: *mut FILE) -> c_int {
    if stream.is_null() { 1 } else { (*stream).eof }
}

#[no_mangle]
pub unsafe extern "C" fn remove(pathname: *const c_char) -> c_int {
    let path = cstr_to_str(pathname);
    akuma_unlink(path)
}

#[no_mangle]
pub unsafe extern "C" fn rename(oldpath: *const c_char, newpath: *const c_char) -> c_int {
    let old = cstr_to_str(oldpath);
    let new = cstr_to_str(newpath);
    akuma_rename(old, new)
}

#[no_mangle]
pub unsafe extern "C" fn gettimeofday(tv: *mut timeval, _tz: *mut c_void) -> c_int {
    if tv.is_null() { return -1; }
    let us = libakuma::time();
    (*tv).tv_sec = (us / 1_000_000) as i64;
    (*tv).tv_usec = (us % 1_000_000) as i64;
    0
}

#[no_mangle]
pub unsafe extern "C" fn stat(pathname: *const c_char, statbuf: *mut Stat) -> c_int {
    let fd = open(pathname, open_flags::O_RDONLY as c_int, 0);
    if fd < 0 {
        return -1;
    }
    let res = fstat_impl(fd, statbuf);
    close(fd);
    res
}

#[no_mangle]
pub unsafe extern "C" fn fstat(fd: i32, statbuf: *mut Stat) -> c_int {
    fstat_impl(fd, statbuf)
}

unsafe fn fstat_impl(fd: i32, statbuf: *mut Stat) -> c_int {
    match libakuma::fstat(fd) {
        Ok(s) => {
            if !statbuf.is_null() {
                *statbuf = s;
            }
            0
        }
        Err(_) => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn mkdir(pathname: *const c_char, _mode: u32) -> c_int {
    let path = cstr_to_str(pathname);
    akuma_mkdir(path)
}

#[no_mangle]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    akuma_close(fd)
}

#[no_mangle]
pub unsafe extern "C" fn freopen(filename: *const c_char, mode: *const c_char, stream: *mut FILE) -> *mut FILE {
    if stream.is_null() { return ptr::null_mut(); }
    
    akuma_close((*stream).fd); 

    let new_stream_ptr = fopen(filename, mode);
    if new_stream_ptr.is_null() { return ptr::null_mut(); }
    
    let new_stream = &*new_stream_ptr;
    (*stream).fd = new_stream.fd;
    (*stream).error = new_stream.error;
    (*stream).eof = new_stream.eof;
    (*stream).ungot = new_stream.ungot;
    
    free(new_stream_ptr as *mut c_void);
    
    stream
}

#[no_mangle]
pub unsafe extern "C" fn unlink(pathname: *const c_char) -> c_int {
    let path = cstr_to_str(pathname);
    akuma_unlink(path)
}

#[no_mangle]
pub unsafe extern "C" fn exit(status: c_int) -> ! {
    akuma_exit(status)
}

#[no_mangle]
pub unsafe extern "C" fn time(tloc: *mut TimeT) -> TimeT {
    let t = (libakuma::time() / 1_000_000) as TimeT; // Akuma returns microseconds, C time() expects seconds
    if !tloc.is_null() {
        *tloc = t;
    }
    t
}

#[no_mangle]
pub unsafe extern "C" fn getenv(_name: *const c_char) -> *mut c_char {
    ptr::null_mut() // No environment variables supported yet
}

#[no_mangle]
pub unsafe extern "C" fn strtod(_nptr: *const c_char, _endptr: *mut *mut c_char) -> f64 {
    0.0 // Stub
}

#[no_mangle]
pub unsafe extern "C" fn strtof(_nptr: *const c_char, _endptr: *mut *mut c_char) -> f32 {
    0.0f32 // Stub
}

#[no_mangle]
pub unsafe extern "C" fn strtold(_nptr: *const c_char, _endptr: *mut *mut c_char) -> f64 { // long double as f64
    0.0 // Stub
}

#[no_mangle]
pub unsafe extern "C" fn strtol(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> i64 {
    let s = cstr_to_str(nptr);
    let mut current_ptr = s.as_ptr();
    let mut result: i64 = 0;
    let mut negative = false;

    while *current_ptr as char == ' ' || *current_ptr as char == '\t' {
        current_ptr = current_ptr.add(1);
    }

    if *current_ptr as char == '-' {
        negative = true;
        current_ptr = current_ptr.add(1);
    } else if *current_ptr as char == '+' {
        current_ptr = current_ptr.add(1);
    }

    let mut current_base = base;
    if current_base == 0 {
        if *current_ptr as char == '0' {
            if *current_ptr.add(1) as char == 'x' || *current_ptr.add(1) as char == 'X' {
                current_base = 16;
                current_ptr = current_ptr.add(2);
            } else {
                current_base = 8;
                current_ptr = current_ptr.add(1);
            }
        } else {
            current_base = 10;
        }
    }

    while *current_ptr != 0 {
        let digit = match *current_ptr as char {
            '0'..='9' => (*current_ptr as i64) - ('0' as i64),
            'a'..='z' => (*current_ptr as i64) - ('a' as i64) + 10,
            'A'..='Z' => (*current_ptr as i64) - ('A' as i64) + 10,
            _ => break,
        };
        if digit >= current_base as i64 {
            break;
        }
        result = result * current_base as i64 + digit;
        current_ptr = current_ptr.add(1);
    }

    if !endptr.is_null() {
        *endptr = current_ptr as *mut c_char;
    }

    if negative { -result } else { result }
}

#[no_mangle]
pub unsafe extern "C" fn strtoul(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> u64 {
    let s = cstr_to_str(nptr);
    let mut current_ptr = s.as_ptr();
    let mut result: u64 = 0;

    while *current_ptr as char == ' ' || *current_ptr as char == '\t' {
        current_ptr = current_ptr.add(1);
    }

    if *current_ptr as char == '+' {
        current_ptr = current_ptr.add(1);
    }

    let mut current_base = base;
    if current_base == 0 {
        if *current_ptr as char == '0' {
            if *current_ptr.add(1) as char == 'x' || *current_ptr.add(1) as char == 'X' {
                current_base = 16;
                current_ptr = current_ptr.add(2);
            } else {
                current_base = 8;
                current_ptr = current_ptr.add(1);
            }
        } else {
            current_base = 10;
        }
    }

    while *current_ptr != 0 {
        let digit = match *current_ptr as char {
            '0'..='9' => (*current_ptr as u64) - ('0' as u64),
            'a'..='z' => (*current_ptr as u64) - ('a' as u64) + 10,
            'A'..='Z' => (*current_ptr as u64) - ('A' as u64) + 10,
            _ => break,
        };
        if digit >= current_base as u64 {
            break;
        }
        result = result * current_base as u64 + digit;
        current_ptr = current_ptr.add(1);
    }

    if !endptr.is_null() {
        *endptr = current_ptr as *mut c_char;
    }
    result
}

#[no_mangle]
pub unsafe extern "C" fn strtoll(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> i64 {
    strtol(nptr, endptr, base)
}

#[no_mangle]
pub unsafe extern "C" fn strtoull(nptr: *const c_char, endptr: *mut *mut c_char, base: c_int) -> u64 {
    strtoul(nptr, endptr, base)
}

#[no_mangle]
pub unsafe extern "C" fn atoi(nptr: *const c_char) -> c_int {
    strtol(nptr, ptr::null_mut(), 10) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn __clear_cache(_beg: *mut c_void, _end: *mut c_void) {
    // AArch64 cache invalidation (simplified, might need more for correctness)
    // For now, a no-op should be fine.
    // asm volatile ("dsb sy" ::: "memory"); // Data synchronization barrier
    // asm volatile ("isb sy" ::: "memory"); // Instruction synchronization barrier
}

// ============================================================================
// Helpers
// ============================================================================

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
