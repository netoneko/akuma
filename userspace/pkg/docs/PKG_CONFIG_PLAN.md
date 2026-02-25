# `pkg config` Subcommand Plan

## 1. Goal

To enhance the `pkg` utility with a `config` subcommand, allowing users to view and modify configuration values, particularly the base URL from which packages are fetched. This centralizes package server management and removes hardcoded URLs.

## 2. Configuration Details

*   **Location**: `/etc/pkg/config`
*   **Format**: Simple key-value pairs, one per line. For example:
    ```
    BASE_URL=http://my.package.server:8000/packages
    ```
*   **Default**: If `/etc/pkg/config` does not exist or `BASE_URL` is not specified, `pkg` should fall back to a sensible default (e.g., `http://10.0.2.2:8000`).

## 3. Implementation Phases

### Phase 3.1: Define Config File Parsing and Storage Logic

1.  **Create a `config` module**: Within `userspace/pkg/src/`, create a new module `config.rs`.
2.  **`PkgConfig` struct**: Define a struct `PkgConfig` to hold configuration values (e.g., `base_url: String`).
3.  **`load()` function**: Implement a function `load() -> PkgConfig` that:
    *   Attempts to read `/etc/pkg/config`.
    *   Parses each line, extracting key-value pairs.
    *   Populates the `PkgConfig` struct.
    *   Handles errors gracefully (e.g., file not found, malformed lines), returning default values or logging warnings.
4.  **`save()` function**: Implement a function `save(&self) -> Result<(), Error>` that:
    *   Serializes the `PkgConfig` struct back into the key-value pair format.
    *   Writes the content to `/etc/pkg/config`, overwriting any existing file.
    *   Uses `libakuma` file I/O syscalls (`open`, `write_fd`, `close`).

### Phase 3.2: Implement `pkg config set`

1.  **Modify `userspace/pkg/src/main.rs`**:
    *   Add `config` as a new subcommand.
    *   Parse arguments to recognize `pkg config set <key> <value>`.
2.  **Load, Modify, Save**:
    *   Inside the `set` handler, call `PkgConfig::load()` to get the current configuration.
    *   Update the relevant field in the `PkgConfig` struct (e.g., `config.base_url = new_url`).
    *   Call `config.save()` to persist the changes.
3.  **Validation**: Ensure the provided URL is valid before saving.
4.  **User Feedback**: Print confirmation or error messages.

### Phase 3.3: Implement `pkg config get`

1.  **Modify `userspace/pkg/src/main.rs`**:
    *   Parse arguments to recognize `pkg config get <key>`.
2.  **Load and Print**:
    *   Inside the `get` handler, call `PkgConfig::load()`.
    *   Retrieve the value for the requested key (e.g., `config.base_url`).
    *   Print the value to `stdout`.
3.  **Error Handling**: Inform the user if the key is unknown or the config file cannot be read.

### Phase 3.4: Integrate Configuration into `pkg install`

1.  **Modify `cmd_pkg`**: In `userspace/pkg/src/main.rs`, modify the `cmd_pkg` function.
2.  **Use `PkgConfig`**: Replace the hardcoded `server = "10.0.2.2:8000"` line with a call to `PkgConfig::load()` and then use `config.base_url`.
3.  **Dynamic URLs**: Adjust the `bin_url`, `archive_url_gz`, and `archive_url_raw` to use the `base_url` read from the configuration, for example:
    ```rust
    let base_url = config.base_url.as_str();
    let bin_url = format!("{}/bin/{}", base_url, package);
    let archive_url_gz = format!("{}/archives/{}.tar.gz", base_url, package);
    // ... etc.
    ```

### Phase 3.5: Error Handling and Edge Cases

*   **Permissions**: Ensure `pkg` has appropriate permissions to read `/etc/pkg/config` and write if `set` is used.
*   **Default Values**: Implement robust fallback to default URLs if the config file is missing or `BASE_URL` is not present.
*   **Concurrency**: For this initial version, assume no concurrent writes to the config file.

## 4. Testing Plan

1.  **Manual Testing**:
    *   Test `pkg config set base_url http://example.com/test_packages`.
    *   Test `pkg config get base_url`.
    *   Verify the content of `/etc/pkg/config` directly.
    *   Run `pkg install <package>` after setting a custom `base_url` and ensure it fetches from the new location.
    *   Test with missing `/etc/pkg/config` to ensure default is used.
    *   Test with an invalid URL format to ensure validation works.
2.  **Unit Tests (if feasible)**: Add unit tests for `PkgConfig::load()` and `PkgConfig::save()` to cover parsing and serialization logic.
