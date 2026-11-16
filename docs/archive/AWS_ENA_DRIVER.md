# AWS ENA driver: why write our own

Direct boot on an EC2 Nitro instance needs an Elastic Network Adapter (ENA)
driver. The existing AWS deployment runs Akuma inside Firecracker on a Linux
metal host and uses virtio-net; it does not exercise an EC2 ENA device. This
document records the decision to implement a small native Rust ENA driver for
Akuma. It is a plan, not a claim that the driver or direct EC2 boot works.

## Decision

Write the driver in Rust against ENA's device interface, starting with the
minimum needed for one interface to exchange Ethernet frames. We looked for an
existing Rust ENA driver and did not find a usable one. AWS maintains a
[FreeBSD driver](https://github.com/amzn/amzn-drivers/tree/77facd6719172a5f39ea5c810b126a8b40b7e4fe/kernel/fbsd/ena),
and [NetBSD has one too](https://github.com/NetBSD/src/blob/trunk/sys/dev/pci/if_ena.c).
Both are C drivers integrated with their respective kernels, not drop-in Rust
crates. Their BSD-licensed code is a useful reference and could be ported, but
bringing their OS-facing machinery across would be more work than the first
Akuma use case requires.

AWS's [ENA interface definitions](https://github.com/amzn/amzn-drivers/tree/77facd6719172a5f39ea5c810b126a8b40b7e4fe/kernel/fbsd/ena/ena-com/ena_defs)
describe registers, administrative commands and packet descriptors. The BSD
drivers show the operational sequence and edge cases. Check the license and
copyright of each file used; if code is adapted, retain its notices, license
conditions and disclaimer in source and binary distributions. Record the exact
upstream revision for any adaptation.

## What we can reuse

The virtio path already connects a NIC to smoltcp and manages DMA-backed frame
ownership in `crates/akuma-net-nic`. Akuma also has DMA and MMIO primitives,
PCI discovery and BAR mapping on amd64. The RTL8169 implementation is another
useful pattern: a host-tested driver core separated from MMIO, DMA and the
`ExternalDevice` integration. Reuse these interfaces and lessons where they
fit; ENA is a different device and cannot use virtio's queue protocol or assume
its DMA and ordering requirements are identical.

## First milestone

Target a single EC2 instance and a single queue pair, with polling where the
device permits it. Discover the PCI function, reset and identify the device,
set up the admin and asynchronous event queues, negotiate the required
features, create transmit and receive queues, and pass Ethernet frames to the
existing stack. Check DMA addressability, alignment, memory ordering, queue
phase bits, timeouts and cleanup on every failure path against the reference
implementation. Keep offloads, multiple queues and performance tuning out of
the initial milestone unless the hardware requires them.

Exercise descriptor layouts and queue state with host tests, then verify device
initialization, DHCP, bidirectional traffic and sustained operation on an actual
EC2 instance. Real hardware is the acceptance test: a build or a mock cannot
establish that the DMA and device protocol are correct. Direct EC2 boot, image
construction and security hardening have separate prerequisites; a working NIC
alone does not complete the AMI.

A couple of days is a reasonable initial goal for bring-up, not a verified
schedule. The first hardware run will reveal how much integration remains.
