# Investigation: net bounce buffer OOM → whole-kernel `brk #1` abort

## Date

2026-06-09 (branch `perf-improvements`, **not yet committed**)

## Status

- ✅ Crash analyzed; mechanism identified from `64mb_llama.cpp_0.log`.
- ✅ Root cause: net syscalls' 64 KB (16-page) bounce buffer is an **infallible**
  kernel-heap allocation that aborts the whole kernel when the heap can't grow a
  contiguous run under PMM exhaustion.
- ✅ Fixed in `src/syscall/net.rs` (`alloc_net_bounce`): fallible alloc +
  single-page fallback + ENOMEM, never aborts. Boot self-test added.
- ✅ Verified: builds dev/`size`/`extreme-size`; boots clean to SSH on a copied
  disk; `[PASS] test_net_bounce_alloc_degradation`; zero `EC=0x3c`.
- 🔲 **Open (broader):** the net buffer was only *one* unprotected multi-page
  kernel allocation. The general fix — an OOM-killer hook for *any* failed
  multi-page kernel growth — is still a TODO in `src/allocator.rs`. See
  "Remaining work".
- ⚠️ **Re-test pending (qwen3.5-0.8B @ "2 GB"):** a `brk #1` at the **same**
  `ELR=0x4012068c` was observed running `llama-server` + qwen3.5-0.8B-Q4. But
  (a) that binary **did not include the net-bounce fix**, and (b) `ELR=0x4012068c`
  is the *shared* abort landing pad — under `panic = "immediate-abort"` **every**
  infallible alloc / `handle_alloc_error` lowers to that one `brk #1`, so the ELR
  alone does **not** prove a distinct site. `llama-server` streams HTTP, so this
  may simply be the **net-bounce path again**, not a new one. Re-run with the net
  fix in the binary before concluding a second site exists. NOTE also: that run
  was nominally `MEMORY=2048` but the kernel only detected ~1048 MB (see
  `docs/DYNAMIC_DTB.md`), so it was effectively a ~1 GB run — qwen-0.8B-Q4
  lazy-mmaps but never evicts, so its ~532 MB of weights stay resident and drain
  the PMM regardless.

> **Note.** The 84 MB model also simply does not fit a 64 MB VM — that part is
> expected and unrelated. This fix is **not** about making the model run; it is
> about making the kernel *survive* the OOM (kill the process, not panic). To
> actually run SmolLM2-135M (84 MB) use `MEMORY=256`+.

## Symptom

`llama-server --model /models/SmolLM2-135M-Instruct.Q2_K.gguf` at 64 MB crashed
the whole kernel:

```
[mmap] pid=3 fd=4 file=/models/SmolLM2-135M-Instruct.Q2_K.gguf off=0 len=0x541da40 = 0x15011000 (lazy-file, 38 regions)
[Exception] Sync from EL1: EC=0x3c, ISS=0x1
  ELR=0x4012068c, FAR=0x1161e000, ...
  Instruction at ELR: 0xd4200020       ← brk #1
PMM: 25/16384 pages free (100 KB / 65536 KB)
```

Key observations:

- **`EC=0x3c` is a `BRK` executed *by the kernel*** (`0xd4200020` = `brk #1`),
  not a data/instruction abort. It is the trap the compiler emits for an abort.
- **No panic message.** The `size`/`extreme-size` profiles use
  `panic = "immediate-abort"` (`Cargo.toml`), which lowers every panic and every
  `handle_alloc_error` straight to `brk #1` — bypassing the `#[panic_handler]`,
  so there is no text. A message-less `EC=0x3c` is the fingerprint of an
  allocation/abort on these profiles.
- **`FAR=0x1161e000` is stale** — `BRK` does not update `FAR`; it is leftover
  from the demand-fault the kernel was servicing when the abort fired.
- **`PMM: 25 pages free`** — the 84 MB model paged into a 64 MB VM had drained
  physical memory to a fragmented handful of pages.

## Root cause

The kernel heap (Talc) grows on demand via `PmmOomHandler::handle_oom`
(`src/allocator.rs`). Because the heap lives in the `phys_to_virt` linear map, a
growth span must be **physically contiguous**. The backoff logic guarantees that
a **single-page** growth succeeds whenever *any* page is free, but a
**multi-page** growth can still fail on a fragmented pool — and on failure
`handle_oom` returns `Err`, which `handle_alloc_error` turns into `brk #1`:

```
src/allocator.rs (handle_oom):
    None => return Err(()),   // genuine multi-page-contiguous shortfall
                              // "the OOM killer will hook in at this point" — TODO
```

The trigger is the network bounce buffer. Every `sendto`/`recvfrom`/`sendmsg`/
`recvmsg` (`src/syscall/net.rs`) did:

```rust
let mut kernel_buf = alloc::vec![0u8; len.min(64 * 1024)];   // 64 KiB == 16 pages
```

`vec![]` is an **infallible** allocation. With the heap full and the PMM
exhausted+fragmented, growing it by 16 contiguous pages fails →
`handle_alloc_error` → `brk #1` → whole kernel dead.

(Ironically, the 64 KB cap itself came from an *earlier* fix —
`docs/KERNEL_OOM_ALLOCATION_FIX.md` — that bounded multi-MB file reads down to
64 KB to stop a different OOM panic. 64 KB stopped the megabyte-scale panic, but
64 KB is still 16 contiguous pages, which is exactly what a fragmented pool
can't grow into.)

### Why `llama-server` crashed but `llama-cli` died cleanly

`logs_qemu_smollm.txt` (also 64 MB) shows `llama-cli` hitting the **same** model
OOM but getting a clean `[Fault] Process N SIGSEGV after ~5s` — the kernel
survives. The difference:

| Path | Allocation | Under OOM |
|---|---|---|
| user demand-paging (cli + server) | `alloc_page_zeroed_user()` → `Option` | `None` → **graceful SIGSEGV** (the OOM-kill-not-panic work, `docs/OOM_BEHAVIOR.md`) |
| thread stacks (cli + server) | 16 contiguous PMM pages, `Option` | returns `false` → ENOMEM (the `thread spawn fix`) |
| **net I/O (server only)** | **64 KB infallible kernel-heap `vec!`** | **`handle_oom` `Err` → `brk #1`** |

`llama-cli` has no network path, so it only ever exercises the protected
single-page user-fault path. `llama-server` streams HTTP, so it allocates 64 KB
bounce buffers — the one unprotected multi-page allocation in a hot path.

## Fix

`src/syscall/net.rs` — replace the four infallible `vec![0u8; len.min(64*1024)]`
sites with a fallible helper that degrades instead of aborting:

```rust
const NET_BOUNCE_MAX: usize = 64 * 1024;

/// Ordered sizes to attempt: full (capped) request, then a single-page
/// fallback that needs only one free page. Pure → unit-testable.
pub(crate) fn net_bounce_size_plan(want: usize) -> [usize; 2] {
    let full = want.min(NET_BOUNCE_MAX).max(1);
    [full, 4096usize.min(full)]
}

pub(crate) fn alloc_net_bounce(want: usize) -> Option<alloc::vec::Vec<u8>> {
    for size in net_bounce_size_plan(want) {
        let mut v = alloc::vec::Vec::<u8>::new();
        if v.try_reserve_exact(size).is_ok() {   // FALLIBLE: Err, not abort
            v.resize(size, 0);
            return Some(v);
        }
    }
    None
}
```

Behaviour under pressure:

1. **Ample memory:** allocate the full size (up to 64 KB) — no throughput loss.
2. **Can't grow 16 contiguous pages:** fall back to a single page (4 KB needs
   only one free page → guaranteed satisfiable whenever any page is free). The
   syscall returns a short count; short read/write is always-legal POSIX
   semantics and the caller loops.
3. **Zero pages free:** return `None` → the syscall returns `ENOMEM`. The process
   sees an error; the kernel lives.

`try_reserve_exact` is the crux: it returns `Err` on allocator failure instead of
routing through `handle_alloc_error` (which is the `brk #1`).

### Test

`run_net_bounce_tests()` in `net.rs`, called from `process_tests::run_all_tests`
(gated `not(any(feature = "no-tests", kernel_profile_size))`, matching the other
boot suites). It checks:

- `net_bounce_size_plan` boundaries (empty / sub-page / page / 16-page / over-cap)
  as pure-fn assertions — no RAM drained.
- A real `alloc_net_bounce(8192)` returns 8192 bytes, zero-initialised.
- An over-cap request is served at the 64 KB cap, not failed.

## Verification

- `cargo check` on dev, `--profile size`, `--profile extreme-size`: clean.
- `INSTANCE=1 DISK=<copy> cargo run --release`: booted to
  `[SSH Server] Listening`, `[PASS] test_net_bounce_alloc_degradation`, **zero**
  `EC=0x3c`/`brk`/panic. (Run on a *copy* of `disk.img` on instance 1 so a
  concurrently-running VM on instance 0 was untouched — `disk.img` is
  write-locked by a live QEMU.)

## Remaining work

The net buffer is fixed, but the **structural** hazard remains: *any* infallible
kernel-heap allocation that needs a multi-page contiguous growth will still
`brk #1` when the PMM is fragmented/exhausted. qwen3.5-0.8B-Q4 at "1 GB" (it
lazy-mmaps but never evicts, so weights stay resident until the PMM is drained)
also crashes at `ELR=0x4012068c` — but as noted in **Status**, that binary
*lacked the net-bounce fix* and `0x4012068c` is the shared abort landing pad, so
it is **not yet confirmed** to be a distinct site. The audit below still applies
either way; whether a *second* site exists is settled by re-running with the net
fix compiled in.

Two complementary directions:

1. **Convert remaining hot-path large kernel allocs to fallible/streaming** (the
   approach taken here and in `docs/KERNEL_OOM_ALLOCATION_FIX.md`). Audit other
   `vec![...; large]` / `with_capacity(large)` in syscall and VFS paths.
2. **Hook the OOM killer at `allocator.rs` where `handle_oom` returns `Err`** —
   the documented intent. Tricky: `handle_oom` runs *inside* `malloc` with the
   Talc lock held, so killing/unwinding the current process from there needs care
   (defer the kill to a safe point rather than acting under the lock).

## References

- `src/syscall/net.rs` — `alloc_net_bounce`, `net_bounce_size_plan`, `run_net_bounce_tests`
- `src/allocator.rs` — `PmmOomHandler::handle_oom`, `heap_grow_backoff`
- `docs/KERNEL_OOM_ALLOCATION_FIX.md` — the earlier 64 KB read-bounding fix
- `docs/OOM_BEHAVIOR.md` — user demand-paging OOM → SIGSEGV (the protected path)
- `Cargo.toml` — `panic = "immediate-abort"` on `size`/`extreme-size`
