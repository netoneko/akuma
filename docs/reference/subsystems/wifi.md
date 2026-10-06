# Wifi: `/dev/wifi0`, `/etc/wifi`, and the `wifi` tool

> **Stability: B (verify behaviour).** Built 2026-10-06. The control path
> (device, protocol, tool, config files) is complete and tested end to end
> against a **simulated radio**. There is no real radio driver yet: ryzen's
> RTL8852CE is the wifi plan's W1–W4
> ([`../../archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](../../archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md) § 5).
> amd64 only.

## The shape: no supplicant

On Linux a userspace daemon (`wpa_supplicant`, `iwd`) owns wifi: it keeps the
networks and their secrets, asks the kernel to scan and associate over
`nl80211`, and runs the WPA2 4-way handshake itself. **Akuma keeps the
handshake in the kernel** (user's decision, 2026-10-06), so userspace does only
three things:

1. **Store** known networks and their keys: `/etc/wifi/<network>`.
2. **Choose** one: `wifi`, run by herd as `wifi auto`.
3. **Hand the choice to the driver**: one line on `/dev/wifi0`,
   `connect wlan0 <ssid-hex> <psk-hex>`.

The kernel never reads a file; `wifi` never touches a frame. Design and
alternatives considered (`nl80211`, Wireless Extensions, a socket, a syscall):
[`proposals/AKUMA_WIFI_CONTROL.md`](../../../proposals/AKUMA_WIFI_CONTROL.md).
It is a device rather than a socket by choice: one radio, one manager, and
`echo`/`cat` debuggability. The protocol names the interface on every line, so
a socket transport later would not change the grammar.

```
herd ─ "wifi" service (/bin/wifi auto)
         │ reads /etc/wifi/*  (0600)
         │ write "scan wlan0" → read results → choose → write "connect wlan0 <ssid> <psk>"
         ▼
/dev/wifi0 ── amd64/src/wifi.rs ── backend: simulated radio (`wifisim`) │ rtw89 (W2: receives; not yet a backend)
```

## Where the code is

| | |
|---|---|
| [`crates/akuma-wifi`](../../../crates/akuma-wifi/src/lib.rs) | the protocol: command grammar (`cmd`), status text writer + parser (`status`), hex (`hex`), the simulated radio (`sim`). `no_std`, no allocation, `forbid(unsafe_code)`, 10 host tests. Shared by kernel and tool, so they cannot drift |
| [`amd64/src/wifi.rs`](../../../amd64/src/wifi.rs) | the device: backend selection, per-descriptor read cursors, command dispatch. `openat`/`read`/`fstat` hooks in `amd64/src/fd.rs`, `write`/`close` in `amd64/src/usermode.rs` |
| [`userspace/wifi`](../../../userspace/wifi/README.md) | the tool. Library half (config files, PBKDF2, network choice) has 7 host tests |
| `crates/akuma-vfs/src/dev.rs` | the `wifi0` devfs node, present when a backend registered (`akuma_vfs_glue::set_wifi_present`) |
| `crates/akuma-exec-core` | `FileDescriptor::DevWifi` |

## The protocol (`/dev/wifi0`)

**Write** one or more command lines. All are parsed before any is applied,
so a bad line applies nothing.

```
scan <if>
connect <if> <ssid-hex> <psk-hex | -> [<bssid aa:bb:cc:dd:ee:ff>]
disconnect <if>
```

The interface is `wlan0`. SSIDs are hex because an SSID is up to 32 arbitrary
bytes. The key is the 32-byte WPA2 **PSK**, never the passphrase, so the
kernel never runs PBKDF2. `-` joins an open network.

| refusal | errno |
|---|---|
| unknown verb | `EOPNOTSUPP` |
| an interface other than `wlan0` | `ENODEV` |
| missing/extra argument, bad hex, a key that is not 64 hex digits, bad BSSID | `EINVAL` |
| more than 512 bytes in one `write` | `EINVAL` |

**Read** the driver's state: `key=value` lines, then one `bss` line per scan
result. Readers ignore keys they do not know, so the kernel may add some.

```
iface=wlan0
radio=sim                      none | sim | rtw89
state=connected                no-radio | down | scanning | associating | connected | failed
ssid=616b756d612d73696d2d77706132
bssid=02:00:00:00:00:02
chan=6
signal=-55                     dBm
security=wpa2-psk              open | wpa2-psk | other
error=none                     none | not-found | auth-failed | unsupported | timeout
scans=1                        completed scans; a scanner waits for this to move
bss ssid=… bssid=… chan=… signal=… security=…
```

**Reads are a snapshot per descriptor.** The first read on a descriptor takes
a snapshot. Later reads continue through it, and the read at its end returns 0
(EOF), after which the next read takes a fresh snapshot. So `cat` shows the
state once, and a poller just reads to EOF each time. Cursors live in a fixed
table of 8 × 4 KiB keyed by (thread group, fd), with **no allocation** on the
path. A full table evicts the least recently used cursor, whose reader simply
restarts at a fresh snapshot. A `fork`ed copy of the fd is its own key and so
gets its own cursor. `close` frees it.

## Backends

| backend | selected by | |
|---|---|---|
| none | default | **no `/dev/wifi0` node at all** (`open` is `ENOENT`, `ls /dev` omits it) |
| simulated | `wifisim` on the kernel command line (PVH and multiboot2 paths) | `akuma_wifi::sim`, below |
| rtw89 | `rtw89wifi` (multiboot2 path, after the root mount) | ryzen's RTL8852CE, kept up after its bring-up; `amd64/src/rtw89_sta.rs` is the backend. **Scan works** (the real networks on channel 1, with security); **`connect` does not complete yet**: authentication goes out and is reported done by the chip, but nothing is received after `JOIN2` — survey doc § 5.4. W1 (`rtw89`, firmware) and W2 (`rtw89rx`, receive) are the earlier stages, below |

**The simulated radio** is deterministic: the same networks every boot, scans
complete at once, and fixed rules for `connect`. A test that passes against it
fails later only because a real radio differs.

| network | security | chan | dBm | joins with |
|---|---|---|---|---|
| `akuma-sim-open` | open | 1 | -40 | `-` |
| `akuma-sim-wpa2` | WPA2-PSK | 6 | -55 | the PSK of passphrase `akuma-sim-passphrase` (`SIM_WPA2_PSK`); anything else is `auth-failed` |
| `akuma-sim-far` | WPA2-PSK | 11 | -82 | any key |
| `sim☃` (non-ASCII bytes) | open | 36 | -70 | `-` |
| `akuma-sim-sae` | other (WPA3) | 44 | -50 | refused: `unsupported` |

These names and the passphrase are the simulator's, fake and public. Real
network names and passphrases never go in the repo. Real passphrases live on
the laptop in `~/.akuma/wifi/<network>`.

## The radio: RTL8852CE (`akuma-rtw89`)

Stage W1 of the wifi plan: from a powered-off card to running firmware.

| piece | where | does |
|---|---|---|
| the sequence | `crates/akuma-rtw89` (`forbid(unsafe_code)`, host-tested) | power-on (`rtw8852c_pwr_on_func`), the DMA engine's download-mode setup (`dle_init(DLFW)`, H2C flow control, `rtw89_pci_ops_mac_pre_init_ax`), the firmware CPU reset, the download on CH12, the wait for `fw ready`; the firmware container and header parser |
| the hardware | `amd64/src/rtw89.rs` | finds `10ec:c852`, D0, maps BAR2, **Bus Master on the root port** as well as the card, `.bss` DMA memory, streams the firmware file through `fs::read_at` (never loaded whole), logs `[rtw]` lines, always shuts the card down |
| the boot token | `rtw89` (multiboot2 path, after the root mount) | ryzen menu entry 8 |
| W2: the start | `akuma_rtw89::script` + `seq/up.seq` | the rest of Linux's `rtw89_core_start` after `fw ready`, **replayed from a recording** of Linux on this card (writes, checked reads, polls, delays, 45 H2Cs); reports where the chip departs from it |
| W2: receive | `akuma_rtw89::rx`, `akuma-ieee80211` | RXQ entries with their own buffers, the RX descriptor, beacons and probe responses (SSID, channel, RSN); token `rtw89rx`, ryzen menu entry 9 |
| W3/W4: the station | `amd64/src/rtw89_sta.rs` over `rtw89::Card`, `akuma_rtw89::{tx, script::JOIN1..4}`, `akuma_ieee80211::sta`, `akuma_wpa` | `/dev/wifi0`'s radio: `JOIN1` at start, scan with the RX filter opened, then `JOIN2` → auth → assoc → `JOIN3` → 4-way handshake → msg 4 → `JOIN4` (keys). WPA2-PSK only; open networks answer `unsupported`. Station address `02:41:4b:55:4d:41`. Token `rtw89wifi`, ryzen menu entry 10 |

**What a good boot logs** (`boot-N.early` on p3):

```
[rtw] firmware /lib/firmware/rtw89/rtw8852c_fw-1.bin, 2375560 bytes
[rtw] root port command was 0x0000000000000003
[rtw] chip cut 0x0000000000000001
[rtw] h2c path ready, FW_CTRL 0x0000000000000023
[rtw] header accepted, FW_CTRL 0x0000000000000027
[rtw] section packets sent 0x00000000000000a6
[rtw] fw ready v0.27.122 (cut 1, 166 packets, FW_CTRL 0x00000000000000e2, 49631 us)
[rtw] card shut down
```

`WCPU_FW_CTRL` (`0x1e0`) is the handshake: `0x01` download enabled → `0x23`
H2C path ready → `0x27` header accepted → `0xc3` image in (status 6) →
`0xe2` **init ready** (status 7). Linux's driver walks the same values (W0
trace). A failure names the stage, the register and its last value, and dumps
`DMAC_ERR_ISR` and `HAXI_IDCT` — the DMA engine's own account of what stuck.

**How it is tested.** `tests/golden_trace.rs` replays the whole bring-up
against Linux's register trace of this very card (`tests/golden/w0_up.txt`,
651 accesses): every write must match register, width and value, ring base
addresses excepted. `tests/sim_chip.rs` checks what the trace cannot see — the
DMA payload: header packet then every section byte in order, slot reuse under a
slow chip, a garbage ring index, a rejected image, a short read.
`tests/real_firmware.rs` runs the parser over the real file when
`AKUMA_RTW89_FW` points at it (166 packets, version 0.27.122.0).

**The trap it hit.** Five metal runs stalled at "header accepted" with the
register sequence provably identical to Linux's. The card's root port had come
out of firmware with command `0x0003`: decode on, **Bus Master off**, so every
DMA read the card made was dropped at the port (`HAXI_IDCT` bit 0,
`TXMDA_STUCK`). Linux's `pci_enable_device` enables bus mastering on every
bridge above a device; Akuma's `pci::enable_full` only ever touched the device.
`pci::enable_bridges_above` now does what Linux does. The NVMe never showed it
because firmware booted through its port.

## `/etc/wifi/<network>`

One file per network, named by the user, mode `0600` (the directory `0700`):

```
ssid = <the network name>        # or ssid_hex = … for bytes that are not plain text
psk = <64 hex digits>            # absent = open network
priority = 0                     # higher wins when several are in range
autoconnect = true
bssid = aa:bb:cc:dd:ee:ff        # optional: pin one access point
```

`passphrase = …` is accepted when the file is written by hand. It is turned
into `psk` (PBKDF2-HMAC-SHA1, 4096 rounds, the SSID as salt) when read, and
`wifi` never writes it back. `wifi add` stores only the PSK, so the human
passphrase, which people reuse, never reaches the disk. The PSK is still the
network's full credential, hence `0600`. Today every Akuma process is uid 0
(`geteuid` answers 0), so the modes state the intent rather than enforce it.

## Testing

| layer | how |
|---|---|
| protocol | `cargo test -p akuma-wifi` (host): grammar round trips and refusals, status write-then-parse, the 4 KiB worst case, the simulator's rules |
| tool | `cargo test -p wifi --lib --no-default-features` in `userspace/` (host): PBKDF2 against IEEE 802.11i Annex H.4 (`password`/`IEEE` → `f42c6fc5…a12e`) and against the simulator's key, config round trips including non-UTF-8 SSIDs, refusals, network choice |
| devfs node | `cargo test -p akuma-vfs` (host): `wifi0` follows the probe, is `0600`, hidden in boxes |
| end to end | ryzen overlay menu entry 7 (`wifisim` + the `wifitest` herd service): the real tool through the real device in the QEMU rehearsal (`DISK=nvme sh qemu.sh 7`), or on the laptop (`arm.sh 7`). The transcript goes to `/var/log/ryzen/wifitest-N.txt` on p3: scan, add, list, a wrong key failing, the right key joining, auto-choice, disconnect, forget |

First end-to-end run, 2026-10-06, in the rehearsal: every step as expected.
`wifi connect` with no name chose `akuma-sim-open` over `akuma-sim-wpa2` (equal
priority, stronger signal), and the non-UTF-8 SSID displayed as `sim\xe2\x98\x83`.

## Known gaps

- **The real radio scans but does not join yet** (`rtw89wifi`): nothing is
  received after `JOIN2`, so authentication times out (survey doc § 5.4).
  Without `rtw89wifi` or `wifisim` there is no `/dev/wifi0`, and `wifi auto`
  says so every poll.
- **No signal strength from the real radio**: scan results report `signal=0`
  (the PPDU status reports that carry RSSI are not parsed), so `wifi` picks
  among equal-priority networks by order, not strength.
- **`disconnect` sends no deauthentication**; the access point times the
  station out. A group rekey replays all of `JOIN4` (the pairwise key goes in
  again unchanged) until a group-only segment is cut from the recording.
- **No `poll(2)` readiness on `/dev/wifi0`.** The tool polls by reading (100 ms
  steps). Readiness-on-state-change is the natural next step once a real radio
  makes state change asynchronously.
- **`wifi add` does not suppress echo** when the passphrase is typed at a
  terminal. Redirect it from a file instead.
- **No IP layer yet.** Joining does not bring up an interface in
  `akuma-net-nic`; that is the `ExternalDevice::Rtw89` work in W5.

## Background

- [`proposals/AKUMA_WIFI_CONTROL.md`](../../../proposals/AKUMA_WIFI_CONTROL.md): the design, accepted 2026-10-06
- [`../../archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](../../archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md): the wifi plan (§ 5) this serves
- [`../../../overlays/ryzen/README.md`](../../../overlays/ryzen/README.md): the reboot loop, the `wifitest` entry and the `rtw89` entry
- [`../../archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`](../../archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md) § 5.1–5.2: the W0 trace and the W1 metal runs
