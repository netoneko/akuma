# Line-count and code-size analysis (2026-08-07)

Investigation record. What `src/` + `crates/` actually contains, how much of it is
tests, where the production lines are concentrated, and — separately — where the
kernel image's *bytes* go, which is a different question with a different answer.

Two tools were written for this, both committed:

- `scripts/cloc_akuma.py` — cloc-style line counter that splits production code
  from test code.
- `scripts/symbol_sizes.py` — attributes an unstripped kernel image's bytes to
  subsystems via `llvm-nm`.

Measured at commit `d3f28d6` on branch `another-smp-attempt-0`.

---

## 1. The counting tool

`cloc` cannot answer "how much of this is tests", so `scripts/cloc_akuma.py` was
written to. It lexes Rust properly rather than regex-stripping: string literals,
raw strings (`r#"…"#`, `b"…"`, `br#"…"#`), char-literal vs lifetime, nested block
comments, and escaped line-continuations inside strings.

```bash
scripts/cloc_akuma.py                      # defaults to src crates
scripts/cloc_akuma.py src crates --by-file
scripts/cloc_akuma.py --json
scripts/cloc_akuma.py --no-kernel-test-gate
```

### What counts as a test

1. **Test files** — `tests.rs`, `*_tests.rs`, `*_test.rs`, `test_*.rs`,
   `test_support.rs`, or anything under a `tests/` or `benches/` directory. All
   their lines are test lines.
2. **`#[test]` / `#[bench]` items.**
3. **Items whose `#[cfg(…)]` only compiles in a tests-enabled build.** The cfg
   predicate is parsed into an `all`/`any`/`not` tree and evaluated with
   three-valued logic (true / false / unknown) against two worlds — "tests on"
   and "tests off". The item is test code only if it cannot exist in the second
   one. This distinguishes the cases that matter here:

   | attribute | verdict |
   |---|---|
   | `#[cfg(test)]` | test |
   | `#[cfg(all(test, feature = "kernel-tls"))]` | test |
   | `#[cfg(not(any(feature = "no-tests", kernel_profile_size)))]` | test |
   | `#[cfg(any(ext2_fs_cache, test))]` | production |
   | `#[cfg(not(test))]` | production |
   | `#[cfg(all(kernel_smp_shared, feature = "no-tests"))]` | production |

Rule 3's third row is the Akuma-specific one. The in-kernel boot suite does not
run under `cargo test` — it runs on bare metal — so it is not gated on rustc's
`test` cfg but on the *absence* of the `no-tests` feature. That gate is therefore
treated as a test gate. `--no-kernel-test-gate` flips this, which moves 234 code
lines from test to production; every number below uses the default.

A test span starts at the gating attribute itself (not the item after it) and
runs through the item's balanced braces, or to its terminating `;` for things
like `#[cfg(test)] use foo::bar;`.

### Validation against cloc

Per-file diff against `cloc 2.08` over all 172 Rust files: **171 match exactly**
on blank/comment/code.

The one difference is `src/sync_tests.rs`, where a literal blank line sits inside
a multi-line string (`src/sync_tests.rs:2531`). It is part of a string token, so
this counter calls it code and cloc calls it blank. Totals agree to that single
line.

Two real bugs surfaced during that cross-check and are fixed in the committed
script — recorded because both are easy to reintroduce:

- **Escaped line-continuation inside a string** (`"… nothing \` + newline, as at
  `crates/akuma-exec/src/threading/mod.rs:1198`) consumed the newline without
  incrementing the line counter, shifting every subsequent line's classification
  by one for the rest of the file.
- **`asm!` / `global_asm!` raw-string interiors** were counted wholesale as code.
  `src/exceptions.rs:14` holds a ~200-line AArch64 vector table inside
  `global_asm!(r#"…"#)`; its `//` annotations are real comments. The scanner now
  switches to assembly comment rules inside `asm!`-family raw strings, which is
  what closed the last large disagreement with cloc (198 lines in that one file).

---

## 2. Line counts

```
Language                   files     blank   comment      code    % test
Rust                         172     12074     23809     74535     34.9%
Markdown                       5       143         0       281      0.0%
TOML                          12        31        86       161      0.0%
SUM                          189     12248     23895     74977     34.7%
```

| bucket | files | blank | comment | code |
|---|---|---|---|---|
| Production | 169 | 7,150 | 17,342 | **48,942** |
| Tests | 20 | 5,098 | 6,553 | **26,035** |

- comment / code = 31.9%
- test code / production code = 0.53x
- 111,120 physical lines total

---

## 3. Where the production code is

Grouped by path prefix over production lines only (48,500 Rust lines; the
remaining 442 are TOML + Markdown):

| area | prod code | share |
|---|---|---|
| Process / threads / MM | 13,997 | 28.9% |
| Syscall layer | 9,226 | 19.0% |
| CPU / exceptions / SMP | 7,369 | 15.2% |
| Networking | 4,289 | 8.8% |
| Filesystems / VFS | 3,846 | 7.9% |
| Shell (in kernel) | 3,425 | 7.1% |
| Boot / drivers / misc | 3,081 | 6.4% |
| SSH server (in kernel) | 2,427 | 5.0% |
| Editor + terminal | 840 | 1.7% |

Grouping: `crates/akuma-exec` + `allocator.rs`/`pmm.rs`/`syscall/mem.rs` +
`akuma-isolation` → process/MM; `src/syscall/` → syscall layer;
`exceptions.rs`/`smp*`/`daif*`/`irq*`/`gic*`/`timer*`/`akuma-smp` → CPU;
`akuma-net`/`akuma-rump`/`rump_proxy.rs` → networking;
`akuma-ext2`/`akuma-vfs`/`src/vfs/` → filesystems.

Three observations:

**Most of it is the Linux ABI, not incidental bulk.** ~14k in process/threads/MM
is what CoW fork, `CLONE_VM`, real address spaces, lazy mmap, demand paging,
thread groups and signals cost. ~9.2k in the syscall layer is 17 syscall families
(`src/syscall/fs.rs` is 2,201 lines by itself). Every musl binary the box runs —
apk, rustc, llama.cpp — widens that surface.

**A caveat on comparisons.** An earlier draft of this analysis compared the ~14k
against xv6 doing "the same job" in a couple thousand lines. That comparison is
unsound and is recorded here only so it is not repeated: xv6 has no `mmap`, no
threads, no signal delivery, no networking, no dynamic linking, and no real libc
(base xv6 `fork` copies eagerly — CoW is a lab exercise). It runs ~20 syscalls and
its own handful of C utilities, and cannot host any real toolchain. The features
it omits *are* the ones costing Akuma those lines. A fair yardstick has to be a
kernel that runs an unmodified Linux userspace.

**~6,692 lines (14%) are services other kernels put in userspace** — SSH, shell,
editor, terminal. That is a deliberate choice, not accident: see §5.

---

## 4. Test coverage is very unevenly distributed

The 0.53x ratio is healthy in aggregate and misleading in detail. 19,781 of the
26,035 test lines sit in three files:

| file | test code |
|---|---|
| `src/process_tests.rs` | 9,644 |
| `src/tests.rs` | 6,480 |
| `src/sync_tests.rs` | 1,679 |

Against that, by component:

| component | prod code | test code |
|---|---|---|
| `src/syscall` | 9,931 | 117 |
| `src/vfs` | 1,144 | 0 |
| `crates/akuma-isolation` | 477 | 0 |
| `src/shell` | 2,571 | 32 |
| `src/ssh` | 1,342 | 13 |

The boot suite carries nearly all real coverage, which is consistent with the
project rule that kernel changes need `src/process_tests.rs` self-tests. But the
syscall layer — the second-largest area of production code — has essentially no
direct tests; it is exercised only indirectly through the boot suite and
acceptance playbooks.

---

## 5. Lines are not bytes: the in-kernel SSH question

The 14% of lines spent on in-kernel services serves a real purpose on the
smallest profile: `extreme-size` at the 4 MB floor is reachable over SSH with no
disk and no userspace process. The question raised was whether moving SSH to
userspace is worth it, and the only way to answer it is measurement — line counts
are the wrong instrument entirely (a 30 KB precomputed table is one line of Rust).

### `--features userspace-sshd` does not shrink the image

`config::ENABLE_USERSPACE_SSHD` (`src/config.rs:775`) is
`pub const ENABLE_USERSPACE_SSHD: bool = cfg!(feature = "userspace-sshd")` — a
runtime `const`, not `#[cfg]`. It dead-codes the *startup* branch at
`src/main.rs:1409`, but the SSH code stays reachable through three other paths:

- `src/main.rs:1404` — `ssh::init_host_key()`, unconditional under `smoltcp`
- `src/main.rs:1827` — `ssh::server::stats()`, the main-loop status line
- `src/shell/mod.rs:24` — `use crate::ssh::protocol::SshChannelStream`

The last is the real coupling: the in-kernel shell is written against the SSH
channel stream, so dropping SSH means giving the shell another transport. Moving
SSH out is a `#[cfg]`-gating change, not a feature flip.

### Measured byte attribution

Symbols retained via `--config`, because the size profiles set
`strip = "symbols"`:

```bash
scripts/build_size.sh --config 'profile.size.strip=false'
scripts/symbol_sizes.py target/aarch64-unknown-none/size/akuma
```

Section totals (stripped images, for reference):

| profile | text | data | bss |
|---|---|---|---|
| `size` | 872,031 | 32,984 | 82,560 |
| `extreme-size` | 665,487 | 32,456 | 52,000 |

Symbol attribution, `size` profile (889,896 bytes of sized symbols):

| group | bytes | KB | share |
|---|---|---|---|
| exec (proc/thread/mmu) | 141,839 | 138.5 | 15.9% |
| tls / x509 | 76,596 | 74.8 | 8.6% |
| shell | 72,077 | 70.4 | 8.1% |
| crypto (ssh + tls) | 63,580 | 62.1 | 7.1% |
| ext2 / vfs | 59,516 | 58.1 | 6.7% |
| smoltcp | 58,570 | 57.2 | 6.6% |
| **ssh: in-kernel server** | **34,853** | **34.0** | **3.9%** |
| akuma-net | 8,202 | 8.0 | 0.9% |
| unattributed / core | 374,663 | 365.9 | 42.1% |

**Read the SSH row as a floor, not an answer.** Two reasons:

1. `lto = true, codegen-units = 1` attributes inlined code to the caller. The
   first attempt at this grouping showed `akuma-ssh-crypto` at 1.1 KB across 10
   symbols, which is implausible for Ed25519 + curve25519 + ChaCha20 — the
   primitives are in the third-party crates, not the wrapper.
2. Those crypto crates are **shared with `kernel-tls`**. On the `size` profile
   (which keeps `kernel-tls`), `curve25519_dalek` / `ed25519_dalek` / `sha2` /
   `aes` serve both SSH and outbound HTTPS, so the 62 KB "crypto" row cannot be
   charged to SSH. On `extreme-size`, `kernel-tls` is dropped — so there the same
   crypto is SSH-only, and SSH's true cost is much closer to 34 + 62 KB. Note one
   symbol alone, `curve25519_dalek::…::ED25519_BASEPOINT_TABLE_INNER_DOC_HIDDEN`,
   is 30,720 bytes of rodata.

That last point is why the profile that most needs the answer is the one that
could not be measured — see §7.

### The userspace side

`bootstrap/bin/sshd`, statically linked against musl, stripped:

| | bytes |
|---|---|
| on disk | 152,120 |
| `PT_LOAD` R+X | 85,368 |
| `PT_LOAD` R | 58,756 |
| `PT_LOAD` R+W (memsz) | 1,024 |
| **loadable image (memsz)** | **145,148 (142 KB)** |

So the static footprint of userspace sshd is ~142 KB of VA at load — already
larger than the 34 KB the in-kernel server is *known* to cost, before adding
runtime heap, thread stacks, page tables, the ext2 disk to hold the binary, or
herd's supervision. Runtime RSS is not measured here (§7).

**The conclusion this points at:** on `extreme-size` at a 4 MB floor, moving SSH
to userspace probably makes the *system* footprint worse even though it makes the
kernel image smaller. On roomy profiles the kernel image saving is real but small
(≤34 KB of ~872 KB text while `kernel-tls` stays in, since the crypto is shared).
Neither half of that is settled until §7 is done.

---

## 6. Found in passing: `extreme-size` does not compile at `d3f28d6`

Blocked the measurement that mattered most, so it is recorded here.

```
error[E0433]: failed to resolve: could not find `file_page_cache` in the crate root
  (× 15)
```

`src/main.rs:45` declares `mod file_page_cache;` behind
`#[cfg(feature = "sc-framebuffer")]`, but the module is referenced
unconditionally from:

- `src/pmm.rs:791` — `file_page_cache::shrink(USER_RECLAIM_BATCH)`
- `src/fs.rs:128` — `file_page_cache::init(ram_bytes)`
- `src/vfs/mod.rs:272,276` — `len()`, `invalidate_inode(inode)`
- `src/main.rs:1531` — `stats_line()`
- ~10 sites in `src/exceptions.rs` (`lookup_and_ref`, `insert`,
  `mark_icache_clean`, `is_shareable_mapping`)

`extreme-size` builds `--no-default-features --features no-tests,smoltcp,extreme`,
which excludes `sc-framebuffer`, so the module declaration disappears while every
call site remains. From `37be208 more fixes and tests + forgotten file page
cache`. The `size` and `release` profiles are unaffected (both keep
`sc-framebuffer`), which is why §5 measures `size`.

Workaround for measurement purposes: `scripts/build_extreme_size.sh --features
sc-framebuffer` — but that inflates the image with the very cache being measured
around, so it is not a substitute for the fix (either gate the call sites to
match, or move `mod file_page_cache;` out from behind `sc-framebuffer`).

---

## 7. Not measured / open

1. **`extreme-size` symbol attribution** — the profile where SSH's crypto is
   *not* shared with `kernel-tls`, and therefore the only one that answers "what
   does in-kernel SSH cost where it matters". Blocked on §6.
2. **The definitive kernel-side A/B** — `#[cfg]`-gate the SSH server out (leaving
   `SshChannelStream` for the shell, or reworking that coupling) and diff `text`.
   Symbol attribution under LTO can only bound this. Do it in a `git worktree` so
   the shared tree is untouched.
3. **Userspace sshd runtime memory** — the number actually needed to decide, and
   the one §5 does not have. Needs a booted VM: RSS, heap, thread stacks, page
   tables, plus herd's own overhead. Two QEMU instances were live during this
   session (one holding port 2222), so no VM was booted; use `INSTANCE=1` to
   avoid colliding.
4. **Whether the 4 MB floor still holds either way** — the boot-stack reservation
   is derived from the linked image size in `linker.ld`, so kernel image size
   feeds back into the RAM floor. A smaller kernel plus a 142 KB userspace
   process is not obviously a net win at 4 MB.

---

## Reproducing

```bash
# line counts
scripts/cloc_akuma.py src crates
scripts/cloc_akuma.py src crates --json          # machine-readable
scripts/cloc_akuma.py src crates --by-file --top 25

# cross-check against cloc
cloc --quiet --by-file --csv --include-lang=Rust src crates

# byte attribution (needs symbols; size profiles strip them)
scripts/build_size.sh --config 'profile.size.strip=false'
scripts/symbol_sizes.py target/aarch64-unknown-none/size/akuma --top 30

# raw symbol listing, when a group's total looks implausible
~/.rustup/toolchains/nightly-*/lib/rustlib/*/bin/llvm-nm \
    --print-size --size-sort --demangle target/aarch64-unknown-none/size/akuma | tail -50
```

---

## Background

- Build targets and what each profile drops: `docs/reference/build-profiles.md`
- Every feature and env knob, including `userspace-sshd`, `no-tests`, `extreme`,
  `kernel-tls`: `docs/reference/subsystems/config-flags.md`
- The `extreme-size` profile's syscall gating and 4 MB floor:
  `docs/archive/EXTREME_SIZE_PROFILE.md`, `docs/archive/4MB_STABLE_AGENT.md`
- In-kernel SSH: `docs/reference/subsystems/ssh.md`
