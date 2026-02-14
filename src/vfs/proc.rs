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
        let current_box_id = crate::process::current_process().map(|p| p.box_id).unwrap_or(0);

        if path.is_empty() {
            // Root: list all process PIDs as directories, filtered by box_id
            let processes = process::list_processes();
            let mut entries: Vec<DirEntry> = processes
                .into_iter()
                .filter(|p| {
                    // Box 0 (Host) sees everything. Box N only sees its own.
                    if current_box_id == 0 {
                        true
                    } else {
                        if let Some(proc) = process::lookup_process(p.pid) {
                            proc.box_id == current_box_id
                        } else {
                            false
                        }
                    }
                })
                .map(|p| DirEntry {
                    name: alloc::format!("{}", p.pid),
                    is_dir: true,
                    size: 0,
                })
                .collect();

            // Host context only: add "boxes" virtual file
            if current_box_id == 0 {
                entries.push(DirEntry {
                    name: String::from("boxes"),
                    is_dir: false,
                    size: 0, // Dynamic
                });
            }

            // Everyone sees "net" directory
            entries.push(DirEntry {
                name: String::from("net"),
                is_dir: true,
                size: 0,
            });

            return Ok(entries);
        }

        let parts: Vec<&str> = path.split('/').collect();

        if parts.len() == 1 {
            if parts[0] == "net" {
                return Ok(alloc::vec![
                    DirEntry { name: String::from("tcp"), is_dir: false, size: 0 },
                    DirEntry { name: String::from("udp"), is_dir: false, size: 0 },
                ]);
            }

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

    fn read_at(&self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, FsError> {
        let (pid, fd_num) = Self::parse_fd_path(path)?;
        let current_proc = crate::process::current_process();
        let current_box_id = current_proc.as_ref().map(|p| p.box_id).unwrap_or(0);

        let proc = process::lookup_process(pid).ok_or(FsError::NotFound)?;
        
        // BOX ISOLATION: Box N only sees its own processes.
        if current_box_id != 0 && proc.box_id != current_box_id {
            return Err(FsError::NotFound);
        }

        // Lock the appropriate buffer
        match fd_num {
            0 => {
                let stdin = proc.stdin.lock();
                if offset >= stdin.data.len() {
                    return Ok(0);
                }
                let n = buf.len().min(stdin.data.len() - offset);
                buf[..n].copy_from_slice(&stdin.data[offset..offset + n]);
                Ok(n)
            }
            1 => {
                let stdout = proc.stdout.lock();
                if offset >= stdout.data.len() {
                    return Ok(0);
                }
                let n = buf.len().min(stdout.data.len() - offset);
                buf[..n].copy_from_slice(&stdout.data[offset..offset + n]);
                Ok(n)
            }
            _ => Err(FsError::NotFound),
        }
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, FsError> {
        let path = path.trim_start_matches('/');
        let current_proc = crate::process::current_process();
        let current_box_id = current_proc.as_ref().map(|p| p.box_id).unwrap_or(0);

        // Handle /proc/boxes
        if path == "boxes" {
            if current_box_id != 0 {
                return Err(FsError::NotFound);
            }
            
            let boxes = process::list_boxes();
            if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
                crate::safe_print!(128, "[ProcFS] Reading boxes (count={})\n", boxes.len());
            }
            let mut out = String::from("ID,NAME,ROOT,CREATOR,PRIMARY\n");
            for b in boxes {
                out.push_str(&alloc::format!("{},{},{},{},{}\n", b.id, b.name, b.root_dir, b.creator_pid, b.primary_pid));
            }
            return Ok(out.into_bytes());
        }

        if path == "net/tcp" {
            let sockets = crate::socket::list_sockets();
            let mut out = String::from("LOCAL_PORT,REMOTE_ADDR,STATE,BOX\n");
            for s in sockets {
                out.push_str(&alloc::format!("{},{}:{},{},{}\n", 
                    s.local_port, 
                    alloc::format!("{}.{}.{}.{}", s.remote_ip[0], s.remote_ip[1], s.remote_ip[2], s.remote_ip[3]),
                    s.remote_port,
                    s.state,
                    s.box_id));
            }
            return Ok(out.into_bytes());
        }

        if path == "net/udp" {
            return Ok(String::from("LOCAL_PORT,REMOTE_ADDR,STATE,BOX\n").into_bytes());
        }

        let (pid, fd_num) = Self::parse_fd_path(path)?;

        let current_proc = crate::process::current_process();
        let current_box_id = current_proc.as_ref().map(|p| p.box_id).unwrap_or(0);

        let proc = process::lookup_process(pid).ok_or(FsError::NotFound)?;
        
        // BOX ISOLATION: Box N only sees its own processes.
        if current_box_id != 0 && proc.box_id != current_box_id {
            return Err(FsError::NotFound);
        }

        // Lock the appropriate buffer and clone data (thread-safe)
        match fd_num {
            0 => Ok(proc.stdin.lock().clone_data()),
            1 => Ok(proc.stdout.lock().clone_data()),
            _ => Err(FsError::NotFound),
        }
    }

    fn write_file(&self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let (target_pid, fd_num) = Self::parse_fd_path(path)?;
        let caller_proc = crate::process::current_process();
        let caller_pid = process::read_current_pid();
        let caller_box_id = caller_proc.as_ref().map(|p| p.box_id).unwrap_or(0);

        let target = process::lookup_process(target_pid).ok_or(FsError::NotFound)?;

        // BOX ISOLATION: Box N only sees its own processes.
        if caller_box_id != 0 && target.box_id != caller_box_id {
            return Err(FsError::NotFound);
        }

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

                // Use the unified helper to write to both legacy buffer and ProcessChannel
                if process::write_to_process_stdin(target_pid, data).is_err() {
                    return Err(FsError::Internal);
                }
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

    fn write_at(&self, path: &str, _offset: usize, data: &[u8]) -> Result<usize, FsError> {
        // For ProcFS, we ignore the offset and treat it as a direct write/append
        // to the process buffers. This avoids the default read-modify-write behavior.
        self.write_file(path, data)?;
        Ok(data.len())
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

        if path == "boxes" {
            let current_box_id = crate::process::current_process().map(|p| p.box_id).unwrap_or(0);
            return current_box_id == 0;
        }

        if path == "net" || path == "net/tcp" || path == "net/udp" {
            return true;
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

        if path == "boxes" {
            let current_box_id = crate::process::current_process().map(|p| p.box_id).unwrap_or(0);
            if current_box_id != 0 {
                return Err(FsError::NotFound);
            }
            return Ok(Metadata {
                is_dir: false,
                size: 0, // Dynamic
                created: None,
                modified: None,
                accessed: None,
            });
        }

        if path == "net" {
            return Ok(Metadata {
                is_dir: true,
                size: 0,
                created: None,
                modified: None,
                accessed: None,
            });
        }

        if path == "net/tcp" || path == "net/udp" {
            return Ok(Metadata {
                is_dir: false,
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
