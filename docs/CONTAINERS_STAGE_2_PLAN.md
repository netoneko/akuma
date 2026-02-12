# Stage 2: Userspace Box Utility

## Goal
Implement the userspace `box` utility (CLI) to manage containers. This tool will act as the primary interface for creating, running, inspecting, and deleting boxes, leveraging the kernel primitives built in Stage 1.

## Tasks

### 1. Create `box` Crate (`userspace/box`)
- Initialize a new binary crate in `userspace/`.
- Add to `userspace/Cargo.toml` workspace members.
- Add to `userspace/build.sh` for build and installation.

### 2. Implement Core Commands
Implement the following subcommands in `box`:

- **`box open <name> [--directory <dir>] [--tmp] <cmd>`**
    - Spawns a new process with `sys_spawn_ext`.
    - Sets `root_dir` and `box_id` (auto-generated or derived from name).
    - Acts as the supervisor/reaper for the container's init process.
    - If `--tmp` is set, cleans up the directory after exit (optional for initial implementation).

- **`box cp <source_dir> <box_name>`**
    - Copies a directory (e.g., a template) to a new box root location (e.g., `/data/boxes/<name>`).
    - Simplifies setting up a new container filesystem.

- **`box ps`**
    - Lists active boxes.
    - Reads from `/proc/boxes` (or filters `/proc` for unique `box_id`s if registry isn't ready).
    - Shows ID, Name (if available), and Status.

- **`box use <name> <cmd>`**
    - "Injects" a command into an existing running box.
    - Finds the target box's ID from the registry or by scanning processes.
    - Calls `sys_spawn_ext` with the target's `box_id` and `root_dir`.

### 3. Box Registry & State Management
- **Kernel-side Registry (Optional but Recommended):**
    - Implement a simple registry in the kernel (`src/process.rs`) to map `box_id` <-> `name`.
    - Expose via `/proc/boxes`.
- **Userspace Fallback:**
    - If kernel registry is deferred, `box` can just manage IDs or use PIDs as IDs.
    - *Decision:* For Stage 2, we will use a simple PID-based or random ID approach to avoid complex kernel state synchronization initially, or implement a minimal kernel registry if time permits. **Let's aim for a minimal kernel registry to support `box ps` properly.**

### 4. Integration with `herd`
- Update `herd` configuration parser to support `boxed = true`.
- When `boxed = true`, `herd` should use `sys_spawn_ext` with a new unique `box_id` for that service.

## Verification Plan

### Userspace Tests
- **Test 1: Box Creation & Isolation**
    - Run `box open testbox --directory /tmp/testbox /bin/sh`.
    - Verify inside the shell that `/` corresponds to `/tmp/testbox`.
- **Test 2: Box Injection**
    - Start a long-running box (e.g., `box open server /bin/httpd`).
    - Run `box use server /bin/ls`.
    - Verify `ls` sees the box's filesystem.
- **Test 3: Process Listing**
    - Run `box ps` and verify it lists the active containers.

## Success Criteria
- [ ] `box` binary builds and runs.
- [ ] `box open` successfully launches a process in a new namespace.
- [ ] `box use` successfully injects a process into an existing namespace.
- [ ] `box ps` shows active containers.
