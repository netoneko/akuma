# forktest

A Go stress-test application for Akuma OS that exercises fork/exec, epoll,
mmap, file I/O, and goroutine scheduling.  It consists of two binaries:

- **`forktest_parent`** — spawns child processes, monitors them with `epoll`
- **`forktest_child`** — runs configurable stress tests, reports over a pipe

## Build

```bash
cd userspace/forktest
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o forktest_parent ./parent
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o forktest_child  ./child
```

Copy both binaries to `/bin/` on the Akuma disk image (or use **`pkg install`** — see below).

The parent resolves `forktest_child` next to its own executable (or falls back to `/bin/forktest_child`), so you do not need `cd /bin` for `exec` to succeed.

### Installing via `pkg install` (SSH)

From the host, serve packages so QEMU can reach them at `10.0.2.2:8000` ([`docs/PACKAGES.md`](../../docs/PACKAGES.md)):

```bash
cd bootstrap && python3 -m http.server 8000
```

In SSH: `pkg install forktest_parent forktest_child` (if those names are served under `bootstrap/bin/`), or copy updated binaries the same way.

For the **C control binary** `mmap_stress` (kernel-vs-Go disambiguation), build or copy it to `bootstrap/bin/mmap_stress`, keep the server running, then:

```text
pkg install mmap_stress
```

## Usage

```
forktest_parent [flags]
```

### Flags

| Flag | Default | Description |
|------|---------|-------------|
| `-num_children N` | `3` | Number of child processes to spawn |
| `-duration D` | `0` | Total test duration (e.g. `30s`, `2m`). `0` = run until all children finish |
| `-combined_stress` | `false` | Run all stress modes concurrently in each child |
| `-mmap_test` | `false` | Enable mmap/munmap stress in children |
| `-mmap_alloc_mb N` | `100` | MB per allocation in mmap mode (forwarded to children with `-mmap_test` / `-combined_stress`) |
| `-file_io` | `false` | Enable O_APPEND file I/O test in children |
| `-goroutine_stress` | `false` | Enable goroutine/channel stress in children |
| `-send_signal` | `false` | Send SIGINT to child 0 after 500 ms |
| `-use_c_child` | `false` | Exec **`/bin/mmap_stress`** (pure C, static musl) instead of **`forktest_child`** — bisect kernel vs Go runtime |

`-duration` is forwarded to each child so all processes share the same deadline.

### Child flags (set automatically by parent, or run directly)

| Flag | Default | Description |
|------|---------|-------------|
| `-duration D` | `0` | How long to loop stress tests. `0` = run once |
| `-mmap_test` | `false` | mmap/munmap stress |
| `-mmap_alloc_mb N` | `100` | MB per `make([]byte, …)` in mmap stress (lower to bisect lazy region size) |
| `-file_io` | `false` | O_APPEND file I/O |
| `-goroutine_stress` | `false` | Goroutine/channel stress |
| `-combined_stress` | `false` | All modes concurrently |

## Examples

**Quick sanity check** (3 children, run once):
```
forktest_parent
```

**30-second combined stress, 5 children:**
```
forktest_parent -duration=30s -combined_stress -num_children=5
```

**Mmap stress only, 60 seconds:**
```
forktest_parent -duration=60s -mmap_test -num_children=2
```

**Smaller heap allocations (bisect kernel lazy paging):**
```
forktest_parent -duration=30s -mmap_test -mmap_alloc_mb=4 -num_children=1
```

**C-only mmap stress (after `pkg install mmap_stress`):**
```
forktest_parent --use_c_child -duration=10s --mmap_test=true --mmap_alloc_mb=70
```
See [`c_stress/README.md`](c_stress/README.md).

**Test SIGINT handling:**
```
forktest_parent -send_signal -goroutine_stress
```

## Stress modes

### mmap/munmap (`-mmap_test`)
Allocates large slices (default **100 MB** each; override with **`-mmap_alloc_mb`**) in a loop,
triggering GC between each to exercise the Go heap's interaction with Akuma's lazy
demand-paging mmap implementation.

Empirical note (see **`GO_FORKTEST_DEBUG.md`**, §Empirical allocation threshold): with
**`GOMAXPROCS=1`**, **10 MB** allocations have been stable; **50 MB** is sometimes stable and
sometimes crashes; **~70–100 MB** tends to fault in Go’s allocator (**`addr=0x2`** or
**`0xffffffffffffffb0`**). Use **`-mmap_alloc_mb=10`** (or **50**) for smoke tests until the
kernel issue is fixed.

### QEMU serial capture (kernel debug)

From the repo: **`./scripts/capture_serial_forktest_mmap.sh [logfile]`** tees serial output while
running the VM; then grep for **`[mmap]`**, **`[DA-MISS]`**, etc. See
[`docs/GO_FORKTEST_DEBUG.md`](../../docs/GO_FORKTEST_DEBUG.md) (**Appendix: mmap / demand-paging investigation**).

### O_APPEND file I/O (`-file_io`)
Creates a temp file, writes 10 lines with `O_APPEND`, reads it back, and
verifies the content matches exactly.

### Goroutine stress (`-goroutine_stress`)
Spawns 50 worker goroutines that process 200 items through a channel,
exercising the Go scheduler and futex-based synchronisation.

### Combined (`-combined_stress`)
Runs all three modes concurrently via `sync.WaitGroup`.

## How it works

```
forktest_parent
├── creates epoll instance (EPOLL_CLOEXEC)
├── for each child:
│   ├── creates a pipe (read end kept by parent)
│   ├── registers read end with EPOLLIN | EPOLLRDHUP | EPOLLONESHOT
│   └── exec forktest_child (stdout → write end of pipe)
├── epoll loop:
│   ├── EPOLLIN  → drain pipe into output buffer, re-arm EPOLLONESHOT
│   ├── EPOLLRDHUP → drain remaining data, mark child done
│   └── deadline exceeded → SIGTERM remaining children, break
└── Wait() all children, print output
```

When `-duration` is set the child loops each stress test until the deadline
is reached, then exits cleanly.  SIGTERM (sent by the parent on timeout) also
causes the child to exit immediately.

## Known issues

**Go / kernel stress failures:** With **`-mmap_test`** or **`-combined_stress`**,
children allocate large slices (`make([]byte, 100MB)` in mmap mode). With **`-file_io`**,
children stress **`/tmp`** (ext2); that mode has caused **deadlocks** as well as normal
I/O. A **known,
still-reproducing** child failure is a panic with **`addr=0x2`** and **`pc≈0x86768`**
(often under `memclrNoHeapPointers` → `mallocgcLarge`). Separately, the **parent**
can **SIGSEGV** inside **`unix.Read`** on the epoll pipe (**`main.go:199`**)
with a heap-range fault address — this has been observed with **`-mmap_test` alone**
and with **`GOMAXPROCS=1`**, so it is **not** fixed by single-threading the parent.
Details: [`docs/GO_FORKTEST_DEBUG.md`](../../docs/GO_FORKTEST_DEBUG.md) (**§Isolation matrix**).

**Not the same as ext2 EIO:** Past **`input/output error`** writes to `/tmp`
were tied to ext2 lock contention and are addressed in the kernel ext2 layer.
The **`addr=0x2`** crash is tracked separately (CoW, lazy paging, thread groups).

**Mitigations:** Plenty of RAM (`MEMORY=2048M`), try `GODEBUG=asyncpreemptoff=1`,
or avoid **`-mmap_test`**, **`-combined_stress`**, and **`-file_io`** until fixed. **`GOMAXPROCS=1`**
does **not** prevent the parent `read()` crash with **`-mmap_test`** (see doc above).
Default runs (**no** child stress flags) are the safest smoke test. **`-file_io`**
has reproduced **deadlocks** (SSH/shell hang) in addition to occasional clean runs—treat
it as **high risk** on Akuma until the kernel I/O path is fully solid.
