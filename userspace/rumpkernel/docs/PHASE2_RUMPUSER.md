# Phase 2 — Rust `rumpuser`: the NetBSD rump kernel boots on our hypercalls 🎉

Status: **major milestone reached, one bug from green.** A NetBSD rump kernel
links against our **Rust** `rumpuser` (no NetBSD C `librumpuser`), starts
`rump_init()`, prints its banner, and runs into `uvm_init()` — i.e. real NetBSD
kernel code executing on hypercalls we wrote. It currently SIGSEGVs on one bad
allocation length during early VM init (details below). This is the
linking/boot proof the plan's Phase 2 was after.

## What runs

```
$ ./docker-rumpuser-test.sh      (links librump.a + librumpuser_akuma.a, runs rump_init)
Copyright (c) 1996, … 2016  The NetBSD Foundation, Inc.  All rights reserved.
Copyright (c) 1982, … 1993  The Regents of the University of California. …
NetBSD 7.99.34 (RUMP-ROAST)
total memory = unlimited (host limit)
<SIGSEGV in early uvm_init>
```

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

## The remaining bug (early uvm_init)

`gdb` backtrace at the crash:
```
#0 rumpns_memset
#1 kmem_intr_zalloc (kmflags=1, size=368)  subr_kmem.c:318  memset(p, 0, size)
#2 kmem_zalloc (size=368)
#3 uvm_init ()                              vm.c:399  rump_vmspace_local = kmem_zalloc(...)
#4 rump_init ()                             rump.c:286
#5 main ()                                  test_init.c:16
```
Registers at the fault: `x0` (dest) = `0xffff85e33000` (a **valid** mmap'd
pointer from our allocator), `x1` (val) = 0, **`x2` (len) = `0x3fffe16bdf0`
(~4.4 TB)**. So the pointer is fine; `memset` is called with a **garbage
length**. The frame says `size=368`, so the `-O2` librump backtrace is
mis-attributing the line — the real 4.4 TB-length `memset` is elsewhere in the
first kmem/pool/vmem bootstrap. `physmem` is a fixed constant (`emul.c:57`), so
it isn't that.

**Narrowed (instrumented `rumpuser_malloc`/`anonmmap` + filtered gdb):**
- Our `rumpuser_malloc` returns **valid** page-aligned blocks (`len=0x1000
  align=0x1000 ptr=0xffff86xxx000`), exactly like NetBSD's own librumpuser
  (which also uses `posix_memalign`). The pool pages are non-contiguous — normal.
- The faulting `memset` is the `kmem_zalloc(sizeof(struct vmspace)=368)` for
  `rump_vmspace_local` (vm.c:399); the chunk handed back is invalid for 368
  bytes, so `memset` walks off. (An earlier `initmsgbuf` 16 KB memset on static
  BSS is benign.)
- Suspect: how rump's bootstrap `kmem_arena = vmem_create("kmem", 0, 1024*1024,
  PAGE_SIZE, …)` (base 0, 1 MB, no import fn) and `kmem_va_arena` are backed —
  the VA→real-memory mapping for the first kmem allocations. Since our
  `rumpuser_malloc` matches the stock one, the divergence is likely a subtler
  hypercall our rumpuser gets wrong during vmem/pool bootstrap (a lock/cv/clock
  return convention, or a missing init the stock librumpuser does), not malloc
  itself.

**Bring-up tracing:** the `rumpuser_debug` cargo feature traces the memory
hypercalls (`cargo build … --features rumpuser_debug`).

**Next step (dedicated):** trace the kmem/vmem bootstrap path (`pool_subsystem_init`
→ `vmem_create`/`vmem_subsystem_init` → first `kmem_intr_alloc`) and compare the
hypercall sequence/return values against NetBSD's C librumpuser, to find the
mis-implemented `rumpuser_*` convention. Then move to the container virtif+DHCP
test, then Akuma integration.

## After green

1. Bring up a `virtif` + DHCP + `rump_sys_*` socket in this same container test
   (still no Akuma), proving the TCP/IP path end-to-end against the host network.
2. Then port the link onto Akuma proper (libakuma syscalls; rebuild core with
   immediate-abort), and wire Phases 4–6 (our `rumpcomp_user` backend to
   `/dev/net/tap0`, the `rump-net` box payload, DHCP + curl = M1).
