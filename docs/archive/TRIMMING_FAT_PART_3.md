# Trimming Fat Part 3

Continued removal of components that add build complexity and maintenance burden without contributing to the core OS goals.

## Removed: needle-server

**What:** `userspace/needle-server/` — an HTTP inference server for the Needle function-routing model, exposing an OpenAI-compatible `/v1/chat/completions` endpoint so `meow` could use it as a local provider without a full LLM.

**Why removed:** Not useful in practice for the author's workflow — superseded by using real LLM providers through `meow` directly.

**Files removed:**
- `userspace/needle-server/` (entire directory)
- `"needle-server"` entry from `userspace/Cargo.toml`

**Files updated:**
- `docs/NEEDLE_SERVER.md` — kept for historical reference, marked as removed

## Removed: crush

**What:** `userspace/crush/` — a port of [Crush](https://github.com/charmbracelet/crush), a terminal-based coding assistant integrating with various LLM providers (Anthropic, OpenAI, Groq, etc.), plus its `modernc.org/sqlite` dependency.

**Why removed:** Redundant with `meow`, Akuma's own AI coding assistant; porting Crush's Go/SQLite dependency chain surfaced missing syscalls and VFS features (POSIX file locking, etc.) for a second assistant that duplicated `meow`'s role.

**Files removed:**
- `userspace/crush/` (entire directory, including `docs/IMPLEMENTATION_DETAILS.md`)
- `"crush"` entry from `userspace/Cargo.toml` and `userspace/build.sh`

**Files updated:**
- `docs/CRUSH_MISSING_SYSCALLS.md` — kept for historical reference, marked as removed

## Removed: stdcheck

**What:** `userspace/stdcheck/` — a test program for heap-allocation `std` compatibility (`Vec`, `String`, `Box` operations including reallocation).

**Why removed:** A one-off diagnostic tool for a since-resolved allocator bug; no longer needed once the underlying issue was fixed.

**Files removed:**
- `userspace/stdcheck/` (entire directory)
- `"stdcheck"` entry from `userspace/Cargo.toml`

**Files updated:**
- `docs/STDCHECK_DEBUG.md` — kept for historical reference, marked as removed

## Removed: top

**What:** `userspace/top/` — a custom `no_std` `top`-like process viewer.

**Why removed:** Redundant with busybox `top` (available via Alpine apk), which covers the same use case without a bespoke reimplementation.

**Files removed:**
- `userspace/top/` (entire directory)
- `"top"` entry from `userspace/Cargo.toml`

**Files updated:**
- `docs/TOP_CORE_COLUMN_PLAN.md` — kept for historical reference, marked as removed

## Removed: stp_test

**What:** `userspace/stp_test/` — a Go/C/assembly test harness (`stp_arm64.s`) probing stack/register parameter-passing behavior, referenced from Go-forktest debugging.

**Why removed:** Purpose-built one-off probe for a debugging session; not part of ongoing test infrastructure.

**Files removed:**
- `userspace/stp_test/` (entire directory)
- `"stp_test"` entry from `userspace/Cargo.toml`

## Removed: quickjs

**What:** `userspace/quickjs/` — a userspace JavaScript runtime (`qjs`) using Bellard's QuickJS engine, providing ES2020 support (BigInt, Promises, async/await, console API) directly on the Akuma kernel.

**Why removed:** No longer maintained as a first-party runtime; JS workloads are covered by Bun (via Alpine apk) instead.

**Files removed:**
- `userspace/quickjs/` (entire directory)
- `"quickjs"` entry from `userspace/Cargo.toml`
- `"quickjs"` from both MEMBERS and BINARIES arrays (and the `qjs`-binary special cases) in `userspace/build.sh`
- quickjs row/entries from `README.md` (capabilities table and architecture diagram) and `userspace/README.md` (prose examples)

**Files updated:**
- `docs/QJS.md` — kept for historical reference, marked as removed
- `docs/C_STUBS.md` — already noted quickjs as the sole remaining C-stub consumer after `sqld`'s removal; that note now needs its own follow-up since quickjs is gone too

## Moved: eintr_repro → forktest/c_stress

**What:** `userspace/eintr_repro/pthread_kill_eintr.c` (the `pthread_kill`-interrupts-`read` EINTR probe) relocated into `userspace/forktest/c_stress/`, alongside the rest of the pure-C musl-static kernel probes.

**Why moved:** `eintr_repro/` was a one-binary directory duplicating the pattern `forktest/c_stress/` already establishes for small deterministic C probes; consolidating avoids a second build-script code path for the same kind of binary.

**Files updated:**
- `userspace/build.sh` — build step now `cd`s into `forktest/c_stress` and copies `forktest/c_stress/pthread_kill_eintr`
- `userspace/forktest/.gitignore` — added `c_stress/pthread_kill_eintr`
- `docs/reference/subsystems/syscalls/signal.md` — repro path updated to `userspace/forktest/c_stress/pthread_kill_eintr.c`
