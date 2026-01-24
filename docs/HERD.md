# Herd Process Supervisor

Herd is a userspace process supervisor for managing background services. Named "herd" because herding cats is an apt metaphor for managing processes.

## Architecture

Herd runs as a userspace binary at `/bin/herd`. The kernel can automatically start it after the network stack is initialized (controlled by `config::AUTO_START_HERD`, disabled by default due to scheduling issues).

To enable automatic startup, set `AUTO_START_HERD = true` in `src/config.rs`.

Alternatively, run herd manually from the shell:
```bash
/bin/herd &
```

If `/bin/herd` is not found when auto-start is enabled, the kernel logs a warning.

## Features

- Automatic service startup on boot
- Stdout/stderr capture to log files
- Log rotation at 32KB (rotates to `.old`)
- Automatic restart on non-zero exit
- Configurable restart delay and max retries
- Periodic config reload every 20 seconds

## Directory Structure

Herd automatically creates these directories at startup if they don't exist:

```
/etc/herd/
├── available/           # All service definitions
│   ├── httpd.conf
│   └── myservice.conf
└── enabled/             # Services to auto-start (copy from available/)
    └── httpd.conf

/var/log/herd/
├── httpd.log            # Current log
└── httpd.log.old        # Rotated log
```

## Configuration Format

Service configuration uses a simple `key=value` format:

```ini
# /etc/herd/available/httpd.conf

# Required: path to the executable
command=/bin/httpd

# Optional: space-separated arguments
args=--port 8080 --verbose

# Optional: delay before restart in milliseconds (default: 1000)
restart_delay=1000

# Optional: max restart attempts, 0 = unlimited (default: 0)
max_retries=5
```

## Commands

The `herd` binary supports both daemon mode and command mode:

```bash
# Run as supervisor daemon (default when no args)
herd
herd daemon

# List enabled services
herd status

# Show service configuration
herd config httpd

# Enable a service (copies from available/ to enabled/)
herd enable httpd

# Disable a service (removes from enabled/)
herd disable httpd

# View service logs
herd log httpd

# Show help
herd help
```

The kernel shell also has a built-in `herd` command with the same interface.

To restart a service, use `kill <pid>` - herd will automatically restart services that exit with a non-zero exit code.

## Syscalls (for userspace herd)

The following syscalls were added to support userspace process supervision:

| Syscall | Number | Description |
|---------|--------|-------------|
| `SPAWN` | 301 | Spawn child process, returns PID and stdout FD |
| `KILL` | 302 | Kill process by PID |
| `WAITPID` | 303 | Check if child exited, get exit status |
| `GETDENTS64` | 61 | List directory entries |

### SPAWN Syscall

```c
// Arguments:
//   x0: path pointer
//   x1: path length
//   x2: args pointer (null-separated strings, or 0)
//   x3: args length
// Returns:
//   On success: (stdout_fd << 32) | child_pid
//   On error: negative errno
```

### WAITPID Syscall

```c
// Arguments:
//   x0: child PID
//   x1: pointer to status (or 0)
// Returns:
//   If exited: child PID
//   If running: 0
//   On error: negative errno
```

## libakuma API

The userspace library provides high-level wrappers:

```rust
use libakuma::{spawn, kill, waitpid, read_dir, SpawnResult};

// Spawn a process
if let Some(SpawnResult { pid, stdout_fd }) = spawn("/bin/httpd", Some(&["--port", "8080"])) {
    // Read child stdout
    let mut buf = [0u8; 1024];
    let n = read_fd(stdout_fd as i32, &mut buf);
    
    // Check if exited
    if let Some((_, exit_code)) = waitpid(pid) {
        println!("Child exited with code {}", exit_code);
    }
    
    // Kill if needed
    kill(pid);
}

// List directory
for entry in read_dir("/etc/herd/enabled").unwrap() {
    println!("{} (dir={})", entry.name, entry.is_dir);
}
```

## Restart Policy

When a service exits with a non-zero exit code:

1. Herd checks if `restart_count < max_retries` (or `max_retries == 0` for unlimited)
2. Schedules restart after `restart_delay` milliseconds
3. Increments restart counter
4. If max retries exceeded, marks service as `Failed`

When a service exits with code 0:
- Service is marked as `Stopped`
- Restart counter is reset to 0
- No automatic restart

## Log Rotation

When a log file exceeds 32KB:

1. Current log content is copied to `<service>.log.old`
2. New data overwrites the main log file
3. Only one `.old` file is kept (no multi-level rotation)

## Auto-start Sequence

On kernel boot:

1. Kernel initializes filesystem
2. Kernel initializes network
3. If `ENABLE_KERNEL_HERD = false` and `/bin/herd` exists:
   - Kernel spawns `/bin/herd` as a userspace process
4. Herd reads `/etc/herd/enabled/`
5. Herd spawns all configured services
6. Herd enters supervisor loop:
   - Poll stdout from children (100ms intervals)
   - Check for exited processes
   - Handle pending restarts
   - Reload config every 20 seconds

## Implementation Files

### Kernel
- `src/config.rs` - `ENABLE_KERNEL_HERD` flag
- `src/syscall.rs` - SPAWN, KILL, WAITPID, GETDENTS64 syscalls
- `src/process.rs` - `ChildStdout` FD type, child channel registry
- `src/herd.rs` - Kernel-integrated supervisor (fallback)
- `src/shell/commands/builtin.rs` - `herd` shell command
- `src/main.rs` - Auto-start of userspace herd

### Userspace
- `userspace/libakuma/src/lib.rs` - spawn, kill, waitpid, read_dir APIs
- `userspace/herd/src/main.rs` - Userspace supervisor binary

## Example: Adding a Service

1. Create config file:
   ```bash
   cat > /etc/herd/available/myservice.conf << EOF
   command=/bin/myservice
   args=--daemon
   restart_delay=2000
   max_retries=3
   EOF
   ```

2. Enable the service:
   ```bash
   herd enable myservice
   ```

3. Service will start on next config reload (within 20 seconds) or on reboot.

4. Check status:
   ```bash
   herd status
   herd log myservice
   ```

5. Disable when done:
   ```bash
   herd disable myservice
   ```
