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
| 60 | `akuma-exec/src/box_mod/access.rs:122` `cascade_kill_order` ↔ `box_mod/hierarchy.rs:75` `validate_nested_root` — **the wrong two symbols; see below** |
| 63 (3 blocks) | `akuma-isolation/src/mount.rs` ↔ `akuma-vfs/src/mount.rs` |
| 23 | `akuma-rump/src/sysproxy.rs` ↔ `src/rump_proxy.rs` — **DONE 2026-08-13**, and there were three impls, not two |
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

### The `box_mod` pair names the wrong two symbols (found 2026-08-13)

CPD's 60 lines are real; the attribution is not. `cascade_kill_order`
(`access.rs:122`) is **four lines** delegating to `hierarchy::get_descendants`;
`validate_nested_root` (`hierarchy.rs:75`) is a ~35-line path-prefix validator
with a component-boundary check. They share no logic at all.

What is byte-identical is **`make_test_registry()`**, defined once in each file's
`#[cfg(test)] mod tests` — same `BoxInfo` literals, same tree. That is where the
60 lines live.

So item 6 is not a production-code merge; it is test-fixture duplication, and it
belongs with **item 9**, not ahead of it. Which also downgrades it: a shared
fixture is the most harmless kind of clone, and the doc's own §6 argument — that
duplication costs you the *next fix* — barely applies to a `BoxInfo` literal.

The lesson repeats §5.555's: **CPD reports a location, not a subject.** A block
that straddles or lands inside a test module gets attributed to the nearest
preceding item in the file listing, and the survey copied that attribution
without opening the file.

### `ClientMem`'s "home question" was already answered (found 2026-08-13)

§8 item 7 and Phase 4 both carried this as *needs a decision*: does `ClientMem`
belong in `src/rump_proxy.rs` or `akuma-rump`? There was nothing to decide. The
trait has lived at `akuma-rump/src/sysproxy.rs:80` throughout, `akuma-rump` is a
dependency-free leaf, and `src/rump_proxy.rs:15` **already imports it**. The bin
crate had simply grown its own private `NoMem` next to the import of the trait it
implements.

**DONE 2026-08-13**, and it was three impls rather than the two the table names:

| was | where | difference |
|---|---|---|
| `NoMem` | `akuma-rump/src/sysproxy.rs:487` (private) | — |
| `NoMem` | `src/rump_proxy.rs:1405` (private) | byte-identical |
| `DiscardMem` | `src/rump_proxy.rs:1310` | `copyout` returns `Ok(())`; errnos spelled `14` rather than `EFAULT` |

Now one `pub struct NoMem` in `akuma-rump` with the single varying axis as a
field, reached through named constructors — `NoMem::faulting()` and
`NoMem::discarding()` — because a bare struct literal at a call site would not
say which behaviour was meant. Six kernel call sites and one in-crate site moved;
the bin crate's `const EFAULT` went with the copy, since that was its only user.
Two host tests pin both behaviours, so the discarding half cannot be lost the way
§6's `write_bounded` was.

The bare `14` is a live instance of **§5.7**: the comment carried the meaning and
the literal carried the behaviour, one file away from the constant that names it.

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

## 5.7 Errno spellings in the syscall layer (raised 2026-08-13, DONE 2026-08-14)

> **Status: DONE 2026-08-14.** One table, in `akuma_primitives::errno`, with the
> pre-negated `u64` forms **generated** from the positive `i32` ones by
> `neg_errno` at compile time — so the two representations can no longer disagree.
> 57 production definitions across four modules in two crates and 59 more
> re-declarations inside five test files collapsed onto 39 names; 120 raw negated
> literals and 30 hand-rolled `i64::from(-libc_errno::X) as u64` negations are
> gone. 5 host tests (512 → 517). What the audit below got wrong, and the two
> comment/value drifts it correctly predicted, are recorded after it.
>
> **It was run on its own, ahead of Phase 5's user-copy sweep, not with it.**
> The sequencing argument below still holds — both passes touch the same syscall
> arms — but settling the table first means the sweep rewrites each arm once, and
> a 213-line diff that changes no behaviour is worth A/B-ing by itself rather
> than inside a 167-site `unsafe` conversion.

**The audit as raised** (the counts are the ones it got wrong; see below).
Return values should use named constants; today three spellings coexist and one
file uses two of them.

**Three tables, one of them a third of a table:**

| where | form | count |
|---|---|---|
| `src/syscall/mod.rs:405` | `const E*: u64`, **pre-negated** (`(-22i64) as u64`), private to the bin crate | 27 |
| `crates/akuma-net/src/socket.rs:966` `pub mod libc_errno` | `pub const E*: i32`, **positive**, re-exported and used across the bin crate | 25 |
| `src/syscall/msgqueue.rs:17` | two more pre-negated consts, local to the module | 2 |

17 names are defined in both of the first two, in two different
representations, bridged by `neg_errno()`. `E2BIG` in `msgqueue.rs:18` is
byte-identical to `mod.rs:410`, one module away.

**Plus 109 raw negative literals** that bypass all three:

| file | sites |
|---|---:|
| `src/process_tests.rs` | 62 |
| `src/syscall/term.rs` | 11 |
| `src/fs_tests.rs` | 11 |
| `src/tests.rs` | 8 |
| `src/sync_tests.rs` | 8 |
| `src/pthread_tests.rs` | 5 |
| `src/syscall/msgqueue.rs`, `proc.rs`, `fs.rs` | 4 |

`term.rs` is the sharp example: `(-(25i64)) as u64 // ENOTTY` eleven times, and
`libc_errno::ENOMEM` at `:291` — both spellings, same file. `ENOTTY` is in
*neither* table, which is why the literal kept getting re-typed.

**Why this is a duplication finding and not just style.** The comment carries
the meaning and the number carries the behaviour, so they can drift silently and
a wrong number is indistinguishable from a right one at the call site. It is the
same failure mode as §6's dead `log::debug!` traces: the code *looks* annotated.
And CPD cannot see any of it — every one of these is a token-level literal.

**Suggested shape** (settle before starting, same as `ClientMem`'s home in §8
item 7): one positive-valued table in a crate both sides can reach, `neg_errno`
as the only place negation happens, and the pre-negated `u64` consts derived
from it rather than hand-written. Then convert the 15 production sites; the 94
in test files are a separate, mechanical pass.

### What the audit got wrong

The suggested shape was right and was implemented as written. The *inventory* was
low on every axis, in the same direction each time — it counted the spellings
someone had thought to grep for:

| The audit said | Actually |
|---|---|
| three tables | **five**. `src/syscall/fs.rs:8` carried a fourth (`EROFS`), one module away from the bin crate's own table, and the five test files carried a fifth spread across 59 per-function declarations |
| "109 raw negative literals", 15 of them production | **120 raw negated literals** plus **30** sites of an entirely uncounted fourth spelling: `i64::from(-libc_errno::X) as u64`, which reaches the *positive* table and then hand-rolls the negation the `neg_errno()` helper exists to do. `proc.rs` had 13 of these, `term.rs` 9, `fb.rs` 7 |
| — | a fifth spelling: `neg_errno(95)` in `mod.rs`'s xattr arm — the right helper called with a raw number, which is how a named table gets bypassed by someone who *is* using it |
| "17 names defined in both" | 17 confirmed, and they are now pinned by `merged_names_kept_the_value_both_old_tables_had`. Picking the wrong side for any one of them would silently change a syscall's errno and nothing else in the tree would have noticed |

`term.rs` also lost its `akuma_net::socket::libc_errno` import entirely, along
with `fb.rs`'s and `proc.rs`'s: three syscall modules no longer reach into the
networking crate to find out what `ENOMEM` is. No `cargo tree` edge was cut —
`akuma-net` already depended on `akuma-primitives` — so judge this one on
definitions collapsed (116 → 39) and on the drift it made impossible.

### The comment/value drift was real, and there were two

The argument for doing this at all was that "the comment carries the meaning and
the number carries the behaviour, so they can drift silently". Both halves of the
tree had already done it:

1. **`term.rs`'s `TIOCSWINSZ` arm returned `-12` (`ENOMEM`) under a comment
   reading `// ENXIO — no terminal attached`.** `ENXIO` is 6. The five sibling
   "no terminal state" arms in the same function return the same `-12` with no
   comment at all, so the *value* is consistent and only the comment was ever
   wrong. **Not fixed** — Linux returns `ENOTTY` for an ioctl on something that is
   not a terminal, so changing it is a behaviour change and does not belong in a
   deduplication pass. The arm now says `ENOMEM` in code with the divergence
   written down next to it. Anyone picking this up should check what busybox and
   musl's `isatty` do with `ENOMEM` first, and read the row for it in
   [`LINUX_COMPATIBILITY_ISSUES.md`](LINUX_COMPATIBILITY_ISSUES.md) §2 — the
   sshd-into-box bridge depends on `TCGETS` reporting *not a tty*.
2. **`mod.rs`'s xattr comment said `x0 = -95` is `0xffffffa9`.** It is
   `0xffffffa1`. The rest of that comment — that `!95` is `-96`
   (`0xffffffa0 = EPFNOSUPPORT`) and breaks musl and Go callers — is correct, and
   is now a host test rather than a comment
   (`eopnotsupp_encodes_as_negation_not_complement`).

### Two traps in the mechanical test-file pass

Both would have been silent behaviour changes, and both look exactly like the
thing being swept:

- **`const AT_FDCWD: u64 = (-100i64) as u64;`** (twice in `process_tests.rs`) is
  not an errno — and `100` is `ENETDOWN`'s value, so a name-blind sweep renames it
  to a plausible-looking wrong constant. The converter matched on the *name* and
  cross-checked the value against the table, and reported both as `SKIP`.
- **`(-1i64) as u64` in `sync_tests.rs:1968`** is `epoll_wait`'s infinite
  timeout, not `EPERM`. Left alone.

Also deleted: two commented-out const declarations in `sync_tests.rs` that were a
sixth copy of the table in comment form.

The `_neg` / `_val` / `_UNSIGNED` locals in the tests were **kept** as bindings
(`let enosys_neg: u64 = ENOSYS;`) rather than inlined. Their names carry what the
test is about — that a negated errno has bits 32+ set — and the duplication was
the literal, which is gone.

### The one check the merge removed, and where it went

Worth stating because it argues against the obvious reading of "delete 59 copies
of the table". Each of those per-test `const EINVAL: u64 = (-22i64) as u64;`
declarations was, however badly it scaled, an **independent restatement** of the
expected number: if a syscall returned the wrong errno, the comparison failed.
After the merge the boot tests import the same constant the kernel returns, so a
mistyped digit in the table would move both sides together and all 59 comparisons
would still pass.

So the merge is only safe with that check re-established in one place:
`errno::tests::every_value_is_the_linux_number` restates all 39 values as
literals, and asserts its own list is the same length as the table so a name
cannot be added without being pinned. It is a deliberate duplication of the
table, and it is the reason this change is verifiable at all.

### Verification (`docs/runbooks/verify-trim-fat-change.md`)

A/B against a `git worktree` at the parent commit (`9bc2dda8`), Tiers 1–3, both
arms via `scripts/verify_trim.py`. The summaries differ in **one line**:

```
7c7
< host.tests: 512      (baseline)
> host.tests: 517      (this change)
```

Everything else is identical arm for arm: 4/4 clippy configurations clean,
`fail_set` **empty** at SMP=1 *and* SMP=4, `pass_marker` 95 on both,
`passed_marker` 276 (SMP=1) / 283 (SMP=4) on both, all six Tier 3 exercises `ok`
(`elftest`, `forkprobe`, `cowstale`, `bssfork`, `bssfork 20 8 1`, `madvshared`),
`host_timejumps: 0` on both, and `bkl_stuck` 0 / 96 matching. Tier 4 not run:
nothing here touches the PMM, the fault path or the reclaim escalation.

The final tree measures **516**, not 517: two of the five new tests were merged
into `every_value_is_the_linux_number` after the arms ran. That is a `#[cfg(test)]`
change in `akuma-primitives`, which does not compile into the kernel, so the Tier
2/3 arms stand; Tier 1 was re-run (516 tests, 0 failures, clippy clean).

## 5.8 The runtime-registration machinery was written three times

**DONE 2026-08-13.** Found while asking why `channel.rs` could not read its
config from a host test (§6.1). The answer to "extract `ExecConfig` into
`akuma-primitives`" is: extract the **registry**, not the payload.

Three crates implement the same "kernel registers callbacks once at boot, then
everything reads them" pattern, and they do not agree on how:

| crate | machinery | read cost |
|---|---|---|
| `akuma-exec/src/runtime.rs:189` | `OnceCopy<ExecRuntime>` + `OnceCopy<ExecConfig>`; `register` / `runtime()` / `config()` / `is_registered()` | lock-free |
| `akuma-net/src/runtime.rs:52` | `Spinlock<Option<NetRuntime>>`; `register` / `runtime()` / `try_runtime()` | **spinlock on every access** |
| `akuma-ext2/src/ext2.rs:357` | `OnceCopy<ThreadHooks>` | lock-free |

Same four operations each time: register-once, get-or-panic-with-a-crate-name,
get-as-`Option`, and a boolean probe. CPD sees none of it — different type
names, different container.

**The divergence has a measurable cost, and akuma-net already knows.** Its own
`NetRuntime` doc comment explains that `virt_to_phys`/`phys_to_virt` were pulled
*out* of the struct because the indirection "cost a spinlocked struct read on the
per-packet DMA path" — while the struct is still reached through exactly that
spinlocked read. `akuma-ext2` calls `OnceCopy` "the crate-wide answer to exactly
this shape". Two crates got the answer; the one on the hottest path did not.

**Shipped:** `akuma_primitives::Registered<T: Copy>` wraps `OnceCopy` with the
panic diagnostic baked in — `register` / `get` / `require` / `is_registered`.
All three crates now use it, with three unit tests on the primitive itself.

### Judge this one on lock acquisitions, not lines

The line count is a **wash and then some**: +69 code lines in `akuma-primitives`
(≈30 of them the type, the rest its tests) against −22/+21 across the three
consumers. Third phase running to the same conclusion as §3, §5.555 and Phase 4
— when the deliverable is a seam, the seam is the cost.

What it actually bought:

- **21 spinlock acquisitions deleted from `akuma-net` read paths**, and they are
  not cold: `uptime_us` ×10 (every smoltcp `poll()`), `current_box_id` ×5 (per
  socket op), `blocking_relax` ×3 (blocking socket loop). Every network poll was
  taking a lock to read a function pointer.
- **Three definitions → one**, and the `get` (degrade) vs `require` (panic) split
  is now an explicit choice at each site rather than three different house
  styles. `akuma-ext2` deliberately keeps `get`, because its lock paths run
  before `init_thread_hooks` and tid-0/not-dead is the right answer there — that
  reasoning is now written down next to the static.
- Idempotent single-shot registration became a **property of the primitive**, so
  the host-test injection §6.1 solved per-crate does not need re-solving.

**No behaviour change in the conversion**, and the one that looked like a risk
is not: `akuma-net` went from last-writer-wins (`Spinlock<Option<_>>`) to
single-shot, but its two `runtime::register` call sites are
`#[cfg(feature = "smoltcp")]` and `#[cfg(not(…))]` — mutually exclusive, exactly
one compiled, called once from boot. Same for the other two crates: one `init`
each, from `src/main.rs` and `src/fs.rs`.

Note this is the *opposite* direction from §4's "nothing in `akuma-vfs` belongs
in `akuma-primitives`" finding, and for the stated reason: extraction pays when
the duplicator cannot reach the canonical version. `akuma-net` and `akuma-ext2`
cannot reach `akuma-exec`'s registry — that edge is precisely what the leaf crate
exists to avoid — so here the criterion is met.

## 5.9 Host-test coverage by crate (surveyed 2026-08-13)

Taken while acting on §6.1's finding that the bar for host-testing kernel code
is lower than assumed. Tests per crate, against size:

| crate | code lines | files | files w/ tests | `#[test]` |
|---|---:|---:|---:|---:|
| akuma-exec | 23,855 | 37 | 14 | 210 |
| akuma-ext2 | 3,248 | 3 | 1 | 52 |
| akuma-net | 3,147 | 9 | 2 | 25 |
| akuma-isolation | 1,475 | 5 | 3 | 43 |
| akuma-vfs | 1,448 | 6 | 1 | 39 |
| akuma-rump | 1,632 | 3 | 3 | 37 |
| **akuma-virtio** | **1,474** | **6** | **0** | **0** |
| akuma-primitives | 1,370 | 7 | 6 | 31 |
| akuma-terminal | 551 | 1 | 1 | 21 |

`akuma-virtio` was the only crate in the workspace with **no tests at all** —
and it is the crate holding the tree's one hand-rolled virtqueue. Closed to 3;
see §6.2.

The remaining large files were described here as "genuinely hardware- or
scheduler-bound" (`threading/mod.rs` 5,128, `process/mod.rs` 3,361, `mmu/mod.rs`
2,176, `children.rs` 2,054, `smoltcp_net.rs` 1,115). The lesson from §6.1 applies
to them piecewise: the untestable part is usually a thin shell around arithmetic
that is not, and splitting the arithmetic out is cheap.

> **Correction (2026-08-13): `threading/mod.rs` is not untested, and listing it
> here was discouraging work that is already possible.** It carries **19 host
> tests** in six modules — `signal_mask_tests` (`:4533`), `pending_kill_tests`
> (`:4564`), `itimer_tests` (`:4612`), `park_wake_race_tests` (`:4689`),
> `state_transition_guard_tests` (`:4813`), `thread_contexts_invariant_tests`
> (`:5020`) — plus **22** in `threading/types.rs`. Between them they pin the
> park/wake race, a stale waker reviving a recycled slot, `WakeHandle` tid
> generations, terminated-thread resurrection, slot double-claim under contention,
> and lock-free context publication ordering: i.e. the scheduler defect classes
> that actually cost time, not incidental helpers.
>
> The reason it *is* testable, and the PMM is not, is worth stating because it
> generalises: the scheduler's state is plain atomic arrays, so a host test can
> drive it directly, whereas the PMM's state only exists behind a live allocator
> (see [`PMM_EXTRACT.md`](PMM_EXTRACT.md) §6). The genuinely boot-only surface in
> `threading/mod.rs` is small — **13 `target_os = "none"` gates and 12 `asm!`
> sites in 5,128 lines**, about 0.5%.
>
> On extracting the scheduler into its own crate: feasible, and the coupling is
> thin — `threading` needs only **7 symbols** from `process`
> (`pid_for_thread`, `find_pid_by_thread`, `lookup_process_shared`,
> `is_current_interrupted`, `raise_sigchld_for_parent`, `reclaim::clear_draining`,
> `dump_orphan_processes`, all lookups or upcalls) while `process` makes **73**
> references the other way, so the dependency already points the right way and
> extraction means inverting 7 hook-shaped edges. `bkl.rs` (514) and `sync.rs`
> (1,683) both reference `threading` and would move with it: ~7,900 lines total.
>
> **But it fails this document's own criterion, twice.** Nothing outside
> `akuma-exec` consumes the scheduler (the only hits are five doc comments in
> `akuma-primitives/src/preempt.rs`), so there is no `cargo tree` edge to cut; and
> testability is not blocked, as the 41 existing tests show. A scheduler crate
> buys **decomposition** — ~7.9k lines out of a 23.8k crate, and compile time —
> not correctness or coverage. Worth doing, second in line behind the PMM, and for
> a different reason than the PMM.

## 5.11 The memory arithmetic wants a crate — and one test already proves it (raised + DONE 2026-08-13)

**DONE 2026-08-13.** Verdict: **`akuma-exec`, not a new crate** — nothing outside
`akuma-exec` and `src/` consumes this arithmetic, so a crate would cut no
`cargo tree` edge, which is the one criterion `addr.rs` records for having moved
things to `akuma-primitives` in the first place. What landed:

- `process::fork_code_start` — one definition, called by **both** `fork_process`
  arms (`:2251`, `:2427`) instead of an inline expression written twice, plus the
  test file's third copy deleted. The Go-AArch64 rationale moved onto the
  function, where the arms can see it.
- `memmath.rs` (new) — `USER_PAGE_RESERVE`, `user_alloc_would_starve`,
  `user_readahead_budget`, the quarantine poison codec (`POISON_MAGIC`,
  `poison_word`, `poison_decode`, `poison_word_frame`) and the mapping predicates
  (`mapping_is_read_only_to_user`, `is_shareable_mapping`). `src/pmm.rs` and
  `src/file_page_cache.rs` now re-export, so every call site is unchanged and
  there is exactly one definition of each.
- **The config gates moved too**, rather than staying behind as `src/` wrappers:
  `ExecConfig` gained `shared_file_pages_enabled` and `pmm_uaf_quarantine`, and
  `ExecConfig::for_test()` sets both **on** so the tests execute the gated path
  instead of skipping it — the §6.1 lesson applied. A gate is not a reason to
  leave a decision host-unreachable when the config is injectable.
- **19 new host tests** (`fork_copy_math_tests` 8, `memmath::tests` 11); five boot
  tests deleted and the pure half of `test_oom_user_page_reserve` moved out,
  leaving its live-allocator half in the VM where it belongs. Host total
  467 → 486; boot `PASSED` markers 273 → 268 (the `[PASS]` count the runbook
  gates on is unchanged — the deleted tests used the other marker format).

### The one trap found doing it: `config()` returns `ExecConfig` **by value**

`runtime::config()` is `CONFIG.require()`, and `OnceCopy::get` does
`assume_init_read()` — so **every call copies the whole ~45-field struct**. That
is fine once per fork or once per syscall, and it is *not* fine per page:
`is_shareable_mapping` is called from the readahead loops in both demand-paging
arms, up to 256 pages per fault in each of two passes, where it had previously
folded to a compile-time constant.

Moving the gate therefore came with hoisting the call out of those loops
(`map_flags` is loop-invariant, so it was always redundant per-page work):
`let shareable_mapping = …` once per fault, `exceptions.rs:3768` and `:4393`.
Net effect is *fewer* per-page operations than before the move, but the hazard
generalises — **never put a bare `config()` read inside a per-page or per-packet
loop.** Related: §5.10, since without `lto` on `[profile.release]` that call does
not inline across the crate boundary either.

Verified: 4 clippy configs clean, 486/0 host tests, boot suite SMP=1 and SMP=4
with the failure set unchanged (`{retired_reclaim_ab}`) and BKL-stuck lines at 93
(identical to pristine, HEAD and `main`), `cowstale`/`bssfork`/`forkprobe`/
`elftest` green at SMP=4, and `[FPCACHE] entries=263 hits=2630 misses=263` —
which is the check that the moved gate is actually live, since a mis-wired
`shared_file_pages_enabled` would read `entries=0 hits=0` and otherwise look fine.

The original argument follows.

### Why it needed doing at all

**Raised as a question ("could all this memory arithmetic be moved to a separate
crate?") while auditing the fork/CoW pile.** The answer is yes, and the argument
does not rest on taste: **the tree already contains a test that tests a copy of
the production logic instead of the production logic**, purely because the
production copy is unreachable from the host.

`src/process_tests.rs:11878`:

```rust
/// Helper mirroring the fork_process code_start selection logic.
fn fork_code_start(code_end: usize) -> usize { … }
```

Four boot tests (`:11894`, `:11919`, `:11935`, `:11958`) exercise **that mirror**.
The production selection is an inline expression written **twice** in
`fork_process` — `process/mod.rs:2223` (CoW arm) and `:2403` (eager arm) — so the
logic exists three times and the tests are attached to the copy that cannot ship.
Those tests cannot fail when production drifts. They are the one kind of test that
is worse than no test, and the cause is purely structural: the arithmetic has no
host-reachable home, so the test grew its own.

`pmm::user_readahead_budget` (`src/pmm.rs:1371`) is the same story from the other
side. Its doc comment says outright:

> Pure fn over the free count so the boundary is unit-testable without draining
> real RAM.

It was written to be unit-testable — and it lives in `src/`, the kernel binary, so
it is only reachable from the boot suite. The intent is already correct; only the
address is wrong.

**What is actually pure** (no MMIO, no page tables, no globals beyond config):

| today | where |
|---|---|
| `fork_page_count_for_len` | `akuma-exec` `process/mod.rs:118` (already in a crate ✓) |
| `code_start` selection | inline ×2 in `fork_process` + a mirror in the test file |
| `user_readahead_budget` / `user_alloc_would_starve` | `src/pmm.rs:1371` |
| `poison_word` / `poison_word_frame` | `src/pmm.rs:772`, `:840` — the XOR identity codec |
| `is_shareable_mapping` | `src/file_page_cache.rs:112` |
| `MAX_FORK_BRK_COPY_PAGES` / `MAX_FORK_MMAP_PAGES` / `MAX_FORK_LAZY_PAGES` cap ordering | scattered consts in `process/mod.rs` |
| page/VA masking (`& !0xFFF`), region-end and straddle predicates | open-coded at ~30 fault sites |
| the CoW refcount *rules* (one ref per address space vs one per mapping) | prose in three comments; §3 of [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) |

**Does it need a stub? Mostly no** — which is the good news, and it also answers
the follow-up question directly. Everything in the first six rows is integer
arithmetic over values passed in as arguments; it needs no stub, no fake MMU and
no fake PMM, just `#[test]`. Only two things need injection, and both have
precedent from §6.1: the reserve/RAM-window readers (`ram_base`, `ram_end`,
`kernel_va_end`, `USER_PAGE_RESERVE`), which take a value from config — inject
with `register_config_for_test()`, do **not** add a production branch that
tolerates an unregistered config. Anything that must *observe* a real page table
(`translate_user_va`, `collect_mapped_pages_*`) is the part that stays where it
is; it is a thin shell over the arithmetic, which is exactly §5.9's point.

Do **not** make this a new crate on day one. `akuma-primitives` already exists as
the dependency-free leaf and already holds `addr.rs` (`phys_to_virt`) — the same
category. The first move is to give the `code_start` selection one named home,
point both fork arms at it, delete the test's mirror, and let the four existing
tests become host tests. That is a ~20-line change that converts four
can't-fail boot tests into four real host tests, and it settles whether the leaf
crate is the right home before anything larger is moved.

**Outcome (see the DONE note above):** `akuma-exec` won on the `cargo tree`
criterion. `akuma-primitives` would also have imported its four forwarded
features and the silent feature-forward hazard (§6-adjacent: a broken forwarding
chain turned `PreemptGuard` into a zero-sized no-op) onto code that needs no
`cfg` at all. If a second crate ever needs page math, promoting a self-contained
module to a crate is mechanical; the reverse is not.

### Still open here

`pmm::alloc_page_zeroed_user`'s **four-step recovery escalation** (gate →
`drain_retired_under_pressure` → `reclaim_clean_file_pages` → `file_page_cache::shrink`
→ give up) is untested in either place, and its own boot test says why:
*"Actually draining RAM to the reserve is unsafe inside the boot suite."* So the
steps that convert an OOM into a clean SIGSEGV — instead of a whole-kernel `BRK`
abort, the 4.5 MB meow+tcc crash — have never been exercised by a test, only by
production incidents. Three of its five dependencies are already injectable
(`free_count`, `alloc_page_zeroed` via `ExecRuntime`; two reclaim calls already in
`akuma-exec`); only `file_page_cache::shrink` would need a hook. Two candidate
shapes, and the choice matters:

1. Move the whole escalation into `akuma-exec` and register a working fake
   allocator in `test_support` (one whose free count the test drives). Tests every
   step — but converts `free_count()` from a direct in-crate call into a
   fn-pointer call on the fault path, 1–4 times per user page, with no `lto`
   (§5.10).
2. Extract only the *decision* — `next_reclaim_step(free, done) -> ReclaimStep` —
   as a pure fn, leaving the effects in `src/`. Captures the bug class that
   actually bites here (a missing re-check between steps, the wrong order, or a
   premature `GiveUp` while memory is merely inside its reclaim cooldown), needs
   no runtime hook, and adds nothing to the fault path. Same shape as
   `completion_copy_len` and `trace_snippet`.

Deliberately **not** called `next_reclaim_rung`: "rung" is already this document's
word for the `akuma-primitives` extraction ladder (§5.555, rungs 1–6), and
"retry" would be wrong too — each step is a different, more expensive action, and
the return value also has to encode "allocate now" and "give up".

Shape 2 is the better trade on current evidence and is the recommendation; it is
still a fault-path control-flow change and wants its own SMP=4 verification, so
it was deliberately not bundled into the move above.

### Why it needed doing at all — the original argument

## 5.10 Open audit: `#[inline]` across the crate boundary (raised 2026-08-13)

**Not yet done — deferred by request.** Flagged on the grounds that inline
attributes have been applied ad hoc as code moved out of `src/` into `crates/`,
and nobody has ever swept them. First measurements, which say the concern is
real and not stylistic:

| | `src/` | `crates/` |
|---|---:|---:|
| `#[inline]` | 64 | 93 |
| `#[inline(always)]` | 2 | 33 |
| `#[inline(never)]` | 0 | 2 |

**The load-bearing fact: `[profile.release]` sets no `lto`** (`Cargo.toml:73` —
only `extreme-size` turns it on, `:85`). So on *the* build target, a call from
`src/` into `crates/akuma-*` crosses a codegen-unit boundary and is inlined only
if the callee carries an attribute. In `crates/` just **75 of 756 `pub fn`** do
(~10%) — and the hot paths now live there: `mmu`, the fault helpers, the Phase 7e
`with_process`/`shared` accessors that replaced ~250 direct field reads with
function calls, and `akuma-primitives`' leaf helpers (`current_tid`,
`with_irqs_disabled`), which are one-liners whose call overhead can exceed their
bodies.

The audit therefore has two directions, and both matter:

1. **Missing.** Small cross-crate `pub fn`s on fault, syscall-entry and
   scheduler paths with no attribute — these genuinely do not inline in
   `--release`. The Phase 7e accessor migration is the prime suspect, because it
   converted field access into cross-crate calls wholesale.
2. **Spurious.** 33 `#[inline(always)]` in `crates/` is a lot for a kernel that
   also ships an `opt-level = "z"` profile, where forced inlining fights the size
   floor that acceptance 05 gates on (4.0 MB). Some of these were almost
   certainly cargo-culted from the site next door.

Do not judge this one on attribute counts either way. The metric is (a) IMAGE
size on `extreme-size` and (b) a microbenchmark on a path that actually crosses
the boundary — per `PSTATS_TIMING_PREEMPTION_ARTIFACT`, not PSTATS. Worth pairing
with the question of whether `[profile.release]` should set `lto = "thin"` at
all, which would make most of direction 1 moot and is a smaller change than
annotating 700 functions.

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

**Merged 2026-08-13, and the survey above named the wrong pair.** Three
corrections, each a different way to be wrong about a clone:

1. **Phase 0 moved the target.** The survey says `write` ↔ `write_stdin`. That
   was true when it was written; Phase 0 item 5 then converted `write_stdin`
   from drop-oldest to short-write, which made it a clone of **`write_bounded`**
   and left `write`'s drop-oldest body the only copy of itself. Fixing half of a
   clone pair does not remove the clone, it *repoints* it — and a survey entry
   is stale the moment a phase edits either side.
2. **The clone family was twice the size.** Beyond the two named pairs:
   `has_stdout_data` ↔ `has_stdin_data`, `try_read` ↔ `read_all` (both
   drain-all, differing only in `Option` vs `Vec`), and at the bottom of the
   file **seven** registry accessors that are three bodies over two statics —
   including `register_system_thread_channel`, byte-identical to
   `register_channel` one function away. Now 13 methods over five FIFO helpers
   and three generic registry accessors.
3. **"Restore the missing traces" was the wrong instruction.** The two traces
   that existed were `log::debug!`, and **this tree never registers a `log`
   logger** — all 68 `log::*` sites in `src/` and `crates/` are no-ops. So the
   asymmetry was not "stdout is traced and stdin is not"; it was "nothing is
   traced and one copy looks like it is." Duplicating a dead trace onto the
   stdin side would have doubled the false promise. They are now one
   `trace_transfer` on `safe_print!`, live for both directions.

   The dead trace was also hiding a latent hazard: `read`'s copy ran **inside**
   the `with_irqs_disabled` + `buffer.lock()` region. Emitting a real console
   write from there is exactly the permanent-freeze shape `wake_pollers`
   documents. The merge moved every trace outside the lock, because the shared
   helper returns the count and the caller traces after.

**One allocation removed, not a clone but found by merging.** `write` copied
`data` into a fresh `Vec` before its critical section "to prevent page faults
while holding a spinlock" — but all four callers pass kernel memory, and
`write_bounded` takes *the same slice* at `syscall/fs.rs:894`/`:907` with no
copy at all. One of the two had to be wrong. The invariant is now stated at the
boundary (`fifo_push_drop_oldest`'s doc comment) instead of paid for with a heap
allocation on every terminal stdout write.

**The remaining divergence is deliberate and now pinned by a test.**
`read_stdin` registers no poller and wakes none, where `read` does both. That is
correct — `pollers` is one set shared by both directions, and stdin has no
parked writer to release — so it is documented at the method and guarded by
`stdin_drain_neither_registers_nor_wakes_pollers`, rather than "tidied" into
symmetry by the next reader.

### 6.1 The scheduler was never the reason this could not be host-tested

The prior plan of record held that `channel.rs` could not move or be host-tested
because it needs `crate::threading::{WakeHandle, wake_handle_for_thread,
wake_by_handle, current_thread_id}` — "the wake, which is the *operation*, not a
diagnostic that can degrade." That conflates the scheduler with three functions
that merely live near it:

| dependency | what it actually is on the host |
|---|---|
| `with_irqs_disabled` | already a no-op outside `target_os = "none"` (`akuma_primitives::irq`) |
| `current_thread_id()` | `akuma_primitives::preempt::current_tid()` — already in the leaf crate |
| `wake_handle_for_thread` / `wake_by_handle` | atomic-array bookkeeping over `SLOT_GEN` / `THREAD_STATES`; a CAS on a state word, no context switch |

`akuma-exec` already ran 202 host tests. The only thing actually blocking
`channel.rs` was `config()` panicking when unregistered.

**And the first fix for that was the wrong one.** It guarded `trace_transfer`
with the existing `runtime::is_registered()` probe, justified as "a diagnostic
must degrade when its config is absent, not panic." That reads well and does not
survive contact: `trace_transfer` is only reachable from channel I/O, which only
happens after processes exist, which is after `init()` — so the panic it guarded
against was unreachable in the kernel. It was a production branch added to solve
a *test-setup* problem, dressed as a design principle.

The fix is to **inject the dependency**, not to teach the code to run without
it: `runtime::register_config_for_test()` sets the `CONFIG` half only, with a
full-literal `ExecConfig::for_test()` next to the struct so adding a field
breaks it and someone has to choose a value. `OnceCopy::set` is already
idempotent, so parallel tests can all call it unconditionally with nothing to
race over. `ExecRuntime` — 27 kernel function pointers — is never needed,
because this logic reads config and nothing else.

That is strictly better than the guard, for a reason beyond tidiness: the test
config sets `syscall_debug_info_enabled = true`, so the tests now **execute**
the tracing path. The `is_registered()` version made every host test skip it, so
the branch it existed to enable was the one branch it guaranteed would never
run. The formatting is also now a pure `trace_snippet(&[u8], &mut [u8; 32])`,
unit-tested over control bytes, DEL, high bytes, oversized input and empty
input — including that its output is always valid UTF-8, which `trace_transfer`
depends on and nothing previously checked.

**The real line is narrower than "needs a scheduler": everything up to the point
a thread must actually stop and resume is host-testable.** Registering a waiter,
signalling one, refusing a stale generation — all of it. Only `schedule_blocking`
needs the boot suite. So the seven new host tests cover the FIFO shape, the
lock separation, the cap arithmetic and the poller policy in ~20 ms, and
`test_process_channel_write_bounded_backpressure` in `src/process_tests.rs`
stays as the in-VM half that proves a woken writer actually runs.

Where the old argument does hold: a *diagnostic* hook that degrades to nothing
is free, but a **wake** hook that silently no-ops is a hang, not a lost log
line. Hoisting the wake into a leaf crate needs a mandatory-registration
contract, not the console hook's optional one. That is a design cost — not an
impossibility, which is what it had been recorded as.

### 6.2 `akuma-virtio` had no tests, and the audit of it was stale

Applying §6.1's method across the workspace (§5.9) turned up one crate with
**zero** tests over ~1,470 lines — the crate that owns the tree's only
hand-rolled virtqueue, which `UNSAFE_AUDIT.md` §4 P2(e) singles out as carrying
two real defects.

**Both of those defects were already fixed.** The audit still reads as open work
("the fix is one word: clamp to `to_read`"); the code does clamp to `to_read`,
`VirtqAvail`/`VirtqUsed` are `AtomicU16` with an acquire load, and there is a
third guard the audit never anticipated — a device completing with `len == 0` is
an error, because otherwise `bytes_read` never advances and the driver reissues
the same request forever. §4 P2(e) now carries a status header saying so.

The gap was that **nothing tested any of it.** A one-word clamp with no test is
one careless edit from being a heap over-read into `getrandom` output again. So
the clamp is now a pure `completion_copy_len(device_len: u32, offered: usize)`,
split out for no reason other than testability, and pinned against a lying
device (`u32::MAX`, `4096`), an honest one, a short final request, and zero.
`calc_queue_layout` got tests too: the three DMA rings must not overlap, across
queue sizes 1…1024, because an overlap is silent corruption of whichever ring
loses the race — and the deliberate page-over-alignment of the used ring is
pinned so relaxing it to the spec's 4 bytes has to be a visible decision.

Generalising: the untestable part of a driver is usually a thin shell around
arithmetic that is not. Two functions, six lines of real logic, and the crate
went from 0 tests to 3 covering the exact code that a stale audit had left
everyone believing was still broken.

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
| 1 | ~~virtio scaffolding: shared `Hal`, `virtio::probe`, one `VIRTIO_MMIO_ADDRS`~~ **DONE (Phase 3)** — verified 2026-08-13: one `VIRTIO_MMIO_ADDRS` (`akuma-virtio/probe.rs:16`), one `Hal` (`hal.rs:51`), zero hand-rolled probe loops left. The row had simply never been struck | 0 left | — | — |
| 2 | ~~`channel.rs` stdout/stdin FIFO → one helper~~ **DONE 2026-08-13** — five shared bodies, 13 methods collapsed onto them, plus 7 host tests the file never had; the survey named the wrong pair (see §6) | 0 left | — | — |
| 3 | ~~`akuma-isolation`/`akuma-vfs` `mount.rs` → shared half into `akuma-vfs`~~ **DONE 2026-08-13** — one `MountSet<const MAX>`, both names as type aliases, −145 code lines across 4 files; the bin crate's third path normaliser went with it (§4) | 0 left | — | — |
| 4 | ~~The `X`/`X_from_path` **quartet**~~ **DONE 2026-08-13** — `elf/mod.rs` ×3 in Phase 2a (−151 code lines, 12 clone blocks → 0), `process/mod.rs` + `process/image.rs` in Phase 2b (−105 code lines, the pairs' 60- and 47-line clone blocks → absent) | 0 left | — | — |
| 5 | ~~`exceptions.rs` duplicated `Drop` impls + the demand-paging bodies~~ **DONE — both halves.** Guard half 2026-08-13 (three byte-identical guards → one `FaultSlotGuard` + `fault_slot_hold`, ~24 lines, not the `~142` the survey claimed — corrections in [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §7). Body half 2026-08-14: the ~330-line DA/IA demand-paging merge, declined three times, is one `demand_page_lazy_region` + an `akuma_exec::mmu::FaultAccess` seam (§12 there). It found **eight** differences, not §6's two — six were formatting and observability — and produced two new findings (F9 W^X on the IA permission upgrade, F10 asymmetric miss recovery), both recorded, neither fixed. `extreme-size` image −8,192 bytes; `host.tests` 508 → 512; `fail_set` identical at SMP=1 and SMP=4; Tier 4 `redis.stage: ok` | 0 left | — | — |
| 6 | ~~`box_mod` `access.rs` / `hierarchy.rs`~~ **DONE 2026-08-14** with item 9 — one `#[cfg(test)] pub(crate) make_test_registry()` in `box_mod/mod.rs`, both test modules importing it. As §4 predicted, it was the *fixture* and not the two named functions | 0 left | — | — |
| 7 | ~~`rump_proxy.rs` / `akuma-rump` `sysproxy.rs`~~ **DONE 2026-08-13** — 3 impls → 1 `pub NoMem` with `faulting()`/`discarding()`; there was no home to settle (§4). Also closes Phase 4's `ClientMem`/`NoMem` row | 0 left | — | — |
| 8 | `mmu/mod.rs` `map_user_page` / `_no_flush` and the three walk clones | ~80 | medium | **high** — see `UNSAFE_AUDIT.md` §5.1 |
| 9 | ~~Test-file clones (`tests.rs`, `process_tests.rs`)~~ **DONE 2026-08-14** — 11 clone families → 11 helpers, absorbing item 6. CPD test-only blocks **17 → 1**; the one left is declined with a reason. Host tests unchanged at 508 and boot tests unchanged at 275/282: every merge extracted a *helper*, none collapsed a test. **The row's framing was wrong** — the duplication is not *between* the two files (they share no identically-named function); all 17 blocks were within-file. Found and fixed two real defects on the way. See §10 | 0 left | — | — |
| 12 | ~~Runtime-registration machinery (§5.8)~~ **DONE 2026-08-13** — one `akuma_primitives::Registered<T>`; 3 definitions → 1 and **21 spinlock acquisitions removed** from `akuma-net`'s poll/socket paths. Line count a wash; judge it on the locks | 0 left | — | — |
| 11 | ~~**Errno spellings (§5.7)**~~ **DONE 2026-08-14** — one `akuma_primitives::errno` table with the negated forms *generated* from the positive ones; 116 definitions → 39, 150 hand-written negations → 0, +5 host tests. It was **five** tables and **four** spellings, not three and two, and the predicted comment/value drift existed in two places (`term.rs` returning `ENOMEM` under an `// ENXIO` comment, recorded and deliberately not fixed). Run **before** the Phase 5 sweep rather than with it, so each syscall arm is rewritten once | 0 left | — | — |
| 13 | **`#[inline]` audit (§5.10)** — deferred by request 2026-08-13. Both directions: missing attrs on cross-crate hot paths (`[profile.release]` sets no `lto`), and 33 `#[inline(always)]` fighting the `extreme-size` floor. Judge on IMAGE size + a microbenchmark, not counts | ~700 fns surveyed | medium | low |
| 14 | ~~**The fork/CoW pile ([`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md))**~~ **DONE 2026-08-14** — every row in that document's §9 findings table is closed: F1/F1b/F2/F3/F8 earlier that day, then F4 (six open-coded `dc cvau`/`ic ivau` sequences → one `mmu::sync_icache_range`, which also moved the completion barrier to the correct side of publication), F5 (**retired — not a defect**, §11.2 there), F6 (`FaultSlot::AlreadyHeld`) and F7 (already fixed). The merges landed as §8.1 (CoW-break middle) and §8.2 (`inherit_from` + `spawn_child_thread_and_publish`). **What is left is not this row**: §8 item 5's ~330-line DA/IA body merge and item 8 below, both still high-risk | 0 left | — | — |
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
| ~~`ClientMem`/`NoMem` across two crates~~ **DONE 2026-08-13** | The home never needed settling — the trait was always in `akuma-rump` and the kernel always imported it (§4). Three impls → one `pub NoMem` with `faulting()`/`discarding()` |

### Phase 5 — the user-copy sweep (−167 `unsafe`, 19% of the tree)

**DONE 2026-08-14.** Safe slice-based API with the check folded in, then the
conversions: `src/syscall/` **192 `unsafe` → 24**, `rump_proxy.rs` 12 → 0,
`exceptions.rs` 107 → 97, `akuma-exec/src/process/mod.rs` 36 → 34. Host tests
516 → 521. **Full record: [`USER_COPY_FOLD.md`](USER_COPY_FOLD.md)**, including the
two things [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) got wrong; the ABI divergences it
surfaced are collected in
[`LINUX_COMPATIBILITY_ISSUES.md`](LINUX_COMPATIBILITY_ISSUES.md). The short version
of each:

- **It is not "all in `src/syscall/*`".** `rump_proxy.rs` (15) and
  `exceptions.rs` (13) are the two files where the decisions were, because a
  copy in an exception handler must not demand-page.
- **The fold does not close the unchecked-destination hole.** Kernel RAM is
  mapped EL1-only in every user address space and the mapped-ness test only checks
  presence, so a mapped kernel VA still validates. Recorded as §4.0a there, not
  fixed — it needs an AP-bit test and its own A/B.
- **"Large but mechanical" undercounted the judgement.** ~140 sites converted by
  rote; ~25 needed a per-site decision, in three groups (must-not-prefault,
  validate-that-has-to-stay, and the one caller that stays raw). Same lesson as
  every other phase here: the mechanical part is not where the risk is.
- One real bug fell out: `mremap`'s payload copy never validated its
  *destination*, so a lazy page in the new mapping silently truncated the move.

**The §5.7 errno table landed first, deliberately.** Both passes touch the same
syscall arms, and doing errno first means each arm is rewritten once — see the
note under §5.7.

**The §5.7 errno audit is DONE (2026-08-14) and landed first, on its own.** The
argument for running them together was that both are "the call site says one thing
and the literal does another" problems in the same files. What actually happened:
settling the table is a 213-line diff that changes no behaviour anywhere, which is
exactly the kind of change that wants its own A/B — bundling it into 167 `unsafe`
conversions would have made every failure ambiguous. So the sequencing lesson is
narrower than the row claimed: **settle the table first, then sweep.** The sweep
now rewrites each arm once, and every arm it touches already returns a named
constant, so a converted arm's return value is reviewable without re-deriving a
number.

### Phase 6 — remaining duplication — IN PROGRESS

**DONE 2026-08-13:** the `mount.rs` shared half into `akuma-vfs` (§8 item 3) as
one `MountSet<const MAX: usize>`, and with it the bin crate's third path
normaliser — which turned out to be a real inconsistency, not a spelling
difference: `.`/`..` were resolved only when a process was current (§4).

**DONE 2026-08-13:** the `channel.rs` FIFO merge (§8 item 2) — five shared
bodies (`fifo_push_bounded`, `fifo_push_drop_oldest`, `fifo_drain_into`,
`fifo_drain_all`, `fifo_len`) plus one `trace_transfer`, and three generic
registry accessors under them. See §6 for the three things the survey had wrong
and §6.1 for the host-testability finding that came out of it.

**DONE 2026-08-13:** `NoMem`/`DiscardMem` (§8 item 7), which also closes Phase
4's last row. Three impls → one `pub NoMem { faulting(), discarding() }` in
`akuma-rump`; the "settle `ClientMem`'s home" question was already answered
years of commits ago (§4).

**Reclassified 2026-08-13:** `box_mod` (§8 item 6) is not a production-code
merge — the two named functions share nothing, and CPD's 60 lines are the
byte-identical `make_test_registry()` in both test modules (§4). It folds into
item 9 and drops in priority accordingly.

**DONE 2026-08-13:** the `exceptions.rs` fault guards (§8 item 5) — three
byte-identical guards (`CowFaultGuard`, `DaFaultGuard`, `FaultGuard`) and their
three identical `log_fault_reclaim` + `fault_slot_acquire` preambles collapsed
onto one `FaultSlotGuard` + `fault_slot_hold(pid, as_owner, page_va)`, which
acquires, traces and guards in one call (a guard travels with its operation — no
bare release helper was published on the `src/` side). Behaviour-preserving:
same acquire, same holder-gated release, same order. The merge also fixed the
field named `pid` that all three assigned `as_owner` to, and moved the release
contract to one place. `FaultSlot::reclaim_report()` split the "which outcomes
print" decision into `akuma-exec` with **4 new host tests** (463 → 467), leaving
the `safe_print!` effect on the fault path. Verified: 4 clippy configs clean,
467/0 host tests, boot suite at SMP=1 **and** SMP=4 with the failure set
unchanged, and `cowstale` (`reader_faults=0 failures=0`), `bssfork`
(`failures=0`) and `forkprobe` (`ALL PASS`) green at SMP=4. What the survey got
wrong about this item — three guards not two, ~24 lines not ~142 — is in
[`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §7, along with two smaller latent
findings the merge surfaced but did not touch.

**DONE 2026-08-14:** the DA/IA demand-paging bodies (§8 item 5's other half, and the
last row of this phase). ~330 duplicated lines → one `demand_page_lazy_region` in
`exceptions.rs` called from both EL0 abort arms, with everything the entry point
decides behind one `akuma_exec::mmu::FaultAccess`: `default_map_flags()` (`RW_NO_EXEC`
for a load/store, `RX` for a fetch) and `tag()` (`[DA-DP]`/`[IA-DP]`, kept distinct
because the archive greps for both). The permission decision moved into
`akuma-exec::mmu::types` as `lazy_map_flags` + `user_flags::is_exec`, which made the
whole behavioural surface of the merge **host-testable** — the reason to judge this one
on the seam and not the line count. 4 host tests (508 → 512) + 1 boot test; the
`extreme-size` image dropped 8,192 bytes. Verified A/B against a worktree at the parent
commit: identical `fail_set` at SMP=1 and SMP=4, Tier 3 all `ok`, Tier 4
`redis.stage: ok`, `host_timejumps: 0` on both arms. The one behaviour change (the
instruction arm no longer runs I-cache maintenance on pages it maps non-exec) is
argued, measured and recorded in [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §12.1 —
including the fact that the empirical check for it came back **empty**, which is a
weaker result than a positive trace and is reported as such.

**Still open, in the order they are worth doing:**

| Item | Notes |
|---|---|
| `src/console.rs` `print_dec` / `print_u64` | a genuine 21-line / 82-token **Type-1** clone differing only in `usize` vs `u64`; CPD has always reported it. Found during the `akuma-primitives` work, unrelated to it |
| ~~`exceptions.rs`'s duplicated `Drop` impls (§8 item 5)~~ | **DONE — both halves.** Guard half 2026-08-13 (above); the ~330-line DA/IA demand-paging body merge landed 2026-08-14 as one `demand_page_lazy_region` + an `akuma_exec::mmu::FaultAccess` entry-point seam, with the policy half (`lazy_map_flags`, `user_flags::is_exec`) moved into `akuma-exec` so it is host-testable — 4 new host tests + 1 boot test. Full record, all eight differences and their decisions, the `is_exec` reasoning and the verification numbers: [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §12. Two new findings out of it (F9, F10 in §9 there), recorded and not fixed. **What the three previous declines got wrong:** the two "behavioural divergences" that supposedly blocked it were a copy-paste artifact and a category error; the real work was scoping where the shared body starts and stops (the `PROT_NONE` arm and the lazy-region-miss branch stay per-arm, and should) |
| ~~The fork/CoW pile (§8 items 13–14)~~ | **DONE 2026-08-14** — all of [`COW_PILE_AUDIT.md`](COW_PILE_AUDIT.md) §9 is closed, including the `cow_fault_lock` finding (deleted with the `akuma-pmm` extraction). Six more open-coded `dc cvau`/`ic ivau` sequences in `exceptions.rs` collapsed onto the existing `mmu::sync_icache_range` on the way out (F4) |

### Phase 7 — `#[repr(C)]` `Statx` + `SigFrame`

Three blocks, −281 unsafe *operations*. Judge by §3.3, not by line count.

### Phase 8 — quality floor

SAFETY comments (11% coverage today), `clippy::undocumented_unsafe_blocks`
starting on the clean crates and ratcheting inward, `missing_safety_doc` allows
removed, and the CPD CI gate (§9) — at **50 tokens for the fault/CoW paths**,
per §5.6.

The `#[inline]` audit (§5.10) belongs here too, but only after the `lto = "thin"`
question is settled — deciding that first may delete most of the work.

### Promoted out of "deferred"

The CoW/`mmu` cluster (`map_user_page` / `_no_flush` and the walk clones) was
parked as high-risk-low-payoff. §5.6 is the payoff: that cluster demonstrably
produces memory corruption. Still do not touch it while the other agent is in
`exceptions.rs` — but it is no longer optional.

Still deferred, genuinely: `Mmio<T>` (~−25 `unsafe`), safe sysreg readers (~−27,
relocation not removal), the `Pte`/`PageTable` newtype (~−50). Specifically do
**not** pick up `Mmio<T>` while in the driver layer for Phase 3 — it reaches into
GIC, console and pmm and has a different blast radius.

### Deferred, inherited from BKL Phase 7: the **7g atomics audit**

**Which locks can just be atomics?** Added to
[`BKL_FINE_GRAINED_LOCKING_PLAN.md`](BKL_FINE_GRAINED_LOCKING_PLAN.md) §7.3a and
never started. It belongs on this list because a lock that should have been an
atomic is the same category of fat as a definition that should have been one: a
structure carried for no reason, paid for on every hot path.

It is not speculative — Phase 7f tranche 3 found a live instance while doing
something else (`UTC_OFFSET_US`, converted to an atomic in
[`BKL_PHASE7F_OPTOUT_LIST.md`](BKL_PHASE7F_OPTOUT_LIST.md) §8.3), which is what
motivated the phase. That is one confirmed hit from an unsystematic look, so the
audit's job is to find the rest before `KernelLock` is deleted — after which
every remaining lock is load-bearing by definition and the question gets harder
to ask.

**Sequencing.** Ahead of it in Phase 7f §11 sits the higher-value item:
**IRQ-mask `terminal_state`/`input_waker`**, which blocks `read` — the biggest
measured un-converted BKL holder (2.9–4.4% on the standing regimen, 56.4% in one
7b run). Verified still open 2026-08-14: `crates/akuma-exec/src/process/mod.rs`
takes it via `lock_bounded` with no `IrqGuard`, and `src/syscall/fs.rs`'s read
side still uses `disable_preemption`, which stops the scheduler but not IRQs.

**Where the live list lives.** `docs/runbooks/bkl-phase7-workplan.md` was deleted
in `c4f16a8e` — correctly, a workplan is not a runbook — but the deletion was a
*filing* decision, not a closure: the remaining-work list survives in
[`BKL_PHASE7F_OPTOUT_LIST.md`](BKL_PHASE7F_OPTOUT_LIST.md) §11, unstruck, and
[`BKL_PHASE7_AUDIT.md`](BKL_PHASE7_AUDIT.md) still declines to green-light
deleting `KernelLock` on two independently disqualifying findings.

### Deferred audit: GIC/timer/ramfb/irq → a leaf crate (raised 2026-08-14)

Raised while auditing whether `gic`, `gic_v3`, `ramfb`, `irq`, `timer`, and
`kernel_timer` could leave `src/` the way the virtio drivers did in Phase 3.
Not started — no code moved, no crate created.

If you want to extract, do a separate crate (`akuma-irq` or similar) for
`gic` + `gic_v3` + `ramfb` + `irq`, not fold them into `akuma-virtio`.
`akuma-virtio`'s own doc scopes it to "the DMA HAL, MMIO device probing, and
the virtio-mmio drivers" — GIC (interrupt controller), the ARM generic timer,
and ramfb (fw_cfg-backed, not virtio) aren't virtio devices, and stuffing them
into a virtio-named crate is the same shared-name trap `irq.rs`'s own
`IrqGuard` doc comment already warns about. `gic.rs`, `gic_v3.rs`, and
`ramfb.rs` only reach into `akuma_exec::mmu` for the VA seam — the same
layering `akuma-virtio` already sits above — so those three are plausibly
movable as-is.

`timer.rs` needs the hardware/RTC half split out from the scheduler-ISR half
before any of it can move: `enable_timer_interrupts`/`timer_irq_handler` are
fused to `akuma_exec::threading`, `process::FORK_IN_PROGRESS`, the preemption
watchdog, and the SMP scheduler-SGI dispatch — that's scheduler-tick logic
wearing a driver's filename, not a driver. `kernel_timer.rs` (the async alarm
queue) could follow once that split exists.

Checked and ruled out in the same pass: `exceptions.rs`'s only touches on
`gic`/`irq` are the three call sites inside `rust_irq_handler_with_sp`
(`gic::acknowledge_irq`, `irq::dispatch_irq`, `gic::end_of_interrupt`), and
that function is the single most fused piece of the exception path — BKL
reconciliation, the SMP scheduler-SGI fast path, and raw trap-frame reads all
live in it. It is a *caller* of the IRQ layer, not IRQ-layer code; none of it
moves with `gic`/`irq`.

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

## 10. Items 9 + 6 — the test-file clones (2026-08-14)

### What the row got wrong

**Item 9 names two files as if the duplication were between them. It is not.**
`src/tests.rs` and `src/process_tests.rs` share **zero** identically-named
top-level functions, and every one of CPD's 17 test-only blocks had both sites in
the *same* file. The row reads as "these two files are copies of each other";
what it actually measured is two large files that each repeat themselves. That
matters for how you work it — there is no cross-file home to settle, only eleven
local fixtures to name.

The `~669` is also not a thing that can be "removed": it is CPD's *covered* line
count across test files at 50 tokens, which counts every instance of every block.
Removable at 100 tokens was **397**, and 397 is what got worked.

### What was collapsed

Eleven families, all replaced by a named helper rather than a merged test:

| clone | sites | now |
|---|---:|---|
| `*at()` syscall fixture (cwd=`/tmp`, fd 7 → `sub`, `BYPASS_VALIDATION`) | 5 | `register_at_syscall_process` / `unregister_at_syscall_process` |
| `cstr()` NUL-terminating closure | 5 | one `cstr` fn |
| `*at()` tree clean-slate + teardown | 10 | `clean_at_test_tree(root, &LEFTOVERS)` |
| boot-TTBR0 page-table teardown walk | 4 | `clear_boot_ttbr0_pte(va, PtClear)` |
| free-thread-slot scan (msgqueue) | 4 | `find_free_thread_slots(n)` |
| eager-mmap region fixture | 3 | `alloc_eager_region` |
| NEON Q0-Q3 / Q4-Q7 thread bodies | 2+2 | `neon_yield_thread`, `neon_preempt_thread` |
| FPCR rounding-mode thread body | 2 | `fpcr_rmode_thread` |
| BKL spawn-storm phase + wait-by-holder report | 2+2 | `bkl_spawn_storm_spins`, `print_bkl_wait_by_holder` |
| parent/child (+ channel) process fixtures | 3+2 | `register_parent_and_child`, `register_parent_child_with_channel` |
| thread-group-of-three fixture | 2 | `register_thread_group_of_three` |
| deferred-free AS fixture | 2 | `new_as_with_one_mapped_page` |
| `make_test_registry` (**item 6**) | 2 | one `pub(crate)` fixture in `box_mod/mod.rs` |

CPD at 100 tokens: **67 → 50** blocks tree-wide, **17 → 1** test-only, removable
test lines **397 → 12**.

### Two real defects, both the "the fix lives in one copy" shape

Neither was the point of the task; both are the reason the task is worth doing.

1. **Three of four boot-TTBR0 teardowns cleared only the L3 entry** — and then
   freed the page-table frames `map_user_page` had returned. Exactly one copy
   (`test_map_user_page_roundtrip`) also cleared L2 and L1, and carried the comment
   saying why: otherwise the boot L1 keeps pointing at a freed L2, and when that
   frame is reused as a new `UserAddressSpace`'s L1 the boot `TTBR0` aliases the new
   address space's tables and later tests take spurious translation faults. The
   other three had the hazard and no comment. All four now go through
   `clear_boot_ttbr0_pte`, and the depth is an explicit `PtClear` argument because
   one caller genuinely needs `LeafOnly` — it unmaps eight pages sharing one L3
   table, so cutting the branch on page 0 would strand the other seven. That
   caller now clears its leaves, then cuts the branch once.
2. **`test_openat`'s teardown removed two files its setup did not.** The symlink
   case creates `link.txt`/`target.txt`; the clean-slate block at the top never
   knew about them, so a crashed run left that case's inputs in place for the next
   boot. One `LEFTOVERS` list per test now, used by both calls, so they cannot
   drift again.

### Differences found between copies, and the decision for each

- **NEON Q0-Q3 vs Q4-Q7** — kept as two helpers, not one parameterised body.
  `asm!` register names cannot be parameters, but the real reason is coverage: the
  two banks are saved by different code paths, and folding them would quietly halve
  what the tests watch.
- **`test_crash_goroutine_exit_kills_group` names its parent** — excluded from
  `register_parent_and_child`. The surviving name is what that test asserts on, so
  the fixture *is* the assertion there.
- **`bkl_spawn_storm_spins`'s `args`** were computed inside the per-copy loop in the
  fault test though loop-invariant; hoisted.
- **Comment asymmetry, again.** `test_unlinkat` and `test_openat` carried the
  load-bearing note about `BYPASS_VALIDATION` and `copy_from_user_str`; the other
  three `*at()` tests carried only "same shape as test_unlinkat". The reasoning now
  lives once, on `register_at_syscall_process`.
- **Two `*at()` teardowns ordered `unregister_process`/`unregister_thread_pid`
  differently from a neighbouring non-`*at()` test.** The five agreed with each
  other, so the helper took their order; the neighbour was left alone.

### Declined, with a reason

`pthread_tests.rs:695/739` — the four-line reset of a two-worker test's
`(tid, done)` statics. The statics must be declared per test, so only the reset is
shareable; the third site's shape differs (`A_READY`/`B_READY` plus a `FINISH`
flag). The helper would be `reset_worker_slots([&A_TID, &B_TID], [&A_DONE,
&B_DONE])` — noisier at the call site than the two obvious stores it replaces, for
~6 net lines, at 107 tokens (barely over threshold). This is §5.5's "noise — do not
chase" tail, and leaving it is the finding: not every CPD block is worth a seam.

### Verification

`scripts/verify_trim.py` A/B against `075ee16f` (the commit before this whole body
of work). Diff is four lines, all from the *previous* change: `host.tests` 506→508
and `passed_marker` +2 at both SMP levels, both of which are `COW_PILE_AUDIT.md`
§11's new tests, plus `smp4.bkl_stuck` 93→96 (load noise). **Items 9 and 6 moved no
counter at all** — 4/4 clippy configs clean, `fail_set: (empty)` at SMP=1 and
SMP=4, `host_timejumps: 0` on all four boots, `pass_marker: 95`, and all six Tier 3
exercises `ok`. That zero is the result: the constraint was that a test-clone merge
must not cost coverage, and the way to satisfy it is to extract helpers rather than
parameterise tests together.

## Background

- [`UNSAFE_AUDIT.md`](UNSAFE_AUDIT.md) — the `unsafe` census; §4 P2 covers the
  virtio HAL findings in depth, including the two defects in `src/rng.rs`'s
  hand-rolled virtqueue.
- `userspace/sshd/docs/EXEC_CHANNEL_LARGE_OUTPUT_TRUNCATION.md` — the open
  drop-oldest truncation bug whose fix reached only the stdout copy (§6).
- `docs/reference/subsystems/locking.md` §399 and
  `docs/runbooks/recover-wedged-vm.md` — the `ProcessChannel` lock-discipline
  history around the same code.
