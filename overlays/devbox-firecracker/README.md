# devbox-firecracker

Akuma as a Firecracker microVM. The sibling of `overlays/devbox-smoltcp` (QEMU,
smoltcp, real SMP) and `overlays/devbox` (QEMU, rump).

**Status: everything but inbound packets, at 1 vCPU.** Boots, mounts its ext2
root (including the 6 GB devbox image), passes the boot suite 290/0/0, runs
userspace, and starts `/bin/sshd` under herd. Outbound networking works and is
correct on the wire. **Inbound (RX) does not**, so SSH is not reachable yet —
see §4. `--vcpus 1` only.

- Procedure: `docs/runbooks/run-on-firecracker.md`
- Platform invariants and constants: `docs/reference/firecracker/`
- How it got here, and the five bugs it exposed:
  `docs/archive/AKUMA_FIRECRACKER_KVM.md`

## Scripts

Idempotent, run in order.

| Script | Runs on | Does |
|---|---|---|
| `host-setup.sh` | macOS | Verifies nested virt, creates the Lima VM, proves `/dev/kvm` and EL2 |
| `guest-setup.sh` | KVM host | Installs Firecracker, creates tap0, starts DHCP, adds NAT |
| `build.sh` | repo root | Builds `platform-firecracker`, asserts the load address, flattens to `akuma-fc.bin` |
| `run.sh` | either | Stages files if needed, writes the config, boots, saves the log |

```bash
overlays/devbox-firecracker/host-setup.sh     # macOS only; skip on metal
overlays/devbox-firecracker/guest-setup.sh    # add --local on metal
overlays/devbox-firecracker/build.sh
overlays/devbox-firecracker/run.sh            # add --local on metal
```

`run.sh --help` lists `--no-disk`, `--no-net`, `--vcpus`, `--mem`,
`--interactive`, `--timeout`. Build a devbox-shaped image (userspace sshd via
herd, boot suite skipped) with
`FC_FEATURES=devbox-smoltcp,no-tests overlays/devbox-firecracker/build.sh`.

SSH is forwarded on host port **4444**, deliberately not 2222: QEMU's devbox
runner uses that, and Lima forwards guest listeners to the host, so both would
claim it — `localhost:2222` then silently resolves to whichever bound first.

## 1. Why there is no QEMU here

Unlike the other overlays, this one cannot be exercised without a hypervisor.
`qemu-system-aarch64 -M virt` has its RAM base and every device address baked
into the machine model, and QEMU's relocatable `microvm` machine is x86-only. So
Firecracker's memory map can only be tested under Firecracker, which makes
`/dev/kvm` a hard prerequisite rather than a convenience.

On macOS that means a Lima VM with nested virtualization (Apple silicon M3+,
macOS 15+). Docker Desktop **cannot** substitute: its linuxkit kernel is built
with `CONFIG_VIRTUALIZATION` unset *and* its VM boots at EL1.

## 2. Why there is a DHCP server and a tap device

Firecracker has no built-in networking. QEMU's `-netdev user` gives you SLIRP — a
user-mode NAT stack with its own DHCP server and `hostfwd` port forwarding, free.
Firecracker ships none of that: virtio-net is a raw bridge to a host tap device,
and addressing, DHCP, routing and NAT are the host's job.

Akuma boots with `config::ENABLE_DHCP = true`, so `guest-setup.sh` runs `dnsmasq`
on tap0 handing out `10.0.2.15-30` with router and DNS at `10.0.2.2` —
deliberately the same addresses QEMU's SLIRP uses, so Akuma's DHCP path and its
no-DHCP fallback (`crates/akuma-net/src/smoltcp_net.rs`) agree.

## 3. Disks

`run.sh` stages the image into the Lima guest because Lima mounts the host
**read-only**. That copy is an artifact of the Lima sandwich and disappears on
metal, where `--local` passes paths straight through. The AWS EBS / instance-store
mapping is written up in `docs/reference/firecracker/disk-and-volumes.md`.

## 4. Known limits

- **`--vcpus 1` only.** The GIC redistributor base is
  `0x3FFF_0000 - vcpu_count * 0x2_0000`; Akuma's bootstrap map assumes one vCPU,
  so anything more makes the boot core drive another core's redistributor and
  silently lose its timer. The FDT-derived device map that fixes this is not
  implemented.
- **Inbound (RX) never reaches the guest.** TX is correct — dnsmasq answers
  `DHCPOFFER` — but no `DHCPREQUEST` follows and host ARP goes unanswered. A
  receive buffer *is* posted; the device still never fills it. The only thing
  between here and SSH. `docs/archive/AKUMA_FIRECRACKER_KVM.md` §5.1.
- **sshd runs but is unreachable**, for the reason above.
- **Requires `virtio-drivers` 0.13+.** 0.7.5 gets the virtio-net header size
  wrong under `VERSION_1` and shifts every frame two bytes.
- `run.sh` always attaches `"entropy": {}`. Without a virtio-rng device three
  boot-suite tests fail on `getrandom` returning `EIO`; QEMU's runner always
  provides one, so its absence looks like a kernel bug.
