# Large File Checkout Optimization

This document summarizes the issues and optimizations implemented for `scratch` to handle large file checkouts (e.g., `sqlite3.c`, 8MB+) in Akuma's `no_std` environment.

## Issues Identified

1.  **Memory Exhaustion (OOM):** The original implementation loaded entire compressed objects into memory and then decompressed them into a single `Vec<u8>`. For an 8MB file, this required at least 8MB for the compressed data and 8MB for the decompressed data, often leading to silent hangs or process crashes.
2.  **Delayed Progress Reporting:** Large file warnings and progress dots were only printed *after* successful decompression. If decompression took minutes or triggered a memory pressure slowdown, the user saw total silence, making the application appear hung.
3.  **Redundant Allocations:** Even after some optimizations, the code was performing unnecessary memory copies of large buffers, putting extreme pressure on the userspace allocator.

## Solutions Implemented

### 1. Instant Header Extraction (`read_info`)
- **Optimization:** Added a specialized `read_info` function that decompresses only the first few bytes of a Git object.
- **Benefit:** Allows `scratch` to determine a file's size and type instantly without decompressing the entire blob. This ensures the "MASSIVE FILE" warning appears immediately.

### 2. Full Pipeline Streaming (`read_to_callback`)
- **Optimization:** Transitioned from "one-shot" reads to a callback-based streaming architecture.
    - **Disk Streaming:** Compressed data is read from the filesystem in 32KB chunks.
    - **Decompression Streaming:** Each chunk is fed into `miniz_oxide`'s streaming API incrementally.
    - **Write Streaming:** Decompressed segments (16KB) are written directly to the target file.
- **Benefit:** The memory footprint is now constant (~64KB buffers) regardless of the file size (1KB or 100MB).

### 3. Real-Time Visual Feedback
- **Optimization:** Dots are now printed during the streaming process itself.
- **Benefit:** The user sees a continuous stream of dots as data is written to disk, providing immediate confirmation that the system is active and progressing.

### 4. Memory Optimization (Zero-Copy)
- **Optimization:** Where possible, buffers are reused or modified in-place to avoid high-frequency allocations of large `Vec`s.

## Summary of Logic Flow
1.  **Scan Tree:** Identify file and SHA-1.
2.  **Probe Size:** Call `read_info` -> Print warning if >300KB.
3.  **Stream Checkout:** 
    - Open source file (Git object).
    - Open destination file.
    - Loop: `Read 32KB Compressed -> Decompress -> Write 16KB Chunks -> Print Dot every 64KB`.
    - Close all handles.

This architecture ensures `scratch` remains responsive and stable even when checking out repositories with massive source files.
