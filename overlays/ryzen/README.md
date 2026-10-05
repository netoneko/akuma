# overlays/ryzen — Akuma/amd64 on the Lenovo laptop, by reboot loop

ryzen (IdeaPad 5 2-in-1 16AHP9, Ryzen 7 8845HS) boots Pop!_OS through
**systemd-boot**. Akuma is a one-shot guest on it: arm it, reboot, and the next
reset — Akuma's own `reboot -f`, the power button, anything — lands back in Pop.
Why this shape, and the wifi plan it exists for:
[`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](../../docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md).

| file | runs | does |
|---|---|---|
| `send.sh` | laptop | ships unpushed work (`amd64 crates userspace/herd overlays/ryzen`) to ryzen as `$W/local.tar`, plus the two scripts below |
| `build.sh` | ryzen, netoneko | fresh clone of `$BRANCH`, `local.tar` over it, both kernels (`no-tests` and plain), `mkdisk.sh` root, then `remove.list` and `rootfs/` layered onto it |
| `qemu.sh [entry] [timeout] [std\|bochs]` | ryzen, root | rehearsal: OVMF → this `grub.cfg` → the same kernel + root off a FAT ESP image, serial to `$W/qemu/serial-<entry>.log`. `-no-reboot`, so an autoreboot entry *exits*. Env: `KERNEL=`, `MEM=`, `ACCEL=tcg` |
| `bisect.sh <entry> <commit>…` | ryzen, root | older kernels against today's root image in the rehearsal, one verdict line each |
| `install.sh` | ryzen, root | kernels + root to `/boot/efi/EFI/akuma/`, `grub.cfg` embedded into a standalone `grubx64.efi`, systemd-boot entry `akuma.conf`. **Arms nothing** |
| `grub.cfg` | — | the menu (5 s). Entry 0 is unattended |
| `remove.list` | — | paths deleted from the image: the framebuffer `console` service (not wanted here) |
| `rootfs/` | — | layered onto the image: the `autoreboot` herd service, `available/` only |

`$W` is `/home/netoneko/akuma-metal` on ryzen.

## The loop

```sh
sh overlays/ryzen/send.sh                                   # laptop, if anything is unpushed
ssh ryzen 'runuser -u netoneko -- sh /home/netoneko/akuma-metal/build.sh'
ssh ryzen 'sh /home/netoneko/akuma-metal/akuma/overlays/ryzen/qemu.sh 0 200 std'   # MUST pass first
ssh ryzen 'sh /home/netoneko/akuma-metal/install.sh'
ssh ryzen 'bootctl set-oneshot akuma.conf && systemctl reboot'
```

**Rehearse before every metal boot.** A rehearsal costs two minutes. A metal
boot that wedges costs someone walking to the laptop and holding the power button.
The first rehearsal found three bugs that would each have wedged the metal (below).

## Menu

| # | entry | returns to Pop by itself |
|---|---|---|
| 0 | unattended: herd runs `sshd` + `autoreboot` (90 s), `nosmp fbverbose` | yes, if it reaches userspace |
| 1 | `nosmp fbverbose`, herd's enabled services | no |
| 2 | the self-test kernel, `nosmp` | no |
| 3 | headless: as 0, plus `nofb` (framebuffer never touched) | yes |
| 4 | SMP | no |
| 5 | reboot | — |

`autoreboot` is opt-in through `initargs=daemon,--service,…`: herd's `--service`
mode loads exactly the files it names, and **`daemon` must come first** — herd
takes argv[1] as a command and prints its usage for `--service`.

## What the first boots found (2026-10-06)

1. **Black screen.** `MAPPED_LIMIT` was a literal 4 GiB, left over from before
   `boot.s` mapped 64 GiB. ryzen's GOP framebuffer is the Radeon's BAR at
   `0x4b0000000` (18.75 GiB), so `Framebuffer::new` refused it. The kernel then
   halted on an EGA text line, and a UEFI machine has no EGA. Now
   `MAPPED_LIMIT = PHYSMAP_LIMIT`. A missing or unusable framebuffer is also no
   longer fatal: the kernel boots headless and the reason goes to `dmesg` (`nofb`
   forces this).
2. **The PMM handed out the kernel image.** Under UEFI the image
   (`0x200000..~0xb1_0000`) spans two firmware regions, and `mem::usable_of` only
   raised the floor of the region containing `kernel_end`. So `0x100000 + 7 MiB`
   went to the PMM whole, and herd's first `fork` overwrote live kernel memory.
   With a framebuffer that was a silent triple fault; headless, it was a `#PF`
   inside `talc_alloc`. It reproduced on every kernel back to `87f1a7c5`, under
   KVM and under TCG, at 2/3/6 GiB. It is a property of the UEFI memory map, not
   of this CPU. Reserved spans are now carved out of every region they overlap.
3. The `--service` / `daemon` ordering above.

## Background

- [`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](../../docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md): the assessment, the wifi plan, the staged state
- [`docs/runbooks/amd64-bare-metal-loop.md`](../../docs/runbooks/amd64-bare-metal-loop.md): the trashcan's loop, which this one inverts (there Akuma is the default)
- [`scripts/benchmarks/ryzen_fc/`](../../scripts/benchmarks/ryzen_fc/README.md): the Firecracker rig on the same machine
