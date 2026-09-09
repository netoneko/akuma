# amd64 C1 step 4b, prerequisites: the open-flag vocabulary

**Date:** 2026-09-09
**Status:** slice A landed, uncommitted. QEMU/TCG `SMP=1` **538/0**, `SMP=4`
**548/0**, ring-3 30/30, host tests +7.
**Parent:** `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, the C1 box's `4b` row.
**Predecessor:** `docs/archive/AKUMA_AMD64_C2_SLICES_6_AND_7.md`.

4b is the fold that retires `amd64/src/fd.rs` — 22 syscall arms in the
dispatcher still go to it rather than to `akuma-syscalls-glue`'s `fs.rs`. This
document is what has to be true *before* an arm moves, found the way C1 step 1's
was: by reading what a folded arm would actually touch.

---

## Slice A — `open(2)`'s flags are a second vocabulary, and it is permuted

C1 step 1 exists because a syscall **number** means different things on the two
architectures, and handing glue the wrong one is a wrong answer rather than a
compile error. The same is true one argument along, and nothing had said so:
aarch64 Linux keeps the **32-bit ARM** fcntl values, so four `open(2)` flag bits
are a *permutation* between the two architectures.

Read out of this tree's own vendored musl headers
(`userspace/tcc/vendor/musl-dev-{aarch64,x86_64}.apk`, `bits/fcntl.h`) — the
right source, because the question is not what a kernel header says but what the
libc in the guest passes:

| bit | aarch64 | x86_64 |
|---:|---|---|
| `0o40000`  | `O_DIRECTORY` | `O_DIRECT` |
| `0o100000` | `O_NOFOLLOW`  | `O_LARGEFILE` |
| `0o200000` | `O_DIRECT`    | `O_DIRECTORY` |
| `0o400000` | `O_LARGEFILE` | `O_NOFOLLOW` |

**Every other `O_*` bit is identical** — `O_CREAT`, `O_EXCL`, `O_NOCTTY`,
`O_TRUNC`, `O_APPEND`, `O_NONBLOCK`, `O_DSYNC`, `O_ASYNC`, `O_NOATIME`,
`O_CLOEXEC`, `O_PATH`, `__O_TMPFILE`. That is what makes it dangerous: a reader
who spot-checks `O_CREAT` and `O_CLOEXEC` concludes the encodings agree, and
`amd64/src/fd.rs` said exactly that in a comment —

> Spelled out locally rather than pulled from `akuma_syscalls_linux` because
> those are the AArch64/`asm-generic` values; on x86_64 they **happen to share
> the same numeric encoding** … naming that coincidence explicitly here is
> cheaper than a reader having to go check.

The file *knew better* in three other places, as `O_NOFOLLOW_X86`,
`O_TMPFILE_X86` and `O_EXCL_X86` declared inline at the three sites that needed
them. Each was correct on its own and none contradicted the comment out loud,
which is how a coincidence survives as a stated fact.

### Why the permutation is worse than a mismatch

It is two transpositions, and **both turn one real flag into another real
flag**, so nothing rejects either direction:

- **`O_DIRECTORY` ↔ `O_DIRECT`.** An x86_64 caller's `O_DIRECTORY` read with the
  aarch64 table is a cache hint, so the "this had better be a directory" check
  never runs. The other way, an ordinary file open acquires a directory
  requirement it never asked for.
- **`O_NOFOLLOW` ↔ `O_LARGEFILE`.** glibc sets `O_LARGEFILE` on almost
  everything, so this one reads as "do not follow symlinks" on a caller that
  never asked.

And the compound flag inherits it. `O_TMPFILE` is `__O_TMPFILE | O_DIRECTORY`,
so it is `0o20040000` on aarch64 and `0o20200000` on x86_64. Glue's `sys_openat`
**refuses** `O_TMPFILE` deliberately — apk-tools 3 probes for it, and an open
that succeeds and then discards the bytes surfaced as `UNTRUSTED signature` over
a download that was fine (`docs/archive/APK_OTMPFILE_DIR_FD.md`). Fed an
untranslated x86_64 word that refusal **does not fire**: `0o20200000 &
0o20040000` is `0o20000000`, not the mask, so the guard tests false and a bug
this tree has already paid for once comes back on the architecture that never
had it.

### This is not a live bug today, and that is the point

Nothing is broken on the current kernel: the three inline `_X86` constants named
the right bits, so `sys_openat` was self-consistent. **The defect is scheduled
rather than present** — it arrives the moment a folded glue arm owns the
refusal, and it arrives silently. That is the same shape as step 1's syscall
numbers, and the same remedy: translate once at the boundary, before anything
depends on it.

### What landed

`akuma_syscalls_abi::open_flags`, in the crate that already exists to answer
"which encoding, on which architecture":

- `x86_64::{O_DIRECT, O_LARGEFILE, O_DIRECTORY, O_NOFOLLOW, O_TMPFILE}` as
  literals, because this crate owns the x86_64 table;
- `aarch64::{…}` re-exporting `akuma_syscalls_linux::flags::open` for the three
  that crate names, so they cannot drift, and declaring `O_DIRECT` /
  `O_LARGEFILE` locally because it does not — its rule is that a constant
  appears when a caller needs one, and no AArch64 code reads either. **They are
  needed here even so**: a translation that passed them through would leave each
  meaning the other flag, which is the whole defect.
- `x86_64_to_aarch64` / `aarch64_to_x86_64`, written out separately rather than
  aliased. They are the same function today because a permutation of
  transpositions is its own inverse, and a test pins that — but leaning on it
  silently would let a future bit that is *not* self-inverse break one direction
  with nothing to say which.

`amd64/src/fd.rs`'s `sys_openat` re-encodes the word **once, at its own
boundary**. Everything below — the file, `KernelFile::flags`, and whatever glue
arm folds next — then speaks one encoding. The local `open_flags` module is
`pub use akuma_syscalls_linux::flags::open`, and the three `_X86` constants are
gone.

### Seven host tests, two of which assert the defect

The two that matter are negative:

- `untranslated_o_tmpfile_defeats_the_guard` asserts `asked & guard != guard`
  for the *untranslated* word — i.e. it fails if the bug ever stops being real —
  and then that the translation restores the refusal.
- `exactly_four_bits_are_permuted` walks all 32 bits and pins the moved set,
  because the assumption a reader makes here is "none of them move".

Plus the round trip over every bit, the involution, and a cross-check of the
aarch64 halves against `akuma-syscalls-linux`.

### And a live gap the translation let me close

`O_DIRECTORY` was **never read at all** on this target: it had no entry in the
local flag module, and `is_dir` is decided by `fs::metadata`, so
`open("/bin/busybox", O_RDONLY|O_DIRECTORY)` returned a working descriptor on a
regular file. It could not simply have been added, either — the bit ring 3 sets
is `0o200000`, which in the tree's own constant is `O_DIRECT`, so reading it
with `open_flags::O_DIRECTORY` would have tested the wrong bit and adding a
fourth inline `_X86` const would have deepened the split. It is `ENOTDIR` now,
which is Linux's answer and the errno `find` and `ls -R` act on.

### The negative control found a bug in the fix

Five boot checks, every constant in them spelled in the **x86_64** encoding on
purpose — they are what a musl-linked guest sets, and proving the hop happened
is their whole job. Each was falsified against a kernel with the translation
line removed, and two came back red as intended:

```
fd: O_DIRECTORY on a regular file is ENOTDIR  [FAIL] got 0x3   want 0xffffffffffffffec
fd: O_TMPFILE is EINVAL, not EISDIR           [FAIL] got EISDIR want EINVAL
```

`got 0x3` is a **live file descriptor** — the pre-fix answer, for every caller
that ever passed the flag.

The first run of that control did something more useful. It reported `ENOENT`
where the broken arm should have opened a file, which said the fixture
(`/etc/passwd`) is not on the image at all — and therefore that the *passing*
arm had been answering `ENOTDIR` for a **missing** path. The `O_DIRECTORY`
refusal was above the existence probe, so `open("/no/such/path", O_DIRECTORY)`
blamed the directory-ness of something that was not there. Moved below the
probe, fixture changed to `/bin/busybox`, and a sixth check pins the ordering.

**A check whose negative control passes for the wrong reason is not a control.**
The only thing that caught this was reading *why* the red arm was red.

### Verification

| gate | baseline | now |
|---|---|---|
| QEMU/TCG `SMP=1` | 533/0 | **538/0** |
| QEMU/TCG `SMP=4` | 543/0 | **548/0** |
| `amd64_ring3_check --smp 1 -n 30` | OK | **OK** — 30/30, `free` unmoved, heap +41 kB (tolerance 8192), `grandfork` ALL PASS |
| host tests, `akuma-syscalls-abi` | 8 | **15** |
| clippy (`akuma-amd64`, `akuma-syscalls-abi`) | clean | clean |

**AArch64 cannot be affected, by construction rather than by measurement:** the
diff is `amd64/` plus `akuma-syscalls-abi`, which the root kernel does not
depend on — the workspace lists it as a member and only `amd64/Cargo.toml` links
it.

Ring-3 witnesses on QEMU: `find /etc -type f`, `ls -R /etc`, `>` truncating a
longer file to two bytes, `: >` leaving a zero-byte file, `echo hi > /dev/null`,
and stderr reaching the session across `sh -c '…' > file` — the slice 5/6/7
behaviours, unchanged.

**Also re-measured, and it closes a roadmap issue:** `echo A > /nosuchdir/file`
answers `/bin/sh: can't create /nosuchdir/file: nonexistent directory`, rc 1.
That is open issue 1 of `AKUMA_SELF_HOSTING_AMD64.md`, reported as `rc 0` and
silence; C2 slice 5 closed it and nobody had re-run it.

## Background

- `docs/archive/AKUMA_AMD64_C1_DISPATCH_VOCABULARY.md` — the same problem for
  syscall numbers, and the shape this copies.
- `docs/archive/APK_OTMPFILE_DIR_FD.md` — the bug the `O_TMPFILE` refusal exists
  to prevent, and which an untranslated fold would reintroduce.
- `crates/akuma-syscalls-linux/src/flags.rs` — the module header that names the
  aarch64/asm-generic split. It was right, and it was one crate away from the
  file that assumed the opposite.
