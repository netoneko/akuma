# Diagrams

Canonical mermaid source for the five figures the doc set keeps true (see
`../README.md` § "Figures every session must keep true"). Each is one `.md`
file with a single fenced `mermaid` block; the same block is embedded inline
in the reference doc that owns it. **Change both in the same commit** — or
better, edit here and paste into the reference doc.

| figure | source | embedded in |
|---|---|---|
| Boot to init | [boot-to-init.md](boot-to-init.md) | `reference/boot.md` |
| The syscall path | [syscall-path.md](syscall-path.md) | `reference/syscalls.md` |
| Process model | [process-model.md](process-model.md) | `reference/processes.md` |
| Scheduling | [scheduling.md](scheduling.md) | `reference/scheduling.md` |
| Memory map | [memory-map.md](memory-map.md) | `reference/memory.md` |

The pipes figure is small and lives only in `reference/pipes.md`.
