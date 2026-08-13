# Trimming fat: duplicated code in `src/` and `crates/`

**Date:** 2026-08-12
**Scope:** kernel bin crate (`src/`) + the seven extracted crates (`crates/`).
Userspace out of scope *for the duplication survey* — but a fix may still have to
reach into it, as Phase 0 item 5 did (a bounded kernel buffer is only correct if
the userspace writer retries the residue).
**Companion:** [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) — several findings overlap, because
deleted code has no `unsafe` in it.

**This doc tracks.** It is a live work list, not a frozen investigation — unlike
its neighbours in `archive/`, update it as phases land.

**Progress: Phases 0, 1, 2a, 2b, 3 all done, and Phase 4's blocked half is
COMPLETE (2026-08-13).** `crates/akuma-primitives` exists and all six rungs
landed: `OnceCopy`, the console primitives, every DAIF access in the tree, the
clock hook, the thread-slot preemption table + `PreemptGuard`, and the identity
phys/virt translators. **Three crates — `akuma-ext2`, `akuma-virtio`,
`akuma-net` — no longer depend on `akuma-exec` at all.** Full writeup:
[`AKUMA_PRIMITIVES_EXTRACTION.md`](AKUMA_PRIMITIVES_EXTRACTION.md); current-state
reference: [`../reference/subsystems/primitives.md`](../reference/subsystems/primitives.md).
What remains of Phase 4 is the *unblocked* half (§8.5) — none of it needs a new
crate. See §8.5 for per-item status.

**~~Known blocker, owned by no phase:~~ FIXED 2026-08-13.** ssh sessions died
instantly on the extreme-size profile, blocking
`acceptance/05_meow_tcc_extreme_4mb.md`. Root cause: the profile's 64 KB
user-thread **kernel stack** was ~10 KB too small for the sshd session path
(measured 74 KB), and the overrun zeroed three PTEs in the session process's own
L3 page table — surfacing as a SIGSEGV with no relationship to the corruption.
It was never a duplication/cleanup regression; the §8.5 Phase 2a A/B that cleared
this work was right. Fixed by sizing the stack to the measurement **and** by
giving the long-painted-but-never-checked stack canary its first caller.
Full autopsy: [`EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md`](EXTREME_SSHD_KERNEL_STACK_OVERFLOW.md).
The playbook is runnable again.

---

## 1. Method

Token-based clone detection with **PMD CPD 7.26.0** (`brew install pmd`):

```bash
pmd cpd --dir src --dir crates --language rust \
        --minimum-tokens 100 --format text
```

CPD normalizes whitespace and comments, then reports runs of identical token
sequences. It finds **Type-1** clones (exact) and, in principle, **Type-2**
(same structure, renamed identifiers) via `--ignore-identifiers`.

> **Caveat that matters for every number below.** `--ignore-identifiers` is a
> **no-op for the Rust tokenizer** in PMD 7.26.0 — the output is byte-identical
> with and without it, at both 50 and 100 tokens. So everything here is Type-1
> only. Every clone that survived a variable rename is invisible to these
> numbers, and the real duplication is meaningfully higher. §6 quantifies the
> miss with two worked examples.

---

## 2. Headline numbers

| `--minimum-tokens` | clone blocks | duplicated lines |
|---:|---:|---:|
| 50 | 461 | **5,370** |
| 75 | 169 | 2,946 |
| 100 | 92 | 1,975 |
| 150 | 38 | 1,135 |

The tree is 96,520 lines, so the 50-token figure is **5.6% of all code** in
exact-duplicate token runs — and that is the floor, not the estimate.

At 100 tokens the split is lopsided:

| | blocks | lines |
|---|---:|---:|
| **intra-file** (same file, twice) | 84 | 1,792 |
| **cross-file** | 8 | 183 |

So this is overwhelmingly *copy-paste within a file* — someone needed a variant
of a function and duplicated it — not modules drifting apart. That is good news:
intra-file clones are the cheapest kind to fix, because both copies are in front
of you and there is no crate boundary to negotiate.

### Duplicated lines by file (100 tokens)

| lines | file |
|---:|---|
| 818 | `src/exceptions.rs` |
| 522 | `crates/akuma-exec/src/process/mod.rs` |
| 434 | `src/syscall/fs.rs` |
| 398 | `crates/akuma-exec/src/mmu/mod.rs` |
| 376 | `src/tests.rs` |
| 336 | `crates/akuma-exec/src/elf/mod.rs` |
| 293 | `src/process_tests.rs` |
| 165 | `src/syscall/net.rs` |
| 94 | `crates/akuma-exec/src/process/image.rs` |
| 63 | `crates/akuma-isolation/src/mount.rs` |
| 63 | `crates/akuma-vfs/src/mount.rs` |
| 60 | `crates/akuma-exec/src/box_mod/access.rs` |
| 60 | `crates/akuma-exec/src/box_mod/hierarchy.rs` |
| 48 | `crates/akuma-exec/src/threading/mod.rs` |
| 47 | `src/rump_proxy.rs` |

669 of the 1,975 lines (34%) are in the two test files. Boot self-tests are a
different risk class — duplication there is annoying, not dangerous — so the
priorities in §5 exclude them.

---

## 3. The dominant pattern: `X` and `X_from_path`

> **Status.** The whole quartet is **done as of 2026-08-13** — the `elf/mod.rs`
> three-of-four in Phase 2a, the `process/mod.rs` + `process/image.rs` consumer
> pairs in Phase 2b. What the plan below got right and wrong is in §8.5. The
> analysis is left as written, because §8.5's corrections only make sense
> against it.

**Three separate copies of one design decision**, in three different files. Each
is "load an ELF from bytes" cloned into "load an ELF from a path":

| Clone | Lines | Tokens |
|---|---:|---:|
| `elf/mod.rs:771` `load_elf_with_stack` ↔ `:1129` `load_elf_with_stack_from_path` | **61** | **627** |
| `process/mod.rs:655` `ProcessImage::from_elf` ↔ `:745` `from_elf_path` | **60** | 404 |
| `process/image.rs:52` `replace_image` ↔ `:151` `replace_image_from_path` | **47** | 258 |

The first is the single largest clone block in the tree. Together they are ~168
duplicated lines expressing the same idea three times.

They also stack: `replace_image_from_path` calls into `from_elf_path` calls into
`load_elf_with_stack_from_path` — so the `_from_path` variant was propagated
down an entire call chain by copy-paste, layer by layer.

**Fix:** resolve the source *once*, at the top, into whatever the common path
needs (a `&[u8]`, or a small `ElfSource { Bytes(&[u8]), Path(&str, usize) }`
that the loader reads through), and keep a single implementation underneath. The
`_from_path` entry points stay as thin wrappers that do the resolution and
delegate. More usefully than the line count: a bug gets fixed in one place
instead of four — exactly the failure mode §6 documents.

**Sizing the merge.** Measuring the full function extents rather than only the
token-identical runs CPD reports:

| | now | merged | saved |
|---|---:|---:|---:|
| `load_elf` + `_from_path` | 254 + 212 | ~270 | ~195 |
| `load_interpreter` + `_from_path` | 136 + 148 | ~150 | ~134 |
| `load_elf_with_stack` + `_from_path` | 72 + 75 | ~80 | ~67 |
| hand-rolled parser in `types.rs` | ~90 | 0 | ~90 |
| `from_elf` + `_path` | 88 + 101 | ~110 | ~79 |
| `replace_image` + `_path` | 97 + 102 | ~115 | ~84 |

**≈ 650 lines**, taking `elf/mod.rs` from 1,259 to roughly 780 and `types.rs`
from 319 to ~230. Those four pairs are **895 of `elf/mod.rs`'s 1,259 lines — 71%
of the file is duplicated implementations of one idea.** The file split
(`source.rs` / `load.rs` / `interp.rs` / `stack.rs`) then falls out of the merge
rather than being a separate exercise.

This is well above the ~1,718 whole-tree "removable" figure's share for these
files, because CPD only counts token-identical runs — see §7.

**Not a crate.** The module already sits in the right place (`akuma-exec` is the
execution crate). Extracting `akuma-elf` is blocked anyway: the loader
constructs and returns a `crate::mmu::UserAddressSpace` inside `LoadedElf`, and
also reaches `crate::bkl::{enter_kernel, leave_kernel}` — so `akuma-elf` would
need `akuma-exec`, which needs `akuma-elf`. Breaking that means extracting `mmu`
first (the page-table-UAF file) or inverting through a runtime-callback trait.
Neither buys anything: `akuma-exec` already builds and tests on the host, so the
module is host-testable *today* — it just has no tests.

### Four pairs, drifted by very different amounts

There is a fourth instance CPD's file-level view obscured —
`load_interpreter` / `load_interpreter_from_path`, inside `elf/mod.rs` itself.
Diffing each pair function-to-function:

| Pair | Sizes | Changed lines | Verdict |
|---|---|---:|---|
| `load_elf_with_stack` / `_from_path` | 72 / 75 | **4** | clean |
| `ProcessImage::from_elf` / `from_elf_path` | 88 / 101 | **41** | drifted |
| `replace_image` / `_from_path` | 97 / 102 | **49** | drifted |
| `load_interpreter` / `_from_path` (`elf/mod.rs:288` / `:434`) | 136 / 148 | **180** | rewritten |

### The interpreter pair is not a clone any more — it is a second ELF parser

180 changed lines out of ~140 means the two have almost nothing in common, and
the reason matters more than the duplication:

```rust
// load_interpreter (:288) — third-party parser
let elf = ElfBytes::<LittleEndian>::minimal_parse(elf_data)?;
for phdr in elf.segments().ok_or(…)?.iter() { … }

// load_interpreter_from_path (:434) — hand-rolled parser
let hdr_buf = file_read_exact(path, 0, ELF64_EHDR_SIZE)?;
let ehdr = parse_elf64_ehdr_checked(&hdr_buf)?;
let e_shoff     = read_u64_le(&hdr_buf, 40) as usize;   // literal spec offsets
let e_shentsize = read_u16_le(&hdr_buf, 58) as usize;
let e_shnum     = read_u16_le(&hdr_buf, 60) as usize;
for i in 0..ehdr.e_phnum as usize {
    let phdr = parse_elf64_phdr(&phdr_buf[i * ehdr.e_phentsize as usize..])?;
```

**The kernel has two ELF parsers**: the vetted `elf` 0.7 crate, and a hand-rolled
one in `elf/types.rs` reading fields at literal byte offsets. Which one validates
your dynamic linker depends on whether `execve` took the eager or the lazy path
(`src/syscall/proc.rs:777` vs `:786`). Two independent sets of malformed-ELF
rejections on the same input class, only one of which gets third-party scrutiny.

`parse_elf64_phdr` does bounds-check its slice (`types.rs:149`), but the
`read_u16_le` / `read_u32_le` / `read_u64_le` helpers underneath it are `pub` and
index unchecked (`types.rs:160-173`). They are safe at their current call sites
because `file_read_exact` yields a fixed 64-byte header — but the kernel builds
`panic = "abort"`, so an unchecked index reached on attacker-shaped input is a
kernel abort, not a process kill. Worth a bounds-checked reader regardless of
whether a live path can reach it today.

Unifying on the `elf` crate for both paths deletes the hand-rolled parser
outright (~90 of `types.rs`'s 319 lines) and removes the asymmetry.

**The apparent obstacle dissolves.** `ElfBytes::minimal_parse` needs the whole
file, which is exactly what the lazy path refuses to read — that is presumably
why the hand-rolled parser exists. But the `elf` crate exposes lower-level
`no_std` pieces that work on small buffers:

```rust
elf::file::parse_ident(&hdr[..16])                        // -> (endian, class, …)
elf::file::FileHeader::parse_tail(ident, &hdr[16..64])    // validated, 64 bytes
elf::segment::SegmentTable::new(endian, class, phdr_buf)  // bounds-checked iter
```

(`ElfStream` is `#[cfg(feature = "std")]` and unavailable; these three are not.)
So the lazy path can read 64 bytes, parse the header properly, read
`e_phnum * e_phentsize` bytes, and iterate segments — all through the vetted
parser, with no whole-file slurp.

### Verified: the two parsers agree on every ELF in the tree

Before deleting the hand-rolled parser, a throwaway differential harness
(scratchpad, not in the repo) reimplemented it verbatim from `elf/mod.rs:848`
and `types.rs:148` and diffed it against the `elf`-crate path over the whole
corpus, comparing `e_type / e_machine / e_entry / e_phoff / e_phnum /
e_phentsize` and every phdr's `p_type / p_flags / p_offset / p_vaddr / p_filesz
/ p_memsz`:

```
ELF files seen      : 2387
  loadable (EXEC/DYN): 356
  relocatable (REL)  : 2031
parsers agreed      : 2387
DISAGREEMENTS       : 0 loadable, 0 relocatable
truncation panics   : 0
hostile-field cases : 280, panics: 0
RESULT: PASS
```

Two conclusions:

1. **The deletion is behaviour-preserving on all real input**, including the
   2,031 `ET_REL` objects both parsers classify identically — a negative-case
   set nobody had ever checked.
2. **The `panic = "abort"` concern above is unfounded for these fields.**
   Mutating `e_phoff` to `u64::MAX` / exactly EOF / EOF−1, and `e_phnum` /
   `e_phentsize` to `0xFFFF` and `0` (280 cases) panics neither parser: the
   guards in front of the unchecked `read_*_le` helpers hold. A bounds-checked
   reader is still tidier, but it is not closing a live hole.

Caveats: the harness reimplements the hand-rolled parser rather than linking the
real one, and 7 mutated fields is not a fuzzer. It is enough to make the
deletion evidence-backed rather than a judgement call. Rebuild it from
[`../runbooks/find-duplicated-code.md`](../runbooks/find-duplicated-code.md) §5
if it is needed again.

### What "consolidate the best version" means per pair

The copy-paste conflated two independent axes: **where the bytes come from**
(slice vs path — pure plumbing) and **eager vs deferred mapping** (a real
behavioural difference that must survive). Right now loaded-from-path *implies*
deferred segments and loaded-from-bytes *implies* eager, which is an accident of
the duplication, not a design. Separate them and each becomes a parameter.

| Pair | Best version |
|---|---|
| interpreter loaders | the `elf` crate, unambiguously — delete the hand-rolled parser |
| `load_elf` / `_from_path` | neither; split the source axis from the mapping strategy behind an `ElfSource` with `read_at(offset, len)` (`file_read_exact` at `:873` is half of it already) |
| `load_elf_with_stack` / `_from_path` | already 4 lines apart; trivial |
| `from_elf` / `_path`, `replace_image` / `_path` | the **union** — `replace_image`'s five `[FORK-DBG]` traces plus whichever copy carries the fuller comment |

The loader pair is disciplined. Its one semantic difference — the bytes variant
returns `Vec::new()` for deferred segments where the path variant returns
`loaded.deferred_segments` — is **correct**, not a bug: `deferred_segments` is
only ever populated by the path loader (`elf/mod.rs:1022`, `:1059`), because
in-memory data has nothing to demand-page.

The two consumer pairs are where it went wrong, and the drift is entirely in
**comments and observability** rather than logic — the classic pattern. The code
stays in sync because it has to work; the reasoning and the tracing decay.

**`replace_image` has five `lifecycle_trace("[FORK-DBG] …")` calls.
`replace_image_from_path` has zero.** Both are live in `execve`
(`src/syscall/proc.rs:777` and `:786`), selected at runtime by whether the file
was already read into memory. So half of every exec path in the kernel is
invisible to lifecycle tracing — in a codebase whose fork/exec history is a long
run of SIGSEGV and lifecycle hunts, that is a real debugging liability, not a
cosmetic one.

**The load-bearing rationale lives in one copy each.** `replace_image` carries
the full seven-line explanation of why the preemption guard is acquired *after*
the ELF load (the SMP=4 heterogeneous SIGSEGVs; holding the guard across block
I/O wedges the box). `replace_image_from_path` has "See the comment in
`replace_image`." — and the comment being pointed at is the one that says "(in
the `from_path` variant) does block I/O". The explanation of copy B's behaviour
is stored in copy A. Same shape in `process/mod.rs`: `from_elf` holds the
six-line note on why the pid-keyed `push_lazy_region` cannot be used before the
`Process` exists ("every region would be silently dropped and the first heap or
deep-stack touch would SIGSEGV"); `from_elf_path` has a three-line summary
deferring to "the sibling constructor".

Merging the pairs is therefore worth more than the ~110 lines it saves: it
restores tracing to the lazy exec path and puts each piece of reasoning next to
the only code it describes.

### The same shape, smaller

- `mmu/mod.rs:1430` `map_user_page` ↔ `:1563` `map_user_page_no_flush` — **37
  lines, 461 tokens.** The names say it: identical except the trailing TLB
  flush. One function with a `flush: bool` (or a `_no_flush` wrapper that calls
  the other with the flush skipped) removes the whole second copy. **Caution:**
  this is `mmu/mod.rs`, the page-table-UAF file — see `UNSAFE_AUDIT.md` §5.1
  before touching it.
- `mmu/mod.rs:1041` ↔ `:1092` (19 lines / 255 tokens), `:1047` ↔ `:1223`,
  `:1098` ↔ `:1223` (13 lines each) — a third and fourth near-copy of the same
  walk, all inside one file.
- `src/exceptions.rs` — three clone blocks (54, 52 and 36 lines) between the
  `Drop` impl at `:3703` and the one at `:4315`. **The same `Drop` written
  twice**, 600 lines apart, in the highest-consequence file in `src/`.

---

## 4. Cross-file clones

Only 8 blocks / 183 lines, but these are the ones that cross a module or crate
boundary, so they are the most likely to drift silently.

| Lines | Pair |
|---:|---|
| 60 | `akuma-exec/src/box_mod/access.rs:122` `cascade_kill_order` ↔ `box_mod/hierarchy.rs:75` `validate_nested_root` |
| 63 (3 blocks) | `akuma-isolation/src/mount.rs` ↔ `akuma-vfs/src/mount.rs` |
| 23 | `akuma-rump/src/sysproxy.rs` ↔ `src/rump_proxy.rs` |
| 22 + 13 | `akuma-net/src/hal.rs` ↔ `src/virtio_hal.rs` |
| 10 | `akuma-exec/src/process/types.rs` ↔ `akuma-exec/src/threading/types.rs` |
| 5 | `akuma-exec/src/process/types.rs` ↔ `src/process_tests.rs` |

Two mount implementations across two crates is the one that should worry you
most: `akuma-vfs` is the leaf that `akuma-isolation` depends on, so the shared
half belongs in `akuma-vfs` and there is no dependency obstacle to putting it
there.

**DONE 2026-08-13.** `MountTable` (8 mounts) and `MountNamespace` (16) were the
same table: `mount` / `unmount` / `resolve` / `resolve_arc` / `list_mounts` /
`child_mount_points` byte-identical apart from the capacity constant, plus one
extra method each (`get_fs`+`sync_all` vs `replace_pristine_root`+`is_empty`).
Now one `MountSet<const MAX: usize>` in `akuma-vfs`, with both names as **type
aliases** — every use site (the `MOUNT_TABLE` static, `Namespace.mount:
Spinlock<MountNamespace>`, and both crates' tests) only ever calls methods, so
nothing moved. `akuma-isolation/src/mount.rs` went 233 → 74 code lines (the
remainder is its four box-root tests, which stay because box-root semantics are
that crate's concern); the four touched files net **−145**.

**`replace_pristine_root` moved with its guard, on purpose.** It is the only
write to an existing mount's `fs` in the tree, and there is deliberately **no**
unguarded `replace_root` anywhere — so the `expected`-name check travels with the
operation rather than living in a wrapper a later caller could bypass. The cost
is that the kernel's global `MountTable` now also exposes the *guarded* form,
which has no caller; the alternative was publishing a bare setter for the
namespace to build on, which is strictly worse.

**Coverage, stated precisely.** `MountSet<8>` matching is exercised on every file
access in-VM (release and devbox-smoltcp boots: `..` traversal, `..` through a
mount point, escape-above-root clamping, relative resolution from a CWD, trailing
slashes — all verified over ssh). `MountSet<16>` *matching* is covered by
`akuma-isolation`'s four host tests and the new `MountSet<16>` cases, **not**
in-VM: neither boot created a box namespace, so the namespace arm of `with_fs`
ran against an empty set and fell through. The two instantiations are the same
monomorphised body apart from the `mounts.len() >= MAX` comparison, which is
unit-tested at both 8 and 16.

> **Both DONE 2026-08-13.** One `MountSet<const MAX: usize>` in `akuma-vfs`;
> `MountTable = MountSet<8>` and `akuma_isolation::MountNamespace =
> MountSet<16>` are type aliases, so no call site moved. The bin crate's
> third path normaliser is gone. Details at the end of each subsection below.

### Three path normalisers, two of them in `akuma-vfs` itself (found 2026-08-13)

Turned up while checking whether anything in `akuma-vfs` wanted to move to
`akuma-primitives` (nothing did — see below). CPD cannot see these: different
return types, renamed identifiers.

| | behaviour |
|---|---|
| `akuma-vfs/src/path.rs:9` `canonicalize_path` | full `.`/`..` resolution → `String` |
| `akuma-vfs/src/mount.rs:185` `normalize_path` | trims trailing `/` only → `&str` |
| `src/vfs/mod.rs:128` `normalize_path_owned` | trims trailing `/` **and prepends a leading `/`** → `String` |

The last two are the problem: same name, same stated intent, **different
results.** A mount path arriving without a leading `/` normalises differently
depending on which one sees it — the bin crate's forces the slash, `akuma-vfs`'s
does not.

**FIXED 2026-08-13, and the real defect was worse than "two spellings."**
`normalize_path_owned` was only ever called in the *no-current-process* arm of
`vfs::with_fs` and `vfs::resolve_absolute`; the with-process arm called
`path::resolve_path(&proc.cwd, path)`. Both arms then fed the same
`MountSet::resolve_arc`. So `.` and `..` were **resolved when a process was
current and left in the path when one was not** — one call site, two
normalisation semantics, selected by whether a process happened to exist.

It is now `resolve_path("/", path)` — the same function the other arm uses, with
the CWD it actually has — behind a named `resolve_without_cwd`. Identical on
`""`, `"/"`, `"/foo/"`, `"foo"`; on `"foo/../bar"` it yields `/bar` where the old
code yielded `/foo/../bar`. Pinned by
`resolve_path_at_root_is_the_no_cwd_normaliser` in `akuma-vfs`'s tests, which
also records that escaping above root clamps (`"../../etc"` → `/etc`) rather than
producing a relative path.

`normalize_mount_path` (was `mount.rs`'s `normalize_path`) **stays**, renamed and
documented as what it is: a non-allocating trailing-slash trim for mount-point
comparison, deliberately not a path normaliser. It returns a borrow so `resolve`
can hand back a subslice of its input. It is now the single copy — the namespace
had inlined the same two lines three times.

### Nothing in `akuma-vfs` belongs in `akuma-primitives`

Asked and answered 2026-08-13, because the chain `akuma-exec → akuma-isolation →
akuma-vfs` invites the question. Three reasons it does not pay:

1. **`akuma-vfs` is already at the bottom of that chain.** Extraction pays when
   the duplicator *cannot reach* the canonical version (§5.55's criterion).
   Everything above vfs can already reach vfs, so moving code down cuts no edge.
2. **`akuma-vfs` is not dependency-free.** `memfs.rs` holds
   `spinning_top::Spinlock<FsNode>` and the crate uses `alloc` throughout, so its
   two largest files cannot enter a `core`-only crate without breaking that
   crate's one rule.
3. **`path.rs` is the only plausible candidate and it has ~51 call sites, all
   above vfs** — `src/syscall/fs.rs` (25), `src/vfs/mod.rs` (12),
   `src/syscall/container.rs` (5), `akuma-ext2` (3),
   `akuma-isolation/subdir_fs.rs` (3) — every one in a crate that already depends
   on `akuma-vfs`. Zero edges cut, and it returns `String`.

**The chain itself looks like the `PreemptGuard` shape and is not.**
`akuma-exec`'s entire use of `akuma-isolation` is two symbols out of 1,655 lines
(`Namespace` ×3, `global_namespace` ×3), and `akuma-exec` does not declare
`akuma-vfs` at all — it reaches vfs types through isolation's re-exports. But
`Namespace` holds `Spinlock<MountNamespace>` + `NetworkNamespace`, and
`MountNamespace` genuinely needs vfs's `Filesystem` trait. `PreemptGuard` only
*looked* like scheduler state; `Namespace` really is filesystem state. Cutting
this edge means inverting the mount table out of the execution crate — a design
change, not an extraction, and out of scope for Phase 4.

---

## 5. The virtio driver layer

Covered in depth in [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) §4 P2; the CPD numbers
confirm it independently.

- **`VIRTIO_MMIO_ADDRS` — 3-way clone caught by CPD** (`audio.rs:83`,
  `block.rs:20`, `rng.rs:19`; 17 lines / 76 tokens), plus a **fourth** inline
  copy at `src/main.rs:1216` that CPD misses because it is written as a `let`
  rather than a `const`.
- **The device probe loop** — `audio.rs:217` ↔ `block.rs:266` (9 lines), with
  two further copies at `rng.rs:512` and `smoltcp_net.rs:536` that CPD misses
  because they drifted (named constant vs `0x008` literal vs a bare `1` for the
  device id).
- **The two `Hal` impls** — `akuma-net/src/hal.rs` ↔ `src/virtio_hal.rs`, 22 + 13
  lines. See §6: CPD only sees a third of this clone.
- `block.rs:175` ↔ `:197` — 16 lines / 114 tokens, `read_bytes` vs `write_bytes`
  sharing an identical offset/sector-arithmetic preamble.

One `virtio::probe(device_id) -> Option<(usize, MmioTransport)>` helper plus one
shared `Hal` collapses the whole cluster.

---

## 5.5 Repeated trait implementations (the Type-2 axis CPD can't cover)

Trait impls are where renamed-identifier clones hide: N implementations of one
trait with the same body shape and different type names. CPD is blind to these
for Rust (§1). A separate pass groups every `impl Trait for Type` by trait and
scores pairwise similarity after normalizing identifiers and literals away, then
clusters transitively (script in the scratchpad; ~120 raw pairs collapse to 10
clusters).

**97 trait impls, 26 distinct traits.** Ten clusters, nominally ~336 removable
lines — but that number needs discounting, and the discount is the interesting
part.

### Real (≈180 lines)

| Cluster | Impls | Lines | Note |
|---|---:|---:|---|
| `Hal` — `VirtioHal` / `NetHal` | 2 | ~50 | 93% shape / 90% literal. Independent confirmation of §5; CPD saw only 35 of these lines |
| `ClientMem` — `DiscardMem`, `NoMem`, `NoMem` | 3 | ~28 | **100% literal.** `NoMem` is implemented *identically in two crates* — `src/rump_proxy.rs:1354` and `akuma-rump/src/sysproxy.rs:488` |
| BKL guard family — `VfsBklGuard`, `DriverBklGuard`, `MmBklGuard`, `NetBklGuard`, `ProcessBklGuard` (+`PreemptGuard`) | 6 | ~40 | One guard per BKL carve-out phase, each copy-pasted from the last. A single generic guard replaces all of them |
| `IrqGuard` — **same name, two crates** | 2 | ~9 | `src/irq.rs:33` and `akuma-exec/src/runtime.rs:296` |
| `core::fmt::Write` hand-rolled buffers — `StackWriter`, `LazyDebugWriter`, `FmtBuf`, `Buf` | 4 | ~19 | Four near-identical stack writers. Worth checking against `console.md` § "Printing rules", which specifically discourages hand-rolled stack writers on print paths |
| `Display` for error enums — `BlockError`, `RngError`, `AudioError` | 3 | ~20 | All `match self { V => write!(f, "…") }`. A small `impl_display!` macro covers these and the 4 other `Display` impls the scorer didn't cluster |
| `Future for MultiPollFuture` — **same type name, same file** | 2 | ~14 | `src/tests.rs:2668` and `:9187` — a test helper defined twice |

### Noise (≈155 lines) — do not chase

The scorer over-clusters short impls, because after erasing identifiers any two
five-line bodies look alike:

- **`impl Drop` × 18** was reported as one TYPE-1 cluster. Most of it is just
  normal RAII: `SharedFdTable`, `CowFaultGuard`, `LifecycleGuard` and friends
  have short destructors that do genuinely different things. Only the BKL-guard
  and `IrqGuard` sub-clusters above are real.
- **`impl Default` × 13** is almost entirely `fn default() -> Self { Self::new() }`.
  That is the idiom, not duplication. A few could be `#[derive(Default)]`; none
  are worth counting as recoverable lines.

**Lesson for re-running this**: cluster size is not evidence. Read the members.

### The healthy negative

**`Filesystem` — 7 impls, 1,912 lines, zero near-duplicates.** The largest trait
in the tree by volume, and `memfs` / `ext2` / `overlay_fs` / `subdir_fs` / proc
genuinely differ. That the scorer flags `Hal` at 93% and `Filesystem` at nothing
is the evidence that it discriminates rather than flagging everything short.

### What this axis adds over CPD

CPD found the `Hal` pair (partially) and nothing else here. It missed the BKL
guard family, the duplicated `IrqGuard`, the four `fmt::Write` buffers, the
`Display` cluster and `MultiPollFuture` — all renamed-identifier clones. Running
both axes is the point; neither subsumes the other.

## 5.55 Primitives that want a leaf crate

> **Status: DONE, 2026-08-13.** `crates/akuma-primitives` exists — no
> dependencies, `core` only — and all six rungs landed. The short version: the
> table *can* move, but three smaller things had to move first, and most of the
> actual duplication turned out to be in those three. §5.555 is the summary;
> [`AKUMA_PRIMITIVES_EXTRACTION.md`](AKUMA_PRIMITIVES_EXTRACTION.md) is the full
> account, including the two places the plan below was wrong.

§5.5 catalogues duplicated trait impls. This is a different cut through some of
the same evidence, and it has a single cause worth naming:

> **Most of these copies exist because the canonical version lives in a crate the
> duplicator cannot depend on.** The bin crate owns `console::StackWriter`, and
> `src/main.rs:1656` even carries a comment telling you to use it rather than
> hand-rolling a local buffer. `akuma-exec` cannot — depending on the bin crate
> is a cycle — so it grew **three** of its own. This is not carelessness; it is
> a missing crate.

The same shape produced the `akuma-ext2 → akuma-exec` edge: `PreemptGuard` needed
a home when it was lifted out of `akuma-net`, `akuma-exec` was the only crate that
owned both it and `threading`, and now three crates compile the 23.8k-line
execution crate to get a ~40-line RAII guard.

A leaf crate — `akuma-primitives`, depending on nothing but `core` — is the fix.
Candidates, with what actually blocks each:

### Tier A — movable today, no blockers

| Primitive | Copies | Where |
|---|---:|---|
| Stack `fmt::Write` buffers | **5** | `src/console.rs:205` `StackWriter<N>`; `akuma-exec/src/threading/mod.rs:35` `StackWriter<N>` (**same name, second copy**); `akuma-exec/src/process/mod.rs:89` `FmtBuf`; `akuma-exec/src/process/children.rs:1039` `LazyDebugWriter<N>`; `akuma-exec/src/mmu/mod.rs:340` `Buf` (function-local) |
| `OnceCopy<T>` | 1, wanted by 2+ | `akuma-exec/src/runtime.rs`; already reached across a crate boundary by `akuma-ext2/src/ext2.rs:48` |
| `irq_save_mask` / `irq_restore` | 1 | `akuma-exec/src/sync.rs:17,30` — pure asm, no state |
| `IrqGuard` | **2** | `src/irq.rs:12` and `akuma-exec/src/runtime.rs:276` — same name, two crates |

§5.5 counted four stack writers; there are five. The fifth
(`threading/mod.rs:35`) shares a *name* with the bin crate's, which is how it
stayed hidden — a grep for the type name looks like one definition and its use
sites.

### Tier B — blocked, and the block is the interesting part

| Primitive | Blocker |
|---|---|
| `PreemptGuard` | Calls `threading::disable_preemption`, which is not a standalone counter: it indexes `PREEMPTION_DISABLED[tid]` by `get_current_thread_register()` (TPIDRRO_EL0) and maintains two diagnostic arrays beside it. That is scheduler state. Moving the guard means moving the thread-slot table, or reintroducing the callback pointer that was deliberately removed (`sync.rs:159` explains why: a direct call works during early boot and in host tests, a registered callback does not). |
| `NoMem` / `DiscardMem` | `src/rump_proxy.rs:1258,1353` and `akuma-rump/src/sysproxy.rs:487` — `NoMem` is implemented identically in two crates, but the `ClientMem` trait's home has to be settled first. |

**What this is worth, stated honestly.** Not line count — Tier A is maybe 120
lines of actual duplication. The payoff is that `akuma-ext2` and `akuma-net` could
stop depending on `akuma-exec` altogether, which is what makes "depends on the
execution crate" mean something again. But **Tier A alone does not achieve that**:
ext2 needs `OnceCopy` *and* `PreemptGuard`, and `PreemptGuard` is Tier B. The
guard is the long pole for the whole untangling, so if this is ever picked up,
start by deciding what happens to the thread-slot table — not by moving the easy
four.

## 5.555 `akuma-primitives`: the decision, and rungs 1–2 (2026-08-13)

§5.55 ends with "if this is ever picked up, start by deciding what happens to the
thread-slot table — not by moving the easy four." That was the right instruction
and it produced the wrong-sounding answer: **the table can move, but only after
three smaller things move first, and those three are where most of the real
duplication was.** Hence five rungs, ordered so each unblocks the next.

`crates/akuma-primitives` depends on **nothing but `core`**, by rule. Where a
primitive needs something only the kernel has — a console, a clock — it takes it
as a boot-registered `OnceCopy` hook and **degrades** when unregistered rather
than panicking. That property is what each of the copies below was hand-rolling.

| Rung | Contents | Cuts |
|---|---|---|
| 1 | `OnceCopy<T>` | — |
| 2 | console hook + one `StackWriter` + one `FmtBuf` + one `safe_print!` | — |
| 3 | **every** DAIF access in the tree — not just the 3 guards, see below | — |
| 4 | the clock hook (`OnceCopy<fn() -> u64>`) | — |
| 5 | `MAX_THREADS`, the tid **read**, `PREEMPTION_DISABLED*`, `PreemptGuard` | **`akuma-ext2`** |
| 6 | the identity `virt_to_phys`/`phys_to_virt` + the `DEV_*_VA` window | **`akuma-virtio`**, and via it **`akuma-net`** |

All six DONE 2026-08-13 (`13f5263` rungs 1–2, `069f1f0` rungs 3–6).

**Rung 3 was scoped wrong and grew.** "Three DAIF implementations" counted
*guards*; counting every DAIF *access* found nine more sites across six files,
plus a bare `mrs daif` read. Two findings fell out: `src/irq.rs`'s `disable_irqs`
and `enable_irqs` had **zero callers**, and two of the six open-coded `msr
daifclr` sites were in `akuma-exec`, which cannot reach the bin crate's
`enable_irqs` — the §5.55 shape for the third time in this phase. Everything now
routes through `akuma_primitives::irq` except `src/exceptions.rs`'s
vector-install block, where the `msr vbar_el1` / `isb` sequence must stay one asm
unit. The `isb`-vs-no-`isb` divergence between `IrqGuard` and `irq_save_mask` is
**preserved deliberately** — resolving it either way is a hot-path behaviour
change that wants a measurement, not a cleanup.

### The payoff is measurable, and §5.55 stated it as a hope

§5.55 said Tier A "does not achieve" the untangling and named `PreemptGuard` the
long pole. Both true, and the size of the pole is now exact:

- **`akuma-net`'s entire *direct* dependency on `akuma-exec` is one line** —
  `pub use akuma_exec::sync::PreemptGuard;` at `akuma-net/src/runtime.rs:50`.
  Every other `akuma_exec` mention in that crate is a comment.
- **`akuma-ext2`'s is three references to two symbols** — `OnceCopy`
  (`ext2.rs:48`; rung 1 moved the definition, so this is now just an import to
  repoint) and `PreemptGuard` (`:579`, `:601`).

> **Read `cargo tree`, not just the import list.** The first draft of this
> section claimed rung 5 frees *both* crates. It frees **one**. `akuma-net`
> depends unconditionally on `akuma-virtio`, and `akuma-virtio` depends on
> `akuma-exec` — so `akuma-net → akuma-virtio → akuma-exec` survives deleting
> `runtime.rs:50` and `akuma-exec` stays in its build regardless. Counting
> `use` statements measures *coupling*; only the dependency graph measures what
> gets compiled, and the whole point of §5.55 was compile cost.

So rung 5 frees **`akuma-ext2`** (its remaining deps are `akuma-vfs` +
`spinning_top`) and cuts `akuma-net`'s direct edge without changing what it
builds.

**Rung 6, if `akuma-net` is wanted too.** After rung 5, `akuma-virtio`'s entire
remaining need is three `mmu` items: `virt_to_phys`, `phys_to_virt` and
`DEV_VIRTIO_VA`. Both translators are literally the identity
(`mmu/mod.rs:171-178`, `#[inline(always)]`), which is what made Phase 3's "either
home was viable" note true. The constraint to respect: Phase 3 **deliberately
deleted** the indirection over them — `NetRuntime`'s function pointers cost "a
spinlocked struct read on the per-packet DMA path to reach two identity
functions" — so rung 6 must move them as plain inline fns with an `akuma-exec`
re-export, never as hooks. That relocates the identity-map assumption into the
leaf crate rather than introducing it. `DEV_VIRTIO_VA` is the weaker half of the
case: the `DEV_*_VA` layout is the L0[1] device mapping, genuinely `mmu`'s
business, and may be better passed in than moved.

### Rung 5's blocker, restated precisely

`PreemptGuard::new()` → `threading::disable_preemption()`, which indexes
`PREEMPTION_DISABLED[tid]` by `get_current_thread_register()` and maintains two
diagnostic arrays beside it. What a leaf crate needs before that can move:

1. **A console** — `get_current_thread_register` `safe_print!`s a `[FATAL]
   TPIDRRO_EL0 CORRUPT` line and halts (`threading/mod.rs:886`), and
   `check_preemption_watchdog` prints. Rung 2.
2. **A clock** — the *only* reason `disable_preemption` touches `runtime()` is a
   diagnostic timestamp on the 0→1 transition (`threading/mod.rs:1856`), and it
   already degrades to `0` when unregistered. Rung 4. This is **not** the
   callback `sync.rs` deliberately removed: that one dispatched the guard's own
   operation, so it had to work during early boot; this reads a clock,
   conditionally, for a log line.
3. **IRQ masking** — `irq_save_mask`/`irq_restore`. Rung 3.

The clean seam is **read vs write of `TPIDRRO_EL0`**. The read
(`get_current_thread_register`) is a bounds-checked `mrs` and moves. The write
(`set_current_thread_register`) does **not**: it also re-points the per-core BKL
attribution cache (`load_thread_tag_to_core`, `threading/mod.rs:907`), which is
scheduler state. `scrub_thread_slot`'s three preemption stores (`:984-986`)
become a call into the leaf crate; `threading` re-exports everything so no call
site moves.

### What rung 2 actually found: not four writers, not five — five *and three macros*

§5.5 counted four stack writers, §5.55 corrected it to five. Both undercounted,
because they counted *writers* and the duplication was in the **macro** on top:

| copy | sink |
|---|---|
| `src/console.rs:251` `safe_print!` + `:205` `StackWriter<N>` | `console::print` |
| `akuma-exec` `threading/mod.rs:68` `safe_print!` + `:35` `StackWriter<N>` | `runtime().print_str` |
| `akuma-virtio` `print.rs:24` `vprint!` | `runtime().print_str`, guarded |
| `akuma-exec` `process/mod.rs:89` `FmtBuf<'a>` | caller's buffer |
| `akuma-exec` `process/children.rs:1039` `LazyDebugWriter<N>` | `runtime().print_str` |
| `akuma-exec` `mmu/mod.rs:340` `Buf<'a>` (function-local) | `runtime().print_str` |

Three copies of the same six-line macro, and two `StackWriter<N>`s **with the
same name in different crates** — which is how the second stayed hidden, exactly
as §5.55 predicted for that pair.

`akuma-virtio/src/print.rs` deserves quoting, because it is §5.55's diagnosis
written by the duplicate itself: *"A library crate cannot reach that macro, and
the obvious substitute — `log::info!` … is not one."* It then reproduces
`safe_print!`'s contract in full. It is deleted now; there is a leaf crate that
can hold the macro.

**Result: five writers → two shapes, three macros → one.**
`StackWriter<N>` owns its buffer; `FmtBuf<'a>` borrows the caller's (kept
because `[PSTATS]`'s top-N line builds two side by side in one stack frame).
`tprint!` stays in the bin crate on purpose — its `[T<secs>.<cs>]` stamp comes
from `crate::timer::uptime_us()`, and rung 2 has no clock yet.

### The one real behaviour change, and where it was dangerous

The `akuma-exec` writers called `(runtime().print_str)(s)` directly, which
**panics** if unregistered. The shared `print_str` is a no-op instead —
`akuma-virtio` already guarded that way, and this makes its caution the rule.
Strictly safer, except in one direction that mattered:

`akuma_exec::init` is at `src/main.rs:754`. Everything between the kernel's Rust
entry (`rust_start`, `:151`) and there prints — DTB scan, memory detection, MMU
and heap bring-up, the layout assertions — and **all of it would have been
silently swallowed** if the bin crate's 1,405 `safe_print!` sites had waited for
that registration. So `rust_start` installs the hook as its **first statement**,
before any output at all. `console::print` needs no initialisation (a const MMIO
base and a volatile store), so there is nothing to order it after, and
`OnceCopy::set` ignores the later duplicate from `init`. Verified in the boot
log: `Akuma Kernel starting…`, `Kernel binary: …`, the `WARNING: Kernel is within
4MB of stack!` line and the whole `=== Memory Layout ===` block are all present,
and every one of them is pre-`init`.

Call sites did not move. `#[macro_use] extern crate akuma_primitives;` in
`main.rs` reproduces what `#[macro_use] mod console;` did for the bare
`safe_print!(…)` spelling, and a `pub use` at the crate root covers the
`crate::safe_print!(…)` spelling; both were in use. In `akuma-exec`,
`threading/mod.rs`'s ~39 bare calls needed one `use crate::safe_print;` because
they used to resolve to a `macro_rules!` a few lines below them.

### CPD scores this phase at 6%, and that is the finding

Whole-tree CPD at 50 tokens, before and after — a `git worktree` at `HEAD`
against the working tree, so it is a controlled A/B and not a recollection:

| | blocks | duplicated lines |
|---|---:|---:|
| before | 434 | 4,856 |
| after | **433** | **4,848** |

**One block. Eight lines.** That block is the `flush()` body shared by
`threading::StackWriter` and `LazyDebugWriter` — the only two of the six copies
that were byte-identical anywhere. The other ~120 lines of the cluster differ by
type name, field name, and `N` versus `self.buf.len()`, so CPD never saw them.

This is §1 and §6's caveat turned into a controlled measurement rather than two
worked examples: **on the Type-2 axis, CPD measured 6% of what was there.** The
practical consequence for the rest of Phase 4 is that CPD is the wrong
instrument for it — count code lines and count *definitions*, and do not expect
the §9 baseline to move.

Code lines (non-blank, non-comment): the 14 touched files go **9,943 → 9,721,
−222**; the new crate adds **127** non-test lines. **Net −95**, plus **113 lines
of tests the five writers never had** (truncation, mid-codepoint UTF-8 that
would make a naive `as_str` panic, the unregistered-hook no-op, two `FmtBuf`s
sharing one buffer).

### Verification

`cargo clippy --release` and `--profile extreme-size --no-default-features
--features no-tests,smoltcp,extreme,userspace-sshd` both warning-clean. **421
host tests green** (414 before; +12 new in `akuma-primitives`, −5 as `OnceCopy`'s
moved with it). QEMU `MEMORY=2048`: 94 `[PASS]`, failure set identical to a clean
tree (`retired_reclaim_ab` only — §8.5 Phase 0's known-bad threshold), and
`ssh` `uname -a` / `busybox echo` / `tcc -v` all fine.

Every rewired sink confirmed live in one boot, which matters because Phase 3
already showed how quietly console output can vanish:

| path | evidence in the log |
|---|---|
| bin crate, pre-`init` | `Akuma Kernel starting…` + `=== Memory Layout ===` |
| bin crate `tprint!` | 122 `[T<n>.<n>]` lines |
| `akuma-exec` `threading` | 631 `[Cleanup]` lines |
| `as_trace` → `print_args::<160>` | 348 `[AS-NEW]`/`[AS-FREE]`/`[AS-EXEC]`/`[AS-DEFER]` |
| `FmtBuf` (two into one buffer) | the `[PSTATS]` top-N syscall lines |
| `akuma-virtio` (was `vprint!`) | `[RNG] Found virtio-rng at slot 2`, `[Block] Capacity: 3072 MB`, `[SND]` |

That last row is the one Phase 3 warned about: following `akuma-net`'s `log::`
pattern would have deleted those lines silently, since every crate pins `log`
with `max_level_off` and no logger is ever registered.

**Found and not fixed:** `src/console.rs`'s `print_dec` (`:160`) and `print_u64`
(`:180`) are a genuine 21-line / 82-token Type-1 clone — CPD has always reported
it — differing only in `usize` vs `u64`. Untouched here because it is unrelated
to the writer cluster. It is a Phase 6 one-liner (`print_u64` delegating, or one
generic over `Into<u64>`).

## 5.6 Case study: the CoW refcount underflow (2026-08-12)

Everything above argues that copy-paste here decays *comments and
observability* rather than logic — the code stays in sync because it has to
work. Same day this audit was written, a counter-example landed: copy-paste
produced a live memory-corruption bug, in triplicate.

The CoW break path exists three times in `src/exceptions.rs`:

| Function | `cow_ref_dec` at |
|---|---|
| `ensure_cow_page_writable` (`:1050`) | `:1100` |
| `try_resolve_el1_cow_fault` (`:2350`) | `:2410` |
| `rust_sync_el0_handler_inner` (`:3018`) | `:3571` |

All three wrote `let _ = aspace.remove_user_frame(...)` and then called
`pmm::cow_ref_dec(old_pa)` **unconditionally**. The authors had reasoned about
not *freeing* — the comment "drop the bookkeeping ref but never free here" is
correct — but not about the decrement needing the same gate. The global count
therefore lost one reference **per VA broken** while it had only ever gained one
**per address space**: a refcount underflow, and underflowed CoW refcounts are
how frames get freed while still mapped (see
[`CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md`](CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md)).

The fix — `if released_last_va { … }` — had to be applied three times, and the
two later copies now carry comments reading "see the identical gate in
`try_break_cow_for_kernel_write`": the same explanation-filed-under-one-copy
pattern as §3's `replace_image` and `from_elf` pairs.

**CPD had already flagged all three as mutual clones**, in the run that produced
this document:

| Clone pair | Lines / tokens |
|---|---:|
| `ensure_cow_page_writable:1067` ↔ `try_resolve_el1_cow_fault:2375` | 13 / 95 |
| `ensure_cow_page_writable:1086` ↔ `try_resolve_el1_cow_fault:2393` | 19 / 66 |
| `ensure_cow_page_writable:1074` ↔ `rust_sync_el0_handler_inner:3542` | 5 / 54 |
| `try_resolve_el1_cow_fault:2382` ↔ `rust_sync_el0_handler_inner:3542` | 5 / 54 |

Each `cow_ref_dec` sits immediately past its function's flagged block.

**Two things this changes.**

1. **The stakes.** "Duplication costs comments and tracing" understates it. This
   one cost a refcount underflow in the page-fault path — the most consequential
   code in the tree — and cost three separate fixes.
2. **The threshold.** Every block above was found at `--minimum-tokens 50`, not
   the 100 used for §2's headline numbers. The 100-token default would have
   missed the highest-consequence duplication in the codebase entirely. The CI
   gate proposed in §8 should run at 50 for `src/exceptions.rs` and the other
   fault/CoW paths even if 150 is right for the tree at large — small clones in
   dangerous code outrank large clones in safe code.

## 6. What CPD cannot see

Two worked examples, both real, both missed:

**The `Hal` impls.** CPD reports 22 + 13 = 35 duplicated lines. The two files are
~60 lines each and are *functionally identical* — around 120 lines of clone. CPD
sees only `dma_dealloc` and `unshare`, the two methods that happen to be
token-identical; `dma_alloc`, `share` and `mmio_phys_to_virt` differ solely by
`akuma_exec::mmu::virt_to_phys(x)` versus `(runtime().virt_to_phys)(x)`. A single
substituted call expression hides two thirds of the clone.

**`process/channel.rs`.** CPD catches 11 lines at 50 tokens (`:116` ↔ `:290`).
The actual duplication is larger: `write` (`:85`) and `write_stdin` (`:280`) are
the same drop-oldest bounded-FIFO body, and `read` (`:222`) and `read_stdin`
(`:312`) are the same drain loop — differing only in which buffer field they
lock (`self.buffer` vs `self.stdin_buffer`) and what the input is called.

That second one has already cost something. The stdout side grew a second write
path — `write_bounded` (`:141`) plus `check_set_writer` (`:165`) — precisely
because drop-oldest "silently corrupts a byte-faithful stream"
(`userspace/sshd/docs/EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md`, still open per
`docs/userspace/sshd.md`). **The stdin copy never got the fix.** `write_stdin`
was still bare drop-oldest with no bounded sibling and no backpressure, reachable
from `src/vfs/proc.rs:645` and `process/spawn.rs:237`. Whether it is live-triggerable
depends on how much unread stdin a caller can queue past `MAX_BUFFER_SIZE` (1 MiB);
it is at minimum an unreviewed asymmetry, and it is the canonical copy-paste
outcome — the fix lands on one copy.

> **Resolved 2026-08-13** (Phase 0 item 5, §8.5). `write_stdin` is now bounded
> and short-writing, and the reachability question has an answer: **not through
> sshd** — but only by accident. sshd's SSH channel window was 1 MiB and never
> replenished, the same number as `MAX_BUFFER_SIZE`, so inbound stdin was capped
> at exactly the size of the buffer that would have overflowed. Two defects whose
> limits coincided, each hiding the other:
> [`SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md`](SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md).
> The lesson for the rest of this document: "the duplicated copy is probably not
> reachable" is a claim about *today's* callers, and it can be true for a reason
> that is itself a bug. The asymmetry was worth fixing on its own terms.

**Implication for tooling.** Do not treat 5,370 as the number. Treat it as the
lower bound that a tool with no semantic understanding can prove. Type-2
detection for Rust needs something else — `ast-grep`/`semgrep` patterns once you
know what to look for, or the compiler (extract the candidate into a generic and
see whether both sites still build).

---

## 7. How much line count is actually recoverable

The per-block line counts in §2 cannot simply be summed: CPD reports overlapping
blocks separately (three of the `mmu/mod.rs` walk clones share the `:1223`
region), and it reports `N` lines per block regardless of how many sites the
block has. The numbers below **union the line ranges per file** to remove the
double-counting, and define:

- **covered** — lines participating in at least one clone, counting every
  instance.
- **removable** — what disappears if each clone group collapses to a single
  copy: `covered` minus one representative copy per group.

Tree total: **96,651 lines** (test files 30,196; non-test 66,455).

| `--min-tokens` | covered | % LOC | **removable** | **% LOC** | removable, non-test | % LOC | removable, test |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 50 | 8,426 | 8.7% | **4,036** | **4.2%** | 2,815 | 2.9% | 1,221 |
| 75 | 5,198 | 5.4% | 2,561 | 2.6% | 1,984 | 2.1% | 577 |
| 100 | 3,485 | 3.6% | **1,718** | **1.8%** | 1,398 | 1.4% | 320 |
| 150 | 2,100 | 2.2% | 1,008 | 1.0% | 885 | 0.9% | 123 |

Subtract 10–20% for the parameters, enums and wrapper functions you add back, so
the practical figures are roughly:

- **~1,400–1,550 lines (1.5–1.6%)** working the 100-token set — the whole-cloned-function tier.
- **~3,200–3,600 lines (3.3–3.7%)** if you chase all the way down to 50 tokens.

Both are floors: they exclude every Type-2 clone (§6). The `Hal` pair alone is
~60 lines beyond what CPD counted, and the `channel.rs` FIFO bodies another ~30.

### It is not the tests

Test files are **31% of the tree** but only **19% of the removable duplication**
at 100 tokens (320 of 1,718) and 30% at 50 tokens. Proportionally the boot
self-tests are *less* duplicated than production code — the copy-paste is
concentrated in `exceptions.rs`, `process/mod.rs`, `syscall/fs.rs`,
`mmu/mod.rs` and `elf/mod.rs`.

So the honest headline is **~1,400 lines of production code, ~2% of the
non-test tree**, from the top ~15 clone sites in six files. The
`X`/`X_from_path` triplet (§3) is ~150 of that on its own and is the best value
per line touched, because it also collapses three divergent copies of the
ELF-loading path into one.

---

## 8. Priority

| # | Item | Lines | Effort | Risk |
|---|---|---:|---|---|
| 1 | virtio scaffolding: shared `Hal`, `virtio::probe`, one `VIRTIO_MMIO_ADDRS` | ~90 | small | low |
| 2 | `channel.rs` stdout/stdin FIFO → one helper; ~~decide whether `write_stdin` needs the `write_bounded` treatment~~ **(decided + shipped 2026-08-13, §8.5 Phase 0 item 5)** — the FIFO-merge half is still open | ~40 | small | low, but see §6 |
| 3 | ~~`akuma-isolation`/`akuma-vfs` `mount.rs` → shared half into `akuma-vfs`~~ **DONE 2026-08-13** — one `MountSet<const MAX>`, both names as type aliases, −145 code lines across 4 files; the bin crate's third path normaliser went with it (§4) | 0 left | — | — |
| 4 | ~~The `X`/`X_from_path` **quartet**~~ **DONE 2026-08-13** — `elf/mod.rs` ×3 in Phase 2a (−151 code lines, 12 clone blocks → 0), `process/mod.rs` + `process/image.rs` in Phase 2b (−105 code lines, the pairs' 60- and 47-line clone blocks → absent) | 0 left | — | — |
| 5 | `exceptions.rs` duplicated `Drop` impls (`:3703` / `:4315`) | ~142 | medium | **high** — exception path |
| 6 | `box_mod` `access.rs` / `hierarchy.rs` | ~60 | small | low |
| 7 | `rump_proxy.rs` / `akuma-rump` `sysproxy.rs` | ~23 | small | low |
| 8 | `mmu/mod.rs` `map_user_page` / `_no_flush` and the three walk clones | ~80 | medium | **high** — see `UNSAFE_AUDIT.md` §5.1 |
| 9 | Test-file clones (`tests.rs`, `process_tests.rs`) | ~669 | medium | low |
| 10 | Trait-impl clusters (§5.5): `ClientMem`/`NoMem` across crates, duplicate `IrqGuard`, BKL guard family → one generic guard, `impl_display!` macro, duplicate `MultiPollFuture`. **In progress — `akuma-primitives` rungs 1–2 done (§5.555); the `~180` is the wrong metric, see there** | ~180 | small–medium | low |

Items 1–3 are ~190 lines for an afternoon, and item 1 is the same work as
`UNSAFE_AUDIT.md`'s tier A — do it once, count it twice.

**Sequencing note.** Do the structural work (this doc) before the mechanical
`unsafe` sweep (`UNSAFE_AUDIT.md` P0). Deleted code needs no `unsafe`
conversion, no SAFETY comment, and no re-verification — converting call sites
you are about to delete is wasted effort. The one exception is the user-copy
sweep, which is syscall-layer and overlaps nothing here.

---

## 8.5 Suggested order of work

Principles, in priority order: **defects before cleanup**, **structure before
mechanics** (deleted code needs no `unsafe` conversion, no SAFETY comment and no
re-verification), **verification before risky merges**, **lint ratchet last** so
the baseline you freeze is the real one.

Cross-cutting constraint as of 2026-08-12: a second agent is working in
`src/config.rs`, `src/main.rs`, `src/pmm.rs`, `src/exceptions.rs` (the KTG and
CoW paths) and `userspace/`. Phases are ordered partly to avoid their files.

### Phase 0 — real defects, independent of any refactor

**DONE 2026-08-13.** All five landed together; `cargo clippy` clean, 413 host
tests + 11 sshd lib tests green, QEMU-verified (details per row).

| | Fix | Files | Status |
|---|---|---|---|
| 1 | `rng.rs`: clamp `copy_len` to `to_read`, not caller-remaining | `src/rng.rs` | **DONE.** Also added a `copy_len == 0` guard — a device completing the descriptor without writing spun the outer loop forever, since `bytes_read` never advanced |
| 2 | `rng.rs`: ring `idx` → `AtomicU16` with Release/Acquire | `src/rng.rs` | **DONE.** `VirtqAvail`/`VirtqUsed`'s `idx`/`flags`/`*_event` are `AtomicU16` (same layout, `repr(C)` unchanged); release store on publish, acquire load on completion. The pre-notify `fence(SeqCst)` **stays** — it orders Normal memory against a Device-memory store, which the release does not cover |
| 3 | ext2 thread hooks → lock-free cell | `crates/akuma-ext2`, `crates/akuma-exec/src/runtime.rs`, `src/fs.rs` | **DONE**, but *not* with a `Spinlock` as written above — see the note below. −5 `unsafe`; `init_thread_hooks` is now a safe fn |
| 4 | `MADV_FREE` → `EINVAL` (unblocks redis) | `src/syscall/mem.rs` | **DONE.** Verified in-VM: `-1 errno=22`, `MADV_DONTNEED`/`MADV_WILLNEED` untouched. The `MADV_DONTNEED` divergence is **still open** and is now the documented tripwire — see below |
| 5 | Decide `write_stdin` backpressure (§6) | `process/channel.rs` + 4 more | **DONE — decided: short write, not blocking.** Uncovered a second, older bug in the process; both fixed. See below |

**Item 3 — why not a `Spinlock`.** The row above said `Spinlock`, and that would
have been wrong. `akuma-exec/src/runtime.rs` already carries `OnceCopy<T>` for
exactly this shape (`RUNTIME`/`CONFIG`), and its doc comment explains why:
*"No spinlock — readers must never block on writers, because reading
`RUNTIME`/`CONFIG` from inside an IRQ that interrupted code holding the same
lock would self-deadlock on a single CPU."* The ext2 hooks are read from the
lock-acquisition retry loops and have the same hazard. `OnceCopy` was made
`pub` and reused rather than a second mechanism invented — which is the point of
this document. A release store at `init_thread_hooks`, an acquire load at each
read, and the `static mut` data race is closed at zero cost.

**Item 4 — what is still open.** Allocators that probe `MADV_FREE` (jemalloc,
mimalloc) fall back to `MADV_DONTNEED` on `EINVAL`, and this kernel's
`MADV_DONTNEED` still zeroes the *physical frame* where Linux drops the
*mapping* — so on a CoW-after-fork or `file_page_cache` frame it also wipes the
peer's live copy (`akuma-exec/src/mmu/mod.rs` `zero_mapped_page` takes no
sharing into account). That divergence predates this change; what changed is how
much traffic can reach it. The existing `DONTNEED_SHARED_FRAME` /
`DONTNEED_UNALIGNED` counters are the tripwire, reported on the 30 s `[MADV]`
PSTATS line. Reading `dontneed_unaligned=0 dontneed_shared_frame=0` after a
boot's worth of sshd sessions, a tcc compile and an 8 MiB hash, 2026-08-13. **If
that starts climbing, fixing `MADV_DONTNEED` to break sharing rather than zero in
place is the prerequisite — not backing item 4 out.**

**Item 5 — the decision, and the bug underneath it.** Blocking the writer (the
symmetric `check_set_writer` treatment the stdout half got) is *wrong here*:
sshd's `bridge_process` is a single loop that must keep draining the child's
stdout to create the stdin space it would be waiting on, which is the deadlock
its own "make BOTH ends non-blocking" comment exists to prevent. So: `write_stdin`
returns bytes accepted and never drops; the count is carried out through
`write_to_process_stdin` → `ProcFilesystem::write_at` → `sys_write` as a short
write; sshd's stdin fd joined the non-blocking set and gained a residue queue
with deferred EOF. `sys_write`'s `File` arm returns `EAGAIN` on a 0-byte accept,
because falling through would spin the chunk loop forever in the kernel.

Verifying it turned up an older, unrelated defect that had been masking this one:
**sshd advertised a 1 MiB SSH channel window and never sent
`SSH_MSG_CHANNEL_WINDOW_ADJUST`**, so no session could ever carry more than
1 MiB of stdin. That window (`0x100000`) is the same number as `MAX_BUFFER_SIZE`,
which answers §6's open question — the drop-oldest overflow was **not**
reachable through sshd, only because a second missing feature capped inbound
stdin at exactly the buffer's size. Fixing either alone would have been worse
than fixing neither: the window alone converts a visible hang into silent
corruption. Full writeup:
[`SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md`](SSHD_CHANNEL_WINDOW_NEVER_ADJUSTED.md).
Verified with 4 MiB through `cat` and 8 MiB through `sha256sum`, both byte-exact.

Not a defect after all: the unchecked `read_*_le` helpers in `elf/types.rs` were
suspected of being a `panic = "abort"` vector on malformed input. 280 hostile
header-field mutations panic neither parser — the guards hold. Closed.

**Unrelated known failure, do not chase.** `[FAIL] retired_reclaim_ab: … ON
recovered 745p … expected >=768p` fires on an unmodified tree (A/B-confirmed
2026-08-13 by stashing the diff and rebooting; identical numbers both arms). The
mechanism works — 0p vs 745p is a clean separation — only the pass threshold is
too tight. It is the sole `[FAIL]` in a default boot, so anything touching memory
looks guilty; diff the failure *sets*:
`diff <(grep -aoE "\[FAIL\] [a-z_0-9]+" base.log | sort -u) <(… mine.log …)`.

### Phase 1 — verification scaffolding

Done for the ELF parser: 2,387 binaries, 0 disagreements (§3). The hand-rolled
parser deletion is evidence-backed. Nothing left to build.

### Phase 2 — ELF quartet, split by file ownership

**2a — DONE 2026-08-13.** `crates/akuma-exec/src/elf/` is now five files:
`mod.rs` (37 lines, re-exports only), `source.rs`, `load.rs`, `interp.rs`,
`stack.rs`. All four sub-parts landed: the hand-rolled parser is deleted, the
source and mapping axes are separate parameters, all three `elf/mod.rs` pairs
are merged, and the file split fell out of the merge exactly as predicted.
Details, and the four places the plan was wrong, below.

**What the numbers actually were.** CPD over just this directory, at the
50-token threshold §5.6 argues for:

| | blocks | duplicated lines |
|---|---:|---:|
| before | 12 | 254 |
| after | **0** | **0** |

Executable lines (excluding comments, blanks and `#[cfg(test)]` blocks):
**1,074 → 923, −151 (−14%)**. Total lines are roughly flat (1,580 → 1,617)
because the merge came with 63 more lines of test and ~140 more of comment.

**§3's "~486 lines for these two files" was about 3× optimistic**, and the
overshoot is instructive rather than embarrassing:

1. **The abstraction is not free.** `ElfSource` + `ElfHeaders` are ~121 lines
   that did not exist in any form before. §7's "subtract 10–20% for the
   parameters and wrappers you add back" is the right idea at the wrong
   magnitude — when the *point* of a merge is to introduce a seam, the seam is
   a large fraction of what you save.
2. **The pairs were not independent.** §3 sized `load_elf`, `load_interpreter`
   and `load_elf_with_stack` as three separate merges and summed them. They
   share the page-mapping loop, so collapsing that loop is counted three times
   in the estimate and once in reality.
3. **Line counts in the table were whole-function extents** (comments and
   blanks included) measured against code-only merged sizes.

The duplication is what actually went away, and it went away completely: **one
page-mapping loop** (`map_segment_eager`) where there were three, **one
relocation applier** (`apply_relocations`) where there were two divergent ones,
**one ELF parser** where there were two.

**What each sub-part turned into.**

- **The parser deletion is smaller than advertised and the tests are the
  point.** `types.rs` lost 41 lines of code, not 90 — §3's figure counted the
  doc comments and blanks around `Elf64Ehdr` / `Elf64Phdr` /
  `parse_elf64_phdr` / `read_u{16,32,64}_le`. What it bought is not lines: the
  `elf` crate's `parse_ident` / `parse_tail` / `ParsingTable` bounds-check
  everything the hand-rolled reader indexed raw, and `ProgramHeader::
  validate_entsize` rejects an `e_phentsize` that is not 56 instead of using it
  as a stride. §3's proposed API worked exactly as described, first try.
- **The `elf` crate's low-level pieces are `pub`,** including
  `parse::ParseAt::validate_entsize`, which §3 did not know. That is what makes
  the lazy path's header validation equal to `minimal_parse`'s rather than
  merely similar.
- **`ElfSource::read_at` returns `Cow<'a, [u8]>`.** Borrowed for a byte source,
  owned for a path source. So the eager loader still copies straight out of the
  slurped image with no intermediate allocation, and the lazy loader still holds
  no more than the chunk it is reading. That is the detail that let one loop
  serve both without a performance regression on either.
- **`MapStrategy::Deferred { path, inode }` carries the pager's inputs,** which
  makes `Deferred` over an in-memory image *unrepresentable*, not merely unused.
  §3's semantic trap resolves itself: `load_elf_with_stack` now returns
  `loaded.deferred_segments` unconditionally, and it is empty for an eager load
  because nothing populated it. The distinction survived by becoming data.

**Four behaviour differences the merge had to resolve** — the pairs were not as
identical as CPD's token view suggested:

1. **Relocations, symbol-less and unresolvable.** The two appliers disagreed:
   the main-binary copy wrote `sym_value + addend` (with `sym_value` left at 0)
   for a GLOB_DAT/JUMP_SLOT naming symbol 0 or an out-of-range symbol; the
   interpreter copy wrote nothing. Converged on the interpreter's rule (only
   ABS64 has a defined meaning without a symbol). **Verified, not assumed**: a
   scan of every ET_EXEC binary in the tree found 118 carrying SHT_RELA, 2,832
   relocations, **zero** symbol-less ABS64/GLOB_DAT/JUMP_SLOT and **zero**
   out-of-range symbol indices. The rule changes nothing on real input.
2. **Malformed-input strictness.** The eager path silently skipped a PT_INTERP
   or a segment whose file extent ran past EOF (`if off + sz <= elf_data.len()`);
   the lazy path propagated the error. Converged on strict. Silently dropping
   PT_INTERP on a dynamic binary means jumping to a non-relocated entry point —
   a SIGSEGV with no explanation, where the error gives a clean ENOEXEC.
3. **`InvalidMagic` vs `InvalidFormat`.** The eager path reported
   `InvalidMagic(bytes)` for any parse failure including truncation; the lazy
   path reported `InvalidFormat("Bad magic")` with no bytes. Now both read 4
   bytes *on the failure path only* and report `InvalidMagic` when the magic
   really is wrong, `InvalidFormat` when it is right but the file is truncated.
   The four bytes are what tell you "it's a shell script" at a glance.
4. **Addend arithmetic.** Both copies used plain `+` on
   `r_addend as usize`, which is a panic if the kernel is ever built with
   overflow checks on (negative addends become huge `usize`s). Now
   `wrapping_add`, which is what release builds were already doing.

**PN_XNUM is now rejected explicitly.** `minimal_parse` handled `e_phnum ==
0xffff` by reading the real count from `shdr[0].sh_info`; the hand-rolled parser
would have tried to read 65535 phdrs. Neither behaviour is reachable by anything
this kernel executes, so the merged parser errors out by name rather than
carrying a branch nothing tests.

**Verification.** `cargo check --release` and `cargo clippy --release` clean;
414 host tests green (13 new, covering the parsing the deleted parser used to
do — including a resident subset of §3's hostile-header mutations, since the
kernel builds `panic = "abort"`). QEMU, `MEMORY=2048`, failure set identical to
a clean tree (`retired_reclaim_ab` only) across every boot:

| path | exercised by |
|---|---|
| Bytes + Eager | `/bin/sshd`, `/bin/paws`, `/bin/tcc` (229 KB) |
| Path + Deferred | `/bin/busybox` (1,116,408 B, over the 1 MiB `HEAP_SLURP_MAX`) |
| interpreter, Bytes source | `/usr/bin/tree` + `/lib/ld-musl-aarch64.so.1` |
| interpreter, **Path** source | same, under a temporary `__probe_path_interp` feature |

That last row needed a trick worth recording. The Path-source interpreter branch
is `#[cfg(kernel_profile_extreme)]`, and **`ssh host <cmd>` is broken on the
extreme-size profile** — every session dies instantly at `FAR=0x10
ELR=0x415330` inside sshd, interactive PTY included. That is **pre-existing**:
A/B-confirmed by stashing the whole diff, rebuilding extreme-size and getting
byte-identical fault registers. So the branch was exercised instead by adding a
throwaway crate feature that forces `ElfSource::Path` on the release profile,
where ssh works — same code, drivable box. The feature was removed afterwards.

> **The extreme-size sshd fault is a degradation, and "pre-existing" undersells
> it.** All the A/B above establishes is that *this phase* did not cause it. It
> is not pre-existing in the sense of never having worked:
> `acceptance/05_meow_tcc_extreme_4mb.md` — one of the four live playbooks —
> drives `ssh()` against this exact profile throughout (`pkg install`,
> `exec /tmp/hello_c`), so it was written against a working extreme-size sshd.
> **That playbook is therefore currently unrunnable.**
>
> Nobody has bisected it, so the breaking change and its date are unknown, and
> no phase in §8.5 owns it. Two things make it easy to under-rate: it is invisible
> on every other profile (release, devbox and rump builds ssh fine), and the
> extreme-size profile is the one nothing routinely boots. Treat the cost as "one
> acceptance playbook is dark", not "one profile has a cosmetic fault".
>
> What is known: every session dies at `FAR=0x10 ELR=0x415330` inside sshd,
> interactive PTY included, and `FAR=0x10` is a null-ish struct-field
> dereference rather than a wild pointer. Bisecting wants the extreme-size build
> (`scripts/build_extreme_size.sh`), not the default one.

**Disk state note.** Verifying the interpreter needed a dynamically-linked
binary, and `disk.img` had none — no `/lib/ld-musl-aarch64.so.1` at all, which
is why the interpreter loader had gone so long without a live test. It now
carries `apk add musl tree` plus two ubase binaries at `/bin/dyn_pwdx` and
`/bin/dyn_df`. Keep them: the ELF loader has no other in-VM coverage of the
dynamic path.

**2b — DONE 2026-08-13.** The consumer half of the quartet. Two commits, as
planned: a pure move of `from_elf` / `from_elf_path` out of `process/mod.rs`
into `process/image.rs` (bodies byte-identical, verified by diffing the cut
region against the pasted one; the only other change is imports), then the
merges. Both `ProcessImage` pairs now run one body each, selected by a value:

```rust
enum ImageSource<'a> {
    Bytes(&'a [u8]),                             // in the heap already → eager
    Path { path: &'a str, file_size: usize },    // still on disk → demand-paged
}
```

`replace_image` / `replace_image_from_path` → `replace_image_from(source, …)`;
`from_elf` / `from_elf_path` → `from_image(name, source, …)`. The four public
entry points survive unchanged as one-line wrappers, so no caller moved.

**What it bought, in order of value:**

1. **The five `[FORK-DBG]` lifecycle traces now cover both exec paths.** They
   existed only on the in-memory one, so which half of `execve` was traceable
   depended on the binary's size (`HEAP_SLURP_MAX`). This was the point of the
   phase; the lines saved are the side effect.
2. **Each piece of load-bearing reasoning now sits next to the code it
   describes.** The preemption-guard comment no longer says "(in the `from_path`
   variant) does block I/O" from inside the variant that doesn't — it says "for
   an `ImageSource::Path`, does block I/O", in the one function both sources run.
   The constructors' six-line note on why the pid-keyed `push_lazy_region` can't
   be used before the `Process` exists, and the replacers' three-line note on why
   it can't be used while holding `&mut self`, are **different** reasons for
   different situations: both were kept, one per site. The `from_elf_path`
   three-line summary deferring to "the sibling constructor" is gone, because
   there is no sibling any more.
3. **`LoadedWithStack` is a struct, not an 8-tuple.** Every consumer used to open
   with the same `let (a, b, c, d, e, f, g, h) = …`, which is a large part of why
   two functions differing in four places looked like two unrelated 100-line
   walls. Four destructures → four field-named bodies. Only these four call sites
   existed, so it was a contained change.

**Measured.** CPD at 50 tokens over the three touched files
(`process/mod.rs`, `process/image.rs`, `elf/stack.rs`):

| | blocks | duplicated lines |
|---|---:|---:|
| before | 22 | 479 |
| after | 14 | 302 |

Both target blocks are gone: the 60-line/404-token `from_elf` pair and the
47-line/258-token `replace_image` pair. Code lines (non-blank, non-comment)
across those three files: **2,337 → 2,232, −105.**

**§3's ~165 was ~1.6× optimistic** — the same failure mode as Phase 2a's 3×, one
step smaller because there was less new seam to build: `ImageSource` plus the
shared `push_deferred_regions` helper are ~50 lines that did not exist in any
form. §3's 165 also counted whole-function extents (comments and blanks) against
a code-only merged size.

**Three differences the diff-before-merging rule caught**, none of them
duplication:

1. **Region-push order.** The constructors pushed heap+stack into a fresh
   `LazyRegionMap`; the path constructor pushed the image's deferred segments
   *first*. The merged body always does deferred-then-heap-then-stack, which is
   a no-op reordering for a byte source because `deferred_segments` is empty
   there — the Phase 2a invariant paying off a second time.
2. **The heap-usage debug line** (`[Process] heap before ELF load: …MB`) existed
   only on the path constructor. Kept, for both: the *eager* source is the one
   that has already slurped the whole binary into that heap, so it is the more
   interesting of the two to see a figure for.
3. **`(on-demand)` in the "PID n replaced" debug line** was the only remaining
   text difference between the replacers. It is now derived from
   `loaded.deferred_segments.is_empty()` rather than passed in — the mapping
   strategy reporting itself, not the caller asserting it.

**Verification.** `cargo check --release`, `cargo clippy --release` and
`cargo clippy --profile extreme-size --no-default-features --features
no-tests,smoltcp,extreme,userspace-sshd` all warning-clean; 414 host tests green.
QEMU `MEMORY=2048`, 93 `[PASS]`, failure set identical to a clean tree
(`retired_reclaim_ab` only). All four merged paths exercised in one boot:

| path | exercised by |
|---|---|
| `from_image` + Bytes | every small kernel spawn at boot (`hello`, herd, sshd) |
| `from_image` + Path | `[PASS] test_sigpipe_terminate_no_deadlock` — spawns `/bin/busybox` (1.09 MB) through `spawn_process_with_channel` |
| `replace_image_from` + Bytes | `/bin/tcc -v` (229 KB) and `/usr/bin/tree --version` over ssh |
| `replace_image_from` + Path | `/bin/busybox echo` over ssh |

The boot log also carries `execve: replace_image failed for …: Invalid ELF
magic: 6e 6f 74 2d` — `process_tests.rs`'s deliberate non-ELF execve, reporting
`"not-"`, which is Phase 2a's `InvalidMagic`-vs-`InvalidFormat` refinement
running through the merged replacer.

**What is left in these files, and it is not this quartet.** The 14 residual CPD
blocks are almost entirely `process/mod.rs`-internal, and the largest family is
the `Process` struct literal written out four times — `fork_process`,
`vfork_process`, `clone_thread` and `from_image`, ~45 fields each. That is a
Phase 6 candidate with a real hazard behind it (a field added to `Process` has
four initialisers to find), but it is a different pair-of-eyes problem from the
`X`/`X_from_path` pattern and was left alone.

### Phase 3 — virtio consolidation

**DONE 2026-08-13**, and it went further than planned: rather than merging the
duplicated pieces in place, they moved into a new leaf crate,
**`crates/akuma-virtio`** — `hal.rs` (the one `Hal`), `probe.rs`, `print.rs`,
plus the `block` / `rng` / `audio` drivers lifted out of the bin crate.

**Why a crate and not an `akuma-exec` submodule.** The `Hal`'s entire dependency
surface is `virtio_drivers::Hal`, `alloc`, and
`akuma_exec::mmu::{virt_to_phys, phys_to_virt}` — and *both translators are the
identity* (`paddr as *mut u8` / `vaddr`, `#[inline(always)]`). So either home was
viable. The crate won because the drivers moved too: at ~120 lines it would have
been the smallest crate in the tree, but with `rng` (596) + `audio` (359) +
`block` (336) it is ~1,400, comparable to `akuma-vfs`. Putting the `Hal` in
`akuma-exec` instead would have meant the driver crate depending on `akuma-exec`
for it anyway — the same edge, worse layering.

**What was actually duplicated** was worse than §5 counted: not 3 copies of
`VIRTIO_MMIO_ADDRS` but **5** (the 4th inline in `main.rs`, the 5th implied by
`rump_tap.rs`), and **2** spellings of the device-id offset (`0x008` literal vs
`VIRTIO_MMIO_DEVICE_ID_OFFSET`).

**Four things the merge had to resolve** — none of them visible to CPD:

1. **`log::` is not a printing mechanism in this tree.** Every crate pins `log`
   with `max_level_off` and the kernel registers no logger, so `log::info!`
   compiles to nothing — which is why `akuma-ext2` "prints nothing" and
   `akuma-net`'s `[SmolNet]` lines never appear. Following that pattern would
   have silently deleted every `[Block] Capacity: …` / `[RNG] Found …` line from
   the boot log. The drivers use a `vprint!` shim over `akuma-exec`'s registered
   `print_str` hook, preserving CLAUDE.md's no-alloc console rule.
2. **`probe()` alone would have changed behaviour.** `block.rs`/`audio.rs` kept
   scanning when a matching slot failed to yield a working device; a
   find-first-then-build helper silently turns "try the next virtio-blk" into
   "give up". Hence `probe_with`, which preserves the retry.
3. **`smoltcp_net.rs` aborted the entire scan** on transport-init failure where
   block/audio skipped the slot. Converged on skip-and-continue (strictly more
   forgiving), with the skip logged so a failure on the slot you cared about
   stays visible.
4. **`FmtBuf` had to become `pub`** so the drivers could format without
   hand-rolling a *fifth* stack writer — the same "reuse rather than invent"
   move Phase 0 item 3 made with `OnceCopy`. See §5.55.

`NetRuntime`'s `virt_to_phys`/`phys_to_virt` are gone (zero readers outside the
deleted `hal.rs`), as is `src/virtio_hal.rs`. The `mmio_addrs` parameter is gone
from `akuma_net::init` / `smoltcp_net::init` / `rump_tap::init` — every caller
passed the same table.

**Not done:** the redundant `UnsafeCell` in `block.rs`/`audio.rs` was listed in
the original plan and left alone; it is unrelated to the duplication and wants
its own look.

**Verification.** `cargo clippy` clean on `--release`, `extreme-size`,
devbox-rump and devbox-smoltcp, plus `akuma-net` × 3 feature sets and
`akuma-virtio` × 2; 414 host tests green. QEMU `MEMORY=2048`: all three drivers
init through the shared probe, 93 `[PASS]`, failure set identical to a clean
tree (`retired_reclaim_ab` only). On devbox-smoltcp at SMP=4: `curl` HTTP **and**
HTTPS (the latter exercising the moved RNG through the TLS handshake),
`apk add redis`, an in-VM `git clone --depth 1` of this repo, and
`redis-server --test-memory 512` ("Your memory passed this test").

**Two things this verification turned up, both pre-existing:**

- **~~The rump devbox cannot ssh~~ FIXED 2026-08-13** —
  `kex_exchange_identification: Connection reset by peer`. **A/B-confirmed
  pre-existing** (identical on `f09de7d`, the parent commit), and the A/B was
  right. Root cause had nothing to do with DHCP (which was working the whole
  time — `rump_server` logs to a file, not the console): `RumpSocket` was the one
  fd family `clone_deep_for_fork` did not refcount, so sshd's post-`fork`
  `drop(stream)` closed the socket out from under its own session child.
  `DEVBOX_ISSUES.md` Issue 10 →
  [`RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`](RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md).
- **No IPv6 anywhere in the stack**, which blocks in-VM `cargo` against a live
  crates.io (Fastly's DNS answer is IPv6-heavy; standalone `curl` falls back to
  IPv4, cargo's libcurl does not). Never implemented — `akuma-net` builds
  smoltcp `proto-ipv4` only. `DEVBOX_ISSUES.md` Issue 9. This is what blocked
  the in-VM self-host build; the runbook's vendored `--offline` route is
  unaffected.

### Phase 4 — trait-impl clusters (§5.5) — IN PROGRESS

**Read §5.555**, which is the plan of record: `akuma-primitives` exists, rungs 1
(`OnceCopy`) and 2 (one console hook, one `StackWriter`, one `FmtBuf`, one
`safe_print!`) have landed, and rungs 3–5 are what remains of the blocked half.

**§5.5's "≈ −180 lines" is not the right target and never was.** Rung 2 removed
five writers and three macro copies for a **net −95** code lines, because a leaf
crate that has to *earn* its console (a registered hook, degrading when
unregistered) is 127 lines of new seam. Same failure mode as §3's estimates for
Phase 2a (3× optimistic) and 2b (1.6×), one step further: when the point of the
work is to build a seam, the seam is most of what you "save". Judge Phase 4 by
definitions collapsed and dependency edges cut, not by line count — and do not
expect CPD to register it at all (§5.555 measured 6%).

Still open on the **unblocked** half — none of these need a new crate, and all
four are mop-up once rungs 3–5 settle the hard part:

| Item | Why it is unblocked |
|---|---|
| `impl_display!` for `BlockError`/`RngError`/`AudioError` | Phase 3 collected all three into `akuma-virtio`; it is now an intra-crate macro |
| BKL guard family → one generic guard | 4 of the 5 (`Net`/`Mm`/`Vfs`/`Driver`) are in `src/syscall/`; only `ProcessBklGuard` is in `akuma-exec` |
| twice-defined `MultiPollFuture` | both copies are in `src/tests.rs` (`:2663`, `:9182`) |
| `ClientMem`/`NoMem` across two crates | needs `ClientMem`'s home settled (`src/rump_proxy.rs` vs `akuma-rump`), not `akuma-primitives` |

### Phase 5 — the user-copy sweep (−167 `unsafe`, 19% of the tree)

Safe slice-based API with `validate_user_ptr` folded in (closes the unchecked
destination hole), then 167 mechanical conversions. All in `src/syscall/*`, so
it overlaps nothing above — but it is a large diff, so do not hold it open
alongside Phase 2's.

### Phase 6 — remaining duplication — IN PROGRESS

**DONE 2026-08-13:** the `mount.rs` shared half into `akuma-vfs` (§8 item 3) as
one `MountSet<const MAX: usize>`, and with it the bin crate's third path
normaliser — which turned out to be a real inconsistency, not a spelling
difference: `.`/`..` were resolved only when a process was current (§4).

**Still open, in the order they are worth doing:**

| Item | Notes |
|---|---|
| `channel.rs` FIFO merge + restore the missing traces (§8 item 2) | the `write_stdin` backpressure half already shipped in Phase 0 item 5; this is the remaining stdout/stdin body merge |
| `box_mod` `access.rs` / `hierarchy.rs` (§8 item 6, ~60 lines) | low risk |
| `rump_proxy.rs` / `akuma-rump` `sysproxy.rs` (§8 item 7, ~23 lines) | overlaps Phase 4's `ClientMem`/`NoMem` question — settle `ClientMem`'s home once, for both |
| `src/console.rs` `print_dec` / `print_u64` | a genuine 21-line / 82-token **Type-1** clone differing only in `usize` vs `u64`; CPD has always reported it. Found during the `akuma-primitives` work, unrelated to it |
| `exceptions.rs`'s duplicated `Drop` impls (§8 item 5, ~142 lines) | **high risk — exception path.** Do last, and read `UNSAFE_AUDIT.md` §5.1 first |

### Phase 7 — `#[repr(C)]` `Statx` + `SigFrame`

Three blocks, −281 unsafe *operations*. Judge by §3.3, not by line count.

### Phase 8 — quality floor

SAFETY comments (11% coverage today), `clippy::undocumented_unsafe_blocks`
starting on the clean crates and ratcheting inward, `missing_safety_doc` allows
removed, and the CPD CI gate (§9) — at **50 tokens for the fault/CoW paths**,
per §5.6.

### Promoted out of "deferred"

The CoW/`mmu` cluster (`map_user_page` / `_no_flush` and the walk clones) was
parked as high-risk-low-payoff. §5.6 is the payoff: that cluster demonstrably
produces memory corruption. Still do not touch it while the other agent is in
`exceptions.rs` — but it is no longer optional.

Still deferred, genuinely: `Mmio<T>` (~−25 `unsafe`), safe sysreg readers (~−27,
relocation not removal), the `Pte`/`PageTable` newtype (~−50). Specifically do
**not** pick up `Mmio<T>` while in the driver layer for Phase 3 — it reaches into
GIC, console and pmm and has a different blast radius.

### Running total

Phases 0–5 land roughly **−240 `unsafe` (27%)** and **−800 lines**, and close
four real defects. Phases 6–8 add ~−300 more lines and the hygiene floor.

## 9. Re-running

The procedure — install, run, aggregate, the traps, and the CI gate — is a
runbook: [`../runbooks/find-duplicated-code.md`](../runbooks/find-duplicated-code.md).
It carries the union-aggregation script that produced §7's numbers.

Baseline to compare against, 2026-08-12 on a clean tree:

```
--minimum-tokens 100 → blocks=92 covered=3485 removable=1718
--minimum-tokens 150 → blocks=38
```

---

## Background

- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) — the `unsafe` census; §4 P2 covers the
  virtio HAL findings in depth, including the two defects in `src/rng.rs`'s
  hand-rolled virtqueue.
- `userspace/sshd/docs/EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md` — the open
  drop-oldest truncation bug whose fix reached only the stdout copy (§6).
- `docs/reference/subsystems/locking.md` §399 and
  `docs/runbooks/recover-wedged-vm.md` — the `ProcessChannel` lock-discipline
  history around the same code.
