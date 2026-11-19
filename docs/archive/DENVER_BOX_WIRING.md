# Denver box wiring: experimental amd64 slice

Status: **file mappings and a real dynamic box workload work (2026-11-19, see the last section); still opt-in, do not use for isolation yet**. The `containers` feature is opt-in. It is not enabled by Denver's defaults, because passing the small probe below is not evidence that all filesystem, process and network boundaries are correct. The aarch64 implementation is not wired; selecting the feature for an aarch64 kernel is a compile error. Other Tokio kernels remain unchanged unless they explicitly select the feature.

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

## Boxed file mappings and real box roots (2026-11-19)

The restriction above is lifted. Incident-style detail: symptom, cause, fix and verification.

**Symptom.** A dynamic musl program in a Denver box died on its first library `mmap` (`ENODEV`); once that was lifted, `mprotect`
inside the box failed with `ENOMEM` ("sh: out of memory").

**Cause 1 (mappings).** Lazy PTE records, the pin table, `akuma_fpcache` and `shmpages` named files by root-filesystem inode only.
**Cause 2 (regions).** A box's first process is started by `akuma-exec`'s `spawn`, whose loader fills no Tokio task record: no
user-layout regions, no brk or mmap window, so every `mprotect`/`mmap` range failed `user_ok`.

**Fix.**
- The lazy record's inode field is a mount-qualified file number, `slot<<24 | inode` (`akuma_tokio_kernel_amd64::lazy::boxfile`, 255
  slots per boot, slot 0 = root; originally 15 slots). `Kernel` gained `read_file`, `file_size`, `pin_inode` and `shareable`; reads go through
  `akuma_vfs_glue::read_at_by_mount_inode` and the process's namespace. Boxed files never enter `akuma_fpcache` or `shmpages`.
  Writable `MAP_SHARED` of a boxed file is still refused (`ENODEV`).
- `twin_enter_user` re-loads a spawn child whose task record has no regions through `proc::exec_strs` (path from the `Process`, argv
  and environment from the SysV stack `akuma-exec` built), which fills the record the way `execve` does.

**Verified** (Denver `containers` build, scratch image with the demo's Alpine roots, `box` built with `--features
libakuma/linux-abi`, since stock `box` uses the Akuma-private syscall ABI Denver does not serve):
`denver/probe-boxes-qemu.sh` PASS (`FILE_MMAP_PASS`, `SHARED_WRITE_REFUSED_PASS`); in the `rust` box `cargo 1.96.0`, `rustc 1.96.0`,
`rustc -O h.rs && ./h` and `cargo new && cargo build && ./demo`; in the `scala` box `java -version` (OpenJDK 21.0.11) and
`java H.java` (source launcher compiles and runs). All four Denver and three Shanghai/Taoxichuan kernel builds pass.

**Second box in one boot (fixed the same day).** `box run` twice panicked in `activate` ("root does not share the live kernel slot
511", the root being a freed frame's garbage). Cause: `akuma-exec`'s `spawn` publishes the child's thread *before* its
`bind_child_task` (the closure is the child), and the first switch to it loads the slot's task-record root; a reused slot still
held the previous process's freed root. Fix: `zombie_root` (core `proc.rs`) writes 0 into the record when the process's root is
handed over (0 = keep the current root until the bind sets the real one). The spawn-child re-load also reads the task record
*after* `set_task_slot`; before, it read the previous task's record, which only worked by luck.

**Verified after the fix.** `rustc`, `cargo build`, `java`/`java H.java` back to back in one boot (three `box run`s); the python box
(`python3 -c ...` with `json, sqlite3, ssl` imports, Python 3.12) on `guest2.img`; `scala-cli version` in the scala box prints
`1.19.0`; `scripts/gate.py --only taoxichuan,shanghai,denver` PASS (the Tokio core changed under all three).

**Still open.** scala-cli *compile* with Maven Central, `cargo` with the network and seccomp/private namespaces inside a Denver box are
untested at that point (see the network probe below); the JVM gate with `containers` on; the demo bootstrap and its guest driver (subsequently exercised below).

## Box network probe (2026-11-19)

`denver/probe-box-network.py` boots the `containers` kernel with a virtio NIC and a disposable ext2 image. A process spawned inside a registered box connects to QEMU SLIRP's host alias and makes an HTTP request to a temporary host server. On the featured amd64 build in the reproduction above, DHCP configured `10.0.2.15` and the probe reported `REGISTER PASS`, `SPAWN PASS`, `BOX_NET_PASS`, `KILL PASS`, and `RESULT PASS`. Run it with:

```sh
DENVER_KERNEL="$PWD/target/x86_64-unknown-none/release/denver" denver/probe-box-network.py
```

It requires QEMU, a C compiler, e2fsprogs, Python 3, and a free host TCP port. It exercises outbound IPv4 TCP and HTTP from a box, not DNS, TLS, internet access, inbound forwarding, network isolation, or the actual demo services. Those remain to be checked on Denver. The ecosystem launcher boots `/bin/herd`; the Denver run below exercises that same bootstrap and userspace. The existing successful reference-demo run used the main amd64 kernel and must not be attributed to Denver.

## Herd and demo guest verification (2026-11-19)

The existing demo images and herd configuration boot on Denver after routing herd's private spawn, wait, kill, time and uptime ABI calls to the existing shared handlers. Both guests fetched the bootstrap through metadata and reached SSH. Supplying `epoch=` from the launcher fixed TLS certificate validation; DNS and HTTPS then passed from the guest and each of the rust, scala and python boxes, including crates.io and Maven Central. Cross-guest HTTP reached both boxed servers through the launcher's forwards.

Repeated `eco-box` launches exhausted the original 15 mount IDs: the mount IDs are never reused, and dynamic libraries began failing with `ENODEV`. The experimental encoding now allows 255 mount IDs at the cost of limiting inode numbers to 24 bits, Box file mappings above that range are refused; root executable pages above it load eagerly and root file mappings are refused. The mount table still exhausts permanently on a long boot. A lifetime-aware mapping identity is needed for an unrestricted deployment.

The original 48 task slots prevented the capacity workload from forking. The opt-in container profile now has 256 slots and matching BKL depth storage. The corrected capacity test builds the load program in its own box and checks that 150 JVM threads and 60 sleeper processes started; Scala and Cargo builds completed beside them, with 248 live user threads observed. The first capacity test's apparent pass was invalid: it compiled the JVM load program in a different box. The rest of the launcher verification, including metadata bootstrap, toolchains and cross-guest traffic, passed in the same run. The unboxed fake-model Codex command-tool probe passed on a combined `containers,codex` build. Codex 0.169.0 also ran inside the Scala box and successfully executed a fake-model command that wrote and read a file; this used `danger-full-access`. Real provider credentials and sandboxing remain unverified.

### Reference service rehearsal and remaining blocker

The reference Rust git server built inside the rust box with crates.io and its pinned Git dependency in 1,375 seconds. The binary was saved as `/var/tmp/akuma-eco/denver-reference-git-server` (SHA-256 `ebeeac12a32b76f5b12c3ae8aeb567f3f42262c0575a19f9f346275a3fe9cb1a`). The normal scala-cli compile crashed in HotSpot (captured in `/var/tmp/akuma-eco/denver-scala-hs_err.log`). A repeat with `JAVA_TOOL_OPTIONS=-Xint` compiled the task board, including Maven downloads, but the service did not become reachable.

A trace-enabled repeat on the rebuilt disks recorded `[execve] non-leader thread slot=4` immediately followed by another thread faulting at the `ret` following musl's futex syscall, with its stack unmapped. The full captured console is `/var/tmp/akuma-eco/denver-reference-exec-trace.log`. This matches the class of failure already fixed on the main amd64 kernel in `EXEC_FROM_THREAD_DETACH.md`: the Tokio exec path replaces the shared address space without ending or detaching the other threads. The trace identifies the immediate relationship; it is not yet a tested Tokio fix. The reference service acceptance suite has **not** passed on Denver. `refdemo.py --scala-interpreted --git-server-bin PATH` permits a repeat without rebuilding the Rust server, and reports the reused binary's digest. Do not treat interpreter mode as a fix for service startup.

The demo host root now uses a real `/var/run` directory. Previously the Alpine symlink caused Denver herd to fail to create its token and leave start/stop unauthenticated. A fresh Denver boot created a nonempty mode-600 token, and the full verification against rebuilt guest disks passed **58 checks**, including a token check on each guest. The rebuilt disks and manifest are in `/var/tmp/akuma-eco/denver-ready`; they are distinct from the older default `$ECO_WORK/out` images.
