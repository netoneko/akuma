# How main ended up broken (2026-11-16 .. 11-18)

Your suspicion was right: the type-refactor series, merged with checks that only covered the main kernel. Two separate things broke, with one cause.

## What was broken on main (`aa95540f`)

1. **Build.** `denver` did not build on either architecture; `taoxichuan`/`shanghai` were exposed to the same drift; the rump devbox failed (`DENVER_BUILD_BREAKAGE.md`, six drifts).
2. **Runtime.** On the tokio kernels (Taoxichuan, Shanghai, Denver) `open` of a lazily mapped `.rodata` path returned `EFAULT`, so tokioprobe's DNS lookup failed (`TOKIO_KERNEL_GATES_RERUN.md`). Invisible to every build.
3. Host tests: zombie-tid tests and two ratchet baselines (address params, feature parity) were stale (`de3417c0`).

## Timeline (from `git log`; commit attribution to a failure is by pickaxe and reading the diffs, not by bisecting each build)

| date | commit | change | what it later broke |
|---|---|---|---|
| 11-16 | `992ddfdb` (`072fd073` 11-17) | `FileKey { mount, ino }` replaces `(mount_id, inode)` `u32` pairs; `Option<FileKey>` | tokio-kernel-core `lazy.rs`/`shmpages.rs` and the arch halves kept passing raw pairs (drifts 2, 5) |
| 11-16 | `22419054` | `HostRoot` / `BoxId` / `CallerBox` / `SpawnBox` permits | `use` of the box types unconditional in glue `proc.rs`; `unused_imports = deny` on kernels without `akuma-native` (drift 1) |
| 11-16 | `7f35af4f` | no `&mut Process` from the table; post-publication fields are cells | `Process::auxv` assigned through a shared ref in `procfs-diag` builds (drift 4) |
| 11-16/17 | `d1742c0a` .. `f84e9f21` | `UserAddr` in glue batches 1-4, amd64 handlers, akuma-exec, time ("budget is 0") | tokio-kernel-core `clone_thread`/`fork_process`/`execve`, `sys_getcpu`, no-smoltcp `sendto`/`recvfrom` (drifts 3, 5, 6) |
| 11-16 | `9cc41207` | one validated NUL-string walk; prefault "uses the answer on AArch64" | prefault narrowed to `cfg!(target_arch = "aarch64")`; tokio kernels need it on amd64 (the runtime regression) |

Each commit was a sound change in a shared crate. The failures are all in callers outside the set that was built.

## Why nothing caught it

* **`cargo build` at the root never reaches the platform kernels.** `taoxichuan`, `shanghai`, `denver` are workspace members but not `default-members`. Pre-commit runs clippy and host tests, which do not compile them either.
* **The refactor's own gate was a count.** The user-pointer ratchet (`check_user_ptr_params`) reached "budget is 0" without the platform kernels being compiled; a type migration is only complete when every caller compiles, and the callers that mattered were not in the build.
* **The platform kernels share three crates with the main kernel** (`akuma-syscalls-glue`, `akuma-exec`, `akuma-tokio-kernel-core`), so a pure signature change is a breaking change for them with no signal.
* **The one runtime regression is not a type error at all.** `9cc41207` was a behaviour change dressed as a refactor ("shares the walk"), justified by an aarch64 security concern; the arch narrowing removed a fix that tokio kernels depended on (`TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md`). The tokio gates (tokioprobe, Denver JVM) are manual, were not run after the series, and were the only thing that could see it.
* The refactor landed in many small commits over three days, each "verified" against the main kernel; no step ran the full set at the end of the series.

## What now exists

* `scripts/premerge_build.sh` (16 builds, both architectures) and the CLAUDE.md rule to run it before merging to main. Its first run found a sixth drift (rump devbox) that the manual sweep missed.
* The prefault is architecture-independent again, with a comment saying why.

## Still open (recommendations, not done)

* The build gate does not boot anything. The tokio gate (`userspace/tokioprobe`, with the `epoch=` command-line argument) and the Denver JVM gate should be scripted into the repo (they are scratch scripts in a session directory today) and run before a merge that touches a shared crate.
* A refactor that changes behaviour on one architecture should say which kernels depend on the old behaviour; `9cc41207`'s message named only aarch64.
* Put `taoxichuan`, `shanghai`, `denver` in CI (or `default-members` for `cargo check`) so the next drift fails at commit time.
