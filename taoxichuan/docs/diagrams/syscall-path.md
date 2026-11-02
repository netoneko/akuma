# The syscall path

Embedded inline in `../reference/syscalls.md`. Keep the two in the same commit.

```mermaid
sequenceDiagram
    participant U as task (ring 3)
    participant S as syscall stub (akuma-ring3-amd64)
    participant D as dispatch (lib.rs)
    participant T as akuma-runtime-tokio
    participant G as glue / glue-amd64
    U->>S: syscall (rax=nr, args; CPU masks IF)
    Note over S: switch to the syscall stack,<br/>stash user rsp, build SyscallFrame
    S->>D: dispatch(&mut frame)
    Note over D: take the big kernel lock;<br/>fold the legacy x86 numbers
    D->>T: dispatch(nr, args)
    T->>G: allow-listed: glue by asm-generic number<br/>else: the x86_64 fd family (glue-amd64)
    G-->>D: Some(answer), or None
    Note over D: None: this kernel's own arms<br/>(memory, clone/exec/exit, signals)
    Note over D: deliver pending signals,<br/>restore the user-rsp stash, drop the lock
    D-->>S: u64 answer (errno = negated in rax)
    S-->>U: sysretq (rflags 0x202: IF set)
```
