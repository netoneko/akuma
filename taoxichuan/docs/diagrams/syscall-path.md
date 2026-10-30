# The syscall path

Embedded inline in `../reference/syscalls.md`. Keep the two in the same commit.

```mermaid
sequenceDiagram
    participant U as task (ring 3)
    participant S as syscall stub (akuma-ring3-amd64)
    participant D as dispatch (main.rs)
    participant C as the crate that decides
    U->>S: syscall (rax=nr, args; CPU masks IF)
    Note over S: switch to the syscall stack,<br/>stash user rsp, build SyscallFrame
    S->>D: dispatch(&mut frame)
    D->>C: the arm asks its crate<br/>(fdtable / pipes / net / syscalls-* / ext2 …)
    C-->>D: decision
    D-->>S: u64 answer (errno = negated in rax)
    S-->>U: sysretq (rflags 0x202: IF set)
```
