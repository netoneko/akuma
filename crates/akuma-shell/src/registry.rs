//! Command registry for looking up shell commands by name or alias.

use alloc::vec::Vec;

use crate::types::Command;

const MAX_COMMANDS: usize = 40;

/// Registry of available commands.
pub struct CommandRegistry {
    commands: Vec<&'static dyn Command>,
}

impl CommandRegistry {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    pub fn register(&mut self, command: &'static dyn Command) {
        if self.commands.len() < MAX_COMMANDS {
            self.commands.push(command);
        }
    }

    #[must_use]
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

    #[must_use]
    pub fn commands(&self) -> &[&'static dyn Command] {
        &self.commands
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}
