# tcc

Tiny C Compiler, ported for in-kernel self-hosting (compiling C inside Akuma).

Docs live at [`userspace/tcc/docs/`](../../userspace/tcc/docs/):
- `IMPLEMENTATION_PLAN.md`, `IMPLEMENTATION_DETAILS.md`.
- `DISTRIBUTION_PLAN.md` — packaging tcc for the disk image.
- `LIBTCC1.md` — the `libtcc1.a` runtime support library.

See also: `archive/TCC_LOW_MEMORY.md`,
[`../runbooks/selfhost-kernel-build.md`](../runbooks/selfhost-kernel-build.md).

## amd64

Ported to `x86_64-unknown-none` 2026-09-04 — `userspace/tcc/src/amd64_shim.rs`
stands in for `libakuma` (which does not build for that target), and
`build.rs` branches on the target triple for `TCC_TARGET_X86_64` vs
`_ARM64` and an x86_64 `setjmp`/`longjmp`. Built and staged by
`amd64/mkdisk.sh` the same way as `paws`/`httpd`/`sshd`.

A real musl static libc followed the same day: `mkdisk.sh` unpacks
`usr/lib`/`usr/include` from the same Alpine `musl-dev` apk `build.rs` already
downloads for tcc's own build headers (the identical package `apk add
musl-dev` installs on the AArch64 image) — so `-static` there means a real
libc now, and the AArch64 acceptance tests' own `tcc -static -B /usr/lib/tcc
-o /tmp/hello_c /tmp/hello.c` on their own `hello.c` runs unmodified, printing
`Hello, Akuma!`. See `docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.27–§3.28 for
the full account.
