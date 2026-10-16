# overlays/ryzen — Akuma/amd64 on the Lenovo laptop, by reboot loop

ryzen is an IdeaPad 5 2-in-1 16AHP9 (Ryzen 7 8845HS, 14 GiB RAM, SK hynix NVMe,
RTL8852CE wifi, no Ethernet, no serial port). It runs Pop!_OS, booted by
**systemd-boot**. Akuma is a **one-shot guest** on it: arm it from Pop and
reboot. Akuma boots once, runs with its root on the laptop's own SSD, writes
its kernel log there, and resets itself. Any reset, whether Akuma's own, the
watchdog's or the power button, lands back in Pop, where the log is read.

Why this shape, and the wifi work it exists for:
[`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](../../docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md).

## Status (2026-10-06)

| | |
|---|---|
| one-shot boot, back to Pop by itself | **works**, unattended (boot 2: 184 s outside Linux) |
| root on NVMe p3 (ex-Windows, 64 GiB ext2), logs in `/var/log/ryzen` | **works**, `e2fsck` clean, even after a hard power-off |
| framebuffer (1920×1200 at `0x4b0000000`) | works |
| hardware watchdog (`wdt`, AMD FCH) | **works**: a deliberately wedged kernel (`arm.sh 6`) was reset by the chipset in 60 s, `FIRED=1` seen from Pop |
| RTL8852CE firmware on p3 (`/lib/firmware/rtw89/`) | staged (`fetch-firmware.sh`); nothing loads it yet |
| wifi W1: firmware download (`rtw89`, entry 8) | **works**: `[rtw] fw ready v0.27.122`, 166 packets in 50 ms, card shut down again; the fix that got it there was Bus Master on the card's root port (survey doc § 5.2) |
| wifi W2: receive (`rtw89rx`, entry 9) | **works**: Linux's recorded start replayed (16 815 ops, 52 ms), then 12 s on channel 1: 1604 frames, 17 networks' beacons, the home network's among them (survey doc § 5.3) |
| wifi W3/W4: the station (`rtw89wifi`, entry 10) | **works**: joins the home network with WPA2-PSK, handshake in the kernel, keys installed (boot 16, survey doc § 5.4) |
| wifi W5: IP over wifi (entries 11, 12) | **works**: DHCP, SNTP, DNS, `ssh -p 2222 root@<address>` from the Mac, outbound HTTP to the LAN and the internet (survey doc § 5.6). Entry 12 is the one to use: quiet boot, framebuffer console, stays up |
| self-host environment on p3 (`stage-dev.sh`) | **staged** 2026-10-06: nightly musl toolchain, a clone at `/src/github.com/netoneko/akuma`, `kbuild` rig, git/make, goose + Kimi (`goose-kimi`), rio. Survey doc § 5.7. Not yet booted |
| touchpad (2026-10-16) | **works on entry 12**: `dmesg | grep touchpad` ends in `up: /dev/input/mouse0`; the pointer works in herdr / any terminal program on the box's own console (`printf '\033[?1000h\033[?1006h'; cat -v` to see the reports), natural scrolling is the default (`echo natural=0 > /dev/input/mouse0` to flip). Reference: `docs/reference/subsystems/touchpad.md` |
| agents on the box (2026-10-16) | **goose with both providers**: `goose-kimi` (Kimi Code, `kimi-for-coding`) and `goose-glm` (z.ai coding plan, `glm-5.3`; tokens in `/root/.akuma/{kimi,z.ai}/token`) each answered a one-shot `run -t` prompt over wifi. **herdr integration for Claude installed** (`herdr integration install claude`; goose is not a herdr target); Claude Code is preset to the dark theme, verbose tool calls and thinking summaries (`~/.claude/settings.json`, `~/.claude.json`) so first run skips the theme dialog. **herdr scrollback verified** over an ssh PTY by `scripts/herdr_scrollback.py` (needs `pyte`): `seq 1 300` in a pane, 30 SGR wheel-up reports move the view from 258-300 to 168-211 (3 lines per tick), wheel-down returns to the tail. **Claude Code 2.1.301 runs** (native `linux-x64-musl` Bun build, 262 MB, fetched on the box by a detached resumable `curl -C -` and sha256-checked against `downloads.claude.ai/claude-code-releases/<ver>/manifest.json`; ~1 h at the wifi's 60-120 KB/s): `claude --version`, `claude-glm -p ...` and the interactive UI (via z.ai's Anthropic endpoint, GLM `glm-5.3`) all work. Trap: closing the ssh pty does NOT kill the foreground child — leaked `claude` processes keep `/root/.claude.json.lock` and the next start's trust dialog hangs on it; `kill -9` them and `rmdir` the lock |
| wifi RX replay protection | **built**, host-tested (`akuma_ieee80211::ccmp`); `replayed` counter in the `[rtw] link:` line; not yet on metal |
| wifi W0: Linux's bring-up traced | **done**: probe + interface-up through `fw ready` (`w0-trace.sh`, results in the survey doc § 5.1; traces in `~/.akuma/w0/` on the laptop) |
| wifi control (`/dev/wifi0`, `/etc/wifi`, `wifi`) | **works** against the simulated radio (entry 7, rehearsed); no real radio yet — [`docs/reference/subsystems/wifi.md`](../../docs/reference/subsystems/wifi.md) |
| network | **wifi** (entry 11): `python3 overlays/ryzen/wifi-ssh.py --key <key>` finds the station by its MAC and runs a command over ssh (port **2222**); the user's key is in p3's `/etc/sshd/authorized_keys`. Every other entry has no network; the log is the channel |
| wifi credentials | `~/.akuma/wifi/<network>` on the laptop holds the passphrase; never copied into the repo, docs or logs |

## Quick start

All of it runs on ryzen; `$W` is `/home/netoneko/akuma-metal`.

```sh
sh overlays/ryzen/send.sh                                    # on the laptop, only if work is unpushed
ssh ryzen 'runuser -u netoneko -- sh /home/netoneko/akuma-metal/build.sh'
ssh ryzen 'cd /home/netoneko/akuma-metal/akuma && DISK=nvme sh overlays/ryzen/qemu.sh 0 240 std'   # rehearse — must pass
ssh ryzen 'sh /home/netoneko/akuma-metal/install.sh'
ssh ryzen 'sh /home/netoneko/akuma-metal/akuma/overlays/ryzen/arm.sh'     # boots Akuma once; ~3 min later Pop is back
ssh ryzen 'mount -o ro /dev/nvme0n1p3 /mnt && ls /mnt/var/log/ryzen; umount /mnt'
```

**Rehearse before every metal boot.** A rehearsal costs two minutes on ryzen
itself. A metal boot that wedges costs someone walking to the laptop. Every bug
found so far on this machine was found by the rehearsal first, or could have been.

## Files

| file | runs on / as | does |
|---|---|---|
| `send.sh` | laptop | ships unpushed work (`amd64 crates userspace/herd overlays/ryzen`) as `$W/local.tar`, plus `build.sh`/`install.sh` |
| `build.sh` | ryzen, netoneko | fresh clone of `$BRANCH` (`ryzen-wifi`), `local.tar` over it, both kernels (`no-tests` and plain), `mkdisk.sh` root image, then `remove.list` and `rootfs/` applied to it. Log `$W/build.log`, output `$W/out/` |
| `qemu.sh [entry] [timeout] [std\|bochs]` | ryzen, root | the rehearsal (below) |
| `bisect.sh <entry> <commit>…` | ryzen, root | older kernels against today's root image in the rehearsal, one verdict line each |
| `install.sh` | ryzen, root | kernels + root image to `/boot/efi/EFI/akuma/`, `grub.cfg` embedded into a standalone `grubx64.efi`, a `grubenv`, and the systemd-boot entry `akuma.conf`. **Arms nothing** |
| `arm.sh [entry]` | ryzen, root | boot Akuma **once**: systemd-boot's one-shot picks Akuma, and with an index GRUB's one-shot (`next_entry` in `grubenv`) picks the menu entry. Both are consumed by that boot |
| `format-p3.sh --yes-destroy-p3 [size]` | ryzen, root | **destructive**: the root image onto `nvme0n1p3`, grown with `resize2fs`. Refuses unless start/length/PARTUUID match the measured partition and it is unmounted. Done once, 2026-10-06, at 64 GiB |
| `fetch-firmware.sh` | ryzen, root | `rtw8852c_fw*.bin` from Alpine's `linux-firmware-rtw89` (no dependencies; files are `.zst`, decompressed here) onto p3, with Realtek's licence beside them |
| `wdt-probe.py` | ryzen, root, from Pop | **read-only** dump of the FCH watchdog and PM registers (`/dev/mem`): decoded? disabled? running? fired? |
| `wifi-ssh.py --key KEY [CMD]` | laptop | finds Akuma on the LAN by the station MAC (ping sweep + ARP) and runs CMD over ssh on port 2222; for entry 11 |
| `cycle.py <entry> [--grep RE] [--log dmesg] [--transcript NAME]` | laptop | one loop cycle through `hpbox.py`: ship this tree's `no-tests` kernel and `grub.cfg`, rehearse the entry in QEMU (stops if it fails), install, arm, wait for Pop, print matching `boot-N.early` (or `boot-N.dmesg`) lines from p3 (default `[rtw]`), and the service's `NAME-N.txt` |
| `rootfs/etc/ryzen/netwatch.sh` | Akuma, herd `netwatch` (entries 12-14) | DNS lookup every 10 s; after 120 s without an answer (or 300 s of boot without ever getting one) — but **1200 s (900 s) while the radio is still `scanning`/`associating`**, so a router restart does not cost the session (2026-10-08) — saves `dmesg` to `/var/log/ryzen/netwatch-N.dmesg`, a line to `netwatch.log` and does `reboot -f`, which lands in **Pop** (the one-shot is consumed): re-arm from there. Copied to p3 by hand 2026-10-07 (p3 does not take `rootfs/`). First run: fired correctly at 300 s on a boot that never joined (below) |
| `rootfs/etc/herd/available/powersave.conf` | Akuma, herd `powersave` (entry 20) | runs `/bin/powersaved` (`userspace/powersaved` — a binary, not a script): the idle-dim staircase through `/dev/power`. Stage the conf **and the binary** onto p3 by hand, as with `wifijoin` |
| `stage-dev.sh` | ryzen, root, from Pop | the self-host environment onto p3 (toolchain, apk packages, the `scripts/box` rig, goose + `goose-kimi`, rio), mounting p3 itself; run it **detached** (`setsid nohup`) — the wifi ssh drops foreground jobs. Does not unmount p3: `umount /mnt/p3` before arming Akuma. `FORCE=1` recopies the toolchain |
| `acpi-dump.sh`, `gfx-dump.sh` | ryzen, root, from Pop | what Linux knows, onto p3: `/root/acpi/` (raw + decompiled tables, `BAT0` readings) and `/root/gfx/` (GPU, panel timings, CPU vector features, Vulkan); mount p3 themselves, serials dropped. Findings: survey doc § 5.8 |
| `ec-sample.sh` | ryzen, root, from Pop | reads the EC's memory mirror (`/dev/mem` at `0xFEEC2300`, 512 B, 3 samples) beside `BAT0`'s uevent into p3 `/root/acpi/linux/ec-eram-sample.txt`; the verified field map is `EC-FIELDS.md` beside it |
| `touchpad-probe.sh` | ryzen, root, from Pop | **read-only** touchpad bring-up constants (M0 of the touchpad work, [`docs/archive/AKUMA_AMD64_TOUCHPAD_I2C_HID.md`](../../docs/archive/AKUMA_AMD64_TOUCHPAD_I2C_HID.md)): the `AMDI0010` controller's MMIO window, the touchpad's i2c bus + address + ACPI HID, the DSDT's `FMCN`/`SSCN` timing packages, and the raw HID report descriptor (for pinning as an `akuma-hid` test fixture) |
| `w0-trace.sh [--check]` | ryzen, root | wifi **W0**: mmiotrace of rtw89 unbind → bind → up → one scan, to `/var/tmp/akuma-w0/<stamp>/`. Detaches into unit `akuma-w0` (drops Pop's wifi ~1–2 min, takes all CPUs but one offline while tracing); NetworkManager is kept off the card so no association or keys enter the trace; every exit path restores the network. `--check` changes nothing |
| `w2-merge.py <run> [--phase P] [--collapse] [--no-fwdl] [--ts]` | laptop | a W2 run's register accesses, H2Cs and C2Hs in one ordered stream (each H2C anchored to its CH12 doorbell) |
| `w2-seqgen.py <merged> <out.seq> [--from-fw-ready] [--until-stop] [--mac M]` | laptop | compiles a merged recording into the op stream `akuma_rtw89::script` replays (`crates/akuma-rtw89/seq/up.seq`) |
| `CHAN=1 sh w0-trace.sh` / `w2-chans.py <run> <seqdir>` | ryzen root / laptop | records Linux switching channels 1–13 (monitor mode, `iw set channel`; `CHANS="13 1 6"` picks and orders them; drops Pop's wifi ~1 min) and compiles one `chanNN.seq` per channel |
| `w0-summary.py <trace> [--dump PHASE]` | laptop | per-phase read/write counts and busiest BAR offsets of a W0 trace; `--dump bind` prints the ordered sequence |
| `grub.cfg` | — | the menu (5 s), below |
| `remove.list` | — | paths deleted from the image: the framebuffer `console` service (not wanted here) |
| `rootfs/` | — | applied to the image: the `autoreboot`, `wifitest`, `wifisoak` and `wifijoin` herd services (`etc/herd/available/`) and their scripts (`etc/ryzen/`). **Not** applied to p3 by anything: p3 was formatted from an older image once, so a service a menu entry names on p3 is copied there by hand from Pop (`wifijoin` and `/bin/wifi` were, 2026-10-06) |

## Menu

| # | entry | comes back to Pop by itself |
|---|---|---|
| 0 | **unattended**, NVMe root p3: `sshd` + `autoreboot`, `wdt nosmp fbverbose` | yes |
| 1 | NVMe root p3, `wdt nosmp fbverbose`, herd's enabled services | no; `wdt` resets it only if the kernel wedges |
| 2 | unattended, RAM root (no disk) | yes, but its logs are lost |
| 3 | the self-test kernel, RAM root, `nosmp` | no |
| 4 | headless: as 0, plus `nofb` (framebuffer never touched) | yes |
| 5 | NVMe root p3, SMP | no |
| 6 | **watchdog self-test** (`wdttest`): arm, then wedge with interrupts off | yes, through the watchdog's reset, about 60 s in |
| 7 | **wifi tool test** (`wifisim` + the `wifitest` service): the `wifi` tool against the simulated radio, transcript to `/var/log/ryzen/wifitest-N.txt`. Rehearsed 2026-10-12 twice: first run's fsck found `deleted inode has zero dtime` — the kernel had no wall clock without a network, every file was dated 1970 — then clean with the CMOS RTC seed (`clock: seeded from the CMOS RTC` in `boot-N.dmesg`, files dated that day) | yes |
| 8 | **wifi W1** (`rtw89`): as 0, plus the RTL8852CE brought up to running firmware and shut down again before `init`; `[rtw]` lines in `boot-N.early` | yes |
| 9 | **wifi W2** (`rtw89rx`): as 8, then Linux's recorded start replayed and 12 s of receiving on channel 1; a summary per network (OUI, channel, SSID hash, security) in `boot-N.early` | yes |
| 10 | **wifi W3/W4** (`rtw89wifi` + the `wifijoin` service): the card kept up as `/dev/wifi0`'s radio, then `wifi connect` joins the best known network in `/etc/wifi` on p3; `[rtw]` lines in `boot-N.dmesg`, the tool's exit status and `/dev/wifi0`'s non-identifying keys in `wifijoin-N.txt` | yes |
| 11 | **wifi W5** (`rtw89wifi` + `wifistay` + `sshd`): joins as entry 10 does, then **stays up 10 minutes** for ssh over wifi; `wifistay-N.txt` gets `/dev/wifi0`'s keys and the DHCP address every 30 s, `boot-N.dmesg` is saved as it goes. Reach it with `python3 overlays/ryzen/wifi-ssh.py --key <key>` (finds the station MAC `02:41:4b:55:4d:41`, ssh on port **2222**) — verified 2026-10-06, boot 20 | yes, after 10 min |
| 12 | **Akuma on wifi, for use**: quiet boot (splash, no `fbverbose`), the framebuffer console (`console.conf`), `sshd`, `wifi auto` (`wifiauto.conf`) keeping the best known network joined, and `klog` (`klog.conf`: `dmesg` to `/var/log/ryzen/klog-N.dmesg` every 5 s, so a hang leaves its log); **no autoreboot** — it stays up until rebooted. Not rehearsable (`cycle.py` wants a guest that resets itself): ship the kernel, `install.sh`, `arm.sh 12` | no |
| 13 | **Akuma on wifi, SMP DIAGNOSTIC**: tests kernel + `fbverbose` + `smp=N`. Bisected 2026-10-07: **2/4/8 stable, 16 wedges** (BKL held forever; SMT width is the trigger — details in `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` §5.9). Edit the `smp=N` in the menuentry (then `install.sh` — it embeds this file) for other widths. **`smp=8` is the usable rio configuration today** | no |
| 14 | **Akuma on wifi, 8 cores, for use — the default** (2026-10-07): entry 12's command line with `smp=8` instead of `nosmp`, for rio's software rasteriser. Moved from `smp=16` to `smp=8` because 2/4/8 were stable and 16 wedged in the bisect (entry 13's row). **Network test at `smp=8`, 2026-10-07: stable but slow.** Up 40+ min on wifi with curl downloads from the Mac (64 MB files): `failed 0`, `retry-limit 0`, `dropped 0`, link stayed `connected`. Throughput is ~110-125 KB/s in total whether one stream or four, so it is a link ceiling, not a parallelism stall (on channel 1, `signal=0`; cause untested). Still seen: `[bkls>]` long holds and the `max gap 1844…` `now_us` underflow. Not A/B-ed against entry 12 (`nosmp`). **Fallback: entry 12 (`nosmp`)** | no |
| 15 | **wifi tool soak against the lossy simulated radio** (`wifisim=lossy` + the `wifisoak` service): `wifi auto` for 5 minutes against a radio whose scans miss networks, whose joins take seconds or time out, and whose link drops and comes back; `wifi status` sampled every 10 s into `/var/log/ryzen/wifisoak-N.txt`, `wifi auto`'s own output in `wifiauto-N.txt`. The fault sequence is seeded, so two transcripts are comparable. **Rehearsed 2026-10-12** (QEMU on ryzen, `DISK=nvme`, kernel built on the Mac): 30 samples, `wifi auto` alive at every one, three link drops (samples 1–2, 14–16, 26–27) each back to `connected` by itself, and the tool issued **one** join in five minutes — it waited through the kernel's rejoins rather than restarting them. Not run on the metal | yes |
| 19 | **powersave verify**: entry 0's unattended shape + `idlesleep` — the AML EC scan live, no `ecram=` override | yes |
| 20 | **Akuma on wifi + powersaved + idlesleep**: entry 14's usable shape (`smp=8`, console, sshd, `wifi auto`, klog) plus `idlesleep` and the `powersave` herd service (`/bin/powersaved`): the screen dims down the idle staircase and a keystroke brings it back; `dmesg` has `[IDLESTAT]`, the daemon's decisions are in `/var/log/herd/powersaved.log`. Needs `/bin/powersaved` and the conf hand-staged on p3 (the image does not take `rootfs/`). **First metal run 2026-10-15 (boot 120, 3 h 19 m)**: the staircase stepped 100→60→30→10 on schedule with the facts honest; `[IDLESTAT] halts=115907355 shots=195` over the session; the kernel log flowed with zero gaps until the power press. Two things to know: **the radio goes dormant with no traffic** — warm it with ~5 s of pings before `wifi-ssh.py` or ssh's kex dies mid-flight; and **each dead-link ssh attempt leaks a session server-side** (no timeout on half-open sessions), so ~24 refused attempts later sshd answers its banner but refuses everyone (`Refusing connection: 24 sessions already live`) — recover by rebooting, fix later with a session idle-timeout | no |
| 16 | reboot | — |

**Network test, 2026-10-13/14 (entry 14, `smp=8`).** Stage3 tarball (233 MiB)
with `aria2c -x8`: 111 KiB/s whole-file before, 117 KiB/s over a 240 s sample
after the timer fix — unchanged, at the link ceiling (ryzen Linux sees ~120 KB/s
there). What changed is that `hget`/`box pull` stopped hanging: a core's LAPIC
timer could be left one-shot and never fire again
([`AKUMA_AMD64_LAPIC_ONESHOT_STALE.md`](../../docs/archive/AKUMA_AMD64_LAPIC_ONESHOT_STALE.md),
which has the full table). Not yet measured: ping loss under load after the
fix, the wifi-side fixes from the parallel branch, the trashcan's rtl8169 fixes.
Run `aria2c` from the Alpine bundle with `--async-dns=false`.

**2026-10-09: `netwatch` is disabled on p3** (its conf moved to
`/root/netwatch.conf.disabled`) so a session survives a lossy link long enough
to collect data; move it back to `/etc/herd/enabled/` to re-arm it. **`klogd`
now also appends every new ring line to `/var/log/ryzen/klog-N.all`** (the
whole boot's log, gap-marked if the 64 KiB ring wrapped between two 2 s
passes, capped at 256 MiB); `klog-N.dmesg` is still the last ring snapshot.

**Building the kernel inside Akuma on ryzen (2026-10-09, boot 89).** p3's
clone (`/src/github.com/netoneko/akuma`) has its own uncommitted work, so build
in a worktree: from the Mac, `git bundle create k.bundle <box-HEAD>..kami`,
pipe it in over ssh, then on the box `git fetch /root/k.bundle
kami:refs/remotes/litter/kami`, `git worktree add --detach /src/kami-wt
litter/kami`, and **copy `crates/akuma-fbcon/vendor/spleen/*` in from the main
checkout** (a submodule; a worktree gets it empty, and `akuma-fbcon`'s build
script panics). `AKUMA_SRC=/src/kami-wt kbuild -j 4 --features no-tests`: 29 s
and 27 s incremental against `/root/ktarget` at `smp=8`. Akuma here touches
only p3, so it cannot install onto the ESP: copy the ELF to p3, `reboot -f`
(lands in Pop), install from Pop, `arm.sh 14`. `uname` then shows a stale
revision (`amd64/build.rs` does not rerun when the worktree's HEAD moves);
check a symbol with `nm` instead.

Entries 12-14 run a plain `herd daemon` (`initargs=daemon`): the services are whatever p3's `/etc/herd/enabled/` holds — `sshd console klog netwatch wifiauto` since 2026-10-08, enabled with `herd enable <svc>` (copied from `available/`). Enumerating them as `--service` flags made `herd status` list one and hid the rest.

`sh arm.sh 6` boots entry 6 once. `autoreboot` is opt-in through
`initargs=daemon,--service,…`, which loads exactly the named herd services.
**`daemon` must come first**: herd takes argv[1] as a command.

## Choices that are on purpose (2026-10-16)

Settings that look arbitrary and are not. Each one cost an afternoon once, so
they are written here; change one deliberately, not by accident. The files that
carry them are in `rootfs/` (applied by `build.sh`) unless noted.

| choice | value | why |
|---|---|---|
| **HOME on the console** | `HOME=/root`, as an `env =` line in `etc/herd/enabled/console.conf` | herd's default console environment has `HOME=/`, and `/etc/akuma-dev.env` only does `HOME=${HOME:-/root}` (it keeps what is set). So the on-screen shell and everything started from it — herdr, claude, goose — kept its dotfiles in `/` while ssh sessions used `/root`: two places for every config, and a claude that "forgot" its settings and sessions. One place now. (`amd64/mkdisk.sh` writes the same line for every image.) |
| **Console font** | `font = 20` in `etc/console.conf` (10x20 cell) | `font =` is the **pixel cell height** of a baked IBM Plex Mono cut (20, 22, 24, 32, 40, 48), not points. On this 16" 1920x1200 panel (~141 ppi) a 10x20 cell is the physical size of Ghostty's 13 pt on a 14" retina Mac (~254 ppi, cell ≈ 3.4 mm tall); 22 ≈ 15 pt-equivalent, a 26 would be 18 pt (not baked). Auto-choice without the file is 11x22 (screens ≥ 1000 px tall). Read at boot. |
| **herdr: no left panel** | `ui.sidebar_start_collapsed = true`, `ui.sidebar_collapsed_mode = "hidden"` in `root/.config/herdr/config.toml` | The workspace list ate a quarter of the screen for a box that mostly has one or two workspaces. `prefix+b` brings it back. Collapse applies at the next launch. |
| **herdr: date and power in the tab row** | `ui.tab_bar_right` = a `datetime` entry and a `command` entry running `usr/local/bin/herdr-status` every 10 s | The console has no other clock or battery readout. The script prints the last line herdr shows: `39% bat 16.1W 1h18` from `/proc/power`. It prints nothing (entry hidden) if the file is missing. |
| **herdr: new groups start with two panes** | `prefix+alt+n` (workspace) / `prefix+alt+c` (tab) run `usr/local/bin/herdr-new-group` | herdr has no config key for a default layout. The script creates the workspace/tab then splits its root pane right. The default `prefix+shift+n` / `prefix+c` still make single panes — bound beside them, not over them. |
| **goose-glm defaults** | `glm-5.3-flash`, `GOOSE_THINKING_EFFORT=low` (`stage-dev.sh`) | Fast and cheap by default; `GOOSE_MODEL=glm-5.3 goose-glm …` for the big one. |
| **goose-kimi states its provider** | provider, host and model in the wrapper (`stage-dev.sh`), not left to the shared `config.yaml` | The two wrappers share one `secrets.yaml` (each rewrites it per run, so don't run both at once) and one `sessions.db` — `goose-kimi session -r` resumes a session goose-glm started. |
| **claude settings** | `/root/.claude/settings.json` (model, effort, `viewMode: verbose`, `showThinkingSummaries`) | Only read when `HOME=/root`; before the HOME fix these lived in `/.claude` and the pane's claude never saw them. |
| **No bash on the box** | busybox ash is `/bin/sh`; there is no `bash` | Claude Code's Bash tool refuses any shell that is not bash/zsh ("No suitable shell found"), so inside claude on this box the Bash tool fails and it can only read files. Open. |
| **The console service is kept** | `remove.list` is empty | It was removed here on 2026-10-06, then wanted back (entries 12 and 20 are console entries). The service file now comes from `rootfs/etc/herd/enabled/console.conf`. |
| **Tools staged by hand** | `rg` 15.2.0 and `gh` 2.102.0 in `/usr/local/bin` (static x86_64 builds, sha256-checked against the release files) | Pushed over ssh as base64 (`gh` is 42 MB: ~7 min over wifi). Neither is in `stage-dev.sh` yet. `gh` has no login on the box. |

Two kernel bugs found while setting these up, both fixed in the tree and
documented where they live: an epoll nested inside another epoll reported ready
forever (`docs/reference/subsystems/syscalls/poll.md` § Nested epoll — what made
the interactive `claude` spin), and the hardware watchdog was fed only when
`tick % 100 == 0`, which `idlesleep`'s tick credits could step over, resetting an
idle box (`docs/reference/subsystems/power.md` § Why an idle box could reset
itself).

## Getting back to Pop

| what ends the Akuma boot | how |
|---|---|
| `autoreboot` (entries 0, 2, 4) | `etc/ryzen/autoreboot.sh`: saves `dmesg` to `boot-N.early` at +10 s and `boot-N.dmesg` just before the reset, then `busybox reboot -f` |
| `reboot -f` (kernel `reboot.rs`) | syncs filesystems, flushes the USB disk if any, NVMe Flush + shutdown + bus-mastering off, **stops the watchdog**, then resets via `0xCF9` (falling back to the i8042 pulse, then a triple fault) |
| a wedged kernel (with `wdt`) | the AMD FCH watchdog resets the machine when the BSP has not taken a timer interrupt for 60 s |
| anything else | the power button |

## The NVMe root

`root=/dev/nvme0n1p3` makes the kernel drive the SSD itself (`amd64/src/nvme.rs`
over `crates/akuma-nvme`). It takes the controller from UEFI (9 ms on this
drive), reads the GPT (both CRCs checked), and from then on touches **only
p3's LBA range**. Every offset is checked against that window before a command
is built. If p3 holds no ext2, the mount refuses before writing anything and the
kernel falls back to the RAM image. A timed-out command disables the controller
rather than risk a late DMA.

**When a boot leaves no log** (the NVMe root did not come up), `autoreboot`'s
delay is the message. Read the time spent outside Linux from
`journalctl --list-boots` (the previous boot's end to this boot's start). That
is about 35–50 s of firmware and boot, plus:

| delay | furthest stage in `dmesg` |
|---|---|
| 140 s | `ext2 mounted on /dev/nvme0n1p3`: the logs are on p3 |
| 100 s | `nvme: p3 = LBA …`: GPT read, mount refused (p3 not ext2) |
| 60 s | `nvme: ns1 …`: Identify worked, GPT did not |
| 20 s | none of those: no controller, or the takeover failed |

## The watchdog

`wdt` (or `wdt=<seconds>`, 10–65535) arms the AMD FCH watchdog, the block Linux
drives with `sp5100_tco`, from `amd64/src/watchdog.rs`. It is gated on the
chipset (`1022:790b` revision ≥ `0x51`) and set to reset on expiry. It is armed
before the root mount, so a hung NVMe or USB bring-up is covered. It is petted
once a second from the BSP's timer interrupt and stopped right before an
orderly reset, so it can never fire during the firmware's POST or Pop's boot.
`wdttest` refuses to wedge unless the watchdog actually armed.

**Verified on the metal, 2026-10-06.**
- A normal boot with `wdt` (entry 0) logged `armed, 60 s, reset on expiry;
  CONTROL=0x11`. It ran the full 140 s `autoreboot` delay, more than twice the
  timeout, and returned exactly as without the watchdog (184 s outside Linux).
  The probe in Pop then showed it stopped.
- The self-test (entry 6) wedged with interrupts off. Pop was back 97 s later
  (~35 s firmware/boot + 60 s), with `CONTROL.FIRED = 1` and `COUNT = 0`: the
  chipset's own record that it reset the machine. `e2fsck` on p3 was clean.

From Pop, `python3 wdt-probe.py` shows the registers (`FIRED` is noted when set). Firmware leaves the
watchdog decoded-off and stopped but not locked; Linux's own driver accepted it
(`heartbeat=60 sec`, MMIO `0xFEB00000`).

## The rehearsal (`qemu.sh`)

OVMF → this `grub.cfg` → the same kernel and root, entry chosen through
`grubenv` exactly as `arm.sh` does it, serial to `$W/qemu/serial-<entry>.log`.
`-no-reboot`, so an autoreboot entry *exits* QEMU. With **`DISK=nvme`**, QEMU's
NVMe device gets a sparse 477 GiB image with **ryzen's exact GPT** (same LBAs,
same p3 window), the ESP on p6 and the root on p3. Afterwards p3 is
`e2fsck -fn`'d and `/var/log/ryzen` shown, from Linux.

| env | |
|---|---|
| `P3=blank` | p3 unformatted: the fallback path; reports whether p3's first 256 MiB are still zeros |
| `P3_SIZE=32G` | smaller ext2 for speed (default: the whole 279 GiB) |
| `KEEP=1` | boot the same disk again (persistence) |
| `KERNEL=`, `MEM=`, `ACCEL=tcg` | another kernel; RAM size; no KVM |

What it cannot rehearse: the FCH watchdog (q35 has Intel's TCO), the
framebuffer's 64-bit address (QEMU's display BARs land below 4 GiB) and the
real controller's timing.

## What the first boots found (2026-10-06)

1. **Black screen.** `MAPPED_LIMIT` was a literal 4 GiB, left over from before
   `boot.s` mapped 64 GiB. ryzen's GOP framebuffer is the Radeon's BAR at
   `0x4b0000000` (18.75 GiB), so it was refused. The kernel then halted on an
   EGA text line, and a UEFI machine has no EGA. Now
   `MAPPED_LIMIT = PHYSMAP_LIMIT`, and a missing or unusable framebuffer means
   a headless boot with the reason in `dmesg` (`nofb` forces it).
2. **The PMM handed out the kernel image.** Under UEFI the image spans two
   firmware memory regions, and `mem::usable_of` only raised the floor of the
   region containing `kernel_end`. So `0x100000 + 7 MiB` went to the PMM, and
   herd's first `fork` overwrote live kernel memory: a silent triple fault, or
   headless, a `#PF` in `talc_alloc`. It reproduced on every kernel back to
   `87f1a7c5`, under KVM and TCG. It is a property of the UEFI memory map, not
   this CPU. Reserved spans (kernel, modules, the multiboot2 info block) are
   now carved out of every region they overlap; spans strictly inside a region
   become PMM holes and the heap is placed around them.
3. **herd's `--service` needs `daemon` first.**
4. **Tooling:** an Alpine `.apk` is three concatenated gzip'd tars (GNU `tar`
   needs `--ignore-zeros`); `git.kernel.org` refuses scripted fetches from ryzen;
   a post-run check under `set -e` once leaked a mount and two loop devices on
   deleted 477 GiB images.

## Background

- [`docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](../../docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md): the assessment, the wifi plan (§ 5), the NVMe results (§ 9)
- [`docs/runbooks/amd64-bare-metal-loop.md`](../../docs/runbooks/amd64-bare-metal-loop.md): the trashcan's loop, which this one inverts (there Akuma is the default)
- [`scripts/benchmarks/ryzen_fc/`](../../scripts/benchmarks/ryzen_fc/README.md): the Firecracker rig on the same machine
