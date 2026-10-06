# wifi

Akuma's wifi manager. It keeps the known networks in `/etc/wifi/<network>`,
picks one, and hands the kernel the key over `/dev/wifi0`. There is no
supplicant: the kernel does the WPA2 handshake. How the pieces fit:
[`docs/reference/subsystems/wifi.md`](../../docs/reference/subsystems/wifi.md).

## Commands

| command | does |
|---|---|
| `wifi status` | the link: `wlan0: connected to <ssid> (<bssid>), chan 6, -55 dBm, wpa2-psk (radio sim)` |
| `wifi scan` | scan, then list what is in range, strongest first; `*` marks known networks |
| `wifi list` | known networks: name, priority, autoconnect, `open`/`psk`, SSID. **Never the key** |
| `wifi add <name> [ssid] < passfile` | one line on stdin (8–63 printable ASCII) → PSK → `/etc/wifi/<name>`, `0600`. The SSID defaults to the name |
| `wifi add-open <name> [ssid]` | an open network |
| `wifi connect [name]` | join `<name>`, or with no name scan and join the best known network in range; waits for `connected` or `failed` (exit 1 on failure) |
| `wifi disconnect` | drop the link |
| `wifi forget <name>` | delete the file (disconnecting first if it is the current network) |
| `wifi auto` | the herd service: every 5 s, if not connected, scan and join the best known network; back off to 60 s after failures |

"Best" means `autoconnect = true`, in range, joinable (an open network with no
key, or WPA2 with one), then highest `priority`, then strongest signal.

Network names are file names: 1–64 of `[A-Za-z0-9._-]`, not starting with a
dot. An SSID can be anything (`ssid_hex =` in the file covers bytes that are
not plain text).

## As a service

`mkdisk.sh` stages `/etc/herd/available/wifi.conf` (`command = /bin/wifi`,
`args = auto`, restart after 5 s). It is **available, not enabled**: a machine
without a wifi backend has no `/dev/wifi0`. Use `herd enable wifi`, or name it
with `--service`.

## Building and testing

```sh
cd userspace && cargo build -p wifi --target x86_64-unknown-none --release
HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo test -p wifi --lib --no-default-features --target $HOST      # the library half
```

The binary links `libakuma`, so like `sshd` and `box` only `src/lib.rs` is
host-tested: config parsing and writing, PBKDF2 (checked against IEEE 802.11i
Annex H.4 and against the kernel simulator's key), and network choice.
End to end, against the kernel's `wifisim` radio: ryzen overlay menu entry 7
(`overlays/ryzen/README.md`).

## Limits

- `wifi add` reads the passphrase without suppressing terminal echo. Redirect
  it from a file.
- Status is polled (100 ms steps); `/dev/wifi0` has no `poll(2)` readiness yet.
- No real radio exists yet. Until one does, only `wifisim` gives the tool
  something to manage.
