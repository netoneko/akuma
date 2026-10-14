# Gentoo in a box: python crashes in C extensions — `_blake2` SIGSEGV, emerge SIGILL

> **Update 2026-10-15 — probably root-caused: the 512 KiB user stack.** amd64
> mapped exactly 128 pages of stack at exec and never grew it; a program that
> used more took a `#PF` with `cr2 == rsp` that nothing serviced. `wc -l` (a 256
> KiB on-stack buffer) died that way, and after the stack became demand-grown to
> 8 MiB, `hashlib.blake2b` and `emerge --version` — the two crashes recorded
> here — both work. The "C-extension crash" framing below was a symptom. Details:
> [`AKUMA_GENTOO.md`](AKUMA_GENTOO.md) § "Result 2026-10-15".

**Date:** 2026-10-14 (**crash SOLVED same day, see Resolution**; context in
[`AKUMA_GENTOO.md`](AKUMA_GENTOO.md)). **Where:** ryzen metal, entry 14
(`smp=8`, wifi), kernel at the `box-devfs` build (symlink walk + PT_INTERP
resolution + box `/dev`, all verified on-target). **Setup:** a complete
Gentoo stage3 (`stage3-amd64-musl-hardened-openrc-20261011T170049Z`,
sha512-verified) at `/opt/gentoo/root` on p3, booted by herd as a boxed
service; OpenRC runs its three runlevels and reaches `openrc done`; python
3.14.7, TCP, DNS and `/dev` all work inside the box.

**The one blocker between this and `emerge openssh`:** the emerge import
path dies in native code.

## Resolution (2026-10-14, verified on ryzen metal, entry 14)

**Root cause: AVX was advertised but never enabled.** The kernel set only
`CR4.OSFXSR`; `CR4.OSXSAVE`/`XCR0` stayed off while cpuid leaf 7 still
reported AVX2. CPython 3.14's HACL* `_blake2` reads cpuid leaves 1 and 7,
**never `xgetbv`** (`objdump`: 696 `ymm` uses, 0 `xgetbv`), so it ran AVX2 and
took `#UD`/`#GP` -> SIGILL / SIGSEGV. The prime suspects above (fpcache, SMP,
musl `dlopen`) were all wrong. Control that located it: the same stage3
chrooted from Pop on the same CPU runs `blake2b` and `emerge --version`
fine, so the bytes were good and the kernel was the variable.

Fix: `smp::enable_xsave()` (BSP in `early_init`, every AP) sets `OSXSAVE` and
`XCR0 = x87|SSE|AVX`; the context switch uses `xsave64`/`xrstor64` on an
832-byte area (`sched.rs::FxArea`, was 512) when `smp::XSAVE_ON`, else
`fxsave`. AVX-512 state is left off (conforming code checks `XCR0`).
Verified: `hashlib.new("blake2b", b"x")` -> `0909377ad35110ca`, identical to Linux.

**Second blocker it exposed:** `emerge` then stopped at "Failed to validate a
sane '/dev'": bash process substitution. Opening `/dev/fd/N` on a **pipe**
returned the link text `pipe:[id]` (read) or ENOENT (write). Fix in
`openat_path`: `/dev/fd/N`, `/dev/std*` and `/proc/self/fd/N` naming a pipe end
now dup it with its refcount (`own_proc_fd_number` learned the `/dev` spellings
because `resolve_symlinks` destroys the number). Verified: `cat <(echo sub)`,
`echo x > >(cat)`, and `emerge --version` -> `Portage 3.0.82.2`, rc=0.

**Still open:** the portage tree (`/var/db/repos/gentoo`) is not synced, then
`emerge openssh`. Not yet run.

## The crash, and what discriminates it

`emerge --version` (through the python-exec wrapper or with `PYTHONPATH`
set, script run directly) dies **deterministically — 5/5 — with rc=132
(SIGILL)**:

```
Fatal Python error: Illegal instruction
  File "/usr/lib/python3.14/hashlib.py", line 158 in __hash_new
  File "/usr/lib/python3.14/site-packages/portage/checksum.py", line 119 in <module>
  ... import chain: emerge:51 -> _emerge/main.py:15 -> repository/config.py:18 -> checksum
Extension modules: portage.dep._parser (total: 1)
```

Standalone probes, same box, same binary (all `rc` from the shell):

| probe | result |
|---|---|
| `hashlib.new("md5" / "sha1" / "sha224" / "sha256" / "sha384" / "sha512" / "sha3_256")` | **OK** — correct digests, rc=0 |
| `hashlib.new("blake2b")` | **rc=139 — SIGSEGV, deterministic** |
| `_hashlib.new("sha256", b"x")` (the OpenSSL module) | OK, correct digest |
| `import portage` (with `PYTHONPATH`) | OK — `PORTAGE-OK 3.0.82.2` |
| `os.listdir`, `mmap` of a file, `socket.connect` (TCP+TLS port) | all OK |
| `emerge --version` with `OPENSSL_ia32cap="0"` | **still SIGILL, rc=132** |

## What this rules out

- **CPU-feature dispatch in OpenSSL assembly.** The obvious first suspect
  (`sha512`/VAES paths picking instructions off `OPENSSL_ia32cap`) dies on
  three facts: every OpenSSL-backed algorithm works uncapped; capping the
  mask to zero changes nothing; and the builtin **`_blake2`** module — which
  is *not* OpenSSL at all — SIGSEGVs on its own.
- **The syscall ABI.** `akuma-syscalls-abi`'s pinned rows for everything
  python touches at startup (`openat` 257, `newfstatat` 262, `getdents64`
  217, `statx` 332, `readlinkat` 267, `mprotect` 10) are present and the
  exercised paths above all behave. (The question was asked because this
  tree has history here — x86_64 88 `symlink` once fell through to
  `utimensat`; nothing in this crash smells like it.)
- **The interpreter generally.** `--version`, `-c` one-liners, imports from
  site-packages, mmap'd file reads and sockets all work in the box.

## What it points at

Both crashes are **native, in or just after a `dlopen`ed C extension doing
its first real work** (`_blake2` directly; via `portage.dep._parser` — the
only extension listed loaded — for the emerge path). dlopen itself works:
`_hashlib.so`, `_parser.so` and every OpenSSL algorithm load and run. The
crash is one layer deeper: a mapped code/data page of a loaded shared
object, or a relocation result, being wrong exactly often enough to be
deterministic.

Prime suspects, in order:

1. **Shared file-page cache (fpcache) page identity for dlopened objects in
   a box.** fpcache keys pages on `(inode, mount id, offset)` and
   `read_at_by_inode` assumes an inode number names the same file across
   every lower (`containers.md` § OverlayFs). p3 is one filesystem, so box
   and host share inode space — but a *wrong or stale* page served as
   executable code is exactly "Illegal instruction", and a wrong data page
   is exactly "SIGSEGV on first module use". Check what the box's exec/fault
   path keys on for a `dlopen`-mapped `.so` (not the main binary — that is
   eager-loaded and works).
2. **The SMP state the dmesg is already grumbling about.** The same boot
   logged `[TRAMP-MISMATCH] tid=26 THREAD_PID_MAP=2486 but table scan found
   825` and long `[bkls>]` holds; a torn page or a half-installed mapping
   under 8 cores would be deterministic-per-code-path without being
   deterministic-per-data. Cheap A/B: boot entry 13's `smp=4` (or 1) and
   rerun the blake2b probe.
3. **musl `dlopen` relocation handling against akuma's `mmap`/`mprotect`
   semantics** for the `.so`'s `PT_LOAD`/RELRO layout — would be a
   user-visible kernel divergence, testable by `dlopen`ing the same `.so`
   from a trivial C program built with the on-metal musl toolchain.

Dead end already checked: `OPENSSL_ia32cap` in all its spellings.

## How to resume

```sh
# the box is still enabled and boots with the metal (herd service `gentoo`)
python3 overlays/ryzen/wifi-ssh.py --key <key> --net 192.168.1   # or ssh -p 2222
box use gentoo /bin/sh -c 'python3.14 -c "import hashlib; hashlib.new(\"blake2b\")"'
# rc=139 == still the bug; rc=0 == fixed, go straight to:
#   box use gentoo /bin/sh -c 'FEATURES=-sandbox MAKEOPTS=-j6 emerge --oneshot net-misc/openssh'
```

Probes from this session live in the box root (`/opt/gentoo/root/p*.sh`,
`s*.txt`, `h-*.txt`, `e*.txt`) — the box's `/` is plain p3, so every file is
readable from the host without entering the box. Note the **fd-offset
clobbering** finding below when reading multi-writer files.

## Two adjacent kernel findings this session surfaced (both documented here for the resumption)

1. **Spawned children do not share the inherited fd's file offset.** POSIX
   says an fd inherited across spawn refers to the same open file
   description — one shared offset. On akuma, a child writing to an
   inherited redirected file writes at its own offset, tearing any script's
   output log into interleaved fragments (observed as `ls: cannot acc` +
   `rc=2` + `uch file or directory` in one captured file). Pipes are
   unaffected (no offsets), which is why most things still work.
2. **p3's ext2 as written by akuma is not bit-compatible with Linux's
   checker.** The first extraction ran under akuma's busybox tar and lost
   ~360 scattered entries with no tar errors; Pop's `e2fsck -fy` then
   reported `HTREE directory index cleared` (×2), `invalid fast symlink
   length 60`, and an entry with `incorrect filetype (was 1, should be 7)`.
   The tree was re-extracted from Pop with GNU tar and verified **complete
   against the stage3's own `.CONTENTS` manifest (58 852 entries, 0
   missing)**. Worth an audit of akuma-ext2's directory-entry filetype byte
   and htree index writes before the next time a Linux fsck touches
   anything akuma wrote. Also: one spot on p3 returned `EUCLEAN`
   (`Structure needs cleaning`) on write from Pop even after a clean fsck —
   `/etc/ssl/certs/vTrus_Root_CA.pem` — which eventually wrote after a
   later fsck pass; watch that region.

## Background

- [`AKUMA_GENTOO.md`](AKUMA_GENTOO.md) — the session this came from: the
  download verification, the intermediate-symlink kernel fix, the ELF
  `PT_INTERP` fix, the box `/dev` instance, and the OpenRC bring-up.
- [`AKUMA_AMD64_LAPIC_ONESHOT_STALE.md`](AKUMA_AMD64_LAPIC_ONESHOT_STALE.md)
  — where the stage file and the box fixes came from.
- [`../reference/subsystems/containers.md`](../reference/subsystems/containers.md)
  — the box model, herd service schema, and the fpcache inode-identity
  assumption under OverlayFs.
