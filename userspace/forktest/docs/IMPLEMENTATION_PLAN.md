# Plan for `forktest` Application Development

## Project Name: `forktest`

## Objective

To create a userspace application named `forktest` written in Golang that simulates the problematic behaviors of the Go compiler (`go build` and its child `compile` processes) on Akuma OS. The goal is to provide a controlled environment to reproduce, analyze, and debug kernel issues by specifically leveraging Golang's native runtime and concurrency patterns (goroutines, channels, `sync` primitives, underlying `futex`/`epoll`/`clone` syscalls).

## Language: Golang

## Project Structure

```
userspace/forktest/
├── .gitignore
├── go.mod
├── go.sum
├── docs/
│   └── IMPLEMENTATION_PLAN.md
├── parent/
│   └── main.go         # Parent process (epoll monitor, child spawner)
└── child/
    └── main.go         # Child process (stress tests)
```

## Completed Phases

*   **Phase 0 (Setup)**:
    *   Directory `userspace/forktest/` created.
    *   Go module initialized with `go.mod` and `go.sum` files.
*   **Phase 1 (Basic Fork/Exec/Pipe Communication)**:
    *   `child/main.go` prints a unique ID and exits.
    *   `parent/main.go` spawns multiple child processes, waits for them, and captures their standard output.
*   **Phase 2 (Epoll Monitoring)**:
    *   `parent/main.go` uses `golang.org/x/sys/unix` for `epoll` monitoring of child process pipes. It handles `EPOLLIN` for data and `EPOLLRDHUP` for child exits.
*   **Phase 3 (Simulate `mmap`/`munmap` Patterns and High Memory Pressure)**:
    *   `child/main.go` includes logic for `mmap`/`munmap` stress testing (allocating and deallocating large byte slices, triggering `runtime.GC()`). Gated by `-mmap_test` flag.
    *   `parent/main.go` accepts `-mmap_test` and forwards it to child processes.
*   **Phase 4 (File I/O with `O_APPEND` and Signal Handling)**:
    *   `child/main.go` implements `O_APPEND` write testing (create temp file, sequential writes, read-back verification). Gated by `-file_io` flag.
    *   `child/main.go` registers signal handlers for `SIGINT` and `SIGSEGV` via `signal.Notify`.
    *   `parent/main.go` accepts `-file_io` and `-send_signal` flags; forwards `-file_io` to children; optionally sends `SIGINT` to one child to test signal delivery.
*   **Phase 5 (High Concurrency, Futex Stress, and Combined System Stress)**:
    *   `parent/main.go` accepts `-num_children` flag (default 3) to control number of spawned child processes.
    *   `child/main.go` implements goroutine stress test (producer -> workers -> collector pipeline with channels and `sync.WaitGroup`). Gated by `-goroutine_stress` flag.
    *   `child/main.go` implements combined stress mode running all tests concurrently. Gated by `-combined_stress` flag.
    *   `parent/main.go` accepts `-combined_stress` and forwards all relevant flags to children.

## Building

Build via the top-level userspace build script with the `--with-forktest` flag:

```bash
cd userspace
./build.sh --with-forktest
```

This cross-compiles both Go binaries (`GOOS=linux GOARCH=arm64 CGO_ENABLED=0`) and copies them to `bootstrap/bin/`.

## Running

Copy both binaries to the same directory on Akuma OS, then:

```bash
# Basic test (3 children, no stress)
./forktest_parent

# Memory pressure test
./forktest_parent -mmap_test

# File I/O test
./forktest_parent -file_io

# Signal delivery test
./forktest_parent -send_signal

# High concurrency (50 children)
./forktest_parent -num_children=50

# Goroutine/futex stress
./forktest_parent -goroutine_stress

# Combined stress with many children
./forktest_parent -num_children=50 -combined_stress

# All flags
./forktest_parent -num_children=50 -combined_stress -send_signal
```

## Feature Flags

All test behaviors are controllable via command-line flags for modular testing:

| Flag | Parent | Child | Description |
|------|--------|-------|-------------|
| `-mmap_test` | forwards | enables | Allocate/free large memory regions, force GC |
| `-file_io` | forwards | enables | O_APPEND file write + read-back verification |
| `-send_signal` | sends SIGINT | — | Test signal delivery to one child |
| `-num_children=N` | sets count | — | Number of child processes (default 3) |
| `-goroutine_stress` | forwards | enables | 50-goroutine channel pipeline |
| `-combined_stress` | forwards | enables | Run all stress modes concurrently |
