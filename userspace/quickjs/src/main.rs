//! QuickJS JavaScript Runtime for Akuma
//!
//! A minimal JavaScript runtime using Bellard's QuickJS engine.

#![no_std]
#![no_main]

extern crate alloc;

use core::ffi::c_int;

use libakuma::{arg, argc, exit, print};

mod runtime;

use runtime::{JSContext, JSValue, Runtime};

// ============================================================================
// Debug Configuration
// ============================================================================

const DEBUG: bool = false;

#[inline]
fn debug(msg: &str) {
    if DEBUG {
        print(msg);
    }
}

// ============================================================================
// Console API Implementation
// ============================================================================

/// Native print function - implements console.log
unsafe extern "C" fn js_print(
    ctx: *mut JSContext,
    _this_val: JSValue,
    argc: c_int,
    argv: *mut JSValue,
) -> JSValue {
    for i in 0..argc {
        if i > 0 {
            print(" ");
        }

        let val = *argv.add(i as usize);
        let mut len: usize = 0;
        let cstr = runtime::JS_ToCStringLen2(ctx, &mut len, val, 0);

        if !cstr.is_null() {
            let bytes = core::slice::from_raw_parts(cstr as *const u8, len);
            libakuma::write(libakuma::fd::STDOUT, bytes);
            runtime::JS_FreeCString(ctx, cstr);
        }
    }
    print("\n");

    JSValue::undefined()
}

/// Setup the console object with log method
fn setup_console(rt: &Runtime) {
    debug("qjs: setup_console start\n");
    unsafe {
        // Get global object
        debug("qjs: getting global\n");
        let global = rt.global_object();
        debug("qjs: got global\n");

        // Create console object
        debug("qjs: creating console object\n");
        let console = runtime::JS_NewObject(rt.context());
        debug("qjs: created console object\n");

        // Create and set console.log function
        debug("qjs: creating log fn\n");
        let log_fn = rt.new_c_function(js_print, "log", 1);
        debug("qjs: setting log\n");
        rt.set_property_str(console, "log", log_fn);

        // Also add console.info, console.warn, console.error as aliases
        let info_fn = rt.new_c_function(js_print, "info", 1);
        rt.set_property_str(console, "info", info_fn);

        let warn_fn = rt.new_c_function(js_print, "warn", 1);
        rt.set_property_str(console, "warn", warn_fn);

        let error_fn = rt.new_c_function(js_print, "error", 1);
        rt.set_property_str(console, "error", error_fn);

        // Set console on global
        debug("qjs: setting console on global\n");
        rt.set_property_str(global, "console", console);

        // Also add a global print function
        let print_fn = rt.new_c_function(js_print, "print", 1);
        rt.set_property_str(global, "print", print_fn);

        debug("qjs: freeing global\n");
        rt.free_value(global);
        debug("qjs: setup_console done\n");
    }
}


// ============================================================================
// Main Entry Point
// ============================================================================

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let code = main();
    exit(code);
}

fn main() -> i32 {
    debug("qjs: starting\n");
    
    // Check command line arguments
    if argc() < 2 {
        print("QuickJS for Akuma\n");
        print("Usage: qjs <script.js>\n");
        print("       qjs -e \"<code>\"\n");
        return 1;
    }

    debug("qjs: parsing args\n");
    
    let first_arg = match arg(1) {
        Some(a) => a,
        None => {
            print("Error: Failed to get argument\n");
            return 1;
        }
    };

    debug("qjs: creating runtime\n");
    
    // Initialize the runtime
    let rt = match Runtime::new() {
        Some(r) => r,
        None => {
            print("Error: Failed to create JavaScript runtime\n");
            return 1;
        }
    };
    
    debug("qjs: runtime created\n");

    // Setup console object
    setup_console(&rt);

    debug("qjs: checking args\n");
    
    // Check if we're evaluating inline code or a file
    if first_arg == "-e" {
        // Inline code execution
        if argc() < 3 {
            print("Error: -e requires code argument\n");
            return 1;
        }

        let code = match arg(2) {
            Some(c) => c,
            None => {
                print("Error: Failed to get code argument\n");
                return 1;
            }
        };

        match rt.eval(code, "<cmdline>") {
            Ok(result) => {
                // Print the result if it's not undefined
                if result.get_tag() != runtime::JS_TAG_UNDEFINED {
                    let str_result = rt.value_to_string(result);
                    print(&str_result);
                    print("\n");
                }
                rt.free_value(result);
                0
            }
            Err(e) => {
                print("Error: ");
                print(&e);
                print("\n");
                1
            }
        }
    } else {
        // File execution
        let script_path = first_arg;
        if DEBUG {
            debug("qjs: script path = ");
            print(script_path);
            print("\n");
        }

        // Read the script file
        debug("qjs: reading file\n");
        let code = match runtime::read_file(script_path) {
            Ok(c) => c,
            Err(e) => {
                print("Error reading file: ");
                print(e);
                print("\n");
                return 1;
            }
        };
        if DEBUG {
            debug("qjs: file read, code=");
            // Print code as a simple check
            if code.len() < 100 {
                print(&code);
            } else {
                print("<long>");
            }
            print("\n");
        }

        // Execute the script
        debug("qjs: evaluating\n");
        match rt.eval(&code, script_path) {
            Ok(result) => {
                debug("qjs: eval ok, freeing result\n");
                rt.free_value(result);
                debug("qjs: done\n");
                0
            }
            Err(e) => {
                print("Error: ");
                print(&e);
                print("\n");
                1
            }
        }
    }
}
