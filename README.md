# Akuma OS

*Bohemian operating system*, now with an official soundtrack, [*Omegashima*](https://tokyorider.bandcamp.com/album/omegashima) by *Tokyo Rider*.

**This project is built with various AI tools as an experiment** to understand if models at the time could be used to produce some working software and dive into a domain I had some familiarity with but never got to explore. Reading Andrew Tannenbaum 20 years ago certainly helped but looking at the code and putting stuff togeter and getting to the point of running real software on custom bare bones kernel is an exciting hobby even if it only runs in QEMU.

**Bare-metal AArch64 OS in Rust — preemptive kernel, Linux ABI, SSH, containers, apk, TCC/Clang/GCC/rustc, Git**

**Self-hosting achieved on aarch64** (2026-08-16) - see [here](./docs/archive/AKUMA_SELF_HOSTING.md).

**Now runs on Graviton 2** via Firecracker (2026-08-21) - verified on `m6g.metal` machine in `ap-northeast-1` Tokyo AWS region, see [here](./docs/archive/AKUMA_FIRECRACKER_TERRAFORM.md).

**Self-hosting achieved on amd64** (2026-09-13) - verified on bare metal HP 500-502nj (2015 setup) see [here](./docs/archive/AKUMA_SELF_HOSTING_AMD64.md).

**Booting Akuma, Linux and NetBSD as KVM guests with Akuma as host kernel achieved** (2026-10-22) - an AMD SVM hypervisor inside the amd64 kernel, verified on bare metal Ryzen 7 8845HS; see [here](./docs/archive/AKUMA_KVM.md) (design and reference: [`kvm.md`](./docs/reference/subsystems/kvm.md)). Works both via KVM backend and [Firecracker via KVM](./docs/archive/AKUMA_FIRECKRACKER_KVM.md).

```
                                             %#%:                +
                                            #####%            %%=
                                            #*###*%        -%%#=
                                            #*####%#**+**%@%**#
                                            *%@%%#%%@#%#*##+%#*
                                            *@@@%@%@%#@%%*%+#%#
                                            %@@%%%%#*%%@@@*+%%#+
                                            *%@@@@%%%@@#@@#++%##
                                            %@@@@@@@@@@@%%%#+%%#
                                            @@@@@@@@@@@@@@%#*-++
                                   **+**####@@@@@@@@@@@%@@@%#+=
                  ########**######%%%%%%@@%%@@@@@@@%%%%@@%#%#+*=
             ###%%%%@@%%%%@@@@%@@@@@@@@@@@#@%@@@@%@@@@@@@%#+#@@#+
         #######%%%@@@@%%%@@@@@@@@@@@@@@@@%%@@@@@@@@@@@@@*#@@@@%%+
       ##%%%%%@%@@%%%%@%@@%%%@@@@@@@@@@@@%%%@@@@@@#@@@@@%@@@@@@@+%
    *#%%%%@%%%%@%%%@%%@%%@%@@@@@@@@@@@@@@@%@%@@@@@@@@@@@@@@@@@@@%**
  *%%%%%%%@@@@%@@%%@@@@@%@@@@@@@@@@@@@@@@%%@@@@@@@@@@@@@@*@@@@@@%*
 %@%@@@%%%@@#%@%%@@@@@%@@@@@@@@@@%@@@@@@@@@@@%@@@@@@@@@@@@@@@@@@%%=
%@@@@@%%%%%%@@%@@@@@@@%%@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@%#@
%@@%@@@%@@@@@@@@@%@@@%@@@@%@@%%%%%%%%@@@@@@#@@@@@@@@@@@@%@@@@@@%@@@@
*%%%@@@%@%@@@@@@@@@@@@@@@@@@%@@@@@%%#%%@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@%
 %#@@@@@@@@@%@@@@@@@@@@@@@@@@@@@@@@%##@@@@@%%%@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@%%
       ++#@@@@@@%@@@@@@@@@@@@@@@@@@@%@@@@@@@%%%@@@@@@@@%#         #@@*@@@@@@@@%
                *@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@%@@@@%%              %@@@@%*
                        #@@@@@@@@@@@@@@@@@@@@@@@%@@@@@@@@@#
                                 @@@@@@@@%@@@@@@@%@@@@@%@%#+
                                                 %@@@@@%@%*+
                                                   %@%@@@%*
```

*悪魔 (Akuma) - "Demon" in Japanese*

---

## Capabilities

**Kernel** — preemptive SMP multitasking, MMU-isolated address spaces with demand
paging and CoW fork, Linux syscall ABI (~140 AArch64, growing x86_64 coverage),
static/static-PIE/dynamic ELF loading, pipes/eventfd/futex IPC, signals,
native containers ("boxes") with per-box root filesystems and OCI compatibility,
so real Linux container images run on this kernel.

**Networking** — TCP/IP (smoltcp) with virtio-net, Realtek and 802.11 drivers,
DHCP, DNS, userspace SSH-2 server, HTTP client/server, TLS in userspace only.

**Filesystems** — VFS with mount table and symlink resolution, read/write ext2,
procfs, page cache.

**Toolchains on-target** — Rust (nightly musl), Go, Python 3, Bun, GCC, Clang.
Self-hosting: the kernel compiles itself in-VM on metal on both architectures.

**Userspace** — two flavors:

- **Akuma** — busybox, herd supervisor, box container manager, sshd, meow;
  Alpine `apk` for packages
- **Gentoo** — stage3 root running on the Akuma kernel (OpenRC, portage,
  openssh) — see [`docs/archive/AKUMA_GENTOO.md`](docs/archive/AKUMA_GENTOO.md)

## Build & Run

```bash
rustup target add aarch64-unknown-none
# macOS: brew install qemu  |  Ubuntu: sudo apt-get install qemu-system-arm

git clone https://github.com/netoneko/akuma.git
cd akuma
cargo run --release
```

To build and populate the userspace disk image:

```bash
scripts/create_disk.sh
userspace/build.sh
scripts/populate_disk.sh
```

Tests (host target required since kernel target is `aarch64-unknown-none`):

```bash
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

Connect via SSH:

```bash
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null user@localhost -p 2222
```

## Documentation

- [`docs/README.md`](docs/README.md) — front door: symptom matrix, task list, subsystem index
- [`docs/runbooks/`](docs/runbooks/) — action-first procedures (debugging, building)
- [`docs/reference/`](docs/reference/) — current-state architecture and invariants, graded A/B/C
- [`docs/archive/`](docs/archive/) — 200+ historical investigation docs
- [`acceptance/`](acceptance/) — numbered acceptance test playbooks

## Project Layout

```
src/              Kernel source (no_std Rust)
crates/           Extracted kernel crates
userspace/
  libakuma/         Rust syscall wrapper library
  meow/             AI coding assistant
  tcc/              Tiny C Compiler
  herd/             Process supervisor
  scratch/          minimal Git implementation (HTTPS only)
  sshd/             SSH-2 server (the only one)
  paws/             experimental minimal shell (4 MB demo)
docs/             Architecture notes and design docs
scripts/          Build and debug scripts
config/           Configuration files
linker.ld         Kernel linker script
```

## License

BSD 2-Clause. Userspace components under different licenses (GPL2, LGPL2) follow their respective licenses.

`userspace/rumpkernel` is under BSD License same as NetBSD rumpkernel project.
