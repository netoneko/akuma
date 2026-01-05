//! Rhai Scripting Engine Module
//!
//! Provides a Rhai scripting engine wrapper for executing scripts
//! in the akuma bare-metal environment. Handles output capture
//! and execution safety limits.

pub mod runner;

pub use runner::run_script;
