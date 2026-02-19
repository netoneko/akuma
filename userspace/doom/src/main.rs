//! DOOM for Akuma
//!
//! Runs doomgeneric (portable DOOM engine) as a userspace ELF binary.
//! Renders to the kernel's ramfb framebuffer via syscalls.
//! Input is received from SSH terminal via poll_input_event.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::alloc::{alloc, dealloc, realloc as rust_realloc, Layout};
use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use libakuma::{print, write, fd};

// ============================================================================
// DOOM resolution (must match DOOMGENERIC_RESX/Y in build.rs defines)
// ============================================================================

const DOOM_WIDTH: u32 = 320;
const DOOM_HEIGHT: u32 = 200;
const DOOM_FB_SIZE: usize = (DOOM_WIDTH as usize) * (DOOM_HEIGHT as usize) * 4;

// ============================================================================
// ANSI terminal rendering (SSH display)
// ============================================================================

/// Output dimensions for ANSI art (80x24 — fits standard 80×24 terminals)
/// Each character row displays 2 pixel rows via half-block, so 24 rows → 48 pixel rows
const ANSI_COLS: usize = 80;
const ANSI_ROWS: usize = 24;

/// Render every Nth frame as ANSI art (~7 fps at 35 tick/s)
const ANSI_FRAME_SKIP: u32 = 5;

/// Frame counter for ANSI render throttling
static mut ANSI_FRAME_COUNT: u32 = 0;

/// Whether we've entered the alternate screen buffer yet
static mut ANSI_SCREEN_INIT: bool = false;

/// Static buffer for ANSI frame data (avoids per-frame heap allocation)
/// 80 cols × 24 rows × ~42 bytes/cell + overhead ≈ 82KB
const ANSI_BUF_SIZE: usize = 90000;
static mut ANSI_BUF: [u8; ANSI_BUF_SIZE] = [0u8; ANSI_BUF_SIZE];

// ============================================================================
// doomgeneric C entry points
// ============================================================================

unsafe extern "C" {
    fn doomgeneric_Create(argc: c_int, argv: *mut *mut c_char);
    fn doomgeneric_Tick();
    static mut DG_ScreenBuffer: *mut u32;
}

// ============================================================================
// C Library FFI (called by stubs.c)
// ============================================================================

/// Allocate memory - called by C stubs via `malloc`
#[no_mangle]
pub unsafe extern "C" fn akuma_malloc(size: usize) -> *mut c_void {
    malloc(size)
}

/// Free memory - called by C stubs via `free`
#[no_mangle]
pub unsafe extern "C" fn akuma_free(ptr: *mut c_void) {
    free(ptr)
}

/// Reallocate memory - called by C stubs via `realloc`
#[no_mangle]
pub unsafe extern "C" fn akuma_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    realloc(ptr, size)
}

/// malloc implementation with size header
#[no_mangle]
pub unsafe extern "C" fn malloc(size: usize) -> *mut c_void {
    if size == 0 {
        return ptr::null_mut();
    }
    let layout = match Layout::from_size_align(size + 8, 8) {
        Ok(l) => l,
        Err(_) => return ptr::null_mut(),
    };
    let ptr = unsafe { alloc(layout) };
    if ptr.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        *(ptr as *mut usize) = size;
        ptr.add(8) as *mut c_void
    }
}

/// free implementation
#[no_mangle]
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let real_ptr = (ptr as *mut u8).sub(8);
        let size = *(real_ptr as *const usize);
        let layout = match Layout::from_size_align(size + 8, 8) {
            Ok(l) => l,
            Err(_) => return,
        };
        dealloc(real_ptr, layout);
    }
}

/// realloc implementation
#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut c_void, new_size: usize) -> *mut c_void {
    if ptr.is_null() {
        return unsafe { malloc(new_size) };
    }
    if new_size == 0 {
        unsafe { free(ptr) };
        return ptr::null_mut();
    }

    unsafe {
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
        *(new_ptr as *mut usize) = new_size;
        new_ptr.add(8) as *mut c_void
    }
}

/// calloc implementation
#[no_mangle]
pub unsafe extern "C" fn calloc(nmemb: usize, size: usize) -> *mut c_void {
    let total = nmemb.saturating_mul(size);
    let ptr = unsafe { malloc(total) };
    if !ptr.is_null() {
        unsafe { ptr::write_bytes(ptr as *mut u8, 0, total) };
    }
    ptr
}

// ============================================================================
// System FFI (called by stubs.c)
// ============================================================================

/// Get system uptime in microseconds
#[no_mangle]
pub extern "C" fn akuma_uptime() -> u64 {
    libakuma::uptime()
}

/// Exit the process
#[no_mangle]
pub extern "C" fn akuma_exit(code: c_int) {
    libakuma::exit(code);
}

/// Print to stdout, converting bare \n to \r\n for correct terminal display.
///
/// The shell's \n→\r\n translation races with the process setting raw_mode:
/// if raw_mode is set before the shell reads buffered output, init text gets
/// bare \n which causes staircase rendering. Converting here avoids the race.
#[no_mangle]
pub unsafe extern "C" fn akuma_print(s: *const c_char, len: usize) {
    if s.is_null() || len == 0 {
        return;
    }
    let bytes = unsafe { core::slice::from_raw_parts(s as *const u8, len) };
    let mut start = 0;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            if i > start {
                write(fd::STDOUT, &bytes[start..i]);
            }
            write(fd::STDOUT, b"\r\n");
            start = i + 1;
        }
    }
    if start < bytes.len() {
        write(fd::STDOUT, &bytes[start..]);
    }
}

/// Open a file, returns fd or negative errno
#[no_mangle]
pub unsafe extern "C" fn akuma_open(path: *const c_char, path_len: usize, flags: c_int) -> c_int {
    if path.is_null() || path_len == 0 {
        return -1;
    }
    let path_bytes = unsafe { core::slice::from_raw_parts(path as *const u8, path_len) };
    let path_str = match core::str::from_utf8(path_bytes) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    libakuma::open(path_str, flags as u32) as c_int
}

/// Close a file descriptor
#[no_mangle]
pub extern "C" fn akuma_close(fd_num: c_int) -> c_int {
    libakuma::close(fd_num as i32) as c_int
}

/// Read from a file descriptor
#[no_mangle]
pub unsafe extern "C" fn akuma_read(fd_num: c_int, buf: *mut c_void, count: usize) -> c_int {
    if buf.is_null() {
        return -1;
    }
    let slice = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, count) };
    libakuma::read_fd(fd_num, slice) as c_int
}

/// Write to a file descriptor
#[no_mangle]
pub unsafe extern "C" fn akuma_write_fd(fd_num: c_int, buf: *const c_void, count: usize) -> c_int {
    if buf.is_null() {
        return -1;
    }
    let slice = unsafe { core::slice::from_raw_parts(buf as *const u8, count) };
    libakuma::write_fd(fd_num, slice) as c_int
}

/// lseek on a file descriptor
#[no_mangle]
pub extern "C" fn akuma_lseek(fd_num: c_int, offset: i64, whence: c_int) -> c_int {
    libakuma::lseek(fd_num, offset, whence) as c_int
}

/// Get file size via fstat
#[no_mangle]
pub extern "C" fn akuma_fstat_size(fd_num: c_int) -> c_int {
    match libakuma::fstat(fd_num) {
        Ok(stat) => stat.st_size as c_int,
        Err(_) => -1,
    }
}

/// mkdir
#[no_mangle]
pub unsafe extern "C" fn akuma_mkdir(path: *const c_char, path_len: usize) -> c_int {
    if path.is_null() || path_len == 0 {
        return -1;
    }
    let path_bytes = unsafe { core::slice::from_raw_parts(path as *const u8, path_len) };
    let path_str = match core::str::from_utf8(path_bytes) {
        Ok(s) => s,
        Err(_) => return -1,
    };
    libakuma::mkdir(path_str) as c_int
}

// ============================================================================
// DOOM key codes (from doomkeys.h)
// ============================================================================

const KEY_RIGHTARROW: u8 = 0xae;
const KEY_LEFTARROW: u8 = 0xac;
const KEY_UPARROW: u8 = 0xad;
const KEY_DOWNARROW: u8 = 0xaf;
const KEY_FIRE: u8 = 0xa3;
const KEY_USE: u8 = 0xa2;
const KEY_ESCAPE: u8 = 27;
const KEY_ENTER: u8 = 13;
const KEY_TAB: u8 = 9;
#[allow(dead_code)]
const KEY_RSHIFT: u8 = 0x80 + 0x36;
#[allow(dead_code)]
const KEY_RCTRL: u8 = 0x80 + 0x1d;

// ============================================================================
// Input key queue and held-key tracking
// ============================================================================

/// Key event: (pressed, doom_keycode)
struct KeyEvent {
    pressed: bool,
    key: u8,
}

/// Simple ring buffer for key events
const KEY_QUEUE_SIZE: usize = 64;
static mut KEY_QUEUE: [KeyEvent; KEY_QUEUE_SIZE] = {
    const EMPTY: KeyEvent = KeyEvent { pressed: false, key: 0 };
    [EMPTY; KEY_QUEUE_SIZE]
};
static mut KEY_QUEUE_HEAD: usize = 0;
static mut KEY_QUEUE_TAIL: usize = 0;

unsafe fn key_queue_push(pressed: bool, key: u8) {
    let next = (KEY_QUEUE_HEAD + 1) % KEY_QUEUE_SIZE;
    if next != KEY_QUEUE_TAIL {
        KEY_QUEUE[KEY_QUEUE_HEAD] = KeyEvent { pressed, key };
        KEY_QUEUE_HEAD = next;
    }
}

unsafe fn key_queue_pop() -> Option<KeyEvent> {
    if KEY_QUEUE_HEAD == KEY_QUEUE_TAIL {
        return None;
    }
    let ev = KeyEvent {
        pressed: KEY_QUEUE[KEY_QUEUE_TAIL].pressed,
        key: KEY_QUEUE[KEY_QUEUE_TAIL].key,
    };
    KEY_QUEUE_TAIL = (KEY_QUEUE_TAIL + 1) % KEY_QUEUE_SIZE;
    Some(ev)
}

/// Track held keys with timestamps for release-timeout simulation.
/// Terminals only send key-down events. When a key is held, autorepeat sends
/// repeated bytes (~30ms apart). We detect "key released" when autorepeat stops
/// (no new byte within RELEASE_TIMEOUT_US microseconds).
const MAX_HELD_KEYS: usize = 16;
const RELEASE_TIMEOUT_US: u64 = 150_000; // 150ms — long enough for autorepeat gaps

struct HeldKey {
    key: u8,
    last_seen_us: u64,
    active: bool,
}

static mut HELD_KEYS: [HeldKey; MAX_HELD_KEYS] = {
    const EMPTY: HeldKey = HeldKey { key: 0, last_seen_us: 0, active: false };
    [EMPTY; MAX_HELD_KEYS]
};

/// Mark a key as pressed. Returns true if this is a new press (was not held).
unsafe fn hold_key(key: u8, now_us: u64) -> bool {
    // Check if already held
    for i in 0..MAX_HELD_KEYS {
        if HELD_KEYS[i].active && HELD_KEYS[i].key == key {
            HELD_KEYS[i].last_seen_us = now_us;
            return false; // Already held, just refresh timestamp
        }
    }
    // New key press — find an empty slot
    for i in 0..MAX_HELD_KEYS {
        if !HELD_KEYS[i].active {
            HELD_KEYS[i] = HeldKey { key, last_seen_us: now_us, active: true };
            return true; // New press
        }
    }
    false // No slot available
}

/// Check for keys that haven't been refreshed within the timeout and release them.
unsafe fn expire_held_keys(now_us: u64) {
    for i in 0..MAX_HELD_KEYS {
        if HELD_KEYS[i].active && now_us.wrapping_sub(HELD_KEYS[i].last_seen_us) > RELEASE_TIMEOUT_US {
            key_queue_push(false, HELD_KEYS[i].key);
            HELD_KEYS[i].active = false;
        }
    }
}

/// Returns true if a key should use held-key tracking (movement, action keys).
/// One-shot keys (Enter, Escape, numbers) get instant press+release instead.
fn is_holdable_key(key: u8) -> bool {
    matches!(key,
        KEY_UPARROW | KEY_DOWNARROW | KEY_LEFTARROW | KEY_RIGHTARROW |
        KEY_FIRE | KEY_USE | KEY_RSHIFT | KEY_RCTRL | KEY_TAB
    )
}

/// Translate a single terminal byte to a DOOM keycode
fn byte_to_doom_key(ch: u8) -> Option<u8> {
    match ch {
        b'w' | b'W' => Some(KEY_UPARROW),
        b's' | b'S' => Some(KEY_DOWNARROW),
        b'a' | b'A' => Some(KEY_LEFTARROW),
        b'd' | b'D' => Some(KEY_RIGHTARROW),
        b' ' => Some(KEY_FIRE),
        b'e' | b'E' => Some(KEY_USE),
        b'q' | b'Q' => Some(KEY_RSHIFT), // Q = run (shift)
        0x0d => Some(KEY_ENTER),
        0x1b => Some(KEY_ESCAPE),
        0x09 => Some(KEY_TAB),
        b'1'..=b'7' => Some(ch),
        b'y' | b'Y' | b'n' | b'N' => Some(ch), // Yes/No prompts
        _ => {
            if ch >= 0x20 && ch < 0x7f { Some(ch) } else { None }
        }
    }
}

/// Read terminal input, translate to DOOM keycodes, and manage held-key state.
fn process_input() {
    let now_us = libakuma::uptime();

    let mut buf = [0u8; 64];
    let n = libakuma::poll_input_event(0, &mut buf);

    if n > 0 {
        let bytes = &buf[..n as usize];
        let mut i = 0;
        while i < bytes.len() {
            let doom_key = if bytes[i] == 0x1b && i + 2 < bytes.len() && bytes[i + 1] == b'[' {
                let k = match bytes[i + 2] {
                    b'A' => Some(KEY_UPARROW),
                    b'B' => Some(KEY_DOWNARROW),
                    b'C' => Some(KEY_RIGHTARROW),
                    b'D' => Some(KEY_LEFTARROW),
                    _ => None,
                };
                i += 3;
                k
            } else {
                let k = byte_to_doom_key(bytes[i]);
                i += 1;
                k
            };

            if let Some(k) = doom_key {
                unsafe {
                    if is_holdable_key(k) {
                        if hold_key(k, now_us) {
                            key_queue_push(true, k);
                        }
                    } else {
                        key_queue_push(true, k);
                        key_queue_push(false, k);
                    }
                }
            }
        }
    }

    // Release keys that haven't been refreshed
    unsafe { expire_held_keys(now_us); }
}

// ============================================================================
// doomgeneric platform callbacks (called by C code)
// ============================================================================

/// Initialize the platform
#[no_mangle]
pub extern "C" fn DG_Init() {
    print("[DOOM] Initializing framebuffer...\r\n");
    let ret = libakuma::fb_init(DOOM_WIDTH, DOOM_HEIGHT);
    if ret < 0 {
        print("[DOOM] ERROR: Failed to initialize framebuffer!\r\n");
        print("[DOOM] Make sure QEMU was started with -device ramfb\r\n");
    } else {
        print("[DOOM] Framebuffer ready (320x200)\r\n");
    }

    // Set terminal to raw mode for input (flag 0x01 = RAW_MODE_ENABLE)
    libakuma::set_terminal_attributes(fd::STDIN, 0, 0x01);
}

/// Copy rendered frame to the ramfb framebuffer AND render ANSI art to SSH terminal
#[no_mangle]
pub extern "C" fn DG_DrawFrame() {
    unsafe {
        if DG_ScreenBuffer.is_null() {
            return;
        }

        // Always update the ramfb (QEMU display window)
        let pixels = core::slice::from_raw_parts(
            DG_ScreenBuffer as *const u8,
            DOOM_FB_SIZE,
        );
        libakuma::fb_draw(pixels);

        // Throttled ANSI art to SSH terminal
        ANSI_FRAME_COUNT += 1;
        if ANSI_FRAME_COUNT % ANSI_FRAME_SKIP == 0 {
            render_ansi_frame(DG_ScreenBuffer);
        }
    }
}

/// Write a u8 as decimal ASCII digits into a static buffer, return bytes written
fn write_u8_dec(buf: &mut [u8], pos: usize, val: u8) -> usize {
    if val >= 100 {
        buf[pos] = b'0' + val / 100;
        buf[pos + 1] = b'0' + (val / 10) % 10;
        buf[pos + 2] = b'0' + val % 10;
        3
    } else if val >= 10 {
        buf[pos] = b'0' + val / 10;
        buf[pos + 1] = b'0' + val % 10;
        2
    } else {
        buf[pos] = b'0' + val;
        1
    }
}

/// Copy a byte slice into a static buffer at position, return new position
fn buf_copy(buf: &mut [u8], pos: usize, src: &[u8]) -> usize {
    buf[pos..pos + src.len()].copy_from_slice(src);
    pos + src.len()
}

/// Render the DOOM framebuffer as ANSI truecolor (24-bit) half-block art
///
/// Uses ▀ (upper half block) with foreground = top pixel, background = bottom pixel.
/// Resolution: 112×35 characters from DOOM's 320×200 framebuffer.
/// Aspect ratio: 112/35 = 3.2; with ~2:1 char cells → 1.6:1 (matches 320:200).
///
/// Uses a static buffer to avoid per-frame heap allocation.
/// Sleeps after writing to let the SSH channel drain (prevents kernel OOM).
unsafe fn render_ansi_frame(fb: *mut u32) {
    const SRC_W: usize = 320;
    const SRC_H: usize = 200;
    // Half-block UTF-8 bytes for ▀ (U+2580)
    const HALF_BLOCK: [u8; 3] = [0xE2, 0x96, 0x80];
    // Virtual pixel rows = ANSI_ROWS * 2 (each char row shows 2 via half-block)
    const VIRT_H: usize = ANSI_ROWS * 2;

    let buf = &mut ANSI_BUF;
    let mut pos: usize = 0;

    if !ANSI_SCREEN_INIT {
        ANSI_SCREEN_INIT = true;
        // First frame: clear visible display + scrollback, then home
        pos = buf_copy(buf, pos, b"\x1b[2J\x1b[3J\x1b[H");
    }

    // Every frame: hide cursor + move to row 1, col 1
    pos = buf_copy(buf, pos, b"\x1b[?25l\x1b[H");

    // Track previous cell colors to skip redundant escape sequences.
    // In DOOM, large horizontal runs share colors (walls, floor, ceiling),
    // so this typically cuts frame size by 40-60%.
    let mut prev_tr: u8 = 255;
    let mut prev_tg: u8 = 255;
    let mut prev_tb: u8 = 255;
    let mut prev_br: u8 = 255;
    let mut prev_bg: u8 = 255;
    let mut prev_bb: u8 = 255;

    for row in 0..ANSI_ROWS {
        // Map row to source Y using proportional scaling
        let top_y = (row * 2) * SRC_H / VIRT_H;
        let bot_y = (row * 2 + 1) * SRC_H / VIRT_H;

        // Reset color tracking at each row start (after the newline/reset)
        prev_tr = 255; prev_tg = 255; prev_tb = 255;
        prev_br = 255; prev_bg = 255; prev_bb = 255;

        for col in 0..ANSI_COLS {
            let x = col * SRC_W / ANSI_COLS;

            let top_px = *fb.add(top_y * SRC_W + x);
            let bot_px = *fb.add(bot_y * SRC_W + x);

            // Extract RGB (pixel format: 0x00RRGGBB)
            let tr = ((top_px >> 16) & 0xFF) as u8;
            let tg = ((top_px >> 8) & 0xFF) as u8;
            let tb = (top_px & 0xFF) as u8;
            let br = ((bot_px >> 16) & 0xFF) as u8;
            let bg = ((bot_px >> 8) & 0xFF) as u8;
            let bb = (bot_px & 0xFF) as u8;

            let fg_same = tr == prev_tr && tg == prev_tg && tb == prev_tb;
            let bg_same = br == prev_br && bg == prev_bg && bb == prev_bb;

            if fg_same && bg_same {
                // Same colors as previous cell — just emit the block character
            } else if fg_same {
                // Only background changed
                pos = buf_copy(buf, pos, b"\x1b[48;2;");
                pos += write_u8_dec(buf, pos, br);
                buf[pos] = b';'; pos += 1;
                pos += write_u8_dec(buf, pos, bg);
                buf[pos] = b';'; pos += 1;
                pos += write_u8_dec(buf, pos, bb);
                buf[pos] = b'm'; pos += 1;
            } else if bg_same {
                // Only foreground changed
                pos = buf_copy(buf, pos, b"\x1b[38;2;");
                pos += write_u8_dec(buf, pos, tr);
                buf[pos] = b';'; pos += 1;
                pos += write_u8_dec(buf, pos, tg);
                buf[pos] = b';'; pos += 1;
                pos += write_u8_dec(buf, pos, tb);
                buf[pos] = b'm'; pos += 1;
            } else {
                // Both changed — full escape
                pos = buf_copy(buf, pos, b"\x1b[38;2;");
                pos += write_u8_dec(buf, pos, tr);
                buf[pos] = b';'; pos += 1;
                pos += write_u8_dec(buf, pos, tg);
                buf[pos] = b';'; pos += 1;
                pos += write_u8_dec(buf, pos, tb);
                pos = buf_copy(buf, pos, b";48;2;");
                pos += write_u8_dec(buf, pos, br);
                buf[pos] = b';'; pos += 1;
                pos += write_u8_dec(buf, pos, bg);
                buf[pos] = b';'; pos += 1;
                pos += write_u8_dec(buf, pos, bb);
                buf[pos] = b'm'; pos += 1;
            }

            prev_tr = tr; prev_tg = tg; prev_tb = tb;
            prev_br = br; prev_bg = bg; prev_bb = bb;

            // ▀ upper half block
            pos = buf_copy(buf, pos, &HALF_BLOCK);
        }

        // Reset colors; newline for all rows except the last (avoids scroll)
        pos = buf_copy(buf, pos, b"\x1b[0m");
        if row < ANSI_ROWS - 1 {
            pos = buf_copy(buf, pos, b"\r\n");
        }
    }

    // Write the whole frame at once — smaller frames from color dedup
    // mean we can push it in fewer chunks
    let chunk = pos / 2;
    let mut offset = 0;
    while offset < pos {
        let end = (offset + chunk).min(pos);
        write(fd::STDOUT, &buf[offset..end]);
        offset = end;
    }

    // Brief sleep to let the SSH channel drain. Shorter than before because
    // deduplicated frames are much smaller (~30-50KB vs ~78KB).
    libakuma::sleep_ms(30);
}

/// Sleep for `ms` milliseconds
#[no_mangle]
pub extern "C" fn DG_SleepMs(ms: u32) {
    if ms > 0 {
        libakuma::sleep_ms(ms as u64);
    }
}

/// Get ticks (milliseconds since boot)
#[no_mangle]
pub extern "C" fn DG_GetTicksMs() -> u32 {
    (libakuma::uptime() / 1000) as u32
}

/// Get next key event
///
/// Returns 1 if an event is available, 0 if not.
#[no_mangle]
pub unsafe extern "C" fn DG_GetKey(pressed: *mut c_int, doom_key: *mut u8) -> c_int {
    // Poll for new input from terminal
    process_input();

    // Return next queued event
    unsafe {
        match key_queue_pop() {
            Some(ev) => {
                *pressed = if ev.pressed { 1 } else { 0 };
                *doom_key = ev.key;
                1
            }
            None => 0,
        }
    }
}

/// Set window title (no-op on bare metal)
#[no_mangle]
pub unsafe extern "C" fn DG_SetWindowTitle(_title: *const c_char) {
    // No window title on bare metal
}

// ============================================================================
// Entry point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    print("=== DOOM on Akuma ===\r\n");
    print("Starting DOOM engine...\r\n");

    // Set up arguments: pass the WAD file path
    // doomgeneric expects: argv[0] = program name, -iwad <path>
    let arg0 = b"doom\0";
    let arg1 = b"-iwad\0";
    let arg2 = b"/doom1.wad\0";

    let mut argv: [*mut c_char; 4] = [
        arg0.as_ptr() as *mut c_char,
        arg1.as_ptr() as *mut c_char,
        arg2.as_ptr() as *mut c_char,
        ptr::null_mut(),
    ];

    unsafe {
        doomgeneric_Create(3, argv.as_mut_ptr());

        print("[DOOM] Engine initialized, entering main loop\r\n");

        loop {
            doomgeneric_Tick();
        }
    }
}
