#![no_std]

extern crate alloc;

mod context;
pub mod parse;
mod registry;
mod types;
mod util;

pub mod exec;

pub use context::ShellContext;
pub use parse::{
    expand_variables, parse_command_chain, parse_command_line, parse_pipeline, ChainOperator,
    ChainedCommand, ParsedCommandLine, RedirectMode,
};
pub use registry::CommandRegistry;
pub use types::{
    ChainExecutionResult, Command, InteractiveRead, ShellError, StreamableCommand, VecWriter,
};
pub use util::{split_first_word, translate_input_keys, trim_bytes};

#[cfg(test)]
mod tests;
