# Taoxichuan: W^X without a JIT (2026-10-30)

**Rule.** No page is both writable and executable, and a page that has been written since it was mapped can never be made
executable. Both refusals answer `EACCES`. Stricter than Linux on purpose (Linux allows both); a JIT (V8, a JVM, LuaJIT) cannot run
under it. Escape hatch: `wx=0` on the kernel command line restores the old behaviour (`PROT_EXEC` widens a mapping to RWX).

**Correction to the request that asked for it.** "taoxichuan accepts and ignores `mprotect`" was out of date: `mprotect` re-maps the PTEs
(only `PROT_NONE` stays a no-op) and has since the CoW work. What was missing was a *policy*: `mmap` widened any `PROT_EXEC` request to
RWX, and nothing looked at what a page had been used for.

## Mechanism
* **Decision** (pure, host-tested): `akuma_syscalls_mem::wx` - `check_prot` (W+X together), `check_page` (X on a written page),
  `check_range` (the whole `mprotect` range first, stopping at the first offender, so a refusal changes nothing). 5 tests.
* **"Written" is the x86 PTE dirty bit** (`PTE_D`, bit 6): the CPU sets it on a user store; the kernel sets it on its own stores into user
  pages (`copy_to_user` -> `mark_written`), so `read(fd, buf)` into a buffer counts, and so does `mremap`'s move (it copies through
  `copy_to_user`). Kernel-side fills at `mmap` time go through the physmap alias and leave the user PTE clean: a loader-mapped text
  page is never "written".
* **The remap in `mprotect_arm` builds a fresh PTE**, which would drop the bit; it reads "written" before the remap and restores it after
  (probe case "written, then R, then RX refused").
* `mmap`: `W|X` refused before any address is allocated; a `PROT_EXEC` mapping is RX (not RWX). `mprotect`: `W|X` refused; `X` refused if
  any page in the range is written; checked for the whole range before any page changes.

## Verification
`scripts/kvm/goose/wx_probe.c` (compile with the guest's `tcc`, run it NOT as the last command of `sh -c`: a crash in the shell's own
process halts the machine, an old documented rule). 11 checks, all PASS on TCG: RWX mmap refused; untouched RW -> RX allowed; user-written -> RX
refused (and still fine as data); kernel-written (`read`) -> RX refused; written -> R allowed then RX refused; mprotect RWX refused;
anonymous RX mmap allowed; a range containing one written page is refused and the other page stays writable. With `wx=0` the RWX mmap succeeds.
Battery (dynamic `git`, `apk`, shells under musl's loader) and `cow_leak_gate.py` (delta 0) green with the policy on.

## What it costs / what it breaks
* A program that maps RW, writes code, and flips to RX (a JIT; text-relocating dynamic objects) is refused. musl's loader and
  rustc/cargo/git/apk, which map text from the file, are unaffected (the build and battery pass).
* A shared library whose single `PT_LOAD` is RWX is refused at `mmap`.
* `mmap(PROT_READ)` still maps the page writable in the PTE (the kernel fills pages and has never enforced read-only at map time);
  W^X holds anyway because the first user store marks the page written before it could become executable.

Related: [bugs found 2026-10-30](TAOXICHUAN_BUGS_FOUND_2026_10_30.md).
