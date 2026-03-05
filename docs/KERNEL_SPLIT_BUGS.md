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
