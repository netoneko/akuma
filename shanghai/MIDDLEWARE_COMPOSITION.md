# Shanghai: composing middleware from external crates

**Status: mostly built (2026-11-05).** Built and gated on both machines: the single table, its tiers, the exec hook, the
exec-policy composition (Shanghai's `exec_policy`: a refusal from any crate wins, otherwise the strongest allow), signed code only
(`akuma-mw-signed-exec`, feature `signed-exec`, below), and the BOOT half of the diagram (write-once command line, the narrowed `Params` view, `ParamSource`, `Hooks::init`; guide:
`crates/akuma-tokio-kernel-core/docs/middleware.md` § Boot parameters). The first crate, `akuma-mw-no-dynamic-loading`, is built and
used by Shanghai's `no-dynamic-loading` feature (two plain functions, no table, host-tested; it takes no parameters). The second, `akuma-mw-signed-exec`, takes its trust lists from
`mw.signed.*`. **Not built:** the label (a per-task record of which binary a process is), minisign/signify sidecar formats, a verified-hash
cache by inode. The detail below describes the original proposal; where it differs from this paragraph, this paragraph wins: the
policy is hashes first and a detached `<path>.sig` second (not an ELF trailer), and `compose` is Shanghai's own function.

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
  smallest and it keeps the policy honest; it needs `ExecVerdict::Allow` to say so.** **Built as `ExecVerdict::AllowFromImage` (same day).** The loader
  then passes no inode to `place_segment` and does not resolve the path again (a `rename` between the read and the load would have named
  another inode). Proved by a probe child that rewrites its own file while running (`selfclobber`): on the stock kernel it dies with
  SIGSEGV (status 11); on the `signed-exec` kernel, on both machines, it survives. A path swap between read and load is closed by the
  same change but has no probe of its own.
- **Crypto (investigated 2026-11-05): nothing to write.** `akuma-ssh-crypto` is a wrapper over RustCrypto's `ed25519-dalek` and
  `sha2`, so there is no hand-rolled primitive to lift. `crates/akuma-ed25519` is the kernel's one door to it (verify only,
  `verify_strict`, no allocation, RFC 8032 vectors 1 and 2 pinned; builds for both `*-unknown-none` targets). Pure Ed25519 signs the
  message itself, so the signature covers the file bytes directly and no separate hash step is needed. Cargo warns that
  `curve25519-dalek` 4.1.3 and `sha2` 0.10.9 contain code a future Rust will reject; theirs, not ours, to watch.
- **Trust:** a key on the command line is chosen by whoever writes the line (VMM or bootloader). Compiled-in keys are the
  strong mode; command-line keys are a convenience.

## What is built: `signed-exec` (2026-11-05)

`akuma-mw-signed-exec`, Shanghai feature `signed-exec` (implies `middleware`, off by default). An image runs only if one of these holds,
and every other way of making code is refused with `EACCES`:

- **Its SHA-256 is allowlisted for its path:** `mw.signed.allow=<64 hex>:<scope>` (repeatable; a scope is an exact absolute path, or a
  directory prefix ending in `/`). The path is the symlink-resolved one the loader read, so a hash for `/bin/busybox` covers its applets.
- **A detached `<path>.sig` verifies:** 64 raw bytes, Ed25519 over the whole file, under a key scoped to the path:
  `mw.signed.key=<64 hex public key>:<scope>[,<scope>...]` (repeatable). The loader reads the sidecar (at most 4 KiB, only when the table
  asks for one) and passes its bytes in the event, so the policy never touches the filesystem; a swapped `.sig` cannot make other bytes
  verify, because the signature is checked against the exact `image` the policy was shown.
- **Refused regardless:** a dynamically linked image (its interpreter and libraries are not covered), a file-backed `PROT_EXEC` mapping,
  `mprotect(+X)`, a path that is not absolute and clean (`..`, `//`, relative).
- **A malformed or over-long trust list makes the policy refuse every image**, with the reason on the console. No `mw.signed.*` word at
  all: nothing runs, init included.
- **The verdict is `AllowFromImage`:** the image is mapped from the bytes that were checked (see the time-of-check note above).

Stock binaries need no change: a hash allowlist entry names the installed file, and a `.sig` can be made by whoever builds the image
with an Ed25519 key (the gate uses OpenSSL 3: `openssl pkeyutl -sign -rawin`). Signatures published by upstream projects cover their
release artifacts, not the installed file, and apk's are RSA over the package, so they are not directly checkable here; minisign and
signify are plain Ed25519 and would be the next sidecar formats.

**Gate (`sh shanghai/mw_gate.sh all`, SMP=4, 2026-11-05, after `ETXTBSY`):** 206 PASS lines, both machines: control 17, `middleware` 23,
`no-dynamic-loading` 39, `no-dynamic-elf` 6, `signed-exec` 18 each (172 before `ETXTBSY` added the T cases to the control and signed kernels). The ten signed cases: an allowlisted hash runs (S1); an unlisted
image, tampered bytes under a valid signature, a valid signature outside the key's scope, and the allowlisted bytes at another path are all
refused (S2, S4, S5, S6); a signature under a scoped key runs (S3); a dynamic image, a file-backed executable mapping and `mprotect(+X)`
are refused (S7, S8, S9); and a signed child that tries to rewrite its own file is refused with `ETXTBSY` and keeps running (S10). **S10 originally proved the
map-from-checked-bytes claim** (the child's write landed; stock kernel: SIGSEGV, signed kernel: survived); once `ETXTBSY` made the write
impossible that proof no longer runs, and `AllowFromImage`'s remaining job (the read-then-load path swap, a write that bypasses `write(2)`
such as a writable shared mapping made before the exec) has no probe of its own. See `docs/archive/ETXTBSY_TEXT_BUSY.md`.

**Costs and limits, stated.** One SHA-256 over the image on every exec (and an Ed25519 verify per matching key when the hash is not
listed); not measured, and a cache by inode is not built. Signed images lose the shared file-page cache. A script run by a signed
interpreter is data, not code, and a signed interpreter runs whatever it is given. The command-line trust list is chosen by whoever
writes the command line: compiled-in keys are the strong mode and are not built. Not tried on KVM or Firecracker.

