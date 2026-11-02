# Memory — address map, slabs, and the user-pointer rule

**Stability: C**.

## The address map

```mermaid
flowchart BT
    subgraph user["task's lower half (its own root page table)"]
        U1["ELF image PT_LOADs at link addresses<br/>(static) or PIE_BASE = 0x1000_0000"]
        U2["brk slab: image_end … +256 MiB<br/>(BRK_SLAB = 0x1000_0000)"]
        U3["interpreter (PT_INTERP): INTERP_BASE = 0x3000_0000<br/>(akuma-elf-exec)"]
        U4["mmap slab: 0x0000_6000_0000_0000 … +16 GiB<br/>(MMAP_SLAB = 0x4_0000_0000, address space only)"]
        U5["stack: 16 pages below 0x0000_7000_0000_0000"]
    end
    subgraph kernel["upper half (shared by PML4 slots 256..512 copy)"]
        K1["physmap: PML4 slot 256 (0xFFFF800000000000)<br/>every physical page, PHYSMAP_LIMIT-bounded"]
        K2["device window: DEVMAP_BASE + pa<br/>(virtio-mmio transports, LAPIC page; device-uncached)"]
        K3["kernel image at 0xFFFFFFFF80000000 + phys<br/>(top -2 GiB, code-model=kernel)"]
    end
```

Canonical mermaid source: [`../diagrams/memory-map.md`](../diagrams/memory-map.md).

- Each task's root is a fresh zeroed PML4 whose **upper half is copied from
  the live kernel CR3** (slots 256–511 — one copy shares the physmap, the
  device window and the kernel image) and whose lower half holds only the
  task. The kernel's own root additionally keeps the boot identity map
  ([`boot.md`](boot.md) § identity map).
- Loading is **eager**: every `PT_LOAD` gets fresh zeroed frames mapped at its
  link address, the file bytes copied through the physmap from `p_offset`;
  `.bss` is the zeroed tail. The ELF decisions are
  [`crates/akuma-elf-exec`](../../../crates/akuma-elf-exec)'s: `program` for the
  image (base, entry, `AT_PHDR`, the end where `brk` starts) and `load_segments`
  for the interpreter, both checking each segment against the file. This
  kernel's `place_segment` maps both through one page loop: writable wins over
  executable, and a segment that overflows or reaches past `USER_TOP` is
  refused before any frame is taken.
- The initial stack is 16 pages below `USER_STACK_TOP`; its top
  `ARG_BLOCK_PAGES` (12) hold the block `ring3::build_stack` lays out backwards:
  argc/argv/envp/auxv (14 aux entries: `AT_BASE` for the interpreter,
  `AT_PHDR/PHENT/PHNUM/ENTRY`, uids 0, `AT_CLKTCK=100`, `AT_RANDOM=0`).
- `brk` grows by mapping fresh zeroed pages on demand inside its slab;
  shrinking is bookkeeping only (no unmap). Linux's contract is honoured: a
  failed `brk` returns the **old** break.
- `mmap` serves `MAP_ANONYMOUS` (zeroed frames) and `MAP_PRIVATE` of an open
  regular file (zeroed frames filled from the inode at `off` — musl ldso's
  `map_library` shape; there is no page cache, so each mapping has its own
  copy). A writable `MAP_SHARED` file mapping is recorded and written back to
  the file by `msync`, `munmap`, exec and exit. Every mapping without `PROT_EXEC` is
  mapped user read-write: `PROT_READ` alone is not enforced. `W+X` is refused at `mmap` too: the W^X decision is
  [`crates/akuma-syscalls-mem`](../../../crates/akuma-syscalls-mem) `wx::check_prot`,
  called at the top of both the `mmap` and `mprotect` arms (`wx=0` turns it off).
  The slab is 16 GiB of *reserved address space* (a page costs memory when mapped, not when reserved); at 1 GiB one `rustc` ran out of it — it maps a 311 MB `librustc_driver` and a ~17 MB stack per codegen thread — and musl reported the failed stack `mmap` as `EAGAIN` ("failed to spawn work thread"). Memory is the next wall: the fill is still eager (see `gaps.md`).
  `MAP_FIXED` is honoured only inside the mmap slab; the bump cursor never
  rewinds past it. `mremap` grows by moving to a fresh region (MAYMOVE;
  `MREMAP_FIXED` refused). `munmap` returns address space (hole table); `mprotect`
  re-maps the PTEs (`PROT_NONE` stays a no-op) under the W^X rule of
  `docs/archive/TAOXICHUAN_WX_POLICY.md`: `W+X` is refused and `PROT_EXEC` is
  refused on a page written since it was mapped (`EACCES`; `wx=0` turns it off).

## The user-pointer rule

The layout bookkeeping is [`crates/akuma-user-layout`](../../../crates/akuma-user-layout)'s `Layout`: the
regions, the `brk` window, the mmap slab with its bump cursor and hole table,
and the writable shared file mappings. It decides; this kernel maps, fills and
frees, and tells it what it did.

This kernel's own arms check every user pointer with `user_ok(va, len)`:
`[va, va+len)` must lie inside one of the task's registered regions (the image
segments, the 256 MiB brk slab, the 16 GiB mmap slab, the stack, the
interpreter span). Then `copy_from_user`/`copy_to_user` walk the task's **own
root page table** one page at a time and move bytes through the physmap
(`TaskWalk`, the effect half of `akuma-user-walk`). The region check is the
gate; a page that fails to translate mid-copy is the backstop (`-EFAULT`).

Glue's arms use [`crates/akuma-user-access`](../../../crates/akuma-user-access)
instead: they touch the user address directly, and a fault inside one of its
copies resumes at `user_copy_fixup` with `EFAULT`. A ring-0 write to a present
copy-on-write page breaks the share in the fault handler first.

## fork's memory: copy-on-write

`clone_user_half` (main.rs) builds a fork child's address space, reached
through the twin's `ExecRuntime::fork_share_memory` hook
([`processes.md`](processes.md)): a fresh root (kernel half shared by the same
PML4 copy), then a subtree-by-subtree clone of the parent's **user half
only**. The page tables are copied; the **leaf frames are
shared**:

- every present 4 KiB leaf takes one `cow_ref_inc` (the first share inserts 2:
  parent + child); the kernel half is never walked. The counts live in
  [`crates/akuma-pmm`](../../../crates/akuma-pmm) `cow_table::CowTable` — a fixed
  array of 4096-entry chunks (one per 16 MiB of physical space, allocated on the
  first share in its range), so a 100k-page fork costs no per-frame allocation
  (it was a `BTreeMap`, one heap node per shared frame);
- a **writable** leaf is demoted to read-only in *both* tables and marked with
  PTE bit 9 (`PTE_COW`, an MMU-ignored AVL bit); the parent's translation is
  `invlpg`'d, because a stale writable entry would write into the shared frame
  with no fault. A read-only leaf (text, `.rodata`, `mprotect(READ)`) is shared
  with its flags verbatim and **no** marker, so a write to it is still a kill;
- a writable `MAP_SHARED` file mapping (`t.shared`) is the exception: those
  pages are copied eagerly, the per-process-copy-plus-writeback model they run
  under;
- huge (PS) leaves are refused. A refused or failed fork releases what it had
  built through `free_page`, so it leaks nothing.

**One break path**, `cow_resolve(root, va)`: re-read the live PTE, ask
`akuma_cow::decide` (`pte_writable` → `Retry`; unmarked → `Fault`; share count
≤ 1 → `TakeInPlace`; else `Copy`), carry it out, `invlpg`. Three callers:

- `fault_trap`, for a ring-3 fault with error code present+write+user (stack
  growth is tried first; only `Fault` falls through to the kill);
- `copy_to_user` — the kernel writes user pages through the physmap, which
  never faults, so it calls `cow_prepare_write` on each destination page first
  and answers `EFAULT` (as Linux does for a kernel write to a page the user
  could not write) when the break is needed and fails, or when the page is
  read-only and shared but *unmarked* (shared text, or `mprotect(PROT_READ)`
  after a fork) — an unshared read-only page stays kernel-writable;
- `mprotect` to a writable mapping calls `cow_privatize`: a frame held by two
  spaces becomes private before the PTE is re-mapped RW (marked or not — a
  shared text page is unmarked).

Teardown needed no new code: every release site (`free_user_root`,
`release_pages`, the exec/reap parking) goes through `akuma_pmm::free_page`,
which only drops a count while the frame is shared. The gate is
`scripts/kvm/goose/cow_leak_gate.py`: an idle machine's free-page count after
N fork/exec/exit rounds must equal the baseline **exactly**, with
`cow_ref_count() == 0`. The reap lines (`trace-err`) print `cowrefs=` and
`cow=(shared, copied, taken, retried)`.

Known limits: a shared read-only page stays read-only to ring 3 (a write
kills, by design — there is no user-text self-modification). Incident, plan and
measurements: `docs/archive/TAOXICHUAN_COW_FORK.md`; the two `cow_prepare_write`
holes closed on 2026-10-30 are §9 of
`docs/archive/TAOXICHUAN_BUGS_FOUND_2026_10_30.md`.

## Allocation on syscall paths

The fd and socket arms are glue's and follow glue's allocation rules. This
kernel's own arms allocate in exec (the argument strings and the stack block)
and nowhere else per call; the caps that bound it are 1 MiB per console
`write` (`WRITE_LIMIT`) and the execve argument block: 448 `argv` / 256 `envp`
entries, 32 KiB per string, `argv + envp <= 512` and a total-bytes guard —
every overrun answers `E2BIG`, never `EFAULT` (one heavy `rustc` call carries
344 arguments and a 5,192-byte argument). A frame that cannot be allocated in
a syscall answers `ENOMEM`; a failed kernel-heap allocation is `[OOM]` + halt.

## Background

`docs/archive/TAOXICHUAN_STAGE6_PLAN.md` (the 64 MiB slabs),
`docs/archive/TAOXICHUAN_PROCESS_MODEL_PLAN.md` § H3-f (the eager fork CoW replaced),
[`TAOXICHUAN_B8_ONE_ELF_LOADER.md`](../../../docs/archive/TAOXICHUAN_B8_ONE_ELF_LOADER.md) (one loader for
program and interpreter),
`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (the stack-block
and double-advance incidents).
