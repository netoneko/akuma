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
/dev/wifi0 ── amd64/src/wifi.rs ── backend: simulated radio (`wifisim`) │ rtw89 (to come)
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
| rtw89 | not yet | ryzen's RTL8852CE (W1–W4) |

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

- **No real radio.** `wifi auto` on a machine without `wifisim` finds no
  `/dev/wifi0` and says so every poll.
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
- [`../../../overlays/ryzen/README.md`](../../../overlays/ryzen/README.md): the reboot loop and the `wifitest` entry
