//! Process Filesystem (procfs)
//!
//! A virtual filesystem that exposes process stdin/stdout as files.
//! Mounted at /proc, provides:
//! - /proc/<pid>/fd/0 - stdin (readable by all, writable by spawner/kernel)
//! - /proc/<pid>/fd/1 - stdout (readable by all, writable by owning process)

use alloc::string::String;
use alloc::vec::Vec;

use super::{DirEntry, Filesystem, FsError, FsStats, Metadata};
use crate::config::{PROC_STDIN_MAX_SIZE, PROC_STDOUT_MAX_SIZE};
use crate::process::{self, Pid};

// ============================================================================
// ProcFilesystem
// ============================================================================

/// Virtual filesystem for process information
pub struct ProcFilesystem;

impl ProcFilesystem {
    /// Create a new procfs instance
    pub fn new() -> Self {
        Self
    }

    /// Parse a path like "/<pid>/fd/<n>" into (pid, fd_num)
    fn parse_fd_path(path: &str) -> Result<(Pid, u32), FsError> {
        let path = path.trim_start_matches('/');
        let parts: Vec<&str> = path.split('/').collect();

        // Expected: ["<pid>", "fd", "<n>"]
        if parts.len() != 3 || parts[1] != "fd" {
            return Err(FsError::NotFound);
        }

        let pid: Pid = parts[0].parse().map_err(|_| FsError::NotFound)?;
        let fd_num: u32 = parts[2].parse().map_err(|_| FsError::NotFound)?;

        Ok((pid, fd_num))
    }

    /// Parse path to get just the PID (for /<pid> or /<pid>/fd)
    fn parse_pid_path(path: &str) -> Result<Pid, FsError> {
        let path = path.trim_start_matches('/');
        let parts: Vec<&str> = path.split('/').collect();

        if parts.is_empty() || parts[0].is_empty() {
            return Err(FsError::NotFound);
        }

        parts[0].parse().map_err(|_| FsError::NotFound)
    }

    /// Check if a process exists
    fn process_exists(pid: Pid) -> bool {
        process::lookup_process(pid).is_some()
    }
}

impl Default for ProcFilesystem {
    fn default() -> Self {
        Self::new()
    }
}

impl Filesystem for ProcFilesystem {
    fn name(&self) -> &str {
        "proc"
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        let path = path.trim_matches('/');

        if path.is_empty() {
            // Root: list all process PIDs as directories
            let processes = process::list_processes();
            let entries = processes
                .into_iter()
                .map(|p| DirEntry {
                    name: alloc::format!("{}", p.pid),
                    is_dir: true,
                    size: 0,
                })
                .collect();
            return Ok(entries);
        }

        let parts: Vec<&str> = path.split('/').collect();

        if parts.len() == 1 {
            // /<pid> - list "fd" directory
            let pid: Pid = parts[0].parse().map_err(|_| FsError::NotFound)?;
            if !Self::process_exists(pid) {
                return Err(FsError::NotFound);
            }
            return Ok(alloc::vec![DirEntry {
                name: String::from("fd"),
                is_dir: true,
                size: 0,
            }]);
        }

        if parts.len() == 2 && parts[1] == "fd" {
            // /<pid>/fd - list stdin (0) and stdout (1)
            let pid: Pid = parts[0].parse().map_err(|_| FsError::NotFound)?;
            if !Self::process_exists(pid) {
                return Err(FsError::NotFound);
            }

            let proc = process::lookup_process(pid).ok_or(FsError::NotFound)?;
            // Lock buffers to get sizes (thread-safe)
            let stdin_len = proc.stdin.lock().len() as u64;
            let stdout_len = proc.stdout.lock().len() as u64;
            return Ok(alloc::vec![
                DirEntry {
                    name: String::from("0"),
                    is_dir: false,
                    size: stdin_len,
                },
                DirEntry {
                    name: String::from("1"),
                    is_dir: false,
                    size: stdout_len,
                },
            ]);
        }

        Err(FsError::NotFound)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        let (pid, fd_num) = Self::parse_fd_path(path)?;

        let proc = process::lookup_process(pid).ok_or(FsError::NotFound)?;

        // Lock the appropriate buffer and clone data (thread-safe)
        match fd_num {
            0 => Ok(proc.stdin.lock().clone_data()),
            1 => Ok(proc.stdout.lock().clone_data()),
            _ => Err(FsError::NotFound),
        }
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let (target_pid, fd_num) = Self::parse_fd_path(path)?;
        let caller_pid = process::read_current_pid();

        let target = process::lookup_process(target_pid).ok_or(FsError::NotFound)?;

        match fd_num {
            0 => {
                // stdin: allow if caller is spawner, or if kernel-spawned (spawner_pid == None)
                if let Some(spawner) = target.spawner_pid {
                    // Process was spawned by another process
                    if caller_pid != Some(spawner) {
                        return Err(FsError::PermissionDenied);
                    }
                }
                // If spawner_pid is None (kernel spawned), allow any caller
                // If caller_pid is None (kernel context), also allow

                // Write with size limit (thread-safe via Spinlock)
                target.stdin.lock().write_with_limit(data, PROC_STDIN_MAX_SIZE);
                Ok(())
            }
            1 => {
                // stdout: only owning process can write
                if caller_pid != Some(target_pid) {
                    return Err(FsError::PermissionDenied);
                }

                // Write with size limit (thread-safe via Spinlock)
                target.stdout.lock().write_with_limit(data, PROC_STDOUT_MAX_SIZE);
                Ok(())
            }
            _ => Err(FsError::NotFound),
        }
    }

    fn append_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        // Append is the same as write for procfs (we always append to buffers)
        self.write_file(path, data)
    }

    fn create_dir(&self, _path: &str) -> Result<(), FsError> {
        // Cannot create directories in procfs
        Err(FsError::NotSupported)
    }

    fn remove_file(&self, _path: &str) -> Result<(), FsError> {
        // Cannot remove files in procfs
        Err(FsError::NotSupported)
    }

    fn remove_dir(&self, _path: &str) -> Result<(), FsError> {
        // Cannot remove directories in procfs
        Err(FsError::NotSupported)
    }

    fn exists(&self, path: &str) -> bool {
        let path = path.trim_matches('/');

        if path.is_empty() {
            return true; // Root always exists
        }

        // Try to parse as fd path first
        if let Ok((pid, fd_num)) = Self::parse_fd_path(path) {
            return Self::process_exists(pid) && fd_num <= 1;
        }

        // Try to parse as pid path
        if let Ok(pid) = Self::parse_pid_path(path) {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() == 1 {
                return Self::process_exists(pid);
            }
            if parts.len() == 2 && parts[1] == "fd" {
                return Self::process_exists(pid);
            }
        }

        false
    }

    fn metadata(&self, path: &str) -> Result<Metadata, FsError> {
        let path = path.trim_matches('/');

        if path.is_empty() {
            // Root directory
            return Ok(Metadata {
                is_dir: true,
                size: 0,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        // Try fd path first
        if let Ok((pid, fd_num)) = Self::parse_fd_path(path) {
            let proc = process::lookup_process(pid).ok_or(FsError::NotFound)?;
            let size = match fd_num {
                0 => proc.stdin.lock().len() as u64,
                1 => proc.stdout.lock().len() as u64,
                _ => return Err(FsError::NotFound),
            };
            return Ok(Metadata {
                is_dir: false,
                size,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        // Try pid or pid/fd path
        if let Ok(pid) = Self::parse_pid_path(path) {
            if !Self::process_exists(pid) {
                return Err(FsError::NotFound);
            }
            return Ok(Metadata {
                is_dir: true,
                size: 0,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        Err(FsError::NotFound)
    }

    fn stats(&self) -> Result<FsStats, FsError> {
        // Procfs doesn't use block storage
        Ok(FsStats {
            block_size: 4096,
            total_blocks: 0,
            free_blocks: 0,
        })
    }
}
