# Build + MCP Server

This project implements a Minimal Continuous Packaging (MCP) server in Rust. It provides a simple API to trigger builds of a specified Rust project and serves the resulting binaries for distribution.

## Features

*   **Build Trigger API**: An HTTP POST endpoint (`/build`) to initiate a build process.
*   **Git Integration**:
    *   Optional `git pull` before building the project.
    *   Aborts build if unstaged changes are detected in the target repository.
*   **Rust Build Process**: Executes `cargo build --release --target=<target_arch>` for the configured project.
*   **Binary Distribution**: An HTTP GET endpoint (`/download/:binary_name`) to serve compiled binaries.
*   **Configurable**: Project path and target architecture can be configured via `config.toml` or environment variables.
*   **Logging**: Uses `tracing` for detailed server and build process logs.

## Setup

### Prerequisites

Ensure you have the following installed:

*   Rust and Cargo (https://www.rust-lang.org/tools/install)
*   Git

### Building and Running

1.  **Clone the repository (if applicable) and navigate to the server directory:**
    ```bash
    # If starting from scratch, you would clone your project first
    # cd your_project
    cd mcp-server
    ```

2.  **Configuration (Optional)**

    You can configure the server by creating a `config.toml` file in the `mcp-server` directory or by using environment variables.

    **`config.toml` example:**
    ```toml
    # config.toml
    project_path = "../.." # Path to your Rust project to build (relative or absolute)
    target_arch = "aarch64-unknown-none" # The target architecture for cargo build
    ```
    *Note: The `project_path` should point to the root of the Rust project you want this server to build.*

    **Environment Variables:**
    Prefix environment variables with `MCP_SERVER_`. For example:
    ```bash
    export MCP_SERVER_PROJECT_PATH=/path/to/your/rust/project
    export MCP_SERVER_TARGET_ARCH=aarch64-unknown-none
    ```

3.  **Run the server:**
    ```bash
    cargo run
    ```
    The server will start on `http://127.0.0.1:3000` by default.

## API Endpoints

### 1. Trigger a Build (`POST /build`)

This endpoint initiates the build process for the configured Rust project.

*   **Method**: `POST`
*   **URL**: `http://127.0.0.1:3000/build`
*   **Query Parameters**:
    *   `pull`: Optional boolean. If `true`, the server will run `git pull` on the `project_path` before building. Defaults to `false`.

**Behavior:**
1.  Checks for unstaged changes in the `project_path`. If found, the build is aborted, and an error is logged.
2.  If `pull=true`, performs a `git pull`. If `git pull` fails, the build is aborted.
3.  Executes `cargo build --release --target=<target_arch>`.
4.  The server responds immediately, and the build process runs in the background. Check the server console for logs regarding the build status.

**Examples:**

*   **Trigger a build without pulling latest changes:**
    ```bash
    curl -X POST http://127.0.0.1:3000/build
    ```

*   **Trigger a build and pull latest changes:**
    ```bash
    curl -X POST "http://127.0.0.1:3000/build?pull=true"
    ```

### 2. Download Binary (`GET /download/:binary_name`)

This endpoint serves the compiled binary files.

*   **Method**: `GET`
*   **URL**: `http://127.0.0.1:3000/download/:binary_name`
*   **Path Parameters**:
    *   `binary_name`: The name of the compiled binary file (e.g., `my_executable`).

**Behavior:**
1.  Looks for the binary in the `project_path/target/<target_arch>/release/` directory.
2.  If the file exists, it is served for download.
3.  If the file does not exist, a `404 Not Found` response is returned.

**Example:**

*   **Download a binary named `my_app`:**
    ```bash
    curl -O http://127.0.0.1:3000/download/my_app
    ```
    (The `-O` flag will save the downloaded file with its original name.)
