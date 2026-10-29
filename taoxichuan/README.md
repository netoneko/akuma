# Taoxichuan (淘熙川) — the minimal composed Akuma amd64 kernel

The composition root of the minimal-self-hosting experiment's metal track: a
kernel built by **pulling crates, one line per stage**, with as few crates as
will serve — and where the logic was still stuck in `amd64/src/`, extracting it
into a crate first and pulling that (`akuma-boot-amd64`, `akuma-comconsole`,
`akuma-ring3-amd64` are the extractions this kernel exists over). A piece that
exists in a crate is pulled, never copied; what is written new here is only
what a composition root owns: the wiring, the table instances, and the syscall
arms' copy loops.

The experiment brief is `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md`; its
running log is `docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md`.

## What it boots as

A kvm0/Firecracker-style **PVH guest**, single vCPU, virtio-mmio only (no PCI
walk), an ext2 root on virtio-blk, virtio-net with DHCP, COM1 console. Ring 3
runs static musl binaries spawned from the kernel command line
(`init=`/`then=`); the staged ladder has reached busybox, sshd, `apk add curl`,
`curl https://…`, and a first C-compile probe with tcc. See
[`docs/README.md`](docs/README.md) for the status ladder and the doc set.

## Build

Not in the workspace `default-members` (an x86_64-only bin the default aarch64
build must never reach). Build it for its target, explicitly:

```bash
cargo build -p taoxichuan --target x86_64-unknown-none --release --features no-tests
```

The linker script is handed over by `build.rs` (`rustc-link-arg-bins`), scoped
to this package's binary — the same reasoning as `akuma-amd64`. Boot as a guest
under the dev box's kvm0: `scripts/kvm/goose/boot_dev.sh` (installs to
`/boot/akuma-guest-dev`, rewrites the vm-dev herd confs, waits for ssh). The
runbook is [`docs/runbooks/build-and-boot.md`](docs/runbooks/build-and-boot.md).

## Layout

| path | what it holds |
|---|---|
| `src/main.rs` | boot order, IDT/traps, LAPIC bring-up, the task record + fd table, ELF loader, the syscall dispatcher, the program list |
| `src/child.rs` | the child model: SPAWN run-to-completion crossing, exec-replace, the M2 blocking fork, wait4/waitpid, the spawn ring |
| `src/exec_twin.rs` | registration of the `akuma-exec`/`akuma-threading` hook tables (the runtime twin of amd64's `exec_runtime.rs`) |
| `src/net.rs` | `NetRuntime` callbacks, the PIT-calibrated TSC clock, RDRAND, the NIC pump |
| `src/pipes.rs` | the `akuma-pipes` table instance and its fd arms |
| `src/sock.rs` | the AF_INET syscall arms over `akuma-net`'s socket table |
| `src/banner.txt` | the boot banner, included verbatim |
| `linker.ld` | low trampoline + high-kernel layout (shared shape with `akuma-amd64`) |
| `docs/` | the kernel's own doc set: reference, runbooks, diagrams, gaps |

## Doc rule

`docs/` is the living documentation of this kernel and is kept current in the
same commit as the code it describes — every syscall arm has a row in
`docs/reference/syscalls.md`, and a diagram changes when the behaviour it
shows changes. The rules are in [`docs/README.md`](docs/README.md).
