# `userspace/build.sh` broken by the out-of-workspace members

**Date:** 2026-08-12
**Symptom:** `userspace/build.sh` aborts partway through with
`error: package ID specification 'meow' did not match any packages`, leaving
`tcc`, `tar`, `sshd`, `llama-cpp`, `wavplay`, `scratch` and `nca` unbuilt.
Running it from the repo root (the documented invocation) fails on the *first*
member instead.
**Files:** `userspace/build.sh`, `userspace/llama.cpp/Cargo.toml`,
`userspace/nca/Cargo.toml`.

## What broke

`userspace/Cargo.toml` no longer lists the four submodule-backed members. Its
own note says why:

> the submodule-backed members — meow, tcc, llama.cpp, nca — are temporarily
> removed from the workspace so the first-party crates build without every
> submodule checked out. Cargo requires *every* listed member's `Cargo.toml` to
> be present just to load the workspace, so a missing submodule blocked
> building even unrelated crates like `hello`.

That decision is sound and stands. What it broke is `build.sh`, which still
built all 21 members the same way: `cargo build --release -p <name>` against
the userspace workspace. For the four that left, cargo cannot resolve `-p`, and
`set -e` takes the whole script down with it — on `meow`, which is 10th in the
list, so half the members silently never got built.

Four separate failures were stacked behind that one symptom, each hidden by the
one before it.

### 1. `-p <name>` no longer resolves (the visible crash)

`cargo build -p meow` / `-p tcc` / `-p llama-cpp` / `-p nca` all fail. Two of
the four names were wrong independently of the workspace change anyway:

| build.sh name | directory | actual package |
|---|---|---|
| `meow` | `meow/` | `meow` |
| `tcc` | `tcc/` | `tcc` |
| `llama-cpp` | `llama.cpp/` | `llama-cpp` |
| `nca` | `nca/` | **`native-cli-ai`** |

**Fix:** an `EXTERNAL_MEMBERS` table in `build.sh` mapping
`name|dir|package|binary|required-path`, and `build_member` builds those four
with `cargo build --release --manifest-path <dir>/Cargo.toml`. The command stays
`cd`-less — cargo resolves `rust-toolchain.toml` (nightly) and
`.cargo/config.toml` (the `aarch64-unknown-none` target) from the *working*
directory, so building from `userspace/` keeps both.

The `required-path` column is a file from the member's own submodule
(`tcc/tinycc/libtcc.c`, `llama.cpp/llama.cpp/CMakeLists.txt`, …). Missing means
the submodule is not checked out, and the member is skipped with a message —
the same "a missing submodule must not break the build" property that motivated
dropping them from the workspace in the first place.

### 2. `llama.cpp` and `nca` could not be built standalone at all

`meow/Cargo.toml` and `tcc/Cargo.toml` each carry an empty `[workspace]` table,
so they are their own workspace roots. `llama.cpp/Cargo.toml` and
`nca/Cargo.toml` did not, and a non-member package that sits under a workspace
root is a hard error:

```
error: current package believes it's in a workspace when it's not:
current:   userspace/llama.cpp/Cargo.toml
workspace: userspace/Cargo.toml
```

Adding them to `userspace/Cargo.toml`'s `exclude` fixes that message and then
produces the identical one for the **repo-root** workspace one directory up —
cargo just keeps walking. **Fix:** an empty `[workspace]` table in each of the
two manifests (both are first-party files; the submodules are `llama.cpp/llama.cpp`
and `nca/native-cli-ai` *below* them), matching what `meow` and `tcc` already do.
That terminates the search at the crate itself.

### 3. The linker script is a relative path (the interesting one)

With 1 and 2 fixed, `meow` and `tcc` reached the linker and died there:

```
rust-lld: error: cannot find linker script linker.ld
```

`/.cargo/config.toml` at the **repo root** contributes
`rustflags = ["-C", "link-arg=-Tlinker.ld"]` for `aarch64-unknown-none`, and
`userspace/.cargo/config.toml` contributes the rest (`relocation-model=static`,
`-z max-page-size=0x1000`). Cargo merges rustflags arrays across config files,
so a userspace member gets both — and the `-T` path is **relative**, resolved by
rustc's cwd, which cargo sets to the *workspace root*. For a member that is
`userspace/`, where `linker.ld` lives. For `meow`/`tcc` built through their own
manifests it is `userspace/meow/` or `userspace/tcc/`, where it does not.

Nothing in the tree can fix this by adding a config file: a `--config` override
would *merge* with the root's array, not replace it, so the relative `-T` would
still be there. `CARGO_ENCODED_RUSTFLAGS` (and `RUSTFLAGS`) sit at a higher
precedence level that *replaces* `target.<triple>.rustflags` wholesale, so
that is what `build.sh` uses:

```bash
EXTERNAL_RUSTFLAGS=(
    -C relocation-model=static
    -C link-arg=-z
    -C link-arg=max-page-size=0x1000
    -C "link-arg=-T$PWD/linker.ld"      # absolute
)
```

Encoded (`\x1f`-separated) rather than space-separated so a path containing a
space cannot split into two arguments. `meow` prepends
`-Zunstable-options -C panic=immediate-abort` to that list: those two used to
ride in on a `--config target.…rustflags=[…]` override, which is exactly the
merge-not-replace mechanism that no longer works here.

Because the flags are now explicit, `EXTERNAL_RUSTFLAGS` is a hand-maintained
copy of two config files. If either `.cargo/config.toml` gains a flag that
matters to a shipped binary, this array needs the same flag. The check that
catches a mistake is the ELF itself:

```
$ aarch64-linux-musl-readelf -lW bootstrap/bin/meow
Entry point 0x400000
  LOAD 0x001000 0x0000000000400000 ... R E 0x1000
```

`0x400000` and `Align 0x1000` are the linker script and `max-page-size`
respectively; either one wrong means the flags drifted.

### 4. Wrong artifact paths, and warnings for binaries that were never there

A member that is its own workspace has its own `target/`, so the copy loop's
`target/aarch64-unknown-none/release/<bin>` is wrong for `meow` and `tcc` —
the binaries land in `meow/target/…` and `tcc/target/…`. `member_bin_path`
resolves this per member.

Two entries in `BINARIES` had never existed at any of those paths: `llama-cli`
and `nca` are installed straight into `bootstrap/bin` by their own build
scripts (`llama.cpp/build.rs` installs `llama-cli`, `llama-server` and
`llama-bench`; `nca/build.rs` installs `nca` and strips it). Both printed
`Warning: Binary … not found` on every successful build. They are dropped from
`BINARIES`; `member_bin_path` deliberately resolves to nothing for them, which
is also what makes `--llama-cpp-only` and `--nca-only` stop warning.

### Also fixed: the script assumed its own cwd

Every path in `build.sh` is relative to `userspace/` (`../bootstrap/bin`,
`tcc/dist/libtcc1.tar`, `forktest/c_stress`), but the documented invocation in
`CLAUDE.md` is `userspace/build.sh` from the repo root. From there the very
first `cargo build -p libakuma` resolved against the *kernel* workspace and
failed. `cd "$(dirname "$0")"` at the top — which also fixes toolchain and
target resolution for every member, since cargo reads
`rust-toolchain.toml` / `.cargo/config.toml` from the working directory.

## Verify

```bash
userspace/build.sh                 # from the repo root; must reach "Build process completed."
userspace/build.sh --meow-only     # exercises the -Zbuild-std + immediate-abort path
userspace/build.sh --tcc-only      # exercises the plain external-member path
aarch64-linux-musl-readelf -lW bootstrap/bin/meow | head -12
```

Expected:

- No `package ID specification … did not match any packages`.
- No `cannot find linker script linker.ld`.
- No `believes it's in a workspace when it's not`.
- No `Warning: Binary … not found` lines at all on a full build (the two
  standing ones, `llama-cli` and `nca`, are gone).
- `bootstrap/bin/{meow,tcc}` refreshed from `{meow,tcc}/target/aarch64-unknown-none/release/`,
  entry point `0x400000`, segment align `0x1000`.
- A member whose submodule is absent prints
  `(skipping <m>: <path> missing — submodule not checked out)` and the build
  continues.

## Background

- `userspace/Cargo.toml` — the self-hosting note that removed the four members
  from `members`, and the reason not to simply put them back.
- `docs/archive/AKUMA_SELF_HOSTING.md`, `docs/archive/KERNEL_SELFHOST_PROCMACRO2_WALL.md`
  — why in-VM builds must not require every submodule.
- `docs/reference/build-profiles.md` — a build target is profile + feature set;
  `build.sh` is the userspace half of every one of them.
- `docs/reference/userspace-layout.md` — the member list and what each binary is.
