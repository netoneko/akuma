//! Shell Commands Module
//!
//! Contains all command implementations organized by category.

pub mod builtin;
pub mod exec;
pub mod fs;
pub mod net;

use super::CommandRegistry;

// Re-export static command instances
pub use builtin::{
    AKUMA_CMD, CD_CMD, CLEAR_CMD, ECHO_CMD, ENV_CMD, EXPORT_CMD, FREE_CMD, GREP_CMD, HELP_CMD,
    KILL_CMD, KTHREADS_CMD, PMM_CMD, PS_CMD, PWD_CMD, RESET_CMD, SET_CMD, STATS_CMD, UNSET_CMD,
    UPTIME_CMD,
};
pub use exec::EXEC_CMD;
pub use fs::{
    APPEND_CMD, CAT_CMD, CP_CMD, DF_CMD, FIND_CMD, LS_CMD, MKDIR_CMD, MOUNT_CMD, MV_CMD, RM_CMD,
    WRITE_CMD,
};
pub use net::{CURL_CMD, NSLOOKUP_CMD, PKG_CMD};

/// Create and populate the default command registry.
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
    registry.register(&KILL_CMD);
    registry.register(&KTHREADS_CMD);
    registry.register(&PWD_CMD);
    registry.register(&CD_CMD);
    registry.register(&UPTIME_CMD);
    registry.register(&PMM_CMD);
    registry.register(&CLEAR_CMD);
    registry.register(&RESET_CMD);
    registry.register(&EXPORT_CMD);
    registry.register(&SET_CMD);
    registry.register(&UNSET_CMD);
    registry.register(&ENV_CMD);

    // Filesystem commands
    registry.register(&LS_CMD);
    registry.register(&FIND_CMD);
    registry.register(&CAT_CMD);
    registry.register(&WRITE_CMD);
    registry.register(&APPEND_CMD);
    registry.register(&RM_CMD);
    registry.register(&MV_CMD);
    registry.register(&CP_CMD);
    registry.register(&MKDIR_CMD);
    registry.register(&DF_CMD);
    registry.register(&MOUNT_CMD);

    // Network commands
    registry.register(&CURL_CMD);
    registry.register(&NSLOOKUP_CMD);
    registry.register(&PKG_CMD);

    // Process execution commands
    registry.register(&EXEC_CMD);

    registry
}
