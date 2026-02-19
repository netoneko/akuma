# Akuma Userspace `tar` CLI Implementation Plan

## 1. Project Goal

Implement a basic `tar` command-line utility for the Akuma userspace environment. This utility should be capable of extracting gzipped tar archives.

## 2. Supported Command

The primary command to be supported initially is:

`tar zxvf <archive.tar.gz>`

This translates to:
- `z`: Decompress with gzip.
- `x`: Extract files from an archive.
- `v`: Verbose output (list files as they are processed).
- `f`: Use archive file (the next argument will be the archive name).

## 3. Scope

The initial scope will focus on:
- Parsing the `zxvf` options.
- Opening and reading a gzipped tar archive specified by the `<archive.tar.gz>` argument.
- Decompressing the gzip stream.
- Extracting files and directories from the tar archive to the current working directory.
- Printing verbose output (file names) during extraction.

## 4. Constraints and Environment

- This application will reside in `userspace/tar/`.
- **Crucially, `userspace/build.sh` and `userspace/Cargo.toml` (the workspace Cargo.toml) should NOT be modified.** The `userspace/tar/Cargo.toml` file will manage its own dependencies and build process.
- The focus for now is to ensure `cargo build` works correctly within the `userspace/tar/` directory, producing an executable.

## 5. High-Level Implementation Steps

1.  **Create `userspace/tar/` directory structure:**
    -   `userspace/tar/Cargo.toml`
    -   `userspace/tar/src/main.rs`
2.  **`userspace/tar/Cargo.toml` setup:**
    -   Define a new binary package.
    -   Add necessary dependencies:
        -   `libakuma` (for syscalls, if needed, though direct file operations might be abstracted).
        -   `flate2` crate for gzip decompression.
        -   `tar` crate for tar archive processing.
        -   `clap` or similar for argument parsing (optional, can be manual for simplicity).
3.  **`userspace/tar/src/main.rs` implementation:**
    -   **Argument Parsing:**
        -   Parse command-line arguments to identify `zxvf` flags and the archive file name.
        -   Handle basic error cases for missing arguments or invalid flags.
    -   **File Handling:**
        -   Open the specified `<archive.tar.gz>` file. This will use `libakuma`'s file I/O functions.
    -   **Gzip Decompression:**
        -   Wrap the file reader with a `flate2::read::GzDecoder` to decompress the stream.
    -   **Tar Archive Processing:**
        -   Pass the decompressed stream to a `tar::Archive`.
        -   Iterate through the entries in the `tar::Archive`.
        -   For each entry:
            -   Extract the file/directory to the appropriate path.
            -   Handle file permissions and metadata as supported by `libakuma`.
            -   If verbose (`v`) flag is set, print the entry's path.
    -   **Error Handling:**
        -   Implement robust error handling for file I/O, decompression, and tar parsing.
        -   Report errors to `stderr`.

## 6. Testing (Initial)

- Ensure `cargo build` within `userspace/tar/` completes successfully.
- Manually test with a simple `tar zxvf` command on a known good archive once the Akuma environment supports running userspace applications.
