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

- [ ] `akuma-ssh-crypto` — currently has no logging (pure crypto, OK as-is)
- [ ] `akuma-terminal` — currently has no logging (pure data, OK as-is)
- [ ] `akuma-vfs` — check for any stray `safe_print!` or direct console use
- [ ] `akuma-ext2` — check for any stray `safe_print!` or direct console use
- [ ] Kernel registers a `log::Log` backend at boot (required for any of the
      above to actually produce output)
