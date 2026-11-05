# Shanghai: composing middleware from external crates

**Status: partly built (2026-11-05).** Built and gated on both machines: the single table, its tiers, the exec hook, and the
BOOT half of the diagram (write-once command line, the narrowed `Params` view, `ParamSource`, `Hooks::init`; guide:
`crates/akuma-tokio-kernel-core/docs/middleware.md` § Boot parameters). The first crate, `akuma-mw-no-dynamic-loading`, is built and
used by Shanghai's `no-dynamic-loading` feature (two plain functions, no table, host-tested; it takes no parameters yet). **Not built:**
the signing crate, `compose`, the label. Shanghai's sample table plays the part of a crate with parameters (`mw.sample.also`).

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
- **Time of check (investigated 2026-11-05): signing the image buffer is not enough.** `load_task` is handed the whole file as a
  `&[u8]` (read once, so verifying *that* is race-free), but `place_segment` maps every whole-page segment page as a **lazy file
  record named by inode**, filled from the file at fault time; only the boundary pages are copied from the buffer. The inode pin
  (`InodePin`) stops the inode being *freed and reused*, not being *written*, and there is no `ETXTBSY` in the tree, so a file
  rewritten between verification and fault-in runs unverified bytes (and `akuma-fpcache` shares the faulted page across
  processes). Options, cheapest first: (a) the verdict carries "map from the verified buffer": the loader passes no inode to
  `place_segment`, every page is eager from the bytes that were hashed (costs the shared-page cache and RAM for signed images);
  (b) text-busy: refuse a write-open/truncate of an inode that has an exec pin (Linux's `ETXTBSY`; a VFS change in ext2, whose
  `is_pinned` check already exists); (c) per-page hashes verified at fault time (fs-verity style; much larger). **(a) is the
  smallest and it keeps the policy honest; it needs `ExecVerdict::Allow` to say so.** Not built.
- **Crypto (investigated 2026-11-05): nothing to write.** `akuma-ssh-crypto` is a wrapper over RustCrypto's `ed25519-dalek` and
  `sha2`, so there is no hand-rolled primitive to lift. `crates/akuma-ed25519` is the kernel's one door to it (verify only,
  `verify_strict`, no allocation, RFC 8032 vectors 1 and 2 pinned; builds for both `*-unknown-none` targets). Pure Ed25519 signs the
  message itself, so the signature covers the file bytes directly and no separate hash step is needed. Cargo warns that
  `curve25519-dalek` 4.1.3 and `sha2` 0.10.9 contain code a future Rust will reject; theirs, not ours, to watch.
- **Trust:** a key on the command line is chosen by whoever writes the line (VMM or bootloader). Compiled-in keys are the
  strong mode; command-line keys are a convenience.
