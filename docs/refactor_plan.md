# Refactor Plan: Replacing console::print with safe_print!

## Goal

Replace all instances of `console::print` with the `safe_print!` macro to improve safety and prevent potential heap allocations.

## Background

`console::print` is a simple printing function that directly writes to the UART. `safe_print!` is a macro that uses a stack-allocated buffer to format the output and then prints it, avoiding heap allocations and potential panics.

## Steps

1.  **Identify all uses of `console::print`:** Use `grep -rn "console::print" .` to find all files and line numbers containing `console::print`.
2.  **Refactor each instance:**
    *   Examine the arguments passed to `console::print`.
    *   Rewrite the call to use `safe_print!` with an appropriate buffer size (`safe_print!(<size>, ...)`).  The buffer size should be large enough to hold the formatted output.  Start with a reasonable size (e.g., 64 or 128) and increase if necessary.
    *   Test the changes after each refactoring to ensure that the output is correct and there are no regressions.
3.  **Compile and test:** After each refactoring step, run `cargo build` and any relevant tests to verify that the changes are working correctly.
4.  **Repeat steps 2 and 3** until all instances of `console::print` have been replaced.

## Considerations

*   **Buffer Size:** Choosing the right buffer size for `safe_print!` is important. Too small, and the output will be truncated. Too large, and we waste stack space.
*   **Formatting:**  Ensure that the formatting used with `safe_print!` is equivalent to the original `console::print` calls.
*   **Testing:** Thorough testing is crucial to ensure that the refactoring does not introduce any regressions.