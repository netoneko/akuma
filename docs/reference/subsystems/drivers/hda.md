# Intel HD Audio (amd64)

Source: `crates/akuma-hda` (pure, host-tested, `forbid(unsafe_code)`),
`amd64/src/hda.rs` (registers, DMA memory, the `/dev/dsp` backend),
`crates/akuma-virtio/src/audio.rs` (`hda_backend`: the one dispatch point).
Bring-up history and the failed first attempt: the runbook
[`../../../runbooks/add-intel-hda-audio.md`](../../../runbooks/add-intel-hda-audio.md).

> **Stability: C.** Rewritten 2026-09-30 against the specification. Verified
> sample-for-sample under QEMU's emulated controller; the real ALC662 on the
> trashcan is the open question until someone has *heard* it.

## Shape

```
wavplay ─ write(2) ─► glue fs.rs ─► audio::play ─► hda_backend::Ops.write
                                                     │
                       hda.rs: Hda::write ───────────┘
                         to_s16_stereo()   akuma-hda::stream   (U8/S16/S24/S32, mono/stereo -> S16 stereo)
                         64 KiB ring (8 × 8 KiB) ── BDL ── SD0 DMA ──► DAC ──► pin
```

One seam, six functions (`is_available`, `play`, `stop`, `set_rate`,
`set_format_oss`, `set_channels`); `audio.rs` dispatches to the registered
backend if there is one and to virtio-snd otherwise. There is no `cfg` inside
either driver.

## What the crate decides (and tests)

* `verb` — every verb word is a function of its fields. **The NID is bits
  27:20.** The first bring-up hand-wrote hex literals with it one nibble high
  (`0x0220_2011` is "verb 0 to widget 0x22"), so it configured a widget that
  does not exist and read back that widget's zeros as success.
* `codec` — `discover` reads the widget graph through a `VerbBus`;
  `find_path` walks connection lists from an output pin to a DAC;
  `path_verbs` powers every node on the route, selects connections, and
  **unmutes every amplifier on it** (a muted mixer input is silence with every
  readable register looking healthy); `stream_verbs` binds a DAC to a tag/format.
* `stream` — the format word, PCM conversion, BDL entries, and `PlayRing`
  (write/consume accounting from `LPIB`).

## Register facts that cost the first attempt weeks

| register | offset | note |
|---|---|---|
| `GCAP` | 0x00 | `OSS[15:12] ISS[11:8] BSS[7:3] NSDO[2:1] 64OK[0]` |
| CORB / RIRB | 0x40–0x5E | RIRB is **not** at 0x70; `CORBCTL` RUN is bit 1 |
| `SDnCTL` | +0x00 (3 bytes) | SRST bit 0, RUN bit 1, **tag bits 23:20** |
| `SDnSTS` | +0x03 | write-1-to-clear; do not write it as part of a 32-bit CTL write |
| `SDnLPIB` | +0x04 | |
| `SDnCBL` | +0x08 | |
| `SDnLVI` | +0x0C | the old code used +0x10 (`FIFOS`, read-only) |
| `SDnFMT` | +0x12 | the old code used +0x14 (reserved) |
| `SDnBDPL/U` | +0x18 / +0x1C | |
| output SD 0 | `0x80 + 0x20 × GCAP.ISS` | 0x100 on the 8-series (ISS = 4) |

* **Aligned, exact-width MMIO only.** A halfword read straddling a dword returns
  `0xff` on real decoders; that produced `version=255.0` for three boots.
* **CRST is software-driven** (write 0, poll 0, write 1, poll 1) — it does not
  self-clear.
* **Never cycle PCI D3→D0 on a healthy controller**; it wedges (all-`0xff`).
* Pin VREF is `[2:0]` of `SET_PIN_WIDGET_CONTROL`; verb `0x701` is *connection
  select*.

## Behaviour

* `init(selftest)` runs at boot (multiboot2 path unconditionally, PVH path with
  the `pci` flag). Best-effort: any failure prints one line and boots without
  sound. Every wait is bounded.
* Transport: CORB/RIRB; falls back to the immediate command registers if the
  rings do not answer. Unsolicited RIRB entries are skipped.
* Every connected output pin (headphone, line-out, speaker) is routed to a DAC
  and enabled; DACs share stream tag 1. The DAC amp is set to 64 % of its 0 dB
  step (`DAC_PCT`) — the level Linux left this machine at when it was audible.
* `write` blocks (yielding) while the ring is full. Underrun (the hardware caught
  up with the writer) or a missed lap stops and restarts the stream cleanly.
  If `LPIB` does not advance within 60 ms of `RUN`, playback is paced by the
  TSC instead (logged once).
* `close` (`stop`) appends 100 ms of silence, waits for the ring to drain and
  stops the stream — the position register runs ahead of the converter, and
  stopping at the last real sample truncates the tail.
* Parameters (`SNDCTL_DSP_SPEED/SETFMT/CHANNELS`) are stored and take effect on
  the next stream setup; the first bring-up accepted them and ignored them.

## Verify

```sh
# host tests (pure half)
cargo test -p akuma-hda --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)

# QEMU, objective: the WAV backend records what the driver played
qemu-system-x86_64 -M q35 -cpu max -m 1024 -kernel target/x86_64-unknown-none/release/akuma-amd64 \
  -append "pci hdatest skiptests" -audiodev wav,id=a0,path=hda.wav \
  -device intel-hda -device hda-output,audiodev=a0 -serial file:hda.log -display none -no-reboot
# hda.log: "[HDA] ready (/dev/dsp)", then the recorded tone matches the generated
# triangle wave sample-for-sample (44 100 frames, 0 mismatches).

# metal: boot with `hdatest` on the kernel command line, or `wavplay file.wav`
```

## Background

* [`../../../archive/AKUMA_AMD64_STREAMLINING.md`](../../../archive/AKUMA_AMD64_STREAMLINING.md)
* Intel High Definition Audio Specification 1.0a §3.3 (registers), §4.3–4.5
  (reset, CORB, RIRB), §7 (verbs).
