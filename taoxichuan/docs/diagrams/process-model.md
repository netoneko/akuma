# Process model — the SPAWN crossing

Embedded inline in `../reference/processes.md`. Keep the two in the same
commit.

```mermaid
sequenceDiagram
    participant P as parent (sshd, ring 3)
    participant K as kernel syscall path (IF=0)
    participant C as child (ring 3)
    P->>K: SPAWN(path, argv, envp, …)
    Note over K: marshal args; claim spawn-ring slot + pid;<br/>load_task on the PARENT's CR3 (all writes via physmap)
    Note over K: slot record ← parent CR3, syscall-stack top,<br/>user-rsp stash, depth=1, out_slot=slot
    K->>K: set_current_slot(slot); lower syscall-stack top by half
    K->>C: child_enter (naked): save parent callee-saved regs + rsp<br/>into the slot's CrossSave, mov cr3, sysretq
    loop child syscalls
        C->>K: syscall → dispatch on the nested stack band
        K-->>C: sysretq with the answer
    end
    C->>K: exit_group(code)
    Note over K: child_exit: record status (code<<8) in the slot,<br/>release the task's pipe ends, set_current_slot(parent)
    K->>K: exit_cross (naked): restore regs from CrossSave,<br/>mov cr3 (parent), mov eax, status, ret
    K-->>P: SPAWN returns pid | stdout_fd<<32
```
