//! Shell Commands Module
//!
//! Contains all command implementations organized by category.

pub mod builtin;
pub mod exec;
pub mod fs;
pub mod net;
pub mod scripting;

use alloc::vec::Vec;

use super::Command;

// Re-export static command instances
pub use builtin::{AKUMA_CMD, CD_CMD, ECHO_CMD, FREE_CMD, GREP_CMD, HELP_CMD, KTHREADS_CMD, PS_CMD, PWD_CMD, STATS_CMD, UPTIME_CMD};
pub use exec::EXEC_CMD;
pub use fs::{APPEND_CMD, CAT_CMD, DF_CMD, LS_CMD, MKDIR_CMD, MV_CMD, RM_CMD, WRITE_CMD};
pub use net::{CURL_CMD, NSLOOKUP_CMD, PKG_CMD};
pub use scripting::RHAI_CMD;

// ============================================================================
// Command Registry
// ============================================================================

/// Maximum number of commands that can be registered
const MAX_COMMANDS: usize = 32;

/// Registry of available commands
pub struct CommandRegistry {
    commands: Vec<&'static dyn Command>,
}

impl CommandRegistry {
    /// Create a new empty registry
    pub const fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// Register a command
    pub fn register(&mut self, command: &'static dyn Command) {
        if self.commands.len() < MAX_COMMANDS {
            self.commands.push(command);
        }
    }

    /// Find a command by name or alias
    pub fn find(&self, name: &[u8]) -> Option<&'static dyn Command> {
        let name_str = core::str::from_utf8(name).ok()?;
        for cmd in &self.commands {
            if cmd.name() == name_str {
                return Some(*cmd);
            }
            for alias in cmd.aliases() {
                if *alias == name_str {
                    return Some(*cmd);
                }
            }
        }
        None
    }

    /// Get all registered commands
    pub fn commands(&self) -> &[&'static dyn Command] {
        &self.commands
    }
}

/// Create and populate the default command registry
pub fn create_default_registry() -> CommandRegistry {
    let mut registry = CommandRegistry::new();

    // Built-in commands
    registry.register(&ECHO_CMD);
    registry.register(&AKUMA_CMD);
    registry.register(&STATS_CMD);
    registry.register(&FREE_CMD);
    registry.register(&HELP_CMD);
    registry.register(&GREP_CMD);
    registry.register(&PS_CMD);
    registry.register(&KTHREADS_CMD);
    registry.register(&PWD_CMD);
    registry.register(&CD_CMD);
    registry.register(&UPTIME_CMD);

    // Filesystem commands
    registry.register(&LS_CMD);
    registry.register(&CAT_CMD);
    registry.register(&WRITE_CMD);
    registry.register(&APPEND_CMD);
    registry.register(&RM_CMD);
    registry.register(&MV_CMD);
    registry.register(&MKDIR_CMD);
    registry.register(&DF_CMD);

    // Network commands
    registry.register(&CURL_CMD);
    registry.register(&NSLOOKUP_CMD);
    registry.register(&PKG_CMD);

    // Scripting commands
    registry.register(&RHAI_CMD);

    // Process execution commands
    registry.register(&EXEC_CMD);

    registry
}
