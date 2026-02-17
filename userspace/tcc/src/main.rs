#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

use alloc::alloc::{GlobalAlloc, Layout};
use core::ffi::{c_char, c_void, c_int};
use core::ptr;
use libakuma::{
    close, exit, open, open_flags, read_fd, seek_mode, write_fd,
    lseek, unlink, rename, mkdir, getcwd, fd, print, Stat,
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
        stdin = &mut STDIN_FILE;
        stdout = &mut STDOUT_FILE;
        stderr = &mut STDERR_FILE;

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
    libakuma::open(path, flags as u32)
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
    let cwd = libakuma::getcwd();
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
    let ret = libakuma::mmap(addr as usize, length, prot as u32, flags as u32);
    if ret == usize::MAX {
        return -1isize as *mut c_void; // MAP_FAILED
    }
    ret as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn munmap(addr: *mut c_void, length: usize) -> c_int {
    libakuma::munmap(addr as usize, length) as c_int
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
    let filename_str = cstr_to_str(filename);
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

    let fd = open(filename_str, flags);
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
pub unsafe extern "C" fn fflush(stream: *mut FILE) -> c_int {
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
    unlink(path)
}

#[no_mangle]
pub unsafe extern "C" fn rename(oldpath: *const c_char, newpath: *const c_char) -> c_int {
    let old = cstr_to_str(oldpath);
    let new = cstr_to_str(newpath);
    rename(old, new)
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
    let path = cstr_to_str(pathname);
    let fd = open(path, open_flags::O_RDONLY);
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
    libakuma::mkdir(path)
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
