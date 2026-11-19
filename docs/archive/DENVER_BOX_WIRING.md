# Denver box wiring: experimental amd64 slice

Status: **incomplete; do not use for isolation**. The `containers` feature is opt-in. It is not enabled by Denver's defaults, because passing the small probe below is not evidence that all filesystem, process and network boundaries are correct. The aarch64 implementation is not wired; selecting the feature for an aarch64 kernel is a compile error. Other Tokio kernels remain unchanged unless they explicitly select the feature.

## Approach

Reuse the shared box registry and policy in `akuma-isolation`, the lifecycle syscalls in `akuma-syscalls-glue`, and mount namespaces in `akuma-vfs-glue`. The feature is relayed from Denver through the JVM and Tokio fronts to the amd64 machine and Tokio core. The machine forwards the x86 private ABI and mount calls, supplies namespace hooks to the existing spawn implementation, and the network runtime reads the current process's box ID. This avoids a second registry or a second implementation of box access policy. Cargo features compose code; the hooks and syscall routes are still necessary.

The Tokio loader previously opened executables and interpreters directly from the host root. Boxed `execve` now resolves and reads through the current namespace. It eagerly loads their ELF pages: Tokio's lazy file-page record currently encodes only an inode in the host filesystem. The ordinary host path keeps its previous loader behavior.

## Evidence and remaining work

`denver/probe-boxes-qemu.sh` builds a disposable image and runs a static, libc-free probe. It checks registration, the box spawn ABI, reading a box-only file, failure to open a host-only file, exec of a box-only executable, and teardown. It also checks that boxed file-backed `mmap` returns `ENODEV`.

That last check is deliberately a **restriction**, not working file mapping: the Tokio lazy mapping and shared writeback records hold root-filesystem inode numbers without a mount identity. Using a box inode there can read or overwrite an unrelated host file. Boxed file mappings are refused until the open file's `FileKey` and mount lifetime can be carried through fault, fork, unmap, shared writeback, and teardown. This means dynamically linked programs and many practical box workloads will fail. A passing probe therefore does not qualify the feature for production or for Denver's default.

Before enabling by default, implement namespace-aware file mappings including shared writes and page-cache identity; exercise the real `box` userspace and overlay mounts; verify fork, exec, inherited descriptors, credentials, namespace lifetime, network policy, cross-box and host access denial, kill/reap, and concurrent boxes; run the existing JVM and non-container Tokio gates. Adapt and boot-test aarch64 separately before advertising support there. The Codex command-tool profile now passes an unboxed, unsandboxed fake-model gate; sandboxed tools still need seccomp and namespace wiring (see `DENVER_CODEX_TOOLS.md`).

## Reproduce

```sh
cargo build -p denver --target x86_64-unknown-none --release --features containers,wx-relaxed,vsyscall,procfs-diag,trace-err,fold-regpreserve,lazy-release-sweep,sysinfo,getcpu,fault-signal,shell
DENVER_KERNEL="$PWD/target/x86_64-unknown-none/release/denver" denver/probe-boxes-qemu.sh
```

The probe needs `cc`, `mke2fs`, `debugfs`, and `qemu-system-x86_64`. It uses no credentials or external network. Denver parks after init exits, so the script terminates its own QEMU at the timeout after checking the output.

## Rerun (2026-11-19, branch `ecosystem-demo-stage1`)

Asked whether the Denver boxes were finished, the probe in "Reproduce" was rerun on the current tree (the build command above, then
`denver/probe-boxes-qemu.sh`): `RESULT PASS`, with REGISTER, SPAWN, BOX_READ, HOST_HIDDEN, INNER_EXEC and KILL passing, and
`FILE_MMAP_REFUSED_PASS` (the serial log shows `[err] s2 nr=9 -19`: `mmap` of a file inside a box answers `ENODEV`).

That is the same result as at the time of writing, and it is **not** "finished": the probe uses a static, libc-free program, and
what passes is the refusal. Every dynamically linked program in a box dies on its first library `mmap`, which rules out the
ecosystem demo's roots (Alpine rust/cargo, JDK + scala-cli, python are all dynamic musl). Nothing in the "Before enabling by
default" list has been done since: namespace-aware file mappings, the real `box` userspace and overlay mounts, the Denver JVM and
Tokio gates with `containers` on, seccomp and private-namespace plumbing. Denver also has no `herd` and no `/etc/herd` services,
which the demo's bootstrap overlay relies on. The demo's guests therefore stay on the main amd64 kernel until Denver's file
mappings carry a box's `FileKey` (`AKUMA_ECOSYSTEM_DEMO.md`, decision 2026-11-19).
