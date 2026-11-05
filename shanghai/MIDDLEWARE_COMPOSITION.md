# Shanghai: composing middleware from external crates

**Status: partly built (2026-11-05).** Built and gated on both machines: the single table, its tiers, the exec hook, and the
BOOT half of the diagram (write-once command line, the narrowed `Params` view, `ParamSource`, `Hooks::init`; guide:
`crates/akuma-tokio-kernel-core/docs/middleware.md` § Boot parameters). **Not built:** the two crates, `compose`, the label.
Shanghai's sample table currently plays the part of both crates' parameters (`mw.sample.also`).

Rules this design keeps: one table per kernel (no chaining), hooks cannot mutate the kernel context, every ban is a feature
and off by default, no runtime switches.

```
 BOOT (single-threaded, before any syscall)
 ──────────────────────────────────────────
   cmdline: "init=/bin/herd smp=4 mw.signed.key=ab12..(64 hex) mw.nodl.log=1"
                    │
                    ▼
   machine boot ──► install(&MW, machine) ──► arm()
                                                │
                       Params view, one namespace per crate
                          ┌─────────────────────┴───────────────────┐
                          ▼                                         ▼
               Params("mw.nodl.")                       Params("mw.signed.")
               sees only: log=1                         sees only: key=ab12..
                          │                                         │
                          ▼                                         ▼
                 nodl::init(&p, &mut S)                 signed::init(&p, &mut S)
                          └──────────────► shanghai State { nodl, signed } ◄──┘
                          (`init=` and `smp=` are invisible to both)


 SHANGHAI = THE ONE TABLE (composition lives here, nothing is chained)
 ─────────────────────────────────────────────────────────────────────
   shanghai/src/middleware.rs
   ┌────────────────────────────────────────────────────────────────────┐
   │ Hooks {                                                            │
   │   kernel_pre : match Pre  { Execve => nodl::exec_guard(..),        │
   │                             Openat => own sample .. }              │
   │   exec       : |ev| compose(nodl::policy, signed::policy)          │
   │ }                                                                  │
   └────────────────────────────────────────────────────────────────────┘
        ▲ calls functions from                       ▲
        │                                            │
 ┌──────┴─────────────────────┐          ┌───────────┴──────────────────┐
 │ crate akuma-mw-no-dynamic- │          │ crate akuma-mw-signed-exec   │
 │ loading                    │          │   uses akuma-ed25519         │
 │  exec_policy(ev)->Verdict  │          │  policy(ev,&State)->Verdict  │
 │  exec_guard(ctx)->Verdict  │          │    Allow(label) | Deny(errno)│
 └──────┬─────────────────────┘          └───────────┬──────────────────┘
        └───────────── both depend only on akuma-tokio-kernel-core ─────┘


 AN execve("/bin/foo") THROUGH IT
 ────────────────────────────────
   user ── syscall execve ──► machine dispatcher
                                   │
                         ┌─────────▼──────────┐
                         │ kernel_pre (Execve)│  nodl::exec_guard: LD_* in env?
                         └─────────┬──────────┘     └─ yes ──► EPERM, stop
                                   │ no
                         ┌─────────▼──────────┐
                         │ kernel runs execve │
                         │  loader::load_task │
                         └─────────┬──────────┘
                                   │ elf crate (build-time, if no-dynamic-elf):
                                   │   dynamic image ──► EACCES, stop
                                   │
                   exec event: ExecKind::Image { path, inode, interp }
                                   │
                         ┌─────────▼───────────────────────────────┐
                         │ compose, first Deny wins                │
                         │  1. nodl::exec_policy  ─ Deny(EACCES)?  │──► stop
                         │  2. signed::policy     ─ trailer ok?    │
                         │        sha512(bytes before trailer)     │
                         │        ed25519 verify with State.key    │
                         │        cache by inode ──► Allow(label)  │
                         └─────────┬───────────────────────────────┘
                                   │ Allow(label)
                         ┌─────────▼──────────┐
                         │ task record        │  label: u64, REPLACED on exec,
                         │   .label = label   │  copied on fork, shared by threads
                         └─────────┬──────────┘
                                   ▼
                   later calls: KernelCtx<Label> ──deref──► KernelBase
                   (cast down is free, a plain middleware only sees KernelBase)


 LATER EXEC EVENTS FROM THE SAME PROCESS
 ───────────────────────────────────────
   mmap(file, PROT_EXEC)  ─► ExecKind::Map     ─► nodl: Deny(EACCES)
   mprotect(+X)           ─► ExecKind::Protect ─► nodl: Deny(EACCES)
                                                   signed: not asked (no binary)
```

## Decisions and open points

- **`exec` takes one policy**, so Shanghai wraps the two in a `compose` function; a deny from either wins. nodl runs first
  because it is cheap; signing hashes only images that survive it.
- **Each crate sees only its own `mw.<name>.` namespace** of the command line.
- **One state type per table**, so Shanghai defines `State { nodl, signed }` and each crate exposes its own part.
- **Only the signed crate produces a label**; map and protect events have no binary, so no label.
- **Open:** how a label from one policy combines with the plain `Allow` of another inside `compose` (simplest: the label
  comes from whichever policy returns one). Whether `compose` should live in Shanghai or as a helper in the core crate.
- **Settled: the command line's size.** The 512 bytes were our own stack buffers, not a PVH limit (the protocol gives only a
  NUL-terminated string at `cmdline_paddr`). The kept line is now 2048 bytes and a longer one is refused whole
  (`StartInfo::cmdline_complete`), never truncated. Still unchecked: whether QEMU or Firecracker cap the line before the kernel
  sees it.
- **Trust:** a key on the command line is chosen by whoever writes the line (VMM or bootloader). Compiled-in keys are the
  strong mode; command-line keys are a convenience.
