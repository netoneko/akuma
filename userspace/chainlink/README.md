# chainlink - Issue Tracker CLI for Akuma

Forked from [dollspace-gay/chainlink](https://github.com/dollspace-gay/chainlink). Thank you, Doll!

A `no_std` userspace application that wraps the [chainlink](https://github.com/netoneko/chainlink) issue tracker library, providing a local-first issue tracking system for Akuma.

## Overview

This application brings the chainlink issue tracker to Akuma's bare-metal ARM64 environment. It uses the `sqld` library's SQLite VFS implementation to persist issues to the ext2 filesystem.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    chainlink CLI                        │
│              (manual argument parsing)                  │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                  chainlink library                      │
│        (Database<B: DatabaseBackend> + models)          │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                    SqldBackend                          │
│     (implements DatabaseBackend using sqld::vfs)        │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                     sqld::vfs                           │
│        (SQLite VFS using libakuma syscalls)             │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                      libakuma                           │
│            (syscalls: open, read, write, ...)           │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│                    Akuma Kernel                         │
│                  (ext2 filesystem)                      │
└─────────────────────────────────────────────────────────┘
```

## Usage

```bash
# Initialize the database (creates .chainlink/issues.db)
chainlink init

# Create a new issue
chainlink create "Fix memory leak in allocator"
chainlink create "Add network timeout" -d "HTTP requests hang forever" -p high

# List issues
chainlink list              # List open issues (default)
chainlink list -s all       # List all issues
chainlink list -s closed    # List closed issues

# Show issue details
chainlink show 1

# Close an issue
chainlink close 1

# Reopen an issue
chainlink reopen 1

# Add a comment
chainlink comment 1 "Fixed in commit abc123"

# Add a label
chainlink label 1 "bug"

# Show help
chainlink help
```

## Supported Commands

| Command | Description |
|---------|-------------|
| `init` | Initialize the chainlink database |
| `create <title> [-d desc] [-p priority]` | Create a new issue |
| `list [-s status]` | List issues (status: open/closed/all) |
| `show <id>` | Show issue details with comments and labels |
| `close <id>` | Close an issue |
| `reopen <id>` | Reopen a closed issue |
| `comment <id> <text>` | Add a comment to an issue |
| `label <id> <label>` | Add a label to an issue |
| `help` | Show usage information |

## Database Location

The database is stored at `.chainlink/issues.db` in the current directory. The SQLite database uses the same schema as the standard chainlink CLI.

## Limitations

Due to the `no_std` environment, some features from the full chainlink CLI are not available:

- **Daemon mode** - Requires process management not available in Akuma
- **Export/Import** - Requires file system iteration
- **Session timestamps** - Uses monotonic counter instead of real-time clock
- **Subcommands** (archive, milestone, session, etc.) - Not yet implemented

## Building

```bash
cd userspace
cargo build --release --bin chainlink
```

The binary will be at `target/aarch64-unknown-none/release/chainlink`.

To include in the Akuma disk image:

```bash
cd userspace
./build.sh
```

## Implementation Notes

### SqldBackend

The `SqldBackend` struct implements chainlink's `DatabaseBackend` trait. Key implementation details:

1. **Parameter Binding**: Since sqld's `execute_sql` doesn't support parameterized queries, parameters are manually substituted into the SQL string with proper escaping.

2. **NULL Handling**: sqld returns "NULL" as a string for NULL values. The backend converts these to `None` for chainlink's `Option<String>` fields.

3. **Row ID Tracking**: Uses SQLite's `sqlite3_last_insert_rowid()` FFI binding to track the last inserted row ID.

### Timestamp Handling

In `no_std` mode, `Utc::now()` is not available. The chainlink library was modified to use a monotonic counter that provides unique, ordered timestamps starting from 2024-01-01.

## See Also

- [DEPENDENCIES.md](docs/DEPENDENCIES.md) - Details on dependency modifications
- [chainlink library](https://github.com/netoneko/chainlink) - The upstream chainlink project
- [sqld](../sqld/README.md) - The SQLite daemon that provides the VFS
