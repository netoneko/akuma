# akuma-net-rtl8169

A driver for the Realtek RTL8169/8168/8111 gigabit MAC, written as **logic over
two traits** rather than as a sequence of pokes at a pointer.

The crate owns what to write, in what order, and what the answers mean. It owns
no memory-mapped pointer, allocates nothing, and cannot touch a device: the
register window arrives as `Regs` and the descriptor/buffer memory as `Rings`,
both implemented by the consumer. `#![forbid(unsafe_code)]` follows from that —
every hardware access is a trait call, so there is nothing here to make unsafe.

## Why the chip

`10ec:8168` rev 0c, **RTL8168g/8111g, XID 0x4c0** — the onboard NIC of the amd64
bring-up host. Bare metal on that machine needs this part driven, and virtio is
no help outside a VMM.

## Provenance

Written fresh against the live chip. Every offset, width and reset value in
`regs.rs` was **read back off real silicon** before it was written down, through
a read-only mapping of BAR2 (`/sys/bus/pci/devices/0000:03:00.0/resource2`) taken
while Linux's own driver had the device up and passing traffic — no unbind, no
disruption to the host.

`tests/golden_registers.rs` keeps that 256-byte dump verbatim and asserts the map
against it: the station address is where the map says, the revision field decodes
to an RTL8168g, `PHYSTATUS` decodes to the "1Gbps/Full" `dmesg` reported, the
receive filter is a non-promiscuous host's, and both ring bases are 256-byte
aligned. It has already earned its keep — see *What the hardware corrected*.

No driver source was copied. Chip models this crate has not run on decode to
`Model::Unknown(xid)` rather than being named, because naming a part nobody has
booted is a claim the crate cannot support.

## Testing without the hardware

`model::FakeChip` is a simulated RTL8168g that behaves like the chip in the four
places that matter, and **panics when the driver breaks the ownership contract**:

- the reset bit clears itself, but only after several polls, so a driver that
  assumes reset is instant fails here;
- `PHYAR` implements the asymmetric busy protocol — busy *set* means a read
  finished, busy *clear* means a write finished — so polling the wrong edge
  hangs the test instead of silently reading a plausible zero on hardware;
- the transmit doorbell walks the ring exactly as the chip does and puts bytes
  on a `Wire` the test can inspect;
- writing a descriptor or buffer the chip owns is a panic with a line number,
  where on real silicon it is an intermittent DMA race.

It also records every register write, which is how the ordering constraints get
tested: "C+ mode before the ring bases" and "rings populated before the receiver
is enabled" are facts about *sequence* that no single register value can express.

```bash
HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo test -p akuma-net-rtl8169 --target $HOST     # 64 tests
cargo build -p akuma-net-rtl8169 --target x86_64-unknown-none --release
```

## What the hardware corrected

The receive FIFO threshold. The crate first used the all-ones "no threshold"
encoding, which is what the field is commonly documented to mean. The live chip,
at a gigabit and passing traffic, runs `0b110`. The golden fixture caught the
disagreement, and the measured value won: `RCR_RXFTH_DEFAULT`.

That is the argument for keeping a dump rather than a summary of one.

## What is deliberately absent

- **No interrupt handler.** `take_interrupts` reads and acknowledges `ISR`
  correctly (write-1-to-clear, only bits actually observed); routing the line and
  deciding when to call it belong to the consumer.
- **No allocation, no buffer ownership.** The rings live in memory the consumer
  allocated, mapped, and can name *physically*. Both bases must be 256-byte
  aligned — the chip ignores the low bits rather than faulting, so a misaligned
  ring silently points somewhere else, and `probe` refuses one.
- **No PHY firmware.** Realtek ships per-part patch blobs; the chip negotiates
  and passes traffic without them.
- **No multicast filter.** The hash table is programmed all-ones.
- **No VLAN stripping or receive checksum offload**, though the chip offers both:
  each changes what the buffer contains, and a driver that does not tell its
  caller so is handing up frames that are not what arrived.

## Next: the same code against the real chip

The host has an IOMMU (VT-d on, the NIC alone in group 10), so `vfio-pci` can
hand this device to a **userspace** process — no KVM, no kernel, no reboot. A
harness implementing `Regs` over an mmap'd BAR and `Rings` over IOMMU-mapped
buffers runs *this crate's* `init`/`transmit`/`receive` against the real silicon
with a debugger attached. The seam is already the right shape for it.

One caution: that NIC is the host's only network interface and its only way in.
Bind it to `vfio-pci` only with a second NIC present, a timed rebind armed, or a
hand on the physical keyboard.
