# Stage 1: Kernel Isolation Primitives

## Goal
Implement the core kernel primitives required for process and filesystem isolation (Boxing). This stage focuses on the "Box 0" (Host) architecture and the infrastructure to support isolated "Box N" containers.

### Blind Isolation Principle
Userspace processes **MUST NOT** be aware of their host-side `root_dir`. To a boxed process, the filesystem appears to start at `/`. All paths provided to and returned from syscalls (like `getcwd` or `read_dir`) must be relative to the virtual root.

## Tasks

### 1. Process Metadata Update (`src/process.rs`)
- Add `box_id: u64` and `root_dir: String` to the `Process` struct.
- Initialize `Process` with `box_id: 0` and `root_dir: "/"` by default (Box 0 / Host).
- **Inheritance Logic:** Update `spawn_process_with_channel_ext` and `sys_spawn` to default to the caller's `box_id` and `root_dir` if not explicitly overridden.
- **Safety:** Prevent `sys_kill_box` from accepting `box_id: 0`.

### 2. VFS Scoping (`src/vfs/mod.rs`)
- Update `vfs::resolve` (and `with_fs`) to respect the current process's `root_dir`.
- **Logic:**
    - If `process.root_dir` is `/`, resolve normally.
    - If `process.root_dir` is `/box1`, an absolute path `/etc/config` must be internally resolved as `/box1/etc/config`.
- **Safety:** Implement "jailbreak" prevention in `normalize_path` or `resolve` to ensure `..` components cannot ascend above the `root_dir`.

### 3. ProcFS Virtualization (`src/vfs/proc.rs`)
- Modify `ProcFilesystem::read_dir` to filter process entries based on `box_id`.
- **Box 0 (Host):** Can see all processes and the upcoming `/proc/boxes` registry.
- **Box N (Isolated):** Can only see processes with the same `box_id`.
- Ensure `/proc/boxes` is hidden from non-zero boxes.

### 4. Basic `sys_spawn_ext` Syscall (`src/syscall.rs`)
- Implement a minimal version of `sys_spawn_ext` (or modify `sys_spawn`) to support:
    - `root_dir`: The virtual root for the new process.
    - `box_id`: The namespace ID for the new process.
- This allows the `box` utility (Stage 2) to actually create isolated processes.

## Verification Plan

### Kernel Unit Tests
- Create `src/container_tests.rs` (or add to `process_tests.rs`).
- **Test Case 1: VFS Scoping**
    - Create a process with `root_dir = "/tmp/test_box"`.
    - Verify that `vfs::exists("/etc/config")` inside that process checks `/tmp/test_box/etc/config`.
- **Test Case 2: ProcFS Isolation**
    - Create two processes in different boxes.
    - Verify that process A cannot see process B in `/proc`.
- **Test Case 3: Box 0 Visibility**
    - Verify that Box 0 can see both process A and B.

## Success Criteria
- [ ] `Process` struct holds isolation state.
- [ ] VFS resolution is correctly scoped based on `process.root_dir`.
- [ ] `ProcFS` correctly filters process listings.
- [ ] Kernel tests pass for multi-box scenarios.
- [ ] **Acceptance Test: Herd Host Context**
    - Verify that the `herd` supervisor process runs with `box_id: 0` when auto-started by the kernel.
- [ ] **Acceptance Test: Blind Root Redirection**
    - Create a file `/tmp/box.txt` with unique content (e.g., "Akuma Container Test 123").
    - Spawn a process with `root_dir: "/tmp"` and `cmd: "cat /box.txt"`.
    - Verify that the process successfully reads the file (internal `/box.txt` -> host `/tmp/box.txt`).
    - Verify the process stdout matches the unique content.
