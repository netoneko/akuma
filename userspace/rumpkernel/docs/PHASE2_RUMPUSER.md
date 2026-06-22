# Phase 2 — Rust `rumpuser`: `rump_init()` is GREEN ✅

Status: **DONE.** A full NetBSD rump kernel boots to completion on our **Rust**
`rumpuser` (no NetBSD C `librumpuser`): `rump_init()` returns 0.

```
$ ./docker-rumpuser-test.sh      (links librump.a + librumpuser_akuma.a, runs rump_init)
NetBSD 7.99.34 (RUMP-ROAST)
total memory = unlimited (host limit)
RUMPUSER-AKUMA: rump_init() returned 0
RUMPUSER-AKUMA: PASS — NetBSD rump kernel booted on our rumpuser
```

## The bug chain to green (each fix peeled the next layer)

Found by tracing every hypercall (`--features rumpuser_debug`) + gdb in-container:

1. **SIGSEGV in early `uvm_init`** — rump's *optimized aarch64* `memset`
   (`rumpns_memset`, the DC-ZVA-style zero fast-path) miscomputed its loop bound
   for a small zero-fill and walked off the allocation. Confirmed via gdb: the
   call site passed the correct `(dest, 0, 368)`, but the routine itself ran
   away. **Fix:** override `rumpns_memset` with a trivial byte loop (csupport.c),
   linked with `-Wl,--allow-multiple-definition` so it wins over librump's.
2. **`panic: evcnt_attach_static: group length`** — same class: the optimized
   `strlen`/`memcpy`/… misbehave. **Fix:** byte-loop overrides for `rumpns_`
   `memcpy`/`memmove`/`strlen`/`strcmp`/`strncmp` too.
3. **`panic: assertion "rw_lock_held(&kauth_lock)" failed`** — our
   `rumpuser_rw_held` stub always returned "not held", failing the kernel's
   `KASSERT`. **Fix:** real reader/writer ownership tracking in `rumpuser_rw_*`
   (writer = current lwp, readers = shared count), mirroring NetBSD's librumpuser.

The optimized-libkern issue (1 & 2) is the notable one: the stock aarch64
`memset`/`strlen`/… in `librump.a` don't behave in our link/run environment. The
byte-loop overrides are a correct (if slow) stopgap. **Proper fix (later):** build
`librump` with the generic C libkern string/mem routines instead of the aarch64
assembly (investigate why the optimized ones run away — DC-ZVA / DCZID assumptions
or how buildrump assembled them), then drop the overrides.

That banner is printed **by the NetBSD kernel**, through our
`rumpuser_putchar`/`rumpuser_dprintf`, after `rump_init()` drove our
`rumpuser_init` / `getparam` / `malloc` / `anonmmap` / mutex / cv hypercalls.

## The `rumpuser` crate (`userspace/rumpkernel/rumpuser/`)

- **Rust, `no_std`**, `crate-type = ["staticlib"]` → `librumpuser_akuma.a`.
  Pure syscall/libc glue: no allocator, no std runtime, no deps (builds offline).
  Exports **59 `rumpuser_*` symbols** (`RUMPUSER_VERSION 17`).
- Init-critical families are real, backed by libc/pthread (exactly how NetBSD's
  own librumpuser works on Linux; on Akuma musl is itself backed by Akuma
  syscalls): memory (`posix_memalign`/`mmap`), clock (`clock_gettime`/`nanosleep`),
  randomness (`getrandom`), console (`putchar`; `dprintf` is the one C shim in
  `csupport.c` for the variadic), errno, params, threads (`pthread_create`/join),
  locks + cv (`pthread_mutex`/`rwlock`/`cond`), and `curlwp` via a pthread TLS key.
- Not-yet-needed families are safe stubs: block/file I/O (`bio`/`iov`/`syncfd`/
  `open`), syscall-proxy (`sp_*`), dynloader. Filled in as later phases need them.

## How it's built and linked (container, for now)

The staticlib is built on the host (no_std, no link step):
```sh
cd userspace/rumpkernel/rumpuser
cargo build --release --target aarch64-unknown-linux-musl   # → librumpuser_akuma.a
```
and linked + run in the Alpine arm64 container (`docker-rumpuser-test.sh`):
```sh
gcc -static -o test_init test_init.c csupport.c -I obj/dest.stage/usr/include \
    -Wl,--whole-archive -L obj/dest.stage/usr/lib -lrump librumpuser_akuma.a \
    -Wl,--no-whole-archive -lpthread
```
Notes learned: `-static` is required (the lib dir also has `librump.so`, which
`-lrump` would otherwise prefer → runtime "librump.so.0 not found"); and the
prebuilt Rust `core` references `rust_eh_personality` for unwinding tables even
under `panic=abort`, so `csupport.c` provides a no-op stub (never called). On
Akuma proper we'll instead rebuild core with `-Cpanic=immediate-abort` via
nightly `build-std`, like the rest of Akuma's userspace.

## Debugging method (for next time)

`--features rumpuser_debug` traces every hypercall (and memory sizes/pointers) to
stderr — that's what localised each crash to "the last hypercall before it." For
the in-routine memset runaway, gdb in the container pinned it: break at the
`memset` call site to read the *correct* args, then disassemble `rumpns_memset` to
see it take the zero fast-path and run away. Set `RUMP_VERBOSE=1` (the test does)
for rump's own boot prints.

## Carried workarounds (revisit)

- **`csupport.c` overrides** `rumpns_{memset,memcpy,memmove,strlen,strcmp,strncmp}`
  with byte loops, linked via `-Wl,--allow-multiple-definition`. Correct but slow;
  proper fix is to build `librump` with the generic C libkern routines.
- **`rust_eh_personality`** no-op stub (prebuilt core references it under
  `panic=abort`). On Akuma proper, rebuild core with `-Cpanic=immediate-abort`.

## Open question worth understanding

Why do the stock optimized aarch64 `memset`/`strlen`/… in `librump.a` run away in
our environment? (DC-ZVA / `DCZID_EL0` assumptions, or how buildrump assembled
them.) Not blocking — the overrides work — but worth a root-cause before shipping.

## After green

1. Bring up a `virtif` + DHCP + `rump_sys_*` socket in this same container test
   (still no Akuma), proving the TCP/IP path end-to-end against the host network.
2. Then port the link onto Akuma proper (libakuma syscalls; rebuild core with
   immediate-abort), and wire Phases 4–6 (our `rumpcomp_user` backend to
   `/dev/net/tap0`, the `rump-net` box payload, DHCP + curl = M1).
