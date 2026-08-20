# scripts/firecracker

Running Akuma as a Firecracker microVM guest. Firecracker needs `/dev/kvm` and an
aarch64 host, so on a Mac everything below runs inside a Lima VM with nested
virtualization; on AWS `.metal` the guest-side scripts run directly on the host.

Narrative walkthrough and troubleshooting: `docs/runbooks/run-on-firecracker.md`.
Current state, and the bugs this shook out: `docs/archive/AKUMA_FIRECRACKER_KVM.md`.

## Scripts

Run in this order. Each is idempotent.

| Script | Runs on | What it does |
|---|---|---|
| `host-setup.sh` | macOS | Checks nested-virt support, creates the Lima VM, verifies `/dev/kvm` |
| `guest-setup.sh` | Lima guest / metal | Installs Firecracker, creates tap0, starts DHCP, adds NAT |
| `build.sh` | macOS (repo root) | Builds the `platform-firecracker` kernel and flattens it to `akuma-fc.bin` |
| `run.sh` | macOS | Copies the kernel in, writes the config, boots, saves the log |

Quick start from a clean machine:

```bash
scripts/firecracker/host-setup.sh
scripts/firecracker/guest-setup.sh
scripts/firecracker/build.sh
scripts/firecracker/run.sh
```

`run.sh` takes options — `--no-disk`, `--no-net`, `--vcpus N`, `--mem MiB`,
`--interactive`. See `run.sh --help`.

## Why there is a DHCP server and a tap device here

Firecracker has **no built-in networking**. QEMU's `-netdev user` provides SLIRP:
a user-mode NAT stack with its own DHCP server and `hostfwd` port forwarding, all
free. Firecracker ships none of it — its virtio-net device is a raw bridge to a
host tap device, and addressing, DHCP, routing and NAT are the host's problem.

Akuma boots with `config::ENABLE_DHCP = true`, so something has to answer its
DHCP request. `guest-setup.sh` runs `dnsmasq` on tap0 handing out
`10.0.2.15-30` with router and DNS at `10.0.2.2` — deliberately the same
addresses QEMU's SLIRP uses, so Akuma's no-DHCP fallback path
(`10.0.2.15/24` via `10.0.2.2`, `crates/akuma-net/src/smoltcp_net.rs`) stays
consistent with the DHCP-assigned one.

## Known limits

- **`--vcpus 1` only.** Firecracker places the GIC redistributors at
  `0x3FFF_0000 - vcpu_count * 0x2_0000`, so CPU0's frames move with the vCPU
  count, and Akuma's compile-time bootstrap map assumes one. The FDT-derived
  device map that fixes this is not implemented — see
  `proposals/FIRECRACKER_PORT.md` §5. `run.sh` warns if you ask for more.
- The Lima virtiofs mount of the host is **read-only**, which is why `run.sh`
  copies the disk image into the guest's own filesystem rather than using it in
  place.
