# mkdir_p and Path Verification Improvements

## Context
The `mkdir_p` utility in `libakuma` had issues with trailing slashes and lacked verification of success, occasionally leading to false-positive results when directory creation failed due to missing parent mounts or filesystem errors.

## Changes

### 1. Robust Path Splitting
The implementation was updated to handle root paths and multiple components more reliably:
- Correctly handles leading slashes by identifying the root component.
- Iterates through path components and builds the hierarchy step-by-step.
- Eliminates issues where trailing slashes caused the final `mkdir` call to fail.

### 2. Post-Creation Verification
Instead of assuming success based on the last `mkdir` syscall result, `mkdir_p` now performs an explicit check:
- Opens the final path with `O_RDONLY`.
- Uses `fstat` to verify the metadata.
- Validates the `st_mode` bits to ensure the path is actually a directory (`S_IFDIR`).

## Impact
These changes ensure that higher-level tools like `herd` can rely on the return value of `mkdir_p` to determine if the environment is correctly set up for writing configuration files or logs.
