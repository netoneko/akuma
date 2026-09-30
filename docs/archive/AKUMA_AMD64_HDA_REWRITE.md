# Intel HDA on amd64: the rewrite that made the trashcan audible — 2026-09-30

**Outcome: it plays.** `wavplay /root/test3s.wav` and the 3-minute
`tokyo_rider_enter_omegashima.wav` are audible through the front headphone jack
of the HP 500-502nj ("the trashcan"), cleanly, at 44.1 and 48 kHz. Getting there
took discarding the previous driver: it had been "complete" (runbook M9, M10,
M11) for three days while producing no sound, and every one of its readbacks
was evidence about the wrong thing.

Current design and the register facts:
[`../reference/subsystems/drivers/hda.md`](../reference/subsystems/drivers/hda.md).
This document is the narrative: what was wrong, in what order it was found, and
which checks were worth their cost.

Code: `crates/akuma-hda/src/{verb,codec,stream}.rs` (pure, host-tested,
`forbid(unsafe_code)`), `amd64/src/hda.rs` (registers, DMA memory, `/dev/dsp`
backend), `crates/akuma-virtio/src/audio.rs` (`hda_backend::Ops`), and one hook
in `akuma-exec` (`ExecRuntime::dsp_close`). Branch on litter:
`cats/claude/hda-rewrite` (`9f8d821e`, `0b800634`, `96656504`), built on the
swarm's `baa5d974`.

## 1. Where it started

An agent swarm (meow + GLM, running *on* the trashcan, one kernel reboot per
experiment) had written an Intel HDA driver over five days and run out of
tokens. Its runbook said M9 complete — "wavplay passes end-to-end on metal" —
and its last milestones (M10, M11) were chasing why the headphones were silent
anyway: SET_PIN_VREF, D0 power states, amp levels from a Linux `alsa-info`
dump. The boot log showed a stream that ran to completion, a converter bound to
its stream tag, an unmuted DAC. The listener heard nothing.

Also in the way: the swarm's own agent process (`kot`) was running on the box
and had to be turned off. It is a herd service (`/etc/herd/enabled/kot.conf`);
disabling it is removing that file (the copy in `available/` stays) plus
`herd stop kot`.

## 2. What was wrong

Ordered by how long it kept the box silent.

| # | fault | how it hid |
|---|---|---|
| 1 | **PCI no-snoop was enabled by firmware** (`DEVC` config 0x78 bit 11 = 1, boot log `devc 0x0800`) and never cleared. Controller DMA did not snoop the CPU caches, so it read the ring from stale RAM. | The stream ran, `BCIS` fired, the codec was bound and unmuted, every register read back correctly — and the DAC was fed zeros. Explained by the swarm as "`LPIB` unimplemented on this silicon". It was the DMA not getting data. |
| 2 | Verb words hand-written as hex with the **NID one nibble too high**: `0x0220_2011` "set converter 2 to 48 kHz" is *verb 0 to widget 0x22*. | No codec has a widget 0x22, so sets were ignored and GETs of the same phantom read 0, which the log presented as "amp unmuted / bind OK". Every "verified readback" in runbook M9–M11 is suspect for this reason. |
| 3 | `SDnFMT` written at +0x14 (reserved; real +0x12) and `SDnLVI` at +0x10 (`FIFOS`, read-only; real +0x0C). | The format register kept its reset value; the ring happened to work because one BDL entry needs LVI 0. |
| 4 | `SET_PIN_VREF` was `0x701` — which is *connection select*. The "VREF" level 0xC3 selected a connection index that does not exist. | Read back as 0x02 after the fact (the write was clamped), reported as routing evidence. |
| 5 | 24-bit audio converted **twice**: `wavplay` already sends 16-bit, and the kernel then treated it as 24-bit. | Not silence but noise, on the rare path where the DMA did deliver. |
| 6 | `/dev/dsp` `SPEED`/`SETFMT`/`CHANNELS` were accepted and discarded; the glue's new ioctl arms were nested inside the `FIONBIO` arm and could never run. | `wavplay` "negotiated" and played at whatever the beep bring-up left. |
| 7 | GCAP decoded wrongly (`BSS` is bits 7:3, `NSDO` 2:1 — decoded as 7:4 and 3:1). And **the crate's own host tests had never compiled** (fakes missing `w32`, `reset` taking `Fn` where the test needed `FnMut`), which is how 1–3 and 7 survived: the swarm's box had no host toolchain and treated "host tests cannot run here" as a fact about the tests. | — |
| 8 | After the audio worked: a **killed player left the ring looping**. The stream is cyclic and free-running; only `close` stopped it, and process exit does not call `close`. | "If you stop wavplay it jitters and keeps going" — the last ~370 ms replaying. |

## 3. The order it was found in

1. **Read the code, not the log.** The first move was reading `hda.rs`
   against the specification. The verb words and the register offsets are wrong
   on inspection; no experiment is needed to see that `0x02202011` addresses
   NID 0x22.
2. **Rewrite, with the pure half host-tested.** Verb encoders are functions of
   their fields with known-answer tests (one of them is the old bad literal,
   asserted to decode to NID 0x22). The codec walk runs against a fake ALC662
   and asserts that **no verb is ever addressed to a widget the graph does not
   have**. 31 tests.
3. **Emulator as the oracle.** Homebrew's QEMU has `intel-hda` and an
   `-audiodev wav` backend that records what the guest plays. The kernel boots
   under `-M q35 -kernel … -append "pci hdatest skiptests"`; the driver plays a
   generated triangle wave; the recorded WAV was compared to the generated
   samples: **44,100 frames, 0 mismatches, starting at sample 0.** That
   validates the format word, BDL, ring, stream binding and CORB/RIRB in one
   run, with no ears and no reboot of the metal. (It also caught the driver
   stopping at the last real sample and truncating the tail — the position
   register runs ahead of the converter — fixed with 100 ms of trailing
   silence on close.)
4. **First boot on metal:** the graph read from the real ALC662, both output
   pins routed (1b→0c→02, 14→0c→02), readbacks (now of real widgets) showed the
   DAC bound to stream 1 at 0x4011, pin control 0xC0. **And silent.**
5. **State dump instead of guessing.** A dump of every DAC, mixer and output
   pin — power, stream/format, pin control, EAPD, jack presence, connection
   select, every amp — showed the analog side was right: mixer input unmuted,
   pin 0x1b `sense=0x80000000` (headphones detected), DAC amp 0x37. The dump
   *removed* the whole class of "Realtek-specific settings" hypotheses (GPIO,
   coefficients) in one boot. What it could not show was the stream: `LPIB`
   stayed 0 and `STS` stayed 0 after `RUN`.
6. **The DMA is the suspect.** `LPIB` frozen next to a healthy stream is a DMA
   problem until proven otherwise. Linux's Intel init does two things this
   driver did not: clear `TCSEL` (0x44) and clear `DEVC.NSNPEN` (0x78 bit 11).
   Added both plus `clflush` around every span the device reads or writes, the
   DMA position buffer as a second position source, and a print of the
   before/after config values. The log said `devc 0x0800->0x0000`, `LPIB` began
   advancing (5968 → 32240 over five 30 ms samples, agreeing with the position
   buffer), and the user heard it.
7. **The kill loop** (fault 8). See §4.

## 4. The wrong turns

* **First theory for the silence was the codec.** Route, amps, EAPD, VREF,
  power, GPIO. The dump refuted all of it, but it took a metal boot to do so.
  `LPIB` being stuck at 0 was in the *previous* attempt's notes for days and
  was dismissed each time.
* **Zeroing consumed ring spans does not stop a loop.** The first kill fix
  zeroed each span as the hardware consumed it, so a stalled writer would loop
  silence. Tested by playing a tone and never calling `stop`: the recording
  was 40 s of replayed tone. Zeroing runs in `observe()`, and `observe()` runs
  in the write loop — with no writer nothing observes. A free-running cyclic
  DMA ring cannot be silenced by anything that needs the writer to be alive;
  the only fix is to stop it when the descriptor dies. That is a hook in
  `akuma_exec::process::fd::release_fd_entry` (`FileDescriptor::DevDsp` →
  `runtime().dsp_close`, wired to `akuma_virtio::audio::stop` in both kernel
  tables and a no-op in `test_support`). The zeroing stayed: it makes a stall
  between writes play silence instead of stale audio.
* **Two self-inflicted incidents**, recorded because both are the kind of
  thing that repeats. (a) To see `herd`'s usage I ran `/bin/herd` on the box;
  with no arguments it *starts a second supervisor* and it launched
  `hda-capture` and a second `kot` before I killed them. On Akuma there is no
  orphan reaper, so the killed one stayed a zombie until reboot. Run
  `herd --help`-style discovery from the source, not the binary. (b) I
  switched the user's checkout to the litter branch to read it, changing their
  working branch (and leaving a submodule showing modified). Reading another
  branch is `git show <ref>:<path>` or a worktree.
* **A false alarm worth a line:** the first QEMU run "lost" 38 ms at the end of
  the tone. It was the driver, not the recorder (§3, item 3).

## 5. Facts to carry forward

* **Read the `[HDA] pci tcsel … devc …` boot line first** if a machine plays
  silence with a healthy-looking stream. The same firmware default is likely on
  other machines.
* **A "verified readback" is evidence only if the verb that produced it is
  known to address a real widget.** The pure crate now makes that checkable.
* The controller MMIO wants **aligned, exact-width** accesses; a halfword read
  straddling a dword returns `0xff`. CRST is software-driven (§4.3). Never
  cycle PCI D3→D0 on a healthy controller.
* The trashcan's ALC662: 34 widgets; DACs 02/03/04/06; mixers 0b/0c/0d/0e/22/23;
  line-out pin 14, headphone pin 1b, both fed through mixer 0c → DAC 02; jack
  sense works; `pcm caps 0x000e0560`; DAC amp 0x57 steps (driver sets 64 % of
  the 0 dB step); 2 GPIOs, all zero, and nothing needed them.
* `hda-capture` (herd one-shot) and `/root/hda-capture.sh` still exist on the
  box; they are harmless and useful for reading a boot log before the ring
  washes (~3.5 min).

## 6. Not done

* **The pop / first-sample click** and any fade at start and stop.
* **Volume policy.** `DAC_PCT = 64` is the level Linux left the ALC662 at; there
  is no mixer ioctl.
* **The rear line-out (pin 14) was never listened to**; only the front headphone
  jack was. Both are enabled and routed.
* **Interrupt-driven playback.** The driver polls; `write` yields while the ring
  is full. Fine for a player, not for latency work.
* **Input (capture) and the HDMI codec** (`10de:0fbc`) are untouched.
* `/boot/akuma-amd64.good` was **not** promoted to the new kernel.
* The runbook's M9–M11 sections are left in place under a correction banner
  rather than deleted, so the record of what the swarm believed survives.
* The `userspace/meow` submodule pointer on the litter branch was bumped by the
  swarm ("last fix from meow"); it came along with the merge and was not
  reviewed here.

## 7. Verify

```sh
# host tests, the pure half
cargo test -p akuma-hda --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)

# QEMU with a recorded output: the tone in hda.wav must equal the generated one
cargo build -p akuma-amd64 --target x86_64-unknown-none --release
qemu-system-x86_64 -M q35 -cpu max -m 1024 -kernel target/x86_64-unknown-none/release/akuma-amd64 \
  -append "pci hdatest skiptests" -audiodev wav,id=a0,path=hda.wav \
  -device intel-hda -device hda-output,audiodev=a0 -serial file:hda.log -display none -no-reboot
# hda.log: "[HDA] ready (/dev/dsp)"; hda.wav: 44100 frames, triangle wave, 0 mismatches

# metal (trashcan): boot log, then listen
ssh akuma 'dmesg | grep -a "\[HDA\]"'      # pci devc 0x0800->0x0000, graph, routes, state
ssh akuma 'wavplay /root/test3s.wav'
# Ctrl-C a long file: the sound must stop, not loop
```

## Background

* Why the swarm experiment itself failed (tooling, models, unenforced testing
  practice), with a dated timeline:
  [`AKUMA_AMD64_BARE_METAL_SELFHOST.md`](AKUMA_AMD64_BARE_METAL_SELFHOST.md) §7.
* The swarm's record: [`../runbooks/add-intel-hda-audio.md`](../runbooks/add-intel-hda-audio.md)
  (correction banner at the top).
* Current state of the driver:
  [`../reference/subsystems/drivers/hda.md`](../reference/subsystems/drivers/hda.md).
* The box and its loop: [`../runbooks/amd64-bare-metal-loop.md`](../runbooks/amd64-bare-metal-loop.md).
* Same shape of failure — a fix that was real, absent on the second arm:
  [`AKUMA_AMD64_SWITCH_FREED_CR3_UAF.md`](AKUMA_AMD64_SWITCH_FREED_CR3_UAF.md).
