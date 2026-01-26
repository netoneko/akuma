//! QuickJS runtime bindings for Akuma
//!
//! This module provides Rust bindings to the QuickJS JavaScript engine.

#![allow(non_snake_case)]
#![allow(dead_code)]

use alloc::alloc::{alloc, dealloc, realloc as rust_realloc, Layout};
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use libakuma::{close, fstat, open, open_flags, read_fd};

// Debug configuration
const DEBUG: bool = true;

#[inline]
fn debug(msg: &str) {
    if DEBUG {
        libakuma::print(msg);
    }
}

// ============================================================================
// C Library Memory Functions (for QuickJS)
// ============================================================================

/// Allocate memory - called by QuickJS
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

/// Free memory - called by QuickJS
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

/// Reallocate memory - called by QuickJS
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

// ============================================================================
// Helper functions for C stubs
// ============================================================================

/// Get system uptime in microseconds - called by C stubs
#[no_mangle]
pub extern "C" fn akuma_uptime() -> u64 {
    libakuma::uptime()
}

/// Exit the process - called by C stubs
#[no_mangle]
pub extern "C" fn akuma_exit(code: c_int) {
    libakuma::exit(code);
}

/// Print to stdout - called by C stubs
#[no_mangle]
pub unsafe extern "C" fn akuma_print(s: *const c_char, len: usize) {
    if s.is_null() {
        return;
    }
    let bytes = core::slice::from_raw_parts(s as *const u8, len);
    libakuma::write(libakuma::fd::STDOUT, bytes);
}

// ============================================================================
// QuickJS Types
// ============================================================================

/// Opaque runtime type
#[repr(C)]
pub struct JSRuntime {
    _private: [u8; 0],
}

/// Opaque context type
#[repr(C)]
pub struct JSContext {
    _private: [u8; 0],
}

/// JSValue union (for 64-bit, non-NaN-boxing mode)
#[repr(C)]
#[derive(Copy, Clone)]
pub union JSValueUnion {
    pub int32: i32,
    pub float64: f64,
    pub ptr: *mut c_void,
}

/// JSValue struct (for 64-bit, non-NaN-boxing mode)
#[repr(C)]
#[derive(Copy, Clone)]
pub struct JSValue {
    pub u: JSValueUnion,
    pub tag: i64,
}

// JSValue tag constants
pub const JS_TAG_INT: i64 = 0;
pub const JS_TAG_BOOL: i64 = 1;
pub const JS_TAG_NULL: i64 = 2;
pub const JS_TAG_UNDEFINED: i64 = 3;
pub const JS_TAG_EXCEPTION: i64 = 6;
pub const JS_TAG_FLOAT64: i64 = 7;
pub const JS_TAG_STRING: i64 = -7;
pub const JS_TAG_OBJECT: i64 = -1;

// JS_Eval flags
pub const JS_EVAL_TYPE_GLOBAL: c_int = 0;
pub const JS_EVAL_FLAG_STRICT: c_int = 1 << 3;

impl JSValue {
    /// Create undefined value
    pub fn undefined() -> Self {
        JSValue {
            u: JSValueUnion { int32: 0 },
            tag: JS_TAG_UNDEFINED,
        }
    }

    /// Check if this is an exception
    pub fn is_exception(&self) -> bool {
        self.tag == JS_TAG_EXCEPTION
    }

    /// Get the tag
    pub fn get_tag(&self) -> i64 {
        self.tag
    }
}

// ============================================================================
// QuickJS FFI
// ============================================================================

extern "C" {
    // Runtime management
    pub fn JS_NewRuntime() -> *mut JSRuntime;
    pub fn JS_FreeRuntime(rt: *mut JSRuntime);
    pub fn JS_SetMaxStackSize(rt: *mut JSRuntime, stack_size: usize);

    // Context management
    pub fn JS_NewContext(rt: *mut JSRuntime) -> *mut JSContext;
    pub fn JS_FreeContext(ctx: *mut JSContext);

    // Intrinsics
    pub fn JS_AddIntrinsicBaseObjects(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicDate(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicEval(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicStringNormalize(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicRegExpCompiler(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicRegExp(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicJSON(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicProxy(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicMapSet(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicTypedArrays(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicPromise(ctx: *mut JSContext);
    pub fn JS_AddIntrinsicBigInt(ctx: *mut JSContext);

    // Evaluation
    pub fn JS_Eval(
        ctx: *mut JSContext,
        input: *const c_char,
        input_len: usize,
        filename: *const c_char,
        eval_flags: c_int,
    ) -> JSValue;

    // Value management - use internal names since the public ones are static inline
    #[link_name = "__JS_FreeValue"]
    pub fn JS_FreeValue(ctx: *mut JSContext, v: JSValue);

    #[link_name = "__JS_FreeValueRT"]
    pub fn JS_FreeValueRT(rt: *mut JSRuntime, v: JSValue);

    pub fn JS_DupValue(ctx: *mut JSContext, v: JSValue) -> JSValue;

    // String conversion
    pub fn JS_ToCStringLen2(
        ctx: *mut JSContext,
        plen: *mut usize,
        val: JSValue,
        cesu8: c_int,
    ) -> *const c_char;
    pub fn JS_FreeCString(ctx: *mut JSContext, ptr: *const c_char);

    // Object operations
    pub fn JS_GetGlobalObject(ctx: *mut JSContext) -> JSValue;
    pub fn JS_SetPropertyStr(
        ctx: *mut JSContext,
        this_obj: JSValue,
        prop: *const c_char,
        val: JSValue,
    ) -> c_int;

    // Function creation
    pub fn JS_NewCFunction2(
        ctx: *mut JSContext,
        func: Option<
            unsafe extern "C" fn(*mut JSContext, JSValue, c_int, *mut JSValue) -> JSValue,
        >,
        name: *const c_char,
        length: c_int,
        cproto: c_int,
        magic: c_int,
    ) -> JSValue;

    // Exception handling
    pub fn JS_GetException(ctx: *mut JSContext) -> JSValue;
    pub fn JS_IsError(ctx: *mut JSContext, val: JSValue) -> c_int;

    // New string
    pub fn JS_NewStringLen(ctx: *mut JSContext, str1: *const c_char, len1: usize) -> JSValue;
    pub fn JS_NewString(ctx: *mut JSContext, str1: *const c_char) -> JSValue;

    // New object
    pub fn JS_NewObject(ctx: *mut JSContext) -> JSValue;
}

// JS_CFUNC_GENERIC constant
pub const JS_CFUNC_GENERIC: c_int = 0;

// ============================================================================
// Runtime Wrapper
// ============================================================================

/// QuickJS Runtime wrapper
pub struct Runtime {
    rt: *mut JSRuntime,
    ctx: *mut JSContext,
}

impl Runtime {
    /// Create a new QuickJS runtime with a context
    pub fn new() -> Option<Self> {
        unsafe {
            debug("qjs: JS_NewRuntime\n");
            let rt = JS_NewRuntime();
            if rt.is_null() {
                debug("qjs: JS_NewRuntime returned NULL\n");
                return None;
            }
            debug("qjs: JS_NewRuntime OK\n");

            // Set a reasonable stack size
            JS_SetMaxStackSize(rt, 256 * 1024);
            debug("qjs: JS_SetMaxStackSize OK\n");

            debug("qjs: JS_NewContext\n");
            // JS_NewContext internally calls JS_NewContextRaw + all JS_AddIntrinsic* functions
            // So we don't need to add intrinsics manually
            let ctx = JS_NewContext(rt);
            if ctx.is_null() {
                debug("qjs: JS_NewContext returned NULL\n");
                JS_FreeRuntime(rt);
                return None;
            }
            debug("qjs: JS_NewContext OK\n");

            Some(Runtime { rt, ctx })
        }
    }

    /// Get the context pointer
    pub fn context(&self) -> *mut JSContext {
        self.ctx
    }

    /// Evaluate JavaScript code
    pub fn eval(&self, code: &str, filename: &str) -> Result<JSValue, String> {
        unsafe {
            debug("qjs: eval() enter\n");
            
            // Create null-terminated filename
            let mut filename_buf = alloc::vec![0u8; filename.len() + 1];
            filename_buf[..filename.len()].copy_from_slice(filename.as_bytes());

            debug("qjs: calling JS_Eval\n");
            let result = JS_Eval(
                self.ctx,
                code.as_ptr() as *const c_char,
                code.len(),
                filename_buf.as_ptr() as *const c_char,
                JS_EVAL_TYPE_GLOBAL,
            );
            debug("qjs: JS_Eval returned\n");

            if result.is_exception() {
                debug("qjs: got exception\n");
                // Get the exception
                let exc = JS_GetException(self.ctx);
                let err_str = self.value_to_string(exc);
                JS_FreeValue(self.ctx, exc);
                return Err(err_str);
            }

            debug("qjs: eval success\n");
            Ok(result)
        }
    }

    /// Convert a JSValue to a Rust String
    pub fn value_to_string(&self, val: JSValue) -> String {
        unsafe {
            let mut len: usize = 0;
            let cstr = JS_ToCStringLen2(self.ctx, &mut len, val, 0);
            if cstr.is_null() {
                return String::from("[error converting to string]");
            }

            let bytes = core::slice::from_raw_parts(cstr as *const u8, len);
            let result = match core::str::from_utf8(bytes) {
                Ok(s) => String::from(s),
                Err(_) => String::from("[invalid utf8]"),
            };

            JS_FreeCString(self.ctx, cstr);
            result
        }
    }

    /// Free a JSValue
    pub fn free_value(&self, val: JSValue) {
        unsafe {
            JS_FreeValue(self.ctx, val);
        }
    }

    /// Get the global object
    pub fn global_object(&self) -> JSValue {
        unsafe { JS_GetGlobalObject(self.ctx) }
    }

    /// Set a property on an object
    pub fn set_property_str(&self, obj: JSValue, name: &str, val: JSValue) -> bool {
        unsafe {
            let mut name_buf = alloc::vec![0u8; name.len() + 1];
            name_buf[..name.len()].copy_from_slice(name.as_bytes());
            JS_SetPropertyStr(self.ctx, obj, name_buf.as_ptr() as *const c_char, val) >= 0
        }
    }

    /// Create a new C function
    pub fn new_c_function(
        &self,
        func: unsafe extern "C" fn(*mut JSContext, JSValue, c_int, *mut JSValue) -> JSValue,
        name: &str,
        length: c_int,
    ) -> JSValue {
        unsafe {
            let mut name_buf = alloc::vec![0u8; name.len() + 1];
            name_buf[..name.len()].copy_from_slice(name.as_bytes());
            JS_NewCFunction2(
                self.ctx,
                Some(func),
                name_buf.as_ptr() as *const c_char,
                length,
                JS_CFUNC_GENERIC,
                0,
            )
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            JS_FreeContext(self.ctx);
            JS_FreeRuntime(self.rt);
        }
    }
}

// ============================================================================
// File Reading Helper
// ============================================================================

/// Read a file into a String
pub fn read_file(path: &str) -> Result<String, &'static str> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err("Failed to open file");
    }

    let stat = match fstat(fd) {
        Ok(s) => s,
        Err(_) => {
            close(fd);
            return Err("Failed to stat file");
        }
    };

    let size = stat.st_size as usize;
    let mut content: Vec<u8> = alloc::vec![0u8; size];

    let mut total_read = 0;
    while total_read < size {
        let n = read_fd(fd, &mut content[total_read..]);
        if n <= 0 {
            break;
        }
        total_read += n as usize;
    }

    close(fd);

    match String::from_utf8(content) {
        Ok(s) => Ok(s),
        Err(_) => Err("File is not valid UTF-8"),
    }
}
