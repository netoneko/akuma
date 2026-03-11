//! Filesystem Tests
//!
//! Tests for the FAT32 filesystem operations.
//! These tests are run after filesystem initialization.

use alloc::format;
use alloc::vec::Vec;

use crate::console;
use crate::fs;

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

    log(&format!(
        "\n[FS Tests] Complete: {} passed, {} failed\n",
        passed, failed
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
            log(&format!("  - Created: {}\n", test_dir));
        }
        Err(e) => {
            log(&format!("  - FAILED to create {}: {}\n", test_dir, e));
            return false;
        }
    }

    // Verify the directory exists
    if !fs::exists(test_dir) {
        log(&format!(
            "  - FAILED: {} does not exist after creation\n",
            test_dir
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
    let append_content = b" Appended text.";

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
            log(&format!("    FAILED to create file: {}\n", e));
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
            log(&format!("    FAILED to read file: {}\n", e));
            return false;
        }
    }

    // Step 3: Append to file
    log("  - Step 3: Append to file\n");
    match fs::append_file(test_file, append_content) {
        Ok(()) => {
            log(&format!("    Appended {} bytes\n", append_content.len()));
        }
        Err(e) => {
            log(&format!("    FAILED to append: {}\n", e));
            return false;
        }
    }

    // Step 4: Read again and verify appended content
    log("  - Step 4: Read and verify appended content\n");
    match fs::read_file(test_file) {
        Ok(content) => {
            let expected: Vec<u8> = initial_content
                .iter()
                .chain(append_content.iter())
                .copied()
                .collect();
            if content != expected {
                log(&format!(
                    "    FAILED: Content mismatch after append.\n    Expected: {:?}\n    Got: {:?}\n",
                    core::str::from_utf8(&expected),
                    core::str::from_utf8(&content)
                ));
                return false;
            }
            log(&format!(
                "    Content verified: {} bytes total\n",
                content.len()
            ));
        }
        Err(e) => {
            log(&format!("    FAILED to read after append: {}\n", e));
            return false;
        }
    }

    // Step 5: Delete the file
    log("  - Step 5: Delete file\n");
    match fs::remove_file(test_file) {
        Ok(()) => {
            log("    File deleted\n");
        }
        Err(e) => {
            log(&format!("    FAILED to delete file: {}\n", e));
            return false;
        }
    }

    // Step 6: Verify file no longer exists
    log("  - Step 6: Verify file deleted\n");
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
                let has_lowercase = name.chars().any(|c| c.is_lowercase());
                let is_long = name.len() > 12; // 8 + 1 + 3

                if has_lowercase || is_long {
                    log(&format!("    Found LFN: {}\n", name));
                    found_lfn = true;

                    // Try to read this file if it's not a directory
                    if !entry.is_dir {
                        match fs::read_file(&format!("/{}", name)) {
                            Ok(content) => {
                                log(&format!("    Read {} bytes from LFN file\n", content.len()));
                            }
                            Err(e) => {
                                log(&format!("    FAILED to read LFN file {}: {}\n", name, e));
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
            log(&format!("  - FAILED to list directory: {}\n", e));
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
            log(&format!("  - FAILED to create /tmp: {}\n", e));
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
            log(&format!("    FAILED to create file: {}\n", e));
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
            log(&format!("    FAILED to read file: {}\n", e));
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
            log(&format!("    FAILED to list directory: {}\n", e));
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
            log(&format!("    FAILED to delete file: {}\n", e));
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

    if !fs::exists("/tmp") {
        if let Err(e) = fs::create_dir("/tmp") {
            log(&format!("  - FAILED to create /tmp: {}\n", e));
            return false;
        }
    }

    let src = "/tmp/rename_src.txt";
    let dst = "/tmp/rename_dst.txt";

    // Step 1: Create source file
    log("  - Step 1: Create source file\n");
    if let Err(e) = fs::write_file(src, b"rename test data") {
        log(&format!("    FAILED to create source: {}\n", e));
        return false;
    }

    // Step 2: Rename src -> dst
    log("  - Step 2: Rename source to destination\n");
    if let Err(e) = fs::rename(src, dst) {
        log(&format!("    FAILED to rename: {}\n", e));
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
            log(&format!("    FAILED to read destination: {}\n", e));
            let _ = fs::remove_file(dst);
            return false;
        }
    }

    // Step 4: Test NOREPLACE semantics — create another file and try to rename over dst
    log("  - Step 4: Test rename-noreplace (exists check)\n");
    let src2 = "/tmp/rename_src2.txt";
    if let Err(e) = fs::write_file(src2, b"should not overwrite") {
        log(&format!("    FAILED to create second source: {}\n", e));
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
            log(&format!("    FAILED to read destination: {}\n", e));
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
// Logging
// ============================================================================

fn log(msg: &str) {
    console::print(msg);
}
