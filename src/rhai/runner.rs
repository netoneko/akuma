//! Rhai Script Runner
//!
//! Executes Rhai scripts with output capture and safety limits.
//! Uses on_print to capture script output and on_progress with
//! max_operations to prevent runaway scripts.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;

use rhai::Engine;
use spinning_top::Spinlock;

/// Maximum number of operations before script is terminated
const MAX_OPERATIONS: u64 = 100_000;

/// Interval for progress callback checks
const PROGRESS_INTERVAL: u64 = 100;

/// Create a minimal Rhai engine suitable for bare metal
///
/// Uses Engine::new_raw() to avoid heavy standard library initialization,
/// then registers only the essential packages we need.
fn create_engine() -> Engine {
    use rhai::packages::Package;

    // Start with a raw engine - no standard library
    let mut engine = Engine::new_raw();

    // Register essential packages for bare metal:
    // LanguageCorePackage: print, debug, type_of, etc.
    // ArithmeticPackage: +, -, *, /, % operators
    // LogicPackage: &&, ||, !, comparisons
    // BasicStringPackage: string operations, interpolation
    // BasicArrayPackage: array operations
    let core = rhai::packages::LanguageCorePackage::new();
    let arithmetic = rhai::packages::ArithmeticPackage::new();
    let logic = rhai::packages::LogicPackage::new();
    let basic_string = rhai::packages::BasicStringPackage::new();
    let more_string = rhai::packages::MoreStringPackage::new();
    let basic_array = rhai::packages::BasicArrayPackage::new();

    core.register_into_engine(&mut engine);
    arithmetic.register_into_engine(&mut engine);
    logic.register_into_engine(&mut engine);
    basic_string.register_into_engine(&mut engine);
    more_string.register_into_engine(&mut engine);
    basic_array.register_into_engine(&mut engine);

    engine
}

/// Run a Rhai script and capture its output
///
/// # Arguments
/// * `code` - The Rhai script source code
///
/// # Returns
/// * `Ok(String)` - The captured output from print statements
/// * `Err(String)` - Error message if script execution fails
pub fn run_script(code: &str) -> Result<String, String> {
    // Use Arc<Spinlock> for thread-safe interior mutability (required by sync feature)
    let output = Arc::new(Spinlock::new(String::new()));
    let output_print = Arc::clone(&output);
    let output_debug = Arc::clone(&output);

    let mut engine = create_engine();

    // Custom print function - captures to output buffer
    engine.on_print(move |text| {
        let mut out = output_print.lock();
        out.push_str(text);
        out.push_str("\r\n");
    });

    // Custom debug function - also captures to output buffer
    engine.on_debug(move |text, source, pos| {
        let mut out = output_debug.lock();
        if let Some(src) = source {
            out.push_str(&format!("[{}] ", src));
        }
        if !pos.is_none() {
            out.push_str(&format!("({}) ", pos));
        }
        out.push_str(text);
        out.push_str("\r\n");
    });

    // Progress callback for cooperative behavior and safety limits
    // Returns None to continue, Some(value) to abort with that value
    engine.on_progress(|ops| {
        if ops % PROGRESS_INTERVAL == 0 {
            // Future: could yield to async executor here
            // For now, just continue
        }
        None // Continue execution
    });

    // Safety limit - prevent infinite loops
    engine.set_max_operations(MAX_OPERATIONS);

    // Run the script
    match engine.run(code) {
        Ok(()) => {
            // Extract the output - Arc::try_unwrap works if we're the only owner
            let out = Arc::try_unwrap(output)
                .map(|spinlock| spinlock.into_inner())
                .unwrap_or_else(|arc| arc.lock().clone());
            Ok(out)
        }
        Err(e) => Err(format!("Script error: {}\r\n", e)),
    }
}

/// Run a Rhai script and return the evaluated result as a string
///
/// # Arguments
/// * `code` - The Rhai script source code (should be an expression)
///
/// # Returns
/// * `Ok(String)` - The result of the expression plus any print output
/// * `Err(String)` - Error message if evaluation fails
pub fn eval_script(code: &str) -> Result<String, String> {
    let output = Arc::new(Spinlock::new(String::new()));
    let output_print = Arc::clone(&output);

    let mut engine = create_engine();

    // Custom print function
    engine.on_print(move |text| {
        let mut out = output_print.lock();
        out.push_str(text);
        out.push_str("\r\n");
    });

    // Safety limits
    engine.set_max_operations(MAX_OPERATIONS);

    // Evaluate and get result
    match engine.eval::<rhai::Dynamic>(code) {
        Ok(result) => {
            let mut out = Arc::try_unwrap(output)
                .map(|spinlock| spinlock.into_inner())
                .unwrap_or_else(|arc| arc.lock().clone());
            // Append the result if it's not unit
            if !result.is_unit() {
                out.push_str(&format!("{}\r\n", result));
            }
            Ok(out)
        }
        Err(e) => Err(format!("Eval error: {}\r\n", e)),
    }
}
