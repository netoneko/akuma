# userspace/amd64

Guest programs for the amd64 bring-up target (`amd64/`, package `akuma-amd64`).

These are **not** members of the `userspace/` cargo workspace and have no
`Cargo.toml`. Each is a single `.rs` file compiled straight by `rustc` from
`amd64/build.rs`, linked with `user.ld`, and embedded in the kernel image with
`include_bytes!` — because the amd64 kernel has no disk driver yet, so there is
nowhere to *put* a binary it could open by path.

```
userspace/amd64/user.ld        shared link script: ET_EXEC at 0x40_0000, page-aligned segments
userspace/amd64/hello/hello.rs the ELF loader's probe
```

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
