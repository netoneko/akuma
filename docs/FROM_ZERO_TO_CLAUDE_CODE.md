# From `mov x0, #0` to Running Claude Code

Talk proposal: building every OS layer an AI coding tool needs, on bare metal, with AI assistance.

## Elevator Pitch

What does it take for an OS to write itself? Start with bare metal — no allocator, no scheduler, no filesystem, no network. Build each layer in Rust until you can run an AI coding assistant on it. Then use that assistant to debug the OS it's running on. This talk traces the full dependency chain of a bare-metal ARM64 kernel — memory management, preemptive threading, a TCP/IP stack, TLS, an SSH-2 server, terminal emulation, a process model — through the specific crashes that each layer produced, and the recursive loop where the AI building the OS became the OS's own resident tool.

## Dependency Tree

What does an AI coding tool actually need from an OS?

```
AI Coding Assistant (meow / Claude Code)
├── LLM API Client
│   ├── TLS 1.3 Handshake
│   │   └── TCP/IP Stack (smoltcp)
│   │       └── VirtIO-net Driver
│   │           └── MMIO + IRQ Handling (GICv2)
│   └── DNS Resolver
├── Tool Execution
│   ├── Filesystem (ext2, memfs, procfs)
│   │   └── VirtIO-blk Driver
│   ├── Process Spawning (fork, execve)
│   │   └── ELF Loader (static, PIE, dynamic)
│   │       └── Demand Paging + MMU
│   └── Shell / Command Execution
│       └── Pipes, Signals, Wait
├── Terminal UI
│   ├── Raw Mode + Cursor Control
│   │   └── Terminal Syscalls (307-313)
│   ├── SSH-2.0 Transport
│   │   ├── Curve25519 Key Exchange
│   │   ├── AES-128-CTR Encryption
│   │   └── Ed25519 Authentication
│   └── Non-blocking I/O
├── Scheduler
│   ├── Preemptive Multithreading (32 threads, 10ms slicing)
│   ├── Context Switching (ARM64 assembly)
│   └── Blocking Syscalls / Wait Queues
└── Memory Management
    ├── Physical Memory Manager
    ├── Heap Allocator (talc, 120MB)
    ├── Per-process Address Space Isolation
    └── Demand Paging + Lazy Regions
```

## Narrative Arc

### 1. The dependency tree nobody draws

Open with the diagram above. Most people who use AI coding tools daily have never considered the full stack beneath them. Then: "I built all of these. In Rust. On bare metal. With no underlying OS. And the AI helped."

### 2. Layer by layer, crash by crash

Walk up the stack, using the actual bugs each layer surfaced as narrative waypoints.

**Memory management.** Boot stack overlapping kernel code by 2MB ([BOOT_STACK_BUG](BOOT_STACK_BUG.md)). The heap allocator racing with IRQ handlers — `alloc` unprotected, `dealloc` protected, creating a window for corruption ([FAR_0x5_AND_HEAP_CORRUPTION_FIX](FAR_0x5_AND_HEAP_CORRUPTION_FIX.md)). Loading bun (93MB JavaScript runtime) and watching the user heap grow straight through the GIC interrupt controller at 0x0800_0000 ([BUN_MEMORY_STUDY](BUN_MEMORY_STUDY.md), [DEVICE_MMIO_VA_CONFLICT](DEVICE_MMIO_VA_CONFLICT.md)). Bun's JIT engine requesting 128GB of virtual address space. Demand-paged ELF loading to avoid consuming 93MB of physical memory upfront ([ON_DEMAND_ELF_LOADER](ON_DEMAND_ELF_LOADER.md)).

**Networking.** Embassy (Rust's embedded async runtime) worked for one SSH connection; the second corrupted the VirtIO ring ([SSH_THREADING_BUG](SSH_THREADING_BUG.md)). Priority inversion deadlocks when async tasks did synchronous filesystem I/O ([NETWORKING_DEADLOCK_INVESTIGATION](NETWORKING_DEADLOCK_INVESTIGATION.md)). Ripped out Embassy entirely, replaced with raw smoltcp behind a spinlock ([EMBASSY_REMOVAL](EMBASSY_REMOVAL.md), [STRATEGY_B_SMOLTCP_MIGRATION](STRATEGY_B_SMOLTCP_MIGRATION.md)). Post-migration: ephemeral port allocation, waker registration, DHCP deconfiguring mid-handshake, VirtIO receive buffers never posted ([SMOLTCP_MIGRATION_CHALLENGES](SMOLTCP_MIGRATION_CHALLENGES.md), [VIRTIO_RECEIVE_FIX](VIRTIO_RECEIVE_FIX.md)). TLS over the custom TCP stack ([TLS_INFRASTRUCTURE](TLS_INFRASTRUCTURE.md)).

**SSH and terminal.** Implementing SSH-2.0 from scratch: Curve25519 key exchange, Ed25519 host keys, AES-128-CTR ([SSH](SSH.md)). Echo latency at 36ms because `flush()` waited for TCP ACK and `block_on` yielded unconditionally — fixed down to <1ms ([SSH_ECHO_LATENCY_FIX](SSH_ECHO_LATENCY_FIX.md)). Delete key printing `~` because the escape sequence parser only handled single-character CSI ([SSH_TERMINAL_KEY_TRANSLATION_FIX](SSH_TERMINAL_KEY_TRANSLATION_FIX.md)). Rich TUI support: raw vs cooked mode, cursor control, screen clearing, scroll regions, all over SSH with no terminfo ([RICH_TERMINAL_INTERFACE_OVER_SSH](RICH_TERMINAL_INTERFACE_OVER_SSH.md)).

**Threading.** Context switch corruption from thread slot reuse races: cleanup zeroed a context after spawn had already initialized it ([CONTEXT_SWITCH_FIX_2026](CONTEXT_SWITCH_FIX_2026.md)). Lock-free thread state with atomic compare-exchange and the INITIALIZING state to close the race window ([LOCK_FREE_THREADING](LOCK_FREE_THREADING.md)). Round-robin starvation: scheduler always started from `current_idx + 1`, so threads at higher indices starved when SSH sessions at lower indices were active ([THREAD_SCHEDULING_INVESTIGATION](THREAD_SCHEDULING_INVESTIGATION.md)).

**The AI assistant itself.** meow runs as a `no_std` binary inside the kernel with a 16KB response cap, 32KB file read limit, and no libc ([MEOW](MEOW.md)). 800MB of lifetime allocation churn from `format!()` in TUI rendering — replaced with zero-allocation `write!()` ([REFACTOR_MEMORY_LEAK](../userspace/meow/docs/REFACTOR_MEMORY_LEAK.md)). Tool output overflow to disk at 32KB ([MEMORY_IMPROVEMENTS_SUMMARY](../userspace/meow/docs/MEMORY_IMPROVEMENTS_SUMMARY.md)). Guardrails that detect hallucinated `[Tool Result]` blocks and intent-without-action patterns ([GUARDRAILS](../userspace/meow/docs/GUARDRAILS.md)). Streaming truncation when `max_tokens` wasn't set; partial continuation with KV cache reuse for interrupted streams ([STREAMING_FIXES](../userspace/meow/docs/STREAMING_FIXES.md)). TUI freezing during model "thinking" because `TcpStream::read` blocked — fixed with non-blocking reads and input handling during wait ([TUI_RESPONSIVENESS_INVESTIGATION](../userspace/meow/docs/TUI_RESPONSIVENESS_INVESTIGATION.md)). Boot hangs when Ollama was running because `read_response` blocked indefinitely on a successful connection ([OLLAMA_BOOT_HANG_INVESTIGATION](../userspace/meow/docs/OLLAMA_BOOT_HANG_INVESTIGATION.md)).

### 3. The recursive loop

Every debugging session documented in the project's 100+ docs was itself an AI pair programming session. The `CLAUDE.md` context file and the `AI_DEBUGGING` workflow ([AI_DEBUGGING](AI_DEBUGGING.md)) are artifacts of this process. Claude helped diagnose FAR=0x5 (11 interrelated bugs). Claude planned the Embassy-to-smoltcp migration. Claude designed the device MMIO remapping.

The structure: AI coding tool → used to build OS → OS runs AI coding tool.

### 4. Live demo

SSH into Akuma. Show `top` (preemptive scheduler). Launch meow. Ask it to read a source file, search for a function, edit code, run a shell command. Every keystroke flows through: SSH-2 decryption → terminal state machine → kernel syscall → VFS/network → SSH encryption → screen. All on bare metal.

### 5. What I learned about AI tools by building the OS under them

- Modern AI tools are heavy. Bun alone needed demand paging, 256GB VA space, JIT cache coherency (`DC CVAU` + `IC IVAU`), and partial munmap with prefix/suffix/middle-split support.
- The "last mile" of making an AI assistant feel responsive — non-blocking TUI, streaming, echo latency — was harder than implementing the crypto or the TCP stack.
- AI is good at diagnosing hardware-level bugs when given the right context (exception syndrome registers, fault addresses, memory maps). It is not good at telling you what to build next.
- The forcing function of "can it run an AI coding tool?" drove more OS completeness than any test suite could.

## Key Docs Referenced

### Memory & ELF Loading
- [BOOT_STACK_BUG](BOOT_STACK_BUG.md) — boot stack overlapping kernel
- [MEMORY_LAYOUT](MEMORY_LAYOUT.md) — physical and virtual memory layout
- [HEAP_CORRUPTION_INVESTIGATION](HEAP_CORRUPTION_INVESTIGATION.md) — kernel heap corruption
- [FAR_0x5_AND_HEAP_CORRUPTION_FIX](FAR_0x5_AND_HEAP_CORRUPTION_FIX.md) — 11 interrelated memory/concurrency bugs
- [BUN_MEMORY_STUDY](BUN_MEMORY_STUDY.md) — bun crashes and memory fixes
- [DEVICE_MMIO_VA_CONFLICT](DEVICE_MMIO_VA_CONFLICT.md) — user heap vs device MMIO
- [ON_DEMAND_ELF_LOADER](ON_DEMAND_ELF_LOADER.md) — demand-paged ELF loading
- [LARGE_BINARY_LOAD_PERFORMANCE](LARGE_BINARY_LOAD_PERFORMANCE.md) — I/O inefficiencies loading 89MB binaries
- [ALLOCATOR_FIXES_AND_IMPROVEMENTS](ALLOCATOR_FIXES_AND_IMPROVEMENTS.md) — VA reclamation and chunked allocator
- [OOM_BEHAVIOR](OOM_BEHAVIOR.md) — out-of-memory failure modes

### Networking
- [SSH_THREADING_BUG](SSH_THREADING_BUG.md) — multi-session VirtIO ring corruption
- [NETWORKING_DEADLOCK_INVESTIGATION](NETWORKING_DEADLOCK_INVESTIGATION.md) — priority inversion deadlocks
- [EMBASSY_REMOVAL](EMBASSY_REMOVAL.md) — removing Embassy, 145-line kernel timer
- [STRATEGY_B_SMOLTCP_MIGRATION](STRATEGY_B_SMOLTCP_MIGRATION.md) — smoltcp migration design
- [SMOLTCP_MIGRATION_CHALLENGES](SMOLTCP_MIGRATION_CHALLENGES.md) — post-migration bugs
- [VIRTIO_RECEIVE_FIX](VIRTIO_RECEIVE_FIX.md) — VirtIO receive buffer fix
- [TLS_INFRASTRUCTURE](TLS_INFRASTRUCTURE.md) — kernel and userspace TLS
- [TLS_DOWNLOAD_PERFORMANCE](TLS_DOWNLOAD_PERFORMANCE.md) — HTTPS download performance

### SSH & Terminal
- [SSH](SSH.md) — SSH-2 server implementation
- [SSH_ECHO_LATENCY_FIX](SSH_ECHO_LATENCY_FIX.md) — echo latency from 36ms to <1ms
- [SSH_TERMINAL_KEY_TRANSLATION_FIX](SSH_TERMINAL_KEY_TRANSLATION_FIX.md) — escape sequence parser
- [SSH_STREAMING_ARCHITECTURE](SSH_STREAMING_ARCHITECTURE.md) — output streaming design
- [RICH_TERMINAL_INTERFACE_OVER_SSH](RICH_TERMINAL_INTERFACE_OVER_SSH.md) — TUI syscalls and raw mode
- [INTERACTIVE_IO](INTERACTIVE_IO.md) — bidirectional I/O over SSH

### Threading & Concurrency
- [MULTITASKING](MULTITASKING.md) — scheduler and thread model
- [CONTEXT_SWITCH_FIX_2026](CONTEXT_SWITCH_FIX_2026.md) — context switch corruption
- [LOCK_FREE_THREADING](LOCK_FREE_THREADING.md) — atomic thread state
- [THREAD_SCHEDULING_INVESTIGATION](THREAD_SCHEDULING_INVESTIGATION.md) — round-robin starvation
- [THREADING_RACE_CONDITIONS](THREADING_RACE_CONDITIONS.md) — CURRENT_THREAD races
- [CONCURRENCY](CONCURRENCY.md) — lock hierarchy and synchronization

### AI Assistant (meow)
- [MEOW](MEOW.md) — meow overview and architecture
- [AI_DEBUGGING](AI_DEBUGGING.md) — AI-assisted debugging workflow
- [GUARDRAILS](../userspace/meow/docs/GUARDRAILS.md) — hallucination detection
- [STREAMING_FIXES](../userspace/meow/docs/STREAMING_FIXES.md) — streaming truncation and continuation
- [REFACTOR_MEMORY_LEAK](../userspace/meow/docs/REFACTOR_MEMORY_LEAK.md) — 800MB allocation churn
- [MEMORY_IMPROVEMENTS_SUMMARY](../userspace/meow/docs/MEMORY_IMPROVEMENTS_SUMMARY.md) — tool output overflow to disk
- [TUI_RESPONSIVENESS_INVESTIGATION](../userspace/meow/docs/TUI_RESPONSIVENESS_INVESTIGATION.md) — blocking reads during thinking
- [OLLAMA_BOOT_HANG_INVESTIGATION](../userspace/meow/docs/OLLAMA_BOOT_HANG_INVESTIGATION.md) — boot hang with Ollama
- [TOOLS](../userspace/meow/docs/TOOLS.md) — tool execution spec
- [UI_REFACTOR_2026](../userspace/meow/docs/UI_REFACTOR_2026.md) — TUI overhaul

### Process Model & Syscalls
- [UNIFIED_PROCESS_ABI](UNIFIED_PROCESS_ABI.md) — Linux-compatible process ABI
- [MUSL_COMPATIBILITY](MUSL_COMPATIBILITY.md) — musl libc support
- [SYSCALL_HARDENING](SYSCALL_HARDENING.md) — pointer validation, Linux ABI alignment
- [BUN_MISSING_SYSCALLS](BUN_MISSING_SYSCALLS.md) — syscalls added for bun

### Architecture
- [ARCHITECTURE](ARCHITECTURE.md) — high-level kernel architecture

## Talk Metadata

- **Audience:** Systems programmers, Rust developers, OS/embedded enthusiasts, anyone curious about what AI tools need from their runtime
- **Difficulty:** Intermediate — the layer-by-layer structure is accessible without kernel experience; the specific crashes and fixes provide depth for experienced OS developers
- **Duration:** 40-45 minutes
- **Demo requirements:** QEMU, SSH client, Ollama running on the host
