# VFS Concurrency and Locking Analysis

**Date:** February 9, 2026  
**Issue:** Global bottleneck in the Virtual Filesystem (VFS) layer due to serialized access via `MOUNT_TABLE`.

## 1. Current Architecture

The Akuma VFS uses a single global `Spinlock` to protect the `MOUNT_TABLE`, which is a `Vec` of `MountEntry` structures.

### The `with_fs` Bottleneck
Almost every filesystem operation (read, write, list, metadata) goes through the `with_fs` helper function. Currently, this function:
1. Disables preemption.
2. Acquires the **global** `MOUNT_TABLE` lock.
3. Resolves the path to a specific filesystem (e.g., `ext2` on `/`).
4. **Executes the entire I/O operation** (e.g., a 10MB read from disk) while still holding the global lock.
5. Releases the global lock.
6. Enables preemption.

### Result
- **Zero Parallelism**: Even if you have multiple disks or multiple independent filesystems (like `memfs` and `ext2`), only one thread can be in the VFS layer at a time.
- **Starvation**: A large file read from `ext2` blocks an SSH session from reading its configuration or a process from writing to its own `procfs` stdout.

## 2. Why do we need a lock at all?

The user correctly asks: *Why would we need a lock for it unless we were trying to modify it?*

In Rust and kernel development, a lock is required for `MOUNT_TABLE` for three main reasons:

1.  **Memory Safety (Data Races)**: `MOUNT_TABLE` is a `Vec`. In Rust, `Vec` is not `Sync`. If one thread is mounting a new filesystem (pushing to the `Vec`) while another is resolving a path (iterating over the `Vec`), it causes a data race, which is undefined behavior and can lead to kernel crashes.
2.  **Lifetime and Unmounting**: If we resolve a path to a reference (`&dyn Filesystem`) and then drop the lock, another thread could call `unmount()`. This would drop the `Box<dyn Filesystem>`, making our reference a "dangling pointer." Accessing it would result in a Use-After-Free.
3.  **Memory Consistency**: Even for read-only access, on modern CPUs, we need synchronization primitives (or atomic operations) to ensure that changes made by one CPU (e.g., a mount) are visible to others.

## 3. Plan: Removing the Global I/O Lock

To allow concurrent I/O while maintaining safety, we need to move from a **"Lock-and-Hold"** strategy to a **"Resolve-and-Refcount"** strategy.

### Step 1: Reference Counting
Change `MountEntry` to store an `Arc<dyn Filesystem>` instead of a `Box<dyn Filesystem>`.
*   `Arc` (Atomic Reference Counting) allows multiple threads to safely own a piece of data.
*   The `MOUNT_TABLE` will hold one reference.
*   The active I/O operation will hold another temporary reference.

### Step 2: Refactor `with_fs`
The new `with_fs` logic will be:
1.  Briefly lock `MOUNT_TABLE`.
2.  Find the matching `MountEntry`.
3.  **Clone the `Arc<dyn Filesystem>`** (increments the refcount).
4.  **Drop the `MOUNT_TABLE` lock immediately.**
5.  Call the filesystem operation on the cloned `Arc`.
6.  When the operation finishes, the `Arc` is dropped (decrements the refcount).

### Step 3: Per-Filesystem Locking
Ensure each filesystem implementation (Ext2, MemFS, ProcFS) continues to use its own internal `Spinlock` for its private state. Since these are separate locks, a read from `/tmp` (memfs) will no longer block a read from `/` (ext2).

## 4. Implementation Steps

1.  **Introduce `Arc`**: Update `src/vfs/mod.rs` to use `alloc::sync::Arc`.
2.  **Modify `with_fs`**: Implement the Resolve-and-Clone pattern.
3.  **Audit Unmount**: Ensure `unmount` properly removes the entry from the table (the `Arc` will ensure the filesystem memory isn't actually freed until the last active I/O finishes).
4.  **Verification**: 
    *   Run `scratch clone` (heavy Ext2 write).
    *   Simultaneously run `ls /proc` or `cat /tmp/test` (ProcFS/MemFS).
    *   Verify they no longer block each other.
