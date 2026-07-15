# libakuma

The musl-based C runtime support library shared by userspace binaries:
syscall wrappers, allocator, terminal helpers.

Docs live at [`userspace/libakuma/docs/`](../../userspace/libakuma/docs/):
- `SYSCALLS.md` — the userspace syscall wrapper surface.
- `ALLOCATOR_OPTIONS.md` / `ALLOCATOR_MEMORY_FIX.md` — page-based vs
  `chunked-allocator` (Talc) modes.
- `TERMINAL_SYSCALLS.md`, `POLL_INPUT_EVENT_FIX.md`, `MKDIR_P_IMPROVEMENTS.md`.

See also: [`../reference/subsystems/memory.md`](../reference/subsystems/memory.md) "libakuma allocator".
