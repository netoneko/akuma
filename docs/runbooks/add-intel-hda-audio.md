# Add Intel HDA audio to Akuma/amd64 — the brief for the on-box agent

**Stability: C.** Nothing here has been built yet. This is the task brief for
the agent (meow + GLM) that will write the driver **on the trashcan itself**,
from the checkout at `/src/github.com/netoneko/akuma`.

It is written to be handed to that agent whole. A human driving the same work
should read it the same way — it is the orientation, not the design.

## The prompt

> You are working inside Akuma/amd64 on the bare-metal HP box, in the checkout
> at `/src/github.com/netoneko/akuma` (branch `amd64-cleanup-and-improvements`).
> The machine you are running on has an **Intel 8 Series/C220 HD Audio
> controller at PCI `00:1b.0` (`8086:8c20`, class `04:03`)** and no sound
> support in the kernel at all.
>
> Your task: make `wavplay /path/to/file.wav` play audible 16-bit PCM through
> that controller, by writing an HDA driver for this kernel. The OSS surface it
> plays through (`/dev/dsp`, `SNDCTL_DSP_SPEED` / `SETFMT` / `CHANNELS`, and
> `write(2)` of PCM periods) **already exists** for virtio-sound on AArch64 —
> reuse it, do not invent a second one.
>
> Work in small steps **on this machine**: build with `kbuild -j 1`, install
> with `kinstall`, reboot with `/bin/busybox reboot -f`, and read the console
> tally and your own `[HDA]` lines on the way back up. The reboot is part of the
> job, not an interruption of it.
>
> **Commit before every install**, to the current branch, with a message that
> says what that kernel is meant to prove. If a kernel does not come back, a
> human picks the known-good entry at the GRUB menu and restarts you — so write
> as if you will resume with no memory: the git log, the doc you are keeping and
> the console are the only things that survive.
>
> **Commit locally; do not push.** This machine has no git credential and that
> is deliberate — your work stays on `why-are-we-here-just-to-suffer` here until
> a human collects it. Do not try to add a credential, do not change the remote,
> do not rewrite history, and never force anything. When you are done, write
> `docs/reference/subsystems/drivers/hda.md` and add a row to the
> `docs/README.md` symptom matrix — as commits, like everything else.
>
> Read "Before you start", "The seam", "The bring-up order" and "Rules you must
> not break" below before writing any code.

## Before you start

Read, in this order:

1. [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) — the machine and the
   build/install/reboot loop. Its § "Working **on** the box" is the part you
   live in; the laptop-driven sections describe how someone else reaches this
   machine and are not your path.
2. `crates/akuma-net-nic/src/rtl8169.rs` — the only other real PCI device driver
   on this target. It is the shape to copy: MMIO on a mapped BAR, descriptor
   rings in `.bss`, ownership written last behind a fence.
3. `crates/akuma-virtio/src/audio.rs` — the existing audio device, and the
   seven functions your driver has to stand behind.
4. `ls docs/archive | grep -i amd64` — this target's failures are mostly
   already written up, and reading is cheaper than re-deriving.

## The seam — what already exists

Do not add syscalls, device nodes or ioctls. Audio reaches userspace through a
path that is already built and already has a client:

| piece | where | what it does |
|---|---|---|
| the device-driver seam | `crates/akuma-virtio/src/audio.rs` | `init`, `is_available`, `set_format_oss`, `set_channels`, `set_rate`, `play(&[u8]) -> usize`, `stop` — **seven functions, and that is the whole interface** |
| `/dev/dsp`, `/dev/audio` | `crates/akuma-vfs/src/dev.rs` (`AUDIO_NODES`, gated on `DevProbe.audio`) | the nodes, majors 14:3 and 14:4, present only when a device was found |
| `open` | `crates/akuma-syscalls-glue/src/fs.rs` (~1835) | `"/dev/dsp" \|\| "/dev/audio"` when `is_available()` |
| `write` | `crates/akuma-syscalls-glue/src/fs.rs` (~1296) | hands the buffer straight to `audio::play` |
| the three ioctls | `crates/akuma-syscalls-glue/src/term.rs` (~158) | `SNDCTL_DSP_SPEED` / `SETFMT` / `CHANNELS` → `set_rate` / `set_format_oss` / `set_channels` |
| the client | `userspace/wavplay` | streams a WAV through one small buffer; 8192-byte periods, S16_LE passthrough, 24→16 downconvert |

**First job, before any hardware work: find out how much of that column
actually runs on amd64.** This target has its own `open`/`write`/`ioctl` in
`amd64/src/fd.rs` and does not go through every glue crate AArch64 does — that
asymmetry is this port's characteristic bug (`docs/archive/AKUMA_AMD64_SWITCH_FREED_CR3_UAF.md`
is the canonical example: a fix that was real, and absent here, because the
second arm was registered by a function amd64 never calls). Find the amd64
`/dev/dsp` open, write and ioctl arms, or add them, and say in the commit
message which it was.

The backend behind that seam is `akuma_virtio::audio` today. Make the amd64
kernel call an HDA implementation instead — a new `crates/akuma-hda` — rather
than teaching `akuma-virtio` about Intel hardware. Which of the two the kernel
gets should be a target/feature decision at one call site, not a `cfg` sprinkled
through the seam.

## The hardware

```
00:1b.0 Audio device [0403]: Intel 8 Series/C220 Chipset HD Audio [8086:8c20] (rev 05)
01:00.1 Audio device [0403]: NVIDIA GM107 HD Audio [10de:0fbc]      <- HDMI, ignore
```

Two of them, so **match on class `0x04`/subclass `0x03` AND vendor `0x8086`**,
or you will bring up the GPU's HDMI audio and hear nothing from the speakers.
`crates/akuma-pci` already parses the header, decodes BARs and walks
capabilities; `is_class(0x04, 0x03)` is the predicate to add next to
`is_ethernet()`.

HDA is one 16 KB MMIO BAR (BAR0), and the register layout is fixed by the
specification, so the controller half of this is well-trodden ground even though
this exact chip is not. The **codec** half is where machine-specific surprises
live: which widgets exist, which pin drives the speaker, what the BIOS left
configured. Print the graph rather than assuming it (step 4).

## Iterate on the metal — the reboot **is** the loop

Do not go looking for an emulator to develop against. This hardware exists on
exactly one machine, it is the machine you are running on, and the point of the
exercise is the loop:

```sh
kbuild -j 1 && kinstall && /bin/busybox reboot -f
```

Build, install, reboot into what you just built, look at the console, change one
thing, go again. That is the experiment
([`../archive/AKUMA_FROM_SCRATCH.md`](../archive/AKUMA_FROM_SCRATCH.md) §8) as
much as the driver is; a driver developed somewhere else would prove the smaller
half.

**You are allowed to brick a boot.** `GRUB_DEFAULT` is Akuma, so a kernel that
does not come up needs a human at the machine to pick `Akuma/amd64 (known good)`
from the GRUB menu — and there is one. The arrangement is: **you reboot, and if
the box does not come back, it gets rescued and you get restarted.** So:

- **Leave a trail.** Commit before every `kinstall`, and print what you expect
  the next boot to show. After a rescue you resume from the git log and the
  console, not from memory — assume you remember nothing.
- **Change one thing per boot.** A reboot costs a minute when it works and a
  human's attention when it does not; spending either on two changes at once
  means learning nothing from the failure.
- **Keep the fallback worth having.** Promote `/boot/akuma-amd64.good` only
  after a kernel has booted *and* passed its self-tests (`cp -f
  /boot/akuma-amd64 /boot/akuma-amd64.good && sync`). Promote at install time
  and the fallback becomes the kernel that was about to hang.

**You can change which kernel boots, but not which entry boots.** GRUB's config
is on filesystems this kernel cannot read (ext4, vfat), so nothing you run can
add, edit or select a menu entry. What you *can* do is an ordinary file write on
your own root — and the default entry loads a **path**, so:

```sh
# self-rescue, while you still have a shell: put a known kernel back under the
# path GRUB boots, and reboot into it
cp -f /boot/akuma-amd64.good /boot/akuma-amd64 && sync && /bin/busybox reboot -f
```

That works for everything except a kernel that cannot boot or cannot serve ssh —
then it is the human at the menu, and the entry they pick is
`Akuma/amd64 (known good)` → `/boot/akuma-amd64.good`. `kinstall` also leaves
`/boot/akuma-amd64.prev`, the kernel it overwrote, usable the same way.

So the fallback is worth exactly what you have kept in `.good`: promote it after
a kernel has booted and passed, never at install time.

## The bring-up order

Each step is a thing you can *see*, and none of them needs the step after it.

1. **Discovery.** Find the controller, map BAR0, print `[HDA] 8086:8c20 bar0=…
   version=1.0 oss=N iss=N bss=N` from `GCAP`. Nothing else. If the version
   register reads `0xFFFF` the BAR is not mapped and everything after this is
   noise.
2. **Reset.** `CRST` low, wait, high, wait for it to read back — then `STATESTS`
   tells you which codec addresses answered. Print them.
3. **Verbs.** CORB/RIRB rings (both are ring buffers in physical memory, the
   same discipline as the NIC's) — or the immediate-command registers if this
   controller has them, which is less code and is enough for bring-up. Prove it
   by reading the vendor/device id of codec 0 (verb `0xF0000`).
4. **Codec walk.** Root → function group → widgets; find an output converter
   and a pin complex with an output-capable, connected device. Print the graph
   once. This is the step where a wrong turn is silent, so print it.
5. **Stream.** One output stream descriptor: BDL (physically contiguous, in
   `.bss`, 128-byte aligned), cyclic buffer of a few periods, format register
   for 48 kHz / 16-bit / 2ch, set the stream tag, route the converter to it,
   unmute the pin, set `RUN`.
6. **`play()`.** Copy the caller's PCM into the next free period, advance, and
   return bytes accepted. **Poll the DMA link position (`LPIB` or the position
   buffer); do not take an interrupt** for the first version — polling is fewer
   moving parts and `wavplay`'s writes are already period-sized.
7. **Rate/format/channels.** Only now wire the three ioctls to the format
   register. `set_format_oss` needs to reject what the codec cannot do rather
   than play noise.

## Rules you must not break

- **The BDL, the CORB/RIRB rings and the PCM buffers are DMA memory.** They
  live in `.bss`, they are never a caller's `&mut [u8]`, and the device owns a
  buffer from the moment you publish it until the completion says otherwise.
  The contract is stated at the top of `crates/akuma-net-nic/src/nic.rs`; hold
  to the same one and keep every `unsafe` block behind it.
- **Allocate nothing on the audio path.** No `Vec`, no `String`, no `format!` —
  fixed arrays and `static` state (`CLAUDE.md` § "Kernel conventions"). Console
  output is `safe_print!`, never `format!`.
- **Never touch the receive poll loop, the scheduler, or the boot path.** A
  kernel that cannot serve ssh can only be replaced by a person standing at the
  machine: measured 2026-09-19, one extra MMIO read per idle lap livelocked the
  box and still passed `cargo check`, clippy and 1463 host tests.
- **`/boot/akuma-amd64.good` is promoted only after a kernel has booted and
  passed its self-tests** — never at install time. It is the only remote-free
  way back.
- **Put the pure logic in the crate and host-test it.** Verb encoding, the
  widget-graph walk, the format-register encoding and the BDL arithmetic are all
  functions of their inputs: they belong in `crates/akuma-hda` with tests that
  run on the laptop (`cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)`).
  Anything that touches MMIO stays behind them.
- **A helper you write must be run as `sh helper.sh`, not as `./helper.sh`.**
  This kernel's `execve` does not understand `#!`, so a script is only
  executable when a shell is the thing starting it. Nothing you hand to
  `execve` — or to a build script, or to cargo as a `runner` — can be a script.
- **Kernel changes need kernel tests.** Add a boot-suite check the way
  `src/process_tests.rs`'s audio test does it (`audio::is_available()` →
  `audio::play(&pcm)`), so a silent regression shows up in the tally.

## Verify

```sh
# 1. it built and installed
kbuild -j 1 && kinstall && /bin/busybox reboot -f

# 2. the console, after the reboot
#    [HDA] 8086:8c20 ... codec 0 ... output pin ... ready (/dev/dsp)
#    and the self-test tally: N passed, 0 failed

# 3. the node is there only because a device answered
ls -l /dev/dsp

# 4. the client. `wavplay` is a userspace workspace member and has never been
#    built for this target; build and install it like any other:
ubuild wavplay && cp -f /root/utarget/x86_64-unknown-none/release/wavplay /bin/

# 5. the whole path, end to end. There is no WAV on this box — make one
#    (44-byte header + PCM) or fetch one with `hget`; keep it small and
#    16-bit/48 kHz stereo so nothing in the path has to convert.
wavplay /tmp/test.wav                 # audible sound, and `echo $?` = 0
```

A run that prints `[HDA] ready` and plays silence is **not** a pass — say so and
keep the step open.

## Postmortem (2026-09-25): the step-1 print that never ran

M1 wired `hda::init()` into the PVH entry point only, inside the block
gated on the `pci` command-line flag. The dev box boots through GRUB —
the multiboot2 entry point — whose command line is `init=/bin/herd
root=/dev/sda1 netprobe`: no `pci` token, so that block (scan, report,
xHCI quiesce, HDA) never executed on this machine. Nothing was wrong
with the driver; its call site was on the wrong path.

Each assumption, next to the evidence that tested it, in arrival order:

- *Assumption: the kernel that booted is not the kernel we built.*
  Held — `uname` carried the merge commit and `/boot` md5 matched
  `ktarget`. Not the cause.
- *Assumption: the print ran but the log was lost.* Broken — a `herd`
  one-shot (`hda-capture`, `/etc/herd/enabled/`) now dumps the full
  ring to `/root/dmesg-boot-*.txt` within seconds of boot, and that
  dump carries the PCI census with no `[HDA]` line anywhere.
- *Assumption: the box has no PCI, so a missing print is a missing
  device.* Broken — the census lists 15 functions including
  `00:1b.0 8086:8c20 class 04/03/00` and the NVIDIA HDMI audio at
  `01:00.1 10de:0fbc`. Same scan is why the Realtek NIC at `03:00.0`
  works with no flag: firmware boots always have PCI.
- *Assumption: the census proves `hda::init()` ran.* Broken — on this
  path the census comes from `multiboot2.rs` scanning unconditionally
  at boot; `hda::init()` simply had no call site there.

Fix: `multiboot2.rs` calls `crate::hda::init()` after
`mem::init_reserving` and `boot::install_shared_sinks` — BAR mapping
allocates frames — unconditionally and best-effort, like its
neighbours. The PVH path keeps its `pci`-flagged call site unchanged:
a VMM boot has a flag to set, a firmware boot does not.

## Background

- [`amd64-bare-metal-loop.md`](amd64-bare-metal-loop.md) — the box, the loop,
  and the self-host install mechanism.
- [`../archive/AKUMA_FROM_SCRATCH.md`](../archive/AKUMA_FROM_SCRATCH.md) — why
  the box has its own checkout, toolchain and cargo cache, and what this task
  is a proof of.
- `crates/akuma-virtio/src/audio.rs`, `userspace/wavplay/src/main.rs` — the
  existing device and the existing client.

## Challenges (2026-09-25, M2-M5)

- **Unaligned MMIO reads lie.** `r16(0x03)` for VMAJ crosses the dword
  boundary; real decoders answer 0xff while aligned neighbours (GCAP at 0x00)
  decode honestly. Three boots printed version=255.0 from this. The fix is one
  aligned halfword at 0x02: VMIN low byte, VMAJ high.
- **A forced D3hot round-trip on a healthy D0 controller wedges it** — every
  BAR0 byte then reads 0xff, and CRST can never self-clear. PMCSR said D0 all
  along; the cycle was fixing a state the device was not in. Power-cycle only
  when PMCSR actually reports otherwise.
- **The console ring washes in ~3.5 minutes** under PSTATS/[probe]/[bkls>]
  spam, far faster than an agent wakes after a reboot. A herd one-shot
  (`hda-capture.conf` -> `/root/hda-capture.sh`) dumps the ring to
  `/root/dmesg-boot-*.txt` seconds after boot — every finding above is read
  from those files, not from a live ring.
- **The box's pipes corrupt tooling**: greps match and print nothing, results
  interleave, `git push`/`pack-objects` die on EBADF after ~26MB pipe writes,
  agent processes restart mid-session, and tool calls occasionally duplicate
  themselves (two kbuilds then race in /root/ktarget and the link dies on a
  missing .rcgu.o). Backups on disk before every edit; one build at a time.

- M6: CRST is software-driven (HDA 1.0a §4.3) — write 0, poll reads 0, write 1, poll reads 1. M2 polled for a self-clear the spec never promises; the controller sat in reset forever while STATESTS already answered (0x0001). Lesson: quote the spec section, then write the poll.

### CRST is software-driven, not a strobe (M6)
- Assumption (M2): "CRST self-clears" (copied mental model from self-clearing status bits).
- Evidence against: M5 boot showed GCTL stuck at 0x0001 after write+poll-for-clear, with GCAP/STATESTS perfectly healthy — hardware was fine, the *sequence* was wrong.
- HDA 1.0a §4.3 reality: write GCTL.CRST=0 → poll reads 0 (controller in reset) → write CRST=1 → poll reads 1 (out of reset) → wait 25us (codec link ramp). STATESTS is only valid after exit.
- Result: 9d1bcfee boots print `post-reset version=1.0` and STATESTS=0x0001 (codec #0 alive).

### 21:50 — the async-edit race, and what it taught
- **Assumption:** tool calls execute in issue-order and complete before the next begins.
- **Evidence:** lib.rs clobbered to `}` mid-edit while two duplicate-spawned kbuilds raced one /root/ktarget; r59 re-applied the same E0596 fix I had just applied. Root confirmed: "the tools are async... I'll make the edits linear".
- **Consequence:** one self-contained command per turn; `git checkout` before every edit chain; no shared temp files; cancel strays before writes; verify balance + commit + build only after a quiet `Running` list.

## Plan evaluation (2026-09-26, M1-M7 done, M7g probe pending metal)

Where the plan met reality (assumption -> evidence -> correction):

1. M1 boot path: plan assumed the `pci`-flagged block in main.rs runs. Evidence: boot banner `cmd: init=/bin/herd root=/dev/sda1 netprobe` (no pci flag); census prints anyway via the multiboot2/GRUB path. Correction: wire init where execution actually is (multiboot2, after mem::init_reserving + install_shared_sinks so map_bar has frames).
2. M2 CRST: plan said CRST self-clears. Evidence: poll timed out while GCTL held the written value. Correction per HDA 1.0a §4.3: CRST is software-driven, two phases (write 0 -> poll 0 -> write 1 -> poll 1 -> settle).
3. M3/M4 power: PMCSR read 0 (already D0); a forced D3->D0 cycle WEDGED the controller (all-ff reads) until cold reset. Lesson: never cycle power unconditionally.
4. M5 MMIO width: unaligned u16 at offset 0x03 read 0xff while aligned neighbors read real data (GCAP 0x4401). Correction: read the aligned halfword at 0x02 (VMIN low, VMAJ high). version=1.0, honest.
5. M7 offsets: CORB 0x40-0x4E right; RIRB is NOT 0x7x (reserved; writes vanish, reads 0) — real homes 0x50-0x5E. Evidence: reads all-zero despite writes; after moving, rintcnt/rirbctl/rirbsize hold values.
6. M7 enables: CORBCTL bit0 is the CORBRP reset strobe, RUN is bit1 (0x2) — with 0x1 the engine stays in reset (corbrp frozen). RIRBCTL likewise: DMAEN is bit1 (0x2), bit0 is IRQ enable. Evidence: corbwp==corbrp==2 only after 0x2.
7. M7 verbs: command field is bits [19:8]; 0xF00000 decoded to CAd 0xF (nonexistent). Vendor-ID = 0x000F_0000; node-count = 0x000F_0400 (parm in [7:0]). Immediate-command interface (ICW 0x60/IRR 0x64/IRS 0x68) answered 10ec0662 = Realtek ALC662 — the codec link is ALIVE and ICI is the oracle for every future verb.
8. Open problem: RIRB delivers nothing (rirbwp=0, rirbsts=0) with spec-correct programming and CORB consuming. M7g probe (commit 1558279e, this branch) discriminates on next boot: ring-memory dump (did responses land without WP moving?), 100ms late re-poll, and a CAd1 verb (does the controller auto-generate no-response entries?).

Gaps between doc and box:
- "Host tests" cannot run here (no host-std rust target) — that is how the VMAJ decode bug and verb-encoding bugs survived until metal.
- The verify section's `(/dev/dsp)` end state needs M8 (widget/pin enumeration, verbs 0xF00/0xF04/0xF0C/0xF1C on the AFG) and M9 (stream descriptors 0x80+, DMA position 0x70-0x74, BDLE ring, SDFMT 48k/16-bit/stereo, SDCTL RUN) — unstarted.
- Tooling: async edit races corrupted source twice mid-build (root is making edits linear). Until then: single-actor gated chains, verify-before-commit.

Next steps (in order):
1. kinstall + reboot the M7g kernel; read the ringdump verdict -> either fix RIRB writeback or declare ICI-only and move on.
2. M8 enumeration: AFG lookup via root-node subnode count, then widget walk; cross-check every verb through ICI in parallel with CORB/RIRB.
3. M9 stream: DMA position buffer (watch it — 0x70-0x74 is exactly the region the old wrong RIRB map pointed at), BDLE in its own aligned page, SDFMT, then SDCTL RUN.
4. /dev/dsp client, wavplay, tokyo_rider_enter_omegashima.wav acceptance.

## Status: what is done (root block 35782)

**Working on metal (verified in boot captures):**
- M1 PCI discovery: 8086:8c20 found (class 04/03), BAR0 mapped 0xf7210000, GCAP=0x4401 (4 out / 4 in), version 1.0 (aligned read at 0x02)
- M6 controller reset: HDA 1.0a §4.3 two-phase CRST (write 0 → poll 0 → write 1 → poll 1) — exits reset, STATESTS=0x0001 (codec 0 alive on link)
- M7 CORB: hardware fetches verbs from ring DMA (corbwp/corbrp advance in lockstep); RIRB delivery works (rirbwp advances, RINTFL sets) but response latency exceeds any busy-poll budget, so verbs ride the Immediate Command Interface
- M7 ICI path (codec_send_ici): IRS=1 trigger, BUSY-clear-only gate — codec0 answers vendor ID 0x10EC0662 (Realtek ALC662) every boot

**Emulator findings:** answers every verb instantly; vendor-ID implemented, node-count (0xF04) and widget reads return 0 — M8 raw scan maps full verb coverage. Single-byte reads at odd offsets return 0xff (unaligned MMIO trap); aligned reads honest.

**Pending:** M8 widget enumeration (raw scan in flight) → M9 /dev/dsp + wavplay (root's headphones test; fixtures at bootstrap/music/ + /bin/wavplay, never committed per root block 29409).
