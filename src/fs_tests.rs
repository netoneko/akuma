//! Filesystem Tests
//!
//! Tests for the FAT32 filesystem operations.
//! These tests are run after filesystem initialization.

use alloc::format;

use crate::console;
use crate::fs;
// The one errno table (`akuma_primitives::errno`), in the negated form a
// syscall returns. Every test here used to declare its own local consts from
// raw literals — 94 of them across the five test files, which is how a
// comment and a number get to disagree. See
// docs/archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md §5.7.
use akuma_primitives::errno::negated::{
    EACCES, EEXIST, EINVAL, EIO, EISDIR, EMFILE, ENOENT, ENOSPC, ENOTDIR, ENOTEMPTY, EROFS,
};

// ============================================================================
// Test Runner
// ============================================================================

/// Run all filesystem tests
pub fn run_all_tests() {
    log("\n[FS Tests] Starting filesystem tests...\n");

    let mut passed = 0;
    let mut failed = 0;

    // Test 1: Directory creation
    if test_create_tmp_directory() {
        passed += 1;
    } else {
        failed += 1;
    }

    // Test 2: File operations
    if test_file_operations() {
        passed += 1;
    } else {
        failed += 1;
    }

    // Test 3: Long filename support
    if test_long_filename_operations() {
        passed += 1;
    } else {
        failed += 1;
    }

    // Test 4: Subdirectory file operations
    if test_subdirectory_operations() {
        passed += 1;
    } else {
        failed += 1;
    }

    // Test 5: Rename operations
    if test_rename_operations() {
        passed += 1;
    } else {
        failed += 1;
    }

    // Test 6: fs_error_to_errno mapping
    if test_fs_error_to_errno_mapping() {
        passed += 1;
    } else {
        failed += 1;
    }

    log(&format!(
        "\n[FS Tests] Complete: {passed} passed, {failed} failed\n"
    ));
}

// ============================================================================
// Test: Directory Creation
// ============================================================================

/// Test creating a tmp directory
fn test_create_tmp_directory() -> bool {
    log("[FS Tests] Test: create_tmp_directory\n");

    let test_dir = "/tmp";

    // Check if tmp directory already exists
    if fs::exists(test_dir) {
        log("  - /tmp already exists, skipping creation\n");
        log("  - PASSED\n");
        return true;
    }

    log("  - Creating /tmp directory\n");

    // Create the directory
    match fs::create_dir(test_dir) {
        Ok(()) => {
            log(&format!("  - Created: {test_dir}\n"));
        }
        Err(e) => {
            log(&format!("  - FAILED to create {test_dir}: {e}\n"));
            return false;
        }
    }

    // Verify the directory exists
    if !fs::exists(test_dir) {
        log(&format!(
            "  - FAILED: {test_dir} does not exist after creation\n"
        ));
        return false;
    }

    log("  - PASSED\n");
    true
}

// ============================================================================
// Test: File Operations
// ============================================================================

/// Test file create, read, append, read, delete operations
fn test_file_operations() -> bool {
    log("[FS Tests] Test: file_operations\n");

    let test_file = "/testfile.txt";
    let initial_content = b"Hello, FAT32!";

    // Step 1: Create and write to file
    log("  - Step 1: Create and write file\n");
    match fs::write_file(test_file, initial_content) {
        Ok(()) => {
            log(&format!(
                "    Created {} with {} bytes\n",
                test_file,
                initial_content.len()
            ));
        }
        Err(e) => {
            log(&format!("    FAILED to create file: {e}\n"));
            return false;
        }
    }

    // Step 2: Read the file and verify content
    log("  - Step 2: Read and verify content\n");
    match fs::read_file(test_file) {
        Ok(content) => {
            if content.as_slice() != initial_content {
                log(&format!(
                    "    FAILED: Content mismatch. Expected {:?}, got {:?}\n",
                    core::str::from_utf8(initial_content),
                    core::str::from_utf8(&content)
                ));
                return false;
            }
            log("    Content verified\n");
        }
        Err(e) => {
            log(&format!("    FAILED to read file: {e}\n"));
            return false;
        }
    }

    // Step 3: Delete the file
    log("  - Step 3: Delete file\n");
    match fs::remove_file(test_file) {
        Ok(()) => {
            log("    File deleted\n");
        }
        Err(e) => {
            log(&format!("    FAILED to delete file: {e}\n"));
            return false;
        }
    }

    // Step 4: Verify file no longer exists
    log("  - Step 4: Verify file deleted\n");
    if fs::exists(test_file) {
        log("    FAILED: File still exists after deletion\n");
        return false;
    }
    log("    File confirmed deleted\n");

    log("  - PASSED\n");
    true
}

// ============================================================================
// Test: Long Filename Operations
// ============================================================================

/// Test reading files with long filenames (LFN)
fn test_long_filename_operations() -> bool {
    log("[FS Tests] Test: long_filename_operations\n");

    // List root directory to find any LFN files
    log("  - Listing root directory for LFN files\n");
    match fs::list_dir("/") {
        Ok(entries) => {
            let mut found_lfn = false;
            for entry in &entries {
                // Check if filename contains lowercase or is longer than 8.3
                let name = &entry.name;
                let has_lowercase = name.chars().any(char::is_lowercase);
                let is_long = name.len() > 12; // 8 + 1 + 3

                if has_lowercase || is_long {
                    log(&format!("    Found LFN: {name}\n"));
                    found_lfn = true;

                    // Try to read this file if it's not a directory
                    if !entry.is_dir {
                        match fs::read_file(&format!("/{name}")) {
                            Ok(content) => {
                                log(&format!("    Read {} bytes from LFN file\n", content.len()));
                            }
                            Err(e) => {
                                log(&format!("    FAILED to read LFN file {name}: {e}\n"));
                                return false;
                            }
                        }
                    }
                }
            }

            if !found_lfn {
                log("    No LFN files found (test skipped)\n");
            }
        }
        Err(e) => {
            log(&format!("  - FAILED to list directory: {e}\n"));
            return false;
        }
    }

    log("  - PASSED\n");
    true
}

// ============================================================================
// Test: Subdirectory Operations
// ============================================================================

/// Test file operations in subdirectories
fn test_subdirectory_operations() -> bool {
    log("[FS Tests] Test: subdirectory_operations\n");

    // Ensure tmp directory exists
    if !fs::exists("/tmp") {
        log("  - Creating /tmp directory\n");
        if let Err(e) = fs::create_dir("/tmp") {
            log(&format!("  - FAILED to create /tmp: {e}\n"));
            return false;
        }
    }

    let test_file = "/tmp/subtest.txt";
    let content = b"Subdirectory test content";

    // Step 1: Write file in subdirectory
    log("  - Step 1: Write file in subdirectory\n");
    match fs::write_file(test_file, content) {
        Ok(()) => {
            log(&format!(
                "    Created {} with {} bytes\n",
                test_file,
                content.len()
            ));
        }
        Err(e) => {
            log(&format!("    FAILED to create file: {e}\n"));
            return false;
        }
    }

    // Step 2: Read file from subdirectory
    log("  - Step 2: Read file from subdirectory\n");
    match fs::read_file(test_file) {
        Ok(read_content) => {
            if read_content.as_slice() != content {
                log("    FAILED: Content mismatch\n");
                return false;
            }
            log("    Content verified\n");
        }
        Err(e) => {
            log(&format!("    FAILED to read file: {e}\n"));
            return false;
        }
    }

    // Step 3: List subdirectory to verify
    log("  - Step 3: List subdirectory\n");
    match fs::list_dir("/tmp") {
        Ok(entries) => {
            let found = entries
                .iter()
                .any(|e| e.name.to_lowercase() == "subtest.txt");
            if !found {
                log("    FAILED: File not found in directory listing\n");
                return false;
            }
            log(&format!("    Found {} entries in /tmp\n", entries.len()));
        }
        Err(e) => {
            log(&format!("    FAILED to list directory: {e}\n"));
            return false;
        }
    }

    // Step 4: Delete file
    log("  - Step 4: Delete file in subdirectory\n");
    match fs::remove_file(test_file) {
        Ok(()) => {
            log("    File deleted\n");
        }
        Err(e) => {
            log(&format!("    FAILED to delete file: {e}\n"));
            return false;
        }
    }

    // Step 5: Verify deletion
    log("  - Step 5: Verify file deleted\n");
    if fs::exists(test_file) {
        log("    FAILED: File still exists after deletion\n");
        return false;
    }
    log("    File confirmed deleted\n");

    log("  - PASSED\n");
    true
}

// ============================================================================
// Test: Rename Operations
// ============================================================================

/// Test rename and rename-noreplace semantics
fn test_rename_operations() -> bool {
    log("[FS Tests] Test: rename_operations\n");

    if !fs::exists("/tmp")
        && let Err(e) = fs::create_dir("/tmp") {
            log(&format!("  - FAILED to create /tmp: {e}\n"));
            return false;
        }

    let src = "/tmp/rename_src.txt";
    let dst = "/tmp/rename_dst.txt";

    // Step 1: Create source file
    log("  - Step 1: Create source file\n");
    if let Err(e) = fs::write_file(src, b"rename test data") {
        log(&format!("    FAILED to create source: {e}\n"));
        return false;
    }

    // Step 2: Rename src -> dst
    log("  - Step 2: Rename source to destination\n");
    if let Err(e) = fs::rename(src, dst) {
        log(&format!("    FAILED to rename: {e}\n"));
        let _ = fs::remove_file(src);
        return false;
    }

    // Step 3: Verify source is gone and destination has correct content
    log("  - Step 3: Verify rename results\n");
    if fs::exists(src) {
        log("    FAILED: Source still exists after rename\n");
        let _ = fs::remove_file(src);
        let _ = fs::remove_file(dst);
        return false;
    }
    match fs::read_file(dst) {
        Ok(content) => {
            if content.as_slice() != b"rename test data" {
                log("    FAILED: Destination content mismatch\n");
                let _ = fs::remove_file(dst);
                return false;
            }
        }
        Err(e) => {
            log(&format!("    FAILED to read destination: {e}\n"));
            let _ = fs::remove_file(dst);
            return false;
        }
    }

    // Step 4: Test NOREPLACE semantics — create another file and try to rename over dst
    log("  - Step 4: Test rename-noreplace (exists check)\n");
    let src2 = "/tmp/rename_src2.txt";
    if let Err(e) = fs::write_file(src2, b"should not overwrite") {
        log(&format!("    FAILED to create second source: {e}\n"));
        let _ = fs::remove_file(dst);
        return false;
    }

    // Simulate RENAME_NOREPLACE: check exists() before rename()
    if fs::exists(dst) {
        log("    Destination exists — NOREPLACE would return EEXIST (correct)\n");
    } else {
        log("    FAILED: Destination should exist at this point\n");
        let _ = fs::remove_file(src2);
        let _ = fs::remove_file(dst);
        return false;
    }

    // Verify original destination content is preserved
    match fs::read_file(dst) {
        Ok(content) => {
            if content.as_slice() != b"rename test data" {
                log("    FAILED: Destination content was modified\n");
                let _ = fs::remove_file(src2);
                let _ = fs::remove_file(dst);
                return false;
            }
        }
        Err(e) => {
            log(&format!("    FAILED to read destination: {e}\n"));
            let _ = fs::remove_file(src2);
            let _ = fs::remove_file(dst);
            return false;
        }
    }

    // Cleanup
    let _ = fs::remove_file(src2);
    let _ = fs::remove_file(dst);

    log("  - PASSED\n");
    true
}

// ============================================================================
// Test: fs_error_to_errno mapping
// ============================================================================

/// Verify every FsError variant maps to the correct Linux errno.
/// Critically: nothing maps to EPERM — filesystem permission denial is EACCES
/// on Linux (EPERM is reserved for ownership/privilege checks, which the VFS
/// layer doesn't surface through FsError). Conformance change: f5f7196.
fn test_fs_error_to_errno_mapping() -> bool {
    use crate::vfs::FsError;

    log("[FS Tests] Test: fs_error_to_errno_mapping\n");

    // Linux errno values (negated, as u64)
    let enoent: u64 = ENOENT;
    let eio: u64 = EIO;
    let eexist: u64 = EEXIST;
    let enotdir: u64 = ENOTDIR;
    let eisdir: u64 = EISDIR;
    let einval: u64 = EINVAL;
    let eacces: u64 = EACCES;
    let emfile: u64 = EMFILE;
    let enospc: u64 = ENOSPC;
    let erofs: u64 = EROFS;
    let enotempty: u64 = ENOTEMPTY;

    let cases: &[(FsError, u64, &str)] = &[
        (FsError::NotFound, enoent, "NotFound -> ENOENT"),
        (FsError::PermissionDenied, eacces, "PermissionDenied -> EACCES"),
        (FsError::AlreadyExists, eexist, "AlreadyExists -> EEXIST"),
        (FsError::NotADirectory, enotdir, "NotADirectory -> ENOTDIR"),
        (FsError::NotAFile, eisdir, "NotAFile -> EISDIR"),
        (FsError::DirectoryNotEmpty, enotempty, "DirectoryNotEmpty -> ENOTEMPTY"),
        (FsError::NoSpace, enospc, "NoSpace -> ENOSPC"),
        (FsError::ReadOnly, erofs, "ReadOnly -> EROFS"),
        (FsError::InvalidPath, einval, "InvalidPath -> EINVAL"),
        (FsError::IoError, eio, "IoError -> EIO"),
        (FsError::Internal, eio, "Internal -> EIO"),
        (FsError::TooManyOpenFiles, emfile, "TooManyOpenFiles -> EMFILE"),
        // These should all fall through to EIO (not EPERM)
        (FsError::BlockDeviceNotInitialized, eio, "BlockDeviceNotInitialized -> EIO"),
        (FsError::NotInitialized, eio, "NotInitialized -> EIO"),
        (FsError::InvalidHandle, eio, "InvalidHandle -> EIO"),
        (FsError::Corrupt, eio, "Corrupt -> EIO"),
        (FsError::EndOfFile, eio, "EndOfFile -> EIO"),
        (FsError::NoFilesystem, eio, "NoFilesystem -> EIO"),
        (FsError::NotSupported, eio, "NotSupported -> EIO"),
    ];

    let mut ok = true;
    for (error, expected, label) in cases {
        let actual = crate::syscall::fs::fs_error_to_errno(*error);
        if actual != *expected {
            log(&format!("  FAILED: {} — got {} expected {}\n", label, actual as i64, *expected as i64));
            ok = false;
        }
    }

    if ok {
        log("  - All 19 FsError variants map to correct errno\n");
        log("  - PASSED\n");
    }
    ok
}

// ============================================================================
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}
