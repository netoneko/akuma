# akuma-rtw89

The bring-up of ryzen's wifi card, the Realtek **RTL8852CE** (`10ec:c852`,
Linux `rtw89_8852ce`), from powered off to running firmware: wifi stage W1
(`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` § 5). `no_std`,
`forbid(unsafe_code)`, no allocation, no dependencies. The MMIO and DMA half is
`amd64/src/rtw89.rs`; the boot token is `rtw89`.

| module | decides |
|---|---|
| `regs` | register offsets and bits, named as Linux's `reg.h`/`pci.h` name them |
| `fw` | the multi-firmware container (`rtw89_mfw_hdr`), image choice by chip cut, the image header and its sections, the packet plan |
| `h2c` | CH12's ring entries, the 16-byte packet descriptor, the H2C header, RX ring entries |
| `bringup` | `power_on`/`power_off`, `pre_init` (DMAC download mode, flow control, PCI pre-init), `disable_cpu`/`enable_cpu`, `download`, `wait_fw_ready`, `bring_up`, `shutdown` |

Everything goes through the `Bus` trait (8/16/32-bit register reads and writes,
a delay) and `fw::Source` (read the firmware file at an offset). DMA memory
arrives as byte slices with their bus addresses.

## Tests

```bash
HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo test -p akuma-rtw89 --target $HOST
AKUMA_RTW89_FW=/lib/firmware/rtw89/rtw8852c_fw-1.bin cargo test -p akuma-rtw89 --target $HOST --test real_firmware
```

- **`tests/golden_trace.rs`** — the whole bring-up replayed against Linux's
  register trace of this very card (`tests/golden/w0_up.txt`, from stage W0's
  mmiotrace). Every write must match the trace in register, width and value,
  and every access the trace has must be made. Only ring base addresses (DMA
  addresses) are not compared. Break one constant or drop one step and it fails
  at the trace line.
- **`tests/sim_chip.rs`** — a simulated chip for what no trace shows, the DMA
  payload: the header packet then every section byte in order, slot reuse
  under a slow chip, a ring index the chip was never given, a rejected image,
  a file that fails partway, `shutdown`.
- **`tests/real_firmware.rs`** — the parser over the real file when
  `AKUMA_RTW89_FW` points at it: cut 1's normal image is 0.27.122.0 and goes
  over in exactly the 166 packets Linux sent. Without the variable it passes
  having checked nothing, and says so.

## On the metal

`overlays/ryzen/cycle.py 8` (menu entry 8) boots it once on ryzen and prints
the `[rtw]` lines. A good boot ends with
`[rtw] fw ready v0.27.122 (cut 1, 166 packets, FW_CTRL 0xe2, ~50000 us)` and
`[rtw] card shut down`.

The one thing the crate cannot do for its caller: **Bus Master Enable on the
root port above the card.** Firmware leaves it off on ryzen, and without it
the download stalls at "header accepted" with `HAXI_IDCT = TXMDA_STUCK`.
`amd64/src/pci.rs`'s `enable_bridges_above` does it.

## Licence of the source it follows

The sequence follows Linux's `drivers/net/wireless/realtek/rtw89/` (v6.17),
which is `GPL-2.0 OR BSD-3-Clause`; this crate takes it under BSD-3-Clause, and
the notice is in `src/lib.rs`. The firmware is Realtek's, redistributable under
`LICENCE.rtlwifi_firmware.txt`, and is not in this repository.
