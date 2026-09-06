# userspace/amd64

Guest programs for the amd64 bring-up target (`amd64/`, package `akuma-amd64`).

These are **not** members of the `userspace/` cargo workspace and have no
`Cargo.toml`. Each is a single `.rs` file compiled straight by `rustc` from
`amd64/build.rs`, linked with `user.ld`, and embedded in the kernel image with
`include_bytes!` — because the amd64 kernel has no disk driver yet, so there is
nowhere to *put* a binary it could open by path.

```
userspace/amd64/user.ld                    shared link script: ET_EXEC at 0x40_0000, page-aligned segments
userspace/amd64/hello/hello.rs             the ELF loader's probe
userspace/amd64/fdprobe/fdprobe.rs         the descriptor table's probe
userspace/amd64/threadprobe/threadprobe.rs clone(CLONE_VM)+futex, raw — a boot check
userspace/amd64/ruststd/ruststd.rs         **the odd one out** — see below
```

## `ruststd` is not like the others

It is an ordinary Rust binary — `println!`, `Vec`, `std::thread` — for
`x86_64-unknown-linux-musl`, linked static-PIE against a **real musl** and a
real `std`. Everything above is `#![no_std]` against `x86_64-unknown-none`.

That difference is its whole purpose. The code that decides whether `rustc` can
run here executes *before `main`* — `__libc_start_main`, the static-PIE
self-relocation, `__init_tp`, `std::rt::init` — and no hand-rolled probe
reproduces it. It is what found that `clone`, not futex, was the wall
(`docs/archive/AKUMA_AMD64_RUST_STD.md`).

Consequences, all of them deliberate:

- **`mkdisk.sh` builds it, not `build.rs`**, and it is staged onto the disk
  rather than `include_bytes!`d — 600 KiB is a guest program, not a self-test
  fixture. Run it with `INIT=/bin/ruststd`.
- **It is best-effort.** The build host is Apple Silicon, so `cc` is Apple clang
  and cannot emit ELF; `x86_64-linux-musl-gcc` (Homebrew `musl-cross`) can. A
  tree without it still gets a bootable image, minus this probe.
- **It prints as well as exiting.** The convention below — report through the
  exit status — assumes the program finishes. This one exists to find out where
  it *stops*, so each stage announces itself first and the status counts the
  stages that completed.

`threadprobe` is the raw counterpart, and follows every convention below: when
both fail, the difference between them is the diagnosis — `threadprobe` failing
means the syscalls are wrong, only `ruststd` failing means musl wants something
`threadprobe` does not ask for.

The rest of `userspace/` is a different world: those link against `libakuma` and
musl, target `aarch64-unknown-linux-musl`, and are built by `userspace/build.sh`
onto an ext2 image. Nothing here shares code with them yet. When the amd64 target
grows a filesystem, that is the direction to converge — not the other way.

## Why `x86_64-unknown-none` and not `x86_64-unknown-linux-musl`

The programs make raw Linux syscalls and link nothing. `-none` needs no musl
sysroot on the build host, which matters because the kernel is cross-built from
an Apple Silicon machine; the resulting ELF is an ordinary static ELF64 either
way. The moment one of these wants a libc, that choice has to be revisited.

## Adding one

Write `userspace/amd64/<name>/<name>.rs`, then add
`build_user_program(&dir, "<name>")` to `amd64/build.rs`. The build script
exports the path as `USER_<NAME>_ELF`; the kernel reads it with
`include_bytes!(env!("USER_<NAME>_ELF"))`.

A program reports what it checked through its **exit status**, not through
`write`: the kernel's self-test compares that status against a value computed in
`amd64/src/usermode.rs`, so a wrong load fails the boot instead of scrolling
past. See the table at the top of `hello/hello.rs`.
