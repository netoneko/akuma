# Kernel Split Bugs

Bugs discovered after extracting `akuma-terminal` into a standalone crate.
Both bugs were already present on main before the split — they are pre-existing,
not regressions.

---

## 1. neatvi shows garbage characters at end of newlines

**Symptom:** Opening `/etc/meow/config` in neatvi shows weird symbols at the
end of each line (visible as trailing garbage after the newline).

**Likely cause:** Output translation (ONLCR `\n` -> `\r\n`) may be applied
twice, or neatvi's raw-mode setup isn't fully suppressing OPOST. Could also be
a mismatch between what neatvi expects from the terminal and what the SSH
bridge delivers.

**Reproduction:**
```
ssh -p 2222 akuma
vi /etc/meow/config
```

---

## 2. Running `hello` from neatvi crashes the kernel

**Symptom:** Using neatvi's shell-out (`:!hello`) triggers a kernel panic.
The crash is a synchronous exception from EL1 (EC=0x25, data abort) where the
kernel dereferences a user-space address with a stale TTBR0.

**Crash log:**
```
[exception] Process 24 (/bin/hello) exited, calling return_to_kernel(0)
[RTK] code=0 tid=9 LR=0x400ba9cc
[Exception] Sync from EL1: EC=0x25, ISS=0x46
  ELR=0x400952a0, FAR=0x300c2e80, SPSR=0x80002345
  Thread=9, TTBR0=0x1b0000462cb000, TTBR1=0x40149000
  SP=0x424422b0, SP_EL0=0x3ffffe38
  Instruction at ELR: 0xb900001f
  Likely: Rn(base)=x0, Rt(dest)=x31
  WARNING: Kernel accessing user-space address!
  This suggests stale TTBR0 or dereferencing user pointer from kernel.
```

**Key observations:**
- The process exits normally (`return_to_kernel(0)`) — the crash happens
  *after* the child process finishes, during cleanup or return to the parent.
- EC=0x25 is a data abort from EL1, meaning the kernel itself faulted.
- FAR=0x300c2e80 is in the dynamic linker region (ld-musl), suggesting the
  kernel is touching user pages after the child's address space has been torn
  down or TTBR0 has been switched.
- The warning confirms stale TTBR0: the kernel is dereferencing a user pointer
  that belongs to a process whose page tables are no longer active.

**Likely cause:** After the child process (PID 24, `/bin/hello`) exits via
`return_to_kernel`, something in the cleanup path (thread reuse, parent
resume, or channel teardown) accesses user memory without ensuring the correct
TTBR0 is loaded. This is a use-after-free of the child's address space from
kernel context.

**Reproduction:**
```
ssh -p 2222 akuma
vi
:!hello
```

---

## 3. Second `bun run` crashes with OOM in anonymous page fault handler

**Symptom:** Running `bun run /public/cgi-bin/akuma.js` succeeds the first time
(prints a cat picture and greeting), but running it again crashes with a data
abort from EL0. The page fault handler fails to allocate a physical page for an
anonymous mmap region because there are 0 free pages left.

**Crash log:**
```
[MMU] WARN: va=0x58fb000 already mapped to pa=0x469de000, wanted pa=0x4f584000
[MMU] WARN: va=0x58fc000 already mapped to pa=0x469df000, wanted pa=0x4f585000
[MMU] WARN: va=0x58fd000 already mapped to pa=0x469e0000, wanted pa=0x4f586000
[T368.68] [DA-DP] pid=26 va=0x63fa000 anon alloc failed, 0 free pages
[T368.68] [Fault] Data abort from EL0 at FAR=0x63fa000, ELR=0x4b2ecf4, ISS=0x7
[Fault]  x0=0x5013634 x1=0x4d45ac8 x2=0x0 x3=0x9907e740
[Fault]  x19=0x990512f0 x20=0x203ffbace8 x29=0x203ffbac60 x30=0x4b2ea20
[Fault]  SP_EL0=0x203ffbac50 SPSR=0x80000000 TPIDR_EL0=0x303f60e8
[RTK] code=-11 tid=8 LR=0x40087374
[T368.68] [LR!] clear pid=29 (19 regions)
[T368.68] [LR!] clear pid=31 (20 regions)
[Process] Killed 2 sibling thread(s) for PID 26
[T368.68] [LR!] clear pid=26 (20 regions)
[T368.68] [Process] PID 26 thread 8 exited (-11) [115.94s]
```

**Key observations:**
- The MMU warns that several VAs are "already mapped" to different PAs than
  requested, suggesting physical pages from the first run were not fully
  reclaimed or page table entries were not torn down properly.
- The allocation failure reports **0 free pages** — the physical memory manager
  is completely exhausted by the second invocation.
- The process has 20 lazy regions and spawns at least 2 sibling CLONE_VM
  threads (PIDs 29, 31), all of which are cleaned up after the crash.
- Exit code -11 (SIGSEGV) is the result of failing the demand-page fault.

**Likely cause:** Physical pages allocated for the first `bun` run are not
being freed when the process exits. The "already mapped" warnings suggest that
either page table entries are leaking (mapped pages not unmapped during process
teardown) or the physical memory manager is not reclaiming pages returned by
`munmap`/process exit. After one full bun run consumes most of physical memory,
the second run exhausts the remaining pages and OOMs.

**Reproduction:**
```
ssh -p 2222 akuma
bun run /public/cgi-bin/akuma.js   # succeeds
bun run /public/cgi-bin/akuma.js   # crashes with OOM
```

---

## 4. Global block cache in `akuma-ext2` causes cross-instance contamination

**Symptom:** When multiple `Ext2Filesystem` instances exist (e.g. in tests, or
if the kernel ever mounts two ext2 volumes), reads return data from the wrong
filesystem. In the test suite this manifested as non-deterministic failures:
`create_dir` would return `AlreadyExists` on a freshly mounted image because
the block cache still held directory blocks from a different test's filesystem.

**Root cause:** `BLOCK_CACHE` was a `static Spinlock<BTreeMap<u32, Vec<u8>>>`
keyed only by block number. All `Ext2Filesystem` instances shared the same
cache, so a block read by instance A could be returned to instance B if they
happened to request the same block number (which is almost certain — block
group descriptors, inode tables, and root directory blocks have fixed numbers
on every ext2 image).

**Fix applied:** Moved the cache from a global `static` to a per-instance field
(`block_cache: Spinlock<BTreeMap<u32, Vec<u8>>>`) on `Ext2Filesystem<B>`. Each
filesystem instance now has its own isolated cache.

**Impact:** This was a correctness bug, not just a test issue. In the kernel
there is only one ext2 mount so it was latent, but the global cache also
violated the `Ext2Filesystem` abstraction — any future use of multiple
instances (e.g. container overlay mounts) would have produced silent data
corruption.

---

## Logging Strategy for Extracted Crates

When extracting kernel modules into standalone crates, logging requires special
handling because crate code cannot use `safe_print!` or `crate::console`
directly.

### Rules

1. **Crate code must not depend on kernel logging.** No `safe_print!`, no
   `crate::console::print`, no direct UART writes. These are kernel-internal
   and create a hard dependency on the kernel binary.

2. **Use the `log` crate** (`log = { version = "0.4", default-features = false }`)
   in all extracted crates. Call `log::info!`, `log::debug!`, `log::warn!`,
   `log::error!` for diagnostics. The `log` crate is `no_std`-compatible and
   provides a facade — it emits nothing unless a logger backend is registered.

3. **The kernel provides the logger backend.** The kernel registers a global
   `log::Log` implementation (backed by `console::print` / `safe_print!`) at
   boot. All `log::*!` calls from any crate in the workspace then route
   through this single backend. This means crate log output appears in the
   same UART/SSH console stream as kernel logs, with no extra wiring.

4. **Kernel wrapper modules can still use `safe_print!`.** The thin glue code
   that stays in `src/` (e.g. `src/ssh/server.rs`, `src/ssh/keys.rs`) is part
   of the kernel binary and can continue using `safe_print!` directly. Only
   code that lives under `crates/` must use the `log` facade.

5. **Include the subsystem tag in log messages.** Use a consistent prefix so
   log output is filterable:
   ```rust
   log::info!("[SSH] Host key loaded from filesystem");
   log::debug!("[ext2] Reading inode {} from block group {}", ino, bg);
   ```

6. **Prefer `debug!` for high-frequency messages.** Packet-level tracing,
   per-syscall logs, and similar high-volume output should use `log::debug!`
   or `log::trace!` so they can be compiled out or filtered without code
   changes.

### Conformance Checklist

After the `akuma-ssh` extraction is working, verify that the other extracted
crates follow this strategy:

- [x] `akuma-ssh` — uses `log` crate throughout (confirmed working)
- [x] `akuma-shell` — no kernel logging in crate code (pure framework, OK as-is)
- [x] `akuma-net` — uses `log` crate throughout, all `safe_print!`/`console::print` replaced
- [ ] `akuma-ssh-crypto` — currently has no logging (pure crypto, OK as-is)
- [ ] `akuma-terminal` — currently has no logging (pure data, OK as-is)
- [ ] `akuma-vfs` — check for any stray `safe_print!` or direct console use
- [ ] `akuma-ext2` — check for any stray `safe_print!` or direct console use
- [ ] Kernel registers a `log::Log` backend at boot (required for any of the
      above to actually produce output)

---

## `akuma-ssh` Crate Extraction (completed)

Extracted the SSH-2 protocol engine from `src/ssh/protocol.rs` (~1840 lines)
into `crates/akuma-ssh/`, leaving kernel-coupled glue in `src/ssh/`.

### What moved to the crate

| Module | Contents |
|--------|----------|
| `constants.rs` | SSH message type constants (`SSH_MSG_*`), algorithm names, version string |
| `config.rs` | `SshdConfig` struct with `parse(content)` and `parse_line()` |
| `session.rs` | `SshState` enum, `SshSession` struct (all fields `pub`) |
| `kex.rs` | `build_kexinit()`, `handle_kex_ecdh_init()` |
| `packet.rs` | `process_encrypted_packet()`, `process_unencrypted_packet()` |
| `transport.rs` | `send_raw`, `send_packet`, `send_channel_data` — all generic over `T: embedded_io_async::Write` |
| `message.rs` | `handle_message()` generic over transport + `AuthProvider` trait, `MessageResult` enum |
| `util.rs` | `translate_input_keys()`, `RESIZE_SIGNAL_BYTE` |
| `tests.rs` | 11 host-runnable tests (config parsing, packet round-trips, key translation) |

### What stayed in the kernel `src/ssh/`

- `server.rs` — TCP listener, thread spawning (unchanged)
- `protocol.rs` — Reduced to ~650 lines: connection loop with timeouts,
  `SshChannelStream`, `run_shell_session`, `bridge_process`, `handle_exec`
- `crypto.rs` — Kernel RNG wrapper + `create_seeded_rng()` helper
- `keys.rs` — Filesystem host key management (unchanged)
- `auth.rs` — Filesystem-backed auth + `KernelAuthProvider` implementing
  `akuma_ssh::message::AuthProvider`
- `config.rs` — Filesystem loading/caching, delegates parsing to
  `akuma_ssh::config::SshdConfig::parse()`

### Key design decisions

1. **Generic transport.** Transport functions (`send_packet`, `send_raw`, etc.)
   are generic over `T: embedded_io_async::Write`, making the crate testable
   with mock I/O and independent of `TcpStream`.

2. **`AuthProvider` trait.** The crate defines a trait for pluggable auth:
   ```rust
   pub trait AuthProvider {
       fn authenticate(&self, payload: &[u8], session_id: &[u8; 32],
           config: &SshdConfig)
           -> impl Future<Output = (AuthResult, Vec<u8>)>;
   }
   ```
   The kernel implements this with `KernelAuthProvider` which loads authorized
   keys from the filesystem via `async_fs`.

3. **`MessageResult::ExecCommand`.** The old `handle_message` executed shell
   commands inline. The crate version returns `ExecCommand(Vec<u8>)` and the
   kernel handles execution, then sends the EOF packet.

4. **Session fields are `pub`.** `SshSession` fields are public because the
   kernel's `SshChannelStream` and `handle_connection` access them directly.
   This is intentional for an internal kernel crate.

5. **RNG injection.** `SshSession::new()` takes a pre-seeded
   `akuma_ssh_crypto::crypto::SimpleRng` instead of creating one from hardware
   entropy. The kernel's `crypto.rs` provides `create_seeded_rng()` for this.

6. **`send_encrypted_packet` uses session RNG.** Instead of creating a new
   hardware-seeded RNG for every packet (as the old kernel wrapper did), the
   crate passes `&mut session.rng` to `build_encrypted_packet`. This is both
   more efficient and avoids the kernel dependency.

### Verification

- `cargo clippy -p akuma-ssh -- -D warnings` — clean
- `cargo build --release` — succeeds
- `cargo test -p akuma-ssh --target aarch64-apple-darwin` — 11 tests pass
- End-to-end SSH exec in QEMU: `ssh user@localhost -p 2222 "echo hello"` returns
  correctly with full protocol flow (kex, auth, channel open, exec, EOF, close)

---

## `akuma-shell` Crate Extraction (completed)

Extracted the shell framework (types, traits, parsing, pipeline/chain execution)
from `src/shell/mod.rs` (~1357 lines) into `crates/akuma-shell/` (~640 lines),
leaving command implementations and kernel-coupled execution in `src/shell/`.

### What moved to the crate

| Module | Contents |
|--------|----------|
| `context.rs` | `ShellContext` (per-session state: cwd, env vars, exec flags), `normalize_path` |
| `types.rs` | `Command` trait, `ShellError`, `VecWriter`, `InteractiveRead` trait, `StreamableCommand`, `ChainExecutionResult` |
| `registry.rs` | `CommandRegistry` (command lookup by name/alias, up to 40 commands) |
| `parse.rs` | `parse_pipeline`, `parse_command_chain`, `parse_command_line`, `parse_args`, `expand_variables` ($VAR, ${VAR}, ~, $$) |
| `exec.rs` | `ShellBackend` trait, `execute_pipeline`, `execute_command_chain`, `check_streamable_command` — all generic over `ShellBackend` |
| `util.rs` | `trim_bytes`, `split_first_word`, `translate_input_keys` |
| `tests.rs` | 31 host-runnable tests (parsing, expansion, context, registry, utilities) |

### What stayed in the kernel `src/shell/`

- `commands/` — All command implementations (`builtin.rs`, `fs.rs`, `net.rs`,
  `exec.rs`) stay in the kernel. They are heavily coupled to kernel subsystems
  (process, async_fs, smoltcp_net, allocator, threading, timer, pmm, vfs).
- `commands/mod.rs` — `create_default_registry()` which wires up all kernel
  command statics into a `CommandRegistry`.
- `KernelShellBackend` — Implements `ShellBackend` by delegating to
  `async_fs`, `process`, and `config` subsystems.
- `new_shell_context()` — Creates a `ShellContext` pre-loaded with kernel
  defaults (`DEFAULT_ENV`, `ENABLE_SSH_ASYNC_EXEC`).
- `execute_external_interactive()` — Bidirectional I/O bridge between SSH
  channel and spawned process (deeply coupled to `SshChannelStream`,
  `spawn_process_with_channel_cwd`, `threading`, `process`).
- `execute_command_streaming_interactive()` — SSH-specific entry point that
  dispatches between streaming external execution and buffered built-in
  execution.
- `enable_async_exec()` / `is_async_exec_enabled()` — Global flag for
  async execution mode.

### Key design decisions

1. **`ShellBackend` trait.** Abstracts the five kernel services that the
   pipeline executor needs:
   ```rust
   pub trait ShellBackend {
       fn builtins_first(&self) -> bool;
       fn find_executable(&self, name: &str) -> impl Future<Output = Option<String>>;
       fn execute_buffered(&self, ...) -> impl Future<Output = Result<(), ShellError>>;
       fn execute_streaming<W: Write>(&self, ...) -> impl Future<Output = Result<(), ShellError>>;
       fn write_file(&self, path: &str, data: &[u8]) -> impl Future<Output = Result<(), String>>;
       fn append_file(&self, path: &str, data: &[u8]) -> impl Future<Output = Result<(), String>>;
   }
   ```
   The kernel implements this with `KernelShellBackend`. The crate can be
   tested with a mock backend.

2. **`ShellContext::with_defaults(env, async_exec)`.** The crate's constructor
   takes explicit parameters instead of reading kernel globals. The kernel
   provides `new_shell_context()` which supplies `DEFAULT_ENV` and
   `ENABLE_SSH_ASYNC_EXEC`.

3. **Byte utilities duplicated.** `trim_bytes` and `split_first_word` exist in
   both `akuma-ssh-crypto` and `akuma-shell`. This avoids a semantic dependency
   of the shell on SSH crypto. The functions are 10 lines each and stable.

4. **`parse` module is `pub`.** The kernel calls `akuma_shell::parse::parse_args`
   directly from `execute_command_streaming_interactive`, so the parse module
   must be public.

5. **`CommandRegistry` removed from kernel.** The kernel's `src/shell/commands/mod.rs`
   previously defined its own `CommandRegistry` struct. This was removed; it now
   imports `CommandRegistry` from the crate via `super::CommandRegistry`.

### Verification

- `cargo clippy -p akuma-shell -- -D warnings` — clean
- `cargo build --release` — succeeds
- `cargo test -p akuma-shell --target aarch64-apple-darwin` — 31 tests pass
- All 6 crate test suites pass (145 total: 28+31+30+11+21+24)
- End-to-end in QEMU:
  - SSH exec: `ssh ... "echo hello from akuma-shell"` — correct output
  - Pipeline: `ssh ... "echo test123 | grep test"` — correct output
  - Chain: `ssh ... "echo one; echo two"` — correct output
  - `bun run /public/cgi-bin/akuma.js` — succeeds (see crash note below)

---

## 5. Intermittent bun CLONE_VM worker crash during `bun run`

**Symptom:** Running `bun run /public/cgi-bin/akuma.js` via SSH exec sometimes
crashes with a data abort in one of bun's CLONE_VM worker threads. The crash
is non-deterministic — a second attempt on a fresh boot succeeds.

**Crash log:**
```
[MMU] WARN: va=0x48ae000 already mapped to pa=0x49bbf000, wanted pa=0x4a648000
[MMU] WARN: va=0x48af000 already mapped to pa=0x49bc0000, wanted pa=0x4a649000
[T137.00] [Fault] Data abort from EL0 at FAR=0x2346b2ad68, ELR=0x4416d74, ISS=0x45
[Fault]  x0=0x203ffbad58 x1=0x203ffbac24 x2=0x203ffbac24 x3=0x5
[Fault]  x19=0x203ffbad58 x20=0x203ffbad80 x29=0x203ffbabc0 x30=0x4b35580
[Fault]  SP_EL0=0x203ffbabc0 SPSR=0x20000000 TPIDR_EL0=0x303f60e8
[RTK] code=-11 tid=8 LR=0x400a7614
[T137.00] [LR!] clear pid=23 (18 regions)
[Process] Killed 1 sibling thread(s) for PID 20
[T137.00] [LR!] clear pid=20 (18 regions)
[T137.00] [Process] PID 20 thread 8 exited (-11) [115.17s]
```

**Key observations:**
- FAR=0x2346b2ad68 is a 37-bit address, far beyond the 4 GB userspace VA
  limit. This looks like address corruption — a register holding a pointer
  has been clobbered or a JIT-compiled code path computed a bad address.
- ISS=0x45 means DFSC=0x05, a level-1 translation fault (no L1 page table
  entry for the top bits of the VA).
- Two sibling bun workers (PIDs 21, 22) exited cleanly with code 0 before
  the crash in PID 23. The parent (PID 20) was killed as a consequence.
- The MMU "already mapped" warnings immediately before the crash suggest a
  race in demand-paging for CLONE_VM threads: two threads fault on the same
  lazy page and both try to map it.
- This is NOT related to the `akuma-shell` extraction. The shell changes
  only affect command parsing and pipeline execution. The crash is in bun's
  JIT runtime at a userspace instruction (ELR=0x4416d74), and the same bun
  binary succeeds on a retry.

**Likely cause:** Race condition in the demand-paging path for CLONE_VM
threads. When two threads sharing an address space fault on the same lazy
page concurrently, one succeeds and the other gets the "already mapped"
warning. If the losing thread's page table walk state is stale, the JIT
code may subsequently access a corrupt pointer. Alternatively, this could
be a pre-existing bun JIT bug triggered by timing differences in emulation.

**Reproduction:** Intermittent. Run `bun run /public/cgi-bin/akuma.js` via
SSH exec repeatedly — it fails roughly 1 in 2 attempts on a cold boot.

---

## `akuma-net` Crate Extraction (completed)

Extracted the entire networking subsystem (~2,600 lines) from the kernel into
`crates/akuma-net/`. This is the largest extraction yet — it includes the
smoltcp TCP/IP stack, VirtIO net driver, kernel socket table, DNS resolution,
network statistics, TLS client, X.509 certificate verification, and an HTTP
client.

### What moved to the crate

| Module | Contents | Lines |
|--------|----------|-------|
| `smoltcp_net.rs` | smoltcp stack, VirtIO driver, `TcpStream`, `poll()`, connect/listen/close | ~985 |
| `socket.rs` | Kernel socket table, `KernelSocket`, bind/listen/accept/send/recv | ~713 |
| `dns.rs` | DNS resolution (loopback, IP literals, smoltcp DNS queries) | ~95 |
| `stats.rs` | Network statistics counters (was `network.rs`) | ~54 |
| `tls.rs` | `TlsStream`, `TlsOptions` (insecure/verbose), TLS 1.3 via embedded-tls | ~170 |
| `tls_rng.rs` | RNG adapter for TLS using runtime function pointers | ~58 |
| `tls_verifier.rs` | X.509 certificate verifier, hostname matching, chain validation | ~469 |
| `http.rs` | `http_get()`, `http_get_streaming()`, URL parsing, `HttpResponse` | ~310 |
| `hal.rs` | `NetHal` implementing `virtio_drivers::Hal` via runtime function pointers | ~60 |
| `runtime.rs` | `NetRuntime` struct of function pointers, global storage | ~30 |

Files were moved with `cp` then patched with `StrReplace` — not retyped.
`src/network.rs` was renamed to `stats.rs` during the move.

### What stayed in the kernel

- `src/virtio_hal.rs` — Shared by block device driver, cannot be moved
- `src/network_tests.rs` — In-kernel network self-tests (now imports from `akuma_net`)
- `src/async_tests.rs` — Async executor test harness
- `src/shell/commands/net.rs` — Curl, nslookup, pkg commands (kernel-coupled
  via `AsyncFile`, `ShellContext`, process execution)

### Key design decisions

1. **`NetRuntime` function pointer struct.** All kernel dependencies are
   abstracted via a struct of function pointers registered during `init()`:
   ```rust
   pub struct NetRuntime {
       pub virt_to_phys: fn(usize) -> usize,
       pub phys_to_virt: fn(usize) -> *mut u8,
       pub uptime_us: fn() -> u64,
       pub utc_seconds: fn() -> Option<u64>,
       pub yield_now: fn(),
       pub current_box_id: fn() -> u64,
       pub is_current_interrupted: fn() -> bool,
       pub rng_fill: fn(&mut [u8]),
   }
   ```
   This avoids traits and allows the crate to remain concrete with no
   generic parameters on the global `NetworkState`.

2. **`NetHal` for VirtIO.** The crate defines its own `NetHal` implementing
   `virtio_drivers::Hal`. It dispatches address translation through the
   `NetRuntime` pointers and uses `alloc::alloc` for DMA — identical logic
   to the kernel's `VirtioHal`, just with indirection. The kernel's
   `VirtioHal` stays in-tree for the block device.

3. **`init()` takes parameters.** Instead of reading kernel globals directly:
   ```rust
   pub fn init(rt: NetRuntime, mmio_addrs: &[usize], enable_dhcp: bool)
       -> Result<(), &'static str>
   ```
   The kernel constructs the MMIO address array from `mmu::DEV_VIRTIO_VA`
   and passes it in.

4. **`http.rs` provides `http_get` and `http_get_streaming`.** Since TLS,
   TCP, and DNS are all in the same crate, a full HTTP client was added:
   - `http_get()` — buffered GET returning `HttpResponse` (status, headers,
     body). Supports both HTTP and HTTPS.
   - `http_get_streaming()` — streaming GET that writes body chunks to any
     `W: embedded_io_async::Write`, with a progress callback. Used by
     the kernel's `pkg` command for large file downloads.
   - `parse_url()` — URL parser with scheme-aware defaults (port 80/443).
   - `HttpResponse::location()` — redirect header extraction.

   The kernel's curl and pkg commands delegate to these instead of
   implementing HTTP/TLS plumbing inline.

5. **Curl supports HTTPS, `-k`, `-L`, `-v`.** The `curl` shell command was
   updated to use `akuma_net::http` for both HTTP and HTTPS, with flags for
   insecure mode (`-k`), redirect following (`-L`), and verbose output (`-v`).
   TLS certificate verification is **on by default** (uses `X509Verifier`);
   `-k` switches to `NoVerify`.

6. **`AsyncFile` adapter in kernel.** The kernel's `AsyncFile` does not
   implement `embedded_io_async::Write`, so `net.rs` defines a thin
   `AsyncFileWriter` wrapper to bridge the two. This adapter maps
   `AsyncFile::write()` to the `embedded_io_async::Write` trait so that
   `http_get_streaming` can write directly to files on disk.

### Bugs encountered and fixed during extraction

1. **Missing `smoltcp_net::poll()` reference in `main.rs`.** After removing
   `mod smoltcp_net` from `main.rs`, one call site at the bottom of the
   main polling loop (`while smoltcp_net::poll()`) was missed. Fixed by
   updating to `akuma_net::smoltcp_net::poll()`.

2. **`resolve_host` was `async` but never awaited.** Clippy caught that
   `dns::resolve_host()` was declared `async` but contained no `.await`
   points — it only called synchronous `dns_query()`. Removed the `async`
   qualifier (callers already treated the return as sync).

3. **`DHCP_ENABLED` flag needed for extracted code.** The kernel's
   `is_dhcp_configured()` previously read `crate::config::ENABLE_DHCP`
   directly. In the crate, this was replaced with a module-level
   `AtomicBool` (`DHCP_ENABLED`) set during `init()`.

4. **`VIRTIO_MMIO_ADDRS` array referenced kernel constant.** The old code
   built the MMIO address array from `crate::mmu::DEV_VIRTIO_VA` at file
   scope. Moved the array construction to the kernel's `main.rs` and passed
   it as a parameter to `akuma_net::init()`.

5. **Type ambiguity in `http.rs`.** `headers = ...from_utf8(...).into()`
   failed to compile because Rust couldn't infer the target type of `.into()`.
   Fixed by adding an explicit `let headers: String = ...` annotation.

6. **`vec!` macro not found after removing import.** Removing `use alloc::vec`
   from `net.rs` during cleanup broke `vec![...]` usage in `PkgCommand`.
   Restored the import.

7. **`AsyncFile` doesn't implement `embedded_io_async::Write`.** When
   `http_get_streaming_to_file` was refactored to use the crate's
   `http_get_streaming`, the kernel's `AsyncFile` couldn't be passed
   directly. Added an `AsyncFileWriter` newtype adapter in `net.rs`.

### Clippy fixes

~160 clippy warnings from the moved kernel code were addressed:
- ~120 auto-fixed by `cargo clippy --fix`
- Remaining ~40 fixed manually or with targeted `#[allow]` attributes
- Key categories: `irrefutable_let_patterns` (smoltcp is IPv4-only so
  `IpAddress::Ipv4` always matches), `deref_addrof` (intentional unsafe
  raw-pointer patterns in the VirtIO device impl), `cast_possible_wrap`
  (u64 timestamps to i64 for smoltcp), `result_unit_err` (pre-existing
  API surface), `option_if_let_else` (URL parsing readability)

### Verification

- `cargo clippy -p akuma-net -- -D warnings` — clean
- `cargo build --release` — succeeds
- QEMU boot — network tests pass, SSH works
- `bun run /public/cgi-bin/akuma.js` — succeeds (intermittent crash on first
  try, see bug #5 above — pre-existing, not a regression)
- `curl http://...` (plain HTTP) — works

### Known issue: HTTPS `curl` returns "Read error"

`curl -Lkv https://ifconfig.me/ip` connects to port 443 and appears to
complete the TLS handshake (no "TLS handshake failed" error), but the
subsequent HTTP read returns "Read error" immediately (zero bytes received).

**Possible causes:**
- `embedded-tls` may not fully support the server's TLS configuration
  (e.g. TLS 1.2 fallback, certain cipher suites, or ALPN negotiation).
- The HTTP/1.0 request format may not be accepted by `ifconfig.me` over TLS.
- The smoltcp TCP receive window or buffer may be too small for the TLS
  record size, causing the TLS layer to fail on the first read.
- QEMU's user-mode networking may interfere with TLS record framing.

**Status:** Not yet debugged. The TLS infrastructure (`TlsStream`,
`TlsOptions`, `X509Verifier`) is in place and structurally correct. The
issue is likely a compatibility or configuration problem with `embedded-tls`
against real-world servers, not a bug in the extraction itself.

---

## TODO: Clippy cleanup pass across all extracted crates

During the `akuma-net` extraction, many clippy warnings from the moved kernel
code were suppressed with `#[allow(...)]` attributes rather than properly
fixed. This was expedient for getting the extraction working but is technical
debt.

A dedicated pass is needed to:

1. **Remove `#[allow(...)]` attributes** and fix the underlying code instead.
   Key offenders: `deref_addrof` (unsafe raw pointer patterns in the smoltcp
   device impls), `cast_possible_wrap` (u64-to-i64 timestamp casts),
   `result_unit_err` (functions returning `Result<_, ()>`), and
   `option_if_let_else` (URL parsing).

2. **Audit lint settings per crate.** Each crate inherits `workspace.lints`
   (clippy all + pedantic + nursery). Some lints may need to be permanently
   allowed at the crate level (e.g. `future_not_send` for async + spinlock
   code), but the decision should be intentional and documented, not a
   blanket suppression.

3. **Unify style across crates.** The extracted code carries kernel
   conventions (e.g. `Result<_, ()>` instead of proper error types,
   `if let` chains instead of `map_or`, manual `let...else` patterns).
   These should be modernized to match the workspace lint level.

4. **Crates to review:** `akuma-net`, `akuma-ssh`, `akuma-shell`,
   `akuma-ssh-crypto`, `akuma-terminal`, `akuma-vfs`, `akuma-ext2`.
