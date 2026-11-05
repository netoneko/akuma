# akuma-tokio-kernel

A whole kernel from one short `main.rs`, and one code base over **two machines**: the same `main.rs`,
`Cargo.toml` and `build.rs` build for both `--target x86_64-unknown-none` (amd64) and
`--target aarch64-unknown-none` (aarch64), and each kernel runs unmodified Linux musl binaries of its own
architecture (busybox, sshd, git, rustc, and tokio programs such as goose). amd64 boots as a PVH guest;
aarch64 boots through the Linux Image protocol with a device tree. Hosts: QEMU (`microvm` on amd64,
`virt` on aarch64) under TCG or KVM, Firecracker, and Akuma's own hypervisor (amd64). "This kernel runs
tokio" is a dependency that ensures Linux binary compatibility.

**This README is the whole brief.** Give it to an agent and it can build a new kernel, boot it, get a shell
in it, and run a tokio program on it. The sample kernel that follows it is [`shanghai/`](../../shanghai): an
agent (goose with GLM) built it from an earlier revision of this brief, and its record, gate by gate, is
[`shanghai/RESULT.md`](../../shanghai/RESULT.md). Re-checked for this revision (2026-11-05): Shanghai builds for
both targets with and without `middleware`, and boots on both under QEMU TCG (`sh shanghai/mw_gate.sh`, below). The
sshd/ssh image path, tokioprobe, goose and the `herd vm-run` spec were run when the brief was written and **not**
re-run for this revision; where a command below has drifted, the file it points at wins.

## What you write

A new kernel is a directory at the repository root with three files (and, if you want one, a banner). The example
is `shanghai/`; pick your own name and use it everywhere `shanghai` appears.

`shanghai/Cargo.toml`:

```toml
[package]
name = "shanghai"            # `uname -n`, and the release suffix in `uname -r`
version = "0.1.0"         # `uname -r`
edition = "2024"

[lints]
workspace = true

[dependencies]
akuma-tokio-kernel = { path = "../crates/akuma-tokio-kernel", features = ["no-tests"] }

[[bin]]
name = "shanghai"
path = "src/main.rs"
```

`shanghai/build.rs` (hands the kernel its link arguments; per architecture, like
[`taoxichuan/build.rs`](../../taoxichuan/build.rs); Shanghai's `build.rs` is this without the Firecracker line, since it
does not forward that feature):

```rust
fn main() {
    // amd64: the linker script `akuma-tokio-kernel` re-exports from its machine half.
    if let Ok(script) = std::env::var("DEP_AKUMA_TOKIO_KERNEL_LINKER_SCRIPT") {
        println!("cargo:rustc-link-arg-bins=-T{script}");
    } else {
        // aarch64: the workspace's .cargo/config.toml already links with -Tlinker.ld; give _boot
        // its stack size, and Firecracker its load address when that feature is on.
        assert!(
            std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64"),
            "akuma-tokio-kernel exports a linker script on amd64"
        );
        println!("cargo:rustc-link-arg-bins=--defsym=BOOT_STACK_SIZE={}", 1024 * 1024);
        if std::env::var_os("CARGO_FEATURE_PLATFORM_FIRECRACKER").is_some() {
            println!("cargo:rustc-link-arg-bins=--defsym=KERNEL_PHYS_BASE_OVERRIDE=0x80300000");
        }
    }
}
```

`shanghai/src/main.rs` (Shanghai's, less its optional `middleware` lines; `banner.txt` is any plain-ASCII art, at most
78 columns by 20 lines):

```rust
#![no_std]
#![no_main]

akuma_tokio_kernel::entry!(main);

fn main(kernel: akuma_tokio_kernel::Kernel) -> ! {
    kernel
        .name("Shanghai")
        .banner(include_str!("banner.txt"))
        .console()
        .disk()
        .net()
        .smp()
        .run_init()
}
```

Then add the directory to `members` in the root `Cargo.toml` (`members = [".", "amd64", "taoxichuan", "shanghai",
"crates/*"]`). Do **not** add it to `default-members`: a kernel bin builds only per target
(`x86_64-unknown-none` and `aarch64-unknown-none`), which the default (host) build must never reach.

## What you get

| builder | feature (default on) | what it wires |
|---|---|---|
| `entry!(main)` | — | the boot entry symbols (amd64: `kmain`/`kmain_mb2` for PVH; aarch64: `rust_start` for the Image protocol, plus the secondary trampoline), the heap, the panic handler; `main` gets a `Kernel` named after your package |
| `.name("Shanghai")` | — | the name on the console and in the banner (the package name stays `uname -n`) |
| `.banner(include_str!("banner.txt"))` | — | art printed before init starts |
| `.console()` | `console` | the serial console (COM1 on amd64, the PL011 on aarch64): boot lines, kernel messages, and the stdout/stderr of init and of every process with no ssh session |
| `.disk()` | `disk` | the root filesystem: ext2 on the first virtio-blk device, mounted at `/` |
| `.root(make)` | — | your own root instead: `make: fn() -> Arc<dyn akuma_vfs::Filesystem>`, called once the heap is up (amd64 only today) |
| `.net()` | `net` | the first virtio-net device, smoltcp, and a DHCP lease (waits up to 10 s). Without it, sockets work on loopback only |
| `.smp()` | `smp` | the application processors `smp=N` on the command line asks for, all under one big kernel lock |
| `.run_init()` | — | boot, then load `init=` from the root and run it as pid 1; never returns |

Without a feature its method does not exist, so a kernel that does not want a part leaves out the call, and can
also leave out the code: `default-features = false, features = ["disk", "no-tests"]` builds a kernel with no
console, no NIC and one core. The root filesystem is the one part a kernel cannot run without (`.disk()` or
`.root(..)`). Other features: `shell` (the calls busybox and bash make that no tokio program does; turn it on if
your image runs a shell beyond `sh -c`), `trace-err` (print every failing syscall on the console), and
`platform-firecracker` (aarch64 under Firecracker, whose memory map differs from QEMU virt's; the `build.rs`
above moves the link base for it — amd64 boots Firecracker through PVH and needs nothing), and
`middleware` ([below](#syscall-middleware-optional); off by default, and a kernel that leaves it off compiles none of it).

Everything else is fixed and needs no code: the memory manager (`mmap`, `brk`, `mprotect`, copy-on-write
`fork`), processes and threads (`clone`, `execve`, `wait4`), signals, pipes, epoll, futexes, the clock, the
RNG, and the Linux syscall surface from [`akuma-runtime-tokio`](../akuma-runtime-tokio) (its README has the
picture). The machine half is picked by the build target: [`akuma-tokio-kernel-amd64`](../akuma-tokio-kernel-amd64)
or [`akuma-tokio-kernel-aarch64`](../akuma-tokio-kernel-aarch64), both driving the arch-neutral decisions
they share in [`akuma-tokio-kernel-core`](../akuma-tokio-kernel-core); how it works inside is documented in
[`taoxichuan/docs/`](../../taoxichuan/docs), Taoxichuan being the first kernel built this way — one `main.rs`,
built twice.

## Syscall middleware (optional)

With the `middleware` feature (off by default, for every kernel) a kernel registers **one** table of hooks around
syscall dispatch: `kernel.middleware(&MW)`. The design is
[`docs/middleware.md`](../akuma-tokio-kernel-core/docs/middleware.md) (the guide: first syscall, the tiers, the rules) and
[`docs/archive/TAOXICHUAN_B22_SYSCALL_MIDDLEWARE.md`](../../docs/archive/TAOXICHUAN_B22_SYSCALL_MIDDLEWARE.md) (the design record); the code is
`akuma_tokio_kernel_core::middleware`, re-exported as `akuma_tokio_kernel::middleware`, and it is the same on
amd64 and aarch64.

```rust
use akuma_tokio_kernel::middleware::{CUSTOM_BASE, Hooks, Middleware, On, Route, Syscall, Verdict};

static MW: Middleware<u64> = Middleware::new(0, Hooks {
    // A syscall of your own: 1000 and up is above every number Linux assigns. `fallback` runs only where the
    // kernel would have answered -ENOSYS, so it cannot break a call the kernel already serves.
    fallback: &[Route::new(&[On::Nr(CUSTOM_BASE)], |_ctx| Some(42))],
    // Look at, change or refuse a call. Runs inside the kernel lock; the caller's memory is reachable.
    kernel_pre: &[Route::new(&[On::Call(Syscall::Openat)], |ctx| {
        let mut buf = [0u8; 128];
        if let Ok(path) = ctx.user_str(ctx.args[1], &mut buf) {
            akuma_primitives::safe_print!(160, "[mw] pid {} openat {}\n", ctx.pid, path);
        }
        Verdict::Pass
    })],
    // Outside the kernel lock, around it: plain integers and private state, never user memory.
    own_pre: &[Route::new(&[On::Call(Syscall::Getpid)], |ctx| *ctx.state += 1)],
    ..Hooks::new()
});

fn main(kernel: akuma_tokio_kernel::Kernel) -> ! {
    kernel.name("Shanghai").console().disk().middleware(&MW).run_init()
}
```

From the guest, `syscall(1000)` returns 42. Shanghai carries a working table behind its own `middleware` feature, [`shanghai/src/middleware.rs`](../../shanghai/src/middleware.rs)
(`cargo build -p shanghai --features middleware --target <arch>-unknown-none --release`), and `sh shanghai/mw_gate.sh
[x86_64|aarch64|all]` (`SMP=4` for four cores) boots it, and a control built without it, under QEMU TCG with
[`shanghai/mwprobe`](../../shanghai/mwprobe) as `init`: the control must print `MWPROBE RESULT CONTROL PASS`, the
middleware kernel `MWPROBE RESULT PASS`. It needs no sshd or key, only `e2fsprogs`. The rules, each with its reason in the module docs:

- **One table, flat, no chaining.** No `next`, no priorities. Two entries for one call in one tier are refused when
  the table is armed (a boot panic naming the number). A second `.middleware(..)` panics. To layer, write a
  function that calls your functions and register that.
- **Two lock modes, chosen by the tier.** `kernel_*` and `fallback` hooks run under the kernel lock, which the
  kernel takes; `own_*` hooks run outside it with a private state `S` that an autolock serialises. A hook never
  sees, names or takes a lock. `own` hooks must be short and must not block, allocate or print, and get **no
  user memory** (the page-table walk behind a user copy races a sibling's `munmap` when the lock is not held).
- **Cheap when idle.** Which numbers a tier wants is a 512-bit mask built when the table is armed, read once per
  call and latched, so an unhooked call costs a relaxed load and a bit test per tier. `Verdict::PassAndForget`
  clears a hook's bit for that number. Numbers from 512 up are always asked (that is where custom calls live).
- **Not everything is interceptable.** `exit`, `exit_group`, `clone`, `execve` and `rt_sigreturn` are observed
  only: a `Return` verdict or a changed result is ignored.
- **No `unsafe`.** The module is `forbid(unsafe_code)` and the API needs none; your crate decides its own.
- A hook that panics is a kernel panic, and `own` hooks run with preemption off.

## Build it

```bash
cargo build -p shanghai --target x86_64-unknown-none --release
# -> target/x86_64-unknown-none/release/shanghai

cargo build -p shanghai --target aarch64-unknown-none --release
# -> target/aarch64-unknown-none/release/shanghai
```

On aarch64 the workspace's linker wrapper (`scripts/link_kernel.sh`) also objcopies a flat `.bin` next to the
ELF — `target/aarch64-unknown-none/release/shanghai.bin` — which is the file QEMU's `-kernel` takes: it speaks
the Linux Image protocol, and QEMU hands the kernel its device tree. When cargo reuses a cached ELF (you switched
feature sets back) it does not relink and the wrapper does not run, so the `.bin` can be the other build's: make it
from the ELF yourself with `sh scripts/mkbin.sh target/aarch64-unknown-none/release/shanghai shanghai.bin`
(`docs/archive/LINK_KERNEL_BIN_LAYOUT.md`).

## The smallest disk that gets you inside

Four things: an ext2 image, `sshd`, the public key you will log in with, and a shell for `sshd` to run commands
with. Needs `e2fsprogs` (`brew install e2fsprogs` puts `mke2fs` and `debugfs` in
`/opt/homebrew/opt/e2fsprogs/sbin`; on Linux they are on `PATH`).

```bash
E=/opt/homebrew/opt/e2fsprogs/sbin   # or E=/sbin on Linux
W=/tmp/shanghai; mkdir -p $W           # any work directory

# 1. sshd: Akuma's own, built as a plain Linux binary (libakuma's `linux-abi`: fork + execve, no private syscalls).
(cd userspace && cargo build -p sshd --release --target x86_64-unknown-none)   # `linux-abi` is on by default now

# 2. a shell: busybox's static build for the kernel's target (sshd runs every command as `/bin/sh -c`).
curl -sSL -o $W/busybox https://www.busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox

# 3. the key you log in with.
ssh-keygen -q -t ed25519 -N '' -C shanghai -f $W/key

# 4. the image.
$E/mke2fs -q -F -t ext2 -b 4096 $W/shanghai.img 64m
cat > $W/cmds <<EOF
mkdir /bin
mkdir /etc
mkdir /etc/sshd
mkdir /tmp
write $PWD/userspace/target/x86_64-unknown-none/release/sshd /bin/sshd
sif /bin/sshd mode 0100755
write $W/busybox /bin/busybox
sif /bin/busybox mode 0100755
symlink /bin/sh /bin/busybox
write $W/key.pub /etc/sshd/authorized_keys
EOF
$E/debugfs -w -f $W/cmds $W/shanghai.img
```

For an aarch64 kernel every guest binary must be aarch64 musl: build sshd with
`--target aarch64-unknown-none` and take busybox's `1.35.0-aarch64-linux-musl` build; the `debugfs` steps
and everything after are identical.

`sshd` reads `/etc/sshd/authorized_keys`, listens on the port its command line gives, and makes its host key on
first boot. `/bin/sh` is the only applet name linked; run the others as `busybox <applet>`, or link more names
the same way (`symlink /bin/uname /bin/busybox`). Write to an image with `debugfs -w` only while no guest has it
open.

## Boot it

On a Mac (QEMU TCG, no KVM) with the repository's rig, which boots the image through a throwaway overlay and
waits until an ssh command completes:

```bash
export TX_KERNEL=$PWD/target/x86_64-unknown-none/release/shanghai TX_DISK=$W/shanghai.img TX_KEY=$W/key \
       TX_RUN=$W/run TX_PORT=2245
python3 scripts/kvm/taoxichuan/tcg_rig.py boot fresh     # BOOT OK ready_in=6s ...
python3 scripts/kvm/taoxichuan/tcg_rig.py run 'busybox uname -a'
#   Linux shanghai 0.1.0-<sha>-shanghai <sha>-release x86_64 GNU/Linux
python3 scripts/kvm/taoxichuan/tcg_rig.py stop
```

`TX_SMP=4` gives the guest four vCPUs and puts `smp=4` on its command line. The console is `$W/run/console.log`
(read it with `grep -a`); a good boot prints `Shanghai/amd64 -- long mode reached`, `fs: ext2 root on /dev/vda`,
`net: DHCP configured, 10.0.2.15`, the banner, and `Shanghai: entering ring 3 -- /bin/sshd`.

Without the rig, the QEMU line is (`accel=kvm` on a Linux host with KVM; the guest writes the image directly):

```bash
qemu-system-x86_64 -M microvm,accel=tcg -cpu max -smp 1 -m 3072 \
  -kernel target/x86_64-unknown-none/release/shanghai \
  -append "init=/bin/sshd initargs=--port,22 epoch=$(date +%s) virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6" \
  -nodefaults -no-reboot -display none -serial file:$W/console.log \
  -global virtio-mmio.force-legacy=false \
  -drive file=$W/shanghai.img,format=raw,if=none,id=d0 -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0 \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2245-:22 -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.1
ssh -i $W/key -p 2245 -o StrictHostKeyChecking=no root@127.0.0.1 'busybox uname -a'
```

On aarch64 the kernel boots on QEMU `virt`, with the same `-append` and the aarch64 image. Every virtio device
must name its bus (`virtio-mmio-bus.N`): QEMU puts the first device with no `bus=` on the *highest* of its 32
slots, while the kernel scans the lowest 8, so an unnamed device is invisible:

```bash
qemu-system-aarch64 -machine virt,gic-version=3 -accel tcg -smp 1 -m 1024 \
  -kernel target/aarch64-unknown-none/release/shanghai.bin \
  -append "init=/bin/sshd initargs=--port,22 epoch=$(date +%s)" \
  -nodefaults -no-reboot -display none -serial file:$W/console.log \
  -global virtio-mmio.force-legacy=false \
  -drive file=$W/shanghai.img,format=raw,if=none,id=d0 \
  -device virtio-blk-device,drive=d0,bus=virtio-mmio-bus.1 \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2245-:22 \
  -device virtio-net-device,netdev=n0,bus=virtio-mmio-bus.0
ssh -i $W/key -p 2245 -o StrictHostKeyChecking=no root@127.0.0.1 'busybox uname -a'
#   Linux shanghai 0.1.0-<sha>-shanghai <sha>-release aarch64 GNU/Linux
```

`-smp 4` is SMP here as `TX_SMP=4` is there. A good boot prints `Shanghai/aarch64 -- EL1, MMU on, dtb at
0x48000000`, the RAM and slot line from the device tree, `ext2 root on /dev/vda`, `DHCP configured,
10.0.2.15`, the banner, and sshd's lines. `scripts/cargo_runner.sh`, the repo's aarch64 runner, is this same
line — net on bus.0, disk on bus.1, a virtio-rng on bus.2 — with KVM inside a Lima VM on the Mac and TCG
outside it; the `tcg_rig.py` battery driver is amd64-only so far.

The command line: `init=` and `initargs=` (comma-separated) name pid 1; `epoch=` is the wall clock in Unix
seconds (a microvm has no RTC, and TLS refuses certificates from 1970); `smp=N` starts N cores with `.smp()`.
`force-legacy=false` matters: the kernel's virtio drivers speak only the v2 transport. On amd64 the microvm's
virtio-mmio windows are reserved on the command line too (`virtio_mmio.device=512@...`); on aarch64 QEMU
`virt` lists its slots in the device tree, which is also how the command line arrives (`/chosen/bootargs`).

Under Akuma's own hypervisor (the ryzen booted into Akuma; amd64 today), the same kernel and image boot as a
`herd vm-run` guest. This spec is the one `scripts/kvm/taoxichuan/fp_metal.py` boots Taoxichuan with; it was not re-run
for this revision:

```bash
cat > vm.spec <<EOF
type=kvm
kernel=/root/shanghai/shanghai
cmdline=init=/bin/sshd initargs=--port,22 smp=4 epoch=$(date +%s)
ram_mib=3072
vcpus=4
disk=/root/shanghai/shanghai.img
disk_ram=0
outbound=1
forward=2246:22
EOF
setsid /bin/herd vm-run vm.spec </dev/null > vm.log 2>&1 &
```

## Run tokio on it

`userspace/tokioprobe` is the smallest check that tokio works: a multi-thread runtime, a child process through
`tokio::process`, and an HTTPS request with reqwest and rustls. It needs DNS and a `uname` on `PATH`, and
builds for either target (`ARCH` names the kernel's):

```bash
(cd userspace/tokioprobe && ARCH=x86_64 ./build-musl.sh)       # -> bootstrap/bin/tokioprobe.x86_64
(cd userspace/tokioprobe && ARCH=aarch64 ./build-musl.sh)      # -> bootstrap/bin/tokioprobe
echo "nameserver 10.0.2.3" > $W/resolv.conf                      # QEMU user networking's resolver
cat > $W/cmds2 <<EOF
write $PWD/bootstrap/bin/tokioprobe.x86_64 /bin/tokioprobe
sif /bin/tokioprobe mode 0100755
symlink /bin/uname /bin/busybox
write $W/resolv.conf /etc/resolv.conf
EOF
$E/debugfs -w -f $W/cmds2 $W/shanghai.img
TX_SMP=4 python3 scripts/kvm/taoxichuan/tcg_rig.py boot fresh
python3 scripts/kvm/taoxichuan/tcg_rig.py run /bin/tokioprobe 300
#   RUNTIME PASS multi-thread workers=4
#   UNAME PASS Linux shanghai ...
#   HTTPS PASS https://akuma.sh/ status=200 ...
#   RESULT PASS
```

goose (an AI agent, tokio throughout) needs a fuller image: the dynamic loader, a CA bundle, its config and an
API key. `scripts/kvm/taoxichuan_goose_image.sh` builds one; the rig's `battery goose` runs the regression
battery plus a goose prompt that must answer `PONG`. goose ships one tarball per architecture
(`goose-x86_64-unknown-linux-musl` / `goose-aarch64-unknown-linux-musl`); both machines answer `PONG`.

## Check a workload before you boot it

Every syscall a program makes either has a server here or it does not. Trace the program on Linux with
`strace -f -c`, then `scripts/kvm/taoxichuan/census_classify.py OUT_DIR UNISTD_64_H` names who serves each
call and exits 1 on one nobody does. A call nobody serves answers `ENOSYS` and the console prints
`[nosys] nr=N` once.

## Known limits

- **Two machines, one brief**: amd64 and aarch64 both run the same battery — sshd, pipes, signals, `smp=4`
  under one kernel lock, tokioprobe, goose. Still amd64-only today: the in-guest self-host build, and the
  `tcg_rig.py` battery driver (for aarch64, drive QEMU `virt` by hand as above).
- **A QEMU/Firecracker guest only**: virtio-mmio devices, no PCI, no multiboot2 (`entry!`'s `kmain_mb2` says
  so and halts). amd64 boots Firecracker through PVH; aarch64 turns on `platform-firecracker`.
- **One root filesystem**, mounted at `/`, plus `/proc`. ext2 inode times are 0.
- The open gaps (no `sendfile`, eager `mmap`, …) are [`taoxichuan/docs/gaps.md`](../../taoxichuan/docs/gaps.md).

## Background

[`proposals/B15_TOKIO_KERNEL_KIT.md`](../../proposals/B15_TOKIO_KERNEL_KIT.md) (the plan),
[`proposals/B21_TOKIO_KERNEL_AARCH64.md`](../../proposals/B21_TOKIO_KERNEL_AARCH64.md) and its record
[`docs/archive/TAOXICHUAN_B21_AARCH64.md`](../../docs/archive/TAOXICHUAN_B21_AARCH64.md) (the aarch64 port,
with the parity table over both machines),
[`docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`](../../docs/archive/TAOXICHUAN_CLEANUP_PLAN.md) (B15),
[`docs/archive/TAOXICHUAN_FIXED_POINT.md`](../../docs/archive/TAOXICHUAN_FIXED_POINT.md) (the kernel this came
from compiles itself).
