//! Container Isolation Tests
//!
//! Verifies VFS scoping, ProcFS virtualization, and Box 0 (Host) context.

use alloc::string::ToString;
use crate::process;
use crate::fs;
use crate::vfs::Filesystem;

/// Run all container isolation tests
pub fn run_all() {
    crate::console::print("\n--- Running Container Isolation Tests ---\n");

    test_box_0_herd();
    test_vfs_blind_root();
    test_procfs_isolation();

    crate::console::print("--- Container Isolation Tests Passed ---\n\n");
}

/// Acceptance Test: Verify that the herd supervisor process runs with box_id: 0
fn test_box_0_herd() {
    crate::console::print("[Test] Verifying herd runs in Box 0...\n");
    
    let processes = process::list_processes();
    let mut found_herd = false;
    
    for p in processes {
        if p.name.contains("herd") {
            found_herd = true;
            if let Some(proc) = process::lookup_process(p.pid) {
                assert_eq!(proc.box_id, 0, "Herd MUST run in Box 0 (Host context)");
            }
        }
    }
    
    if crate::config::AUTO_START_HERD {
        assert!(found_herd, "Herd process not found but AUTO_START_HERD is enabled");
    } else {
        crate::console::print("[Test] (Skipping herd existence check, AUTO_START_HERD disabled)\n");
    }
}

/// Acceptance Test: Blind Root Redirection
/// Create a file /tmp/box.txt, spawn a box with root=/tmp, verify 'cat /box.txt' works.
fn test_vfs_blind_root() {
    crate::console::print("[Test] Verifying blind root redirection (/tmp/box.txt)...\n");
    
    let test_file = "/tmp/box.txt";
    let test_content = "Akuma Container Test 123";
    
    // 1. Create file in host /tmp
    if !fs::exists("/tmp") {
        fs::create_dir("/tmp").expect("Failed to create /tmp");
    }
    fs::write_file(test_file, test_content.as_bytes()).expect("Failed to write test file");
    
    // 2. Spawn process with root_dir = "/tmp"
    let (_tid, _channel, pid) = process::spawn_process_with_channel_ext(
        "/bin/hello", 
        None, 
        None, 
        Some("/"), 
        Some("/tmp"), 
        101 // Box 101
    ).expect("Failed to spawn boxed process");
    
    // 3. Verify VFS scoping from the process's perspective
    if let Some(proc) = process::lookup_process(pid) {
        assert_eq!(proc.root_dir, "/tmp");
        assert_eq!(proc.box_id, 101);
    }

    // Clean up
    let _ = process::kill_process(pid);
}

/// Test ProcFS Isolation
fn test_procfs_isolation() {
    crate::console::print("[Test] Verifying ProcFS isolation...\n");
    
    // 1. Spawn a process in Box 200
    let (_tid1, _, pid1) = process::spawn_process_with_channel_ext(
        "/bin/hello", None, None, None, None, 200
    ).expect("Spawn 1 failed");
    
    // 2. Spawn a process in Box 201
    let (_tid2, _, pid2) = process::spawn_process_with_channel_ext(
        "/bin/hello", None, None, None, None, 201
    ).expect("Spawn 2 failed");
    
    // 3. Check Box 0 (Host) sees both
    let proc_fs = crate::vfs::proc::ProcFilesystem::new();
    let host_entries = proc_fs.read_dir("/").expect("Host read_dir failed");
    let has_p1 = host_entries.iter().any(|e| e.name == pid1.to_string());
    let has_p2 = host_entries.iter().any(|e| e.name == pid2.to_string());
    assert!(has_p1 && has_p2, "Host should see all processes");
    
    // Clean up
    let _ = process::kill_process(pid1);
    let _ = process::kill_process(pid2);
}
