# devbox-firecracker

Akuma as a Firecracker microVM. The sibling of `overlays/devbox-smoltcp` (QEMU,
smoltcp, real SMP) and `overlays/devbox` (QEMU, rump).

**Status: works at 1 vCPU, SSH included.** Boots, mounts its ext2 root, passes
the boot suite **292/0/0**, runs userspace, starts `/bin/sshd` under herd, takes
a DHCP lease, and **accepts an SSH session from outside the host.**

Verified 2026-08-21 on an AWS `m6g.metal` host (`../akuma-terraform`), 1 vCPU,
1024 MiB, at `fab3e50c`:

```
[Platform] FDT device map: GICR=0x3ffd0000
[Memory] Detected from DTB: base=0x80200000, size=1022 MB
[SmolNet] DHCP configured / IP: 10.0.2.15/24
lines=3491 PASSED=292 FAILED=0 POISON=0
$ ssh -p 4444 root@<eip> -- uname -a
Akuma akuma 0.0.7 fab3e50-release-smp-shared aarch64 Linux
```

That closes the inbound-RX question this file used to carry as unverified: the
receive buffer was too small to open Firecracker's delivery gate (§4), and with
`RX_BUFFER_LEN = 65568` the DHCP handshake completes, which it cannot do without
inbound frames. `--vcpus 1` is still the only tested topology here.

## Known unstable here: `thread_slot_reclaim_on_spawn`

**2026-09-01, under Lima nested virt on Apple silicon:** the boot suite reports
`PASSED=305 FAILED=1`, and the one failure is always

```
[Test] thread_slot_reclaim_on_spawn FAILED: hot_reclaim=85 (want 0,
       in_cooldown_window=true) respawn_ok=true gated_from_other_thread=0 (want 0)
```

Treat it as **unstable on Firecracker, not as a regression.** Evidence:

- A/B'd the same day against a stashed-clean tree: **both arms report exactly
  `PASSED=305 FAILED=1`**, same test, on the same host.
- The same build passes the full suite on QEMU at `SMP=4 MEMORY=2048M`:
  265 pass, 0 fail.
- The assertion is a *timing* property — `in_cooldown_window=true` says the
  reclaim happened inside a window the test expects to suppress it — and this
  host is a Firecracker microVM inside a Lima VM inside macOS. The AWS
  `m6g.metal` run above (real hardware, 1 vCPU) reported `292/0/0`.

So the useful signal from a Firecracker run here is **`FAILED` going above 1, or
this line changing to a different test**, not `FAILED=0`. Anything that touches
thread-slot reclaim or the scheduler cooldown should be checked on metal.

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

- ~~**`--vcpus 1` only.**~~ **`--vcpus 2` verified 2026-08-21** on
  `m6g.metal`. The GIC redistributor base is
  `0x3FFF_0000 - vcpu_count * 0x2_0000`, so a map pinned to one vCPU makes the
  boot core drive *another* core's redistributor and silently lose its timer.
  The FDT-derived device map (`crates/akuma-firecracker` ->
  `platform::install_fdt_device_map`) now reads it at run time, and the boot log
  says so out loud:

  ```
  vcpus=1  [Platform] FDT device map: GICR=0x3ffd0000
  vcpus=2  [Platform] FDT device map: GICR=0x3ffb0000 (moved from bootstrap literal)
           [SMP-shared] probed 2 core(s) / CPU_ON core 1 (mpidr=0x1) -> ok
           [SMP-shared] ✓ 1 secondary core(s) online (shared kernel)
  ```

  At 2 vCPUs the suite is **302/0/0** (the +10 over 1 vCPU are the SMP tests),
  SSH works, and `nproc` in the guest returns 2. **`(moved from bootstrap
  literal)` is the line to check** — its absence at `vcpus > 1` means the parse
  fell back and the boot core is about to lose its tick. `--vcpus 4` is untried
  here; `fab3e50c` records `smp=4 breaks disk in lima`.
- ~~**Inbound (RX): fixed in code, unverified on a boot.**~~ **Verified
  2026-08-21** (see the status block above). The symptom was a NIC that
  transmitted perfectly and received absolutely nothing — dnsmasq answered
  `DHCPOFFER`, no `DHCPREQUEST` followed, host ARP went unanswered, and a receive
  buffer *was* posted that the device never filled. The counter to read if it
  ever comes back is `recvd` in the `Heartbeat` line's `rx posted=N fail=N
  recvd=N`: it was 0 before the fix and tracks `posted` after it.

  The cause is a Firecracker behaviour with no guest-visible error:
  **its virtio-net will not read a single frame off the host tap until the
  *total* capacity of the posted receive descriptors reaches `MAX_BUFFER_SIZE` =
  65562 bytes** (`src/vmm/src/devices/virtio/net/device.rs`,
  `read_from_mmds_or_tap`). One 2 KB buffer is 2048 of that 65562, so the gate
  never opened and every inbound frame was dropped into the device's
  `no_rx_avail_buffer` metric. QEMU asks for no such thing, which is why the same
  driver had worked for years.

  `akuma_net::smoltcp_net::RX_BUFFER_LEN` is now 65568 (65562 rounded up to a
  multiple of 8) on every platform rather than behind a Firecracker `cfg`, so the
  receive path exercised daily is the one Firecracker needs.
  `VIRTIO_NET_F_MRG_RXBUF` would let the device chain smaller buffers to the same
  total, but `virtio-drivers` does not offer that feature, so the capacity has to
  come from one descriptor. Background:
  `docs/archive/AKUMA_FIRECRACKER_KVM.md` §5.1.
- **`extreme-size` has no inbound networking under Firecracker.** It keeps the
  2 KB buffer deliberately — it boots in 4 MB of RAM, where 64 KB of BSS is 1.6%
  of the machine, and it is a QEMU target (`acceptance/05`, the 4 MB floor). Run
  that profile here and RX is dead for the reason above.
- **Audio and the framebuffer are compiled out.** Firecracker's device tree has
  no `fw_cfg` node and no sound device, and upstream implements neither, so
  `kernel_framebuffer` / `kernel_audio` (build.rs) keep `src/ramfb.rs`,
  `src/fw_cfg.rs` and the virtio-sound driver out of the image entirely — 0
  `ramfb`/`fw_cfg`/`audio` symbols against 14 in the QEMU build. This also
  removes the fault that `ramfb::init` used to take on an unmapped fw_cfg
  window. Boot-suite check: `test_platform_device_gates`.
- **Requires `virtio-drivers` 0.13+.** 0.7.5 gets the virtio-net header size
  wrong under `VERSION_1` and shifts every frame two bytes.
- `run.sh` always attaches `"entropy": {}`. Without a virtio-rng device three
  boot-suite tests fail on `getrandom` returning `EIO`; QEMU's runner always
  provides one, so its absence looks like a kernel bug.
- **The `devbox` image root has no busybox applet links and no CA bundle.** Both
  are consequences of one packaging rule, and both were hit on 2026-08-21:

  `overlays/devbox-firecracker-aws/build-rootfs-image.sh --profile devbox` takes
  `/etc` from `overlays/devbox/rootfs/etc` **only** (mirroring
  `overlays/devbox/bootstrap.sh` step 3, which wipes the base `/etc` first). So:

  | Missing | Where it actually lives | Symptom in the guest |
  |---|---|---|
  | applet links (`ls`, `uname`, …) | nowhere — `bootstrap/bin` holds 59 *regular* files and 1 symlink, no applet links to copy | every command is `not found`; only `/bin/busybox <applet>` and `/bin/sh` work until someone runs `busybox --install` |
  | `etc/ssl/certs/ca-certificates.crt` | `bootstrap/etc/ssl/certs/` — which the `devbox` profile never reads | `curl: (77) Error reading ca cert file /etc/ssl/certs/ca-certificates.crt` on any HTTPS fetch |

  `--profile full` takes `/etc` from `bootstrap/` and so carries the CA bundle;
  the applet links are absent from the tree either way. Neither is a Firecracker
  issue — the same image on QEMU behaves identically.
- **The guest's DNS default does not match this host.** `smoltcp_net.rs` has
  `QEMU_DNS_SERVER = 10.0.2.3`, which is SLIRP's built-in forwarder. Under
  Firecracker nothing listens there: `20-net.sh`'s dnsmasq is the resolver and it
  is at `10.0.2.2` (and hands itself out as DHCP option 6). The boot log prints
  `[SmolNet] DNS socket initialized (server: 10.0.2.3)` and no later line
  supersedes it, so kernel-path DNS is aimed at an address with no server on it.
