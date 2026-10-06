# Next agent: ryzen wifi W3/W4 — join the home network with WPA2

You are continuing work in the Akuma repo (`/Users/netoneko/github.com/netoneko/akuma`,
branch `ryzen-wifi`). Akuma is a bare-metal Rust OS; read `CLAUDE.md` first and
follow it. Goal: Akuma on the laptop "ryzen" (RTL8852CE, `rtw89_8852ce` in
Linux) associates with the user's home WPA2-PSK network, completes the 4-way
handshake in the kernel, installs keys in the card, and gets a DHCP lease.

## Rules that are specific to this task

- **The user authorized commit and push to the `litter` remote** for this work.
  Commit at each working step. Never rewrite history.
- **Never print, log, commit or write into docs** the home network's name, its
  passphrase, the router's MAC (BSSID) or the card's real MAC. Refer to it as
  "the home network". The passphrase file is `~/.akuma/wifi/<network>` on the
  Mac: read it only to derive the PSK, never echo it. Never `cat`/`od` a file
  you did not create (one stray file leaked the password once).
- The user's decision (2026-10-06): derive the PSK with
  `PBKDF2-HMAC-SHA1(passphrase, ssid, 4096, 32)` on the Mac and stage it on
  ryzen's p3 as `/etc/wifi/<network>` (`ssid = …`, `psk = <64 hex>`, mode 0600)
  per `proposals/AKUMA_WIFI_CONTROL.md`. **The kernel never reads that file**:
  the userspace `wifi` tool (`userspace/wifi`, `wifi auto`) reads it and writes
  `connect wlan0 <ssid-hex> <psk-hex>` to `/dev/wifi0`.
- Logs from the metal must carry SSIDs only as FNV-1a hashes and BSSIDs only
  as OUI (`amd64/src/rtw89.rs` already does this). Home network = hash
  `0x2ce1df73`, on channel 1, 20 MHz, WPA2-PSK CCMP, PMF capable-not-required.
- Talk to ryzen only through `scripts/utils/hpbox.py` (`ryzen_root`, `RZ`).
  One metal boot cycle = `python3 overlays/ryzen/cycle.py <menu entry>`
  (ships kernel, rehearses in QEMU on ryzen, installs, arms, waits, prints
  `[rtw]` lines from p3). Build the kernel first with
  `cargo build -p akuma-amd64 --target x86_64-unknown-none --release --features no-tests`.

## Read first

- `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md` § 5 (W0–W2 results, the
  "recorded start" method), `overlays/ryzen/README.md`, `crates/akuma-rtw89/README.md`.
- `amd64/src/rtw89.rs` (W1/W2 glue: BAR, DMA statics, RX loop),
  `amd64/src/wifi.rs` (`/dev/wifi0`, today only the `wifisim` backend),
  `crates/akuma-wifi` (command/status protocol).
- Linux source of the same driver: `~/.akuma/src/rtw89` (v6.17; dual
  GPL-2.0/BSD-3, used under BSD-3 — keep the notice in `akuma-rtw89/src/lib.rs`).

## State (committed to `litter`/ryzen-wifi as of 2026-10-06 evening)

| piece | state |
|---|---|
| `crates/akuma-wpa` | **done**: SHA-1, HMAC, 802.11 PRF, PBKDF2, AES-128, RFC 3394, `eapol::Supplicant` (msg1→msg2, msg3→msg4 + keys, group rekey). 14 host tests, clippy clean |
| `crates/akuma-ieee80211/src/sta.rs` | **done**: auth/assoc request builders (assoc IEs = Linux's own from this card, RSN caps 0), auth/assoc/deauth parsers, QoS data frames with LLC/SNAP and CCMP header space, RX data parser. 12 tests |
| `crates/akuma-rtw89/src/script.rs` | **done, tests run**: `Vars` + op `0x41`; `run()` takes `&Vars` (glue caller updated) |
| `crates/akuma-rtw89/src/tx.rs` | **done (commit b2b8959e)**: `Desc::{mgmt,eapol,data}` with the recorded classes (mgmt QSEL_MGMT/CH8 hw-seq; EAPOL = tid-7 data → QSEL_VO/ACH3 sw-seq, unencrypted; data QSEL_BE/ACH0, CCMP128 sec_type 6, pairwise cam 0, PN in body words 4–5), `wd_page` (body v1 + info + WP + one address info; frames > 2044 bytes refused), `tx::Ring` over per-channel DMA memory ([`tx::CHAN_BYTES`]: ring + 8 WD pages + 8 frame buffers, FIFO page reclaim from the chip's index register). `bringup::Dma` gained `tx_phys: [u64; 3]` in `tx::USED` = (ACH0, ACH3, CH8) order; `pre_init` points those rings at it; the amd64 glue backs them with `TX_MEM` and asserts < 4 GiB. Host-tested against three recorded `txd` records + ring/kick/reclaim tests; clippy clean |
| `overlays/ryzen/w3-timeline.py` | done: type-only timeline of a join recording |
| `Cargo.toml` | `crates/akuma-wpa` added to default-members |

What the next session still owes, in order:

1. Seqgen extension + `join{1..4}.seq`, privacy check, parse tests like
   `recorded_up_sequence_parses_to_the_end`. (`w2-seqgen.py` currently emits
   `up.seq` only; the merged join stream is reproducible with
   `python3 overlays/ryzen/w2-merge.py ~/.akuma/w0/20261006-102808 --phase join --no-fwdl`.)
2. `amd64/src/rtw89.rs`: keep the card up after `init` as the real
   `/dev/wifi0` backend (`Backend::Rtw89` in `wifi.rs`); the join task per
   step 3 below. `Dma`'s `tx_phys` is already plumbed; the join task gets
   each channel's memory as `&mut TX_MEM[k * tx::CHAN_BYTES..]` and a
   `tx::Ring::new()`.
3. Join task (`sched::spawn_daemon`, pattern `net.rs` `netpoll_daemon`):
   scan ch1 → J1/J2 → auth → assoc → J3 → `akuma_wpa::eapol::Supplicant`
   → msg4 → J4 → keys. EAPOL frames go out via `Desc::eapol` on the ACH3
   ring; status lines into `akuma_wifi::status::Status`. Shut the card down
   on the reboot path.
4. Stage the PSK on p3 from the Mac (never printed), add a ryzen grub entry
   (next free index) running herd with `wifi auto` + `autoreboot`, rehearse,
   cycle. Iterate on `[rtw]` lines.
5. Then data path: DHCP via `ExternalDevice` in `akuma-net-nic` (W5) or a
   minimal DHCP probe first.
6. Docs: survey doc § 5.4, `crates/akuma-rtw89/README.md`,
   `docs/reference/subsystems/wifi.md`, `overlays/ryzen/README.md` menu.

## What the join recordings showed

Recordings: `~/.akuma/w0/20261006-102808` (best: has `txd`/`txh`/`txm`/`rxm`
probes) and `-102211`, made with `JOIN=1 sh overlays/ryzen/w0-trace.sh`.
Merge with `python3 overlays/ryzen/w2-merge.py <run> --phase join --collapse --no-fwdl --ts`,
view with `w3-timeline.py`.

- Linux goes into idle power save between scans; the final join starts with
  a full power-on + firmware download, then the start, then join setup.
- **Only H2C commands carry the BSSID, our MAC or the AID**; no register write
  does. The station MAC lives only in the address CAM, so the driver may pick
  its own: plan was locally administered `02:41:4b:55:4d:41`.
- Four segments to replay (each compiled by an extended `w2-seqgen.py` into
  `crates/akuma-rtw89/seq/join{1..4}.seq`, with the values above blanked and
  recorded as `0x41` substitutions; add a check that fails generation if the
  real MAC/BSSID bytes appear anywhere in the output):
  1. **J1**: after the last `R8 0x1e0 = 0xe2` up to (not incl.) the first
     ADDR_CAM H2C carrying the BSSID — power-on start + interface setup.
     Then listen on ch 1 (open the RX filter as W2 does, restore it after) to
     find the BSSID for the SSID `connect` named.
  2. **J2**: from that ADDR_CAM up to the auth frame TX (coex, RF
     calibration, channel notify).
  3. **J3**: after the association response up to the first periodic
     `OFLD_RSSI` C2H — EDCA, CCTL, JOININFO, ADDR_CAM (AID at byte 44, low 12
     bits), RA, packet-offload templates (PS-Poll carries AID|0xc000 at 14).
     Do **not** replay the later BA_CAM/ADDBA exchange.
  4. **J4**: after EAPOL msg 4 TX through the second key's ADDR_CAM and
     BCNFLTR. The two SEC_CAM bodies in the recording are zeroed (redacted);
     synthesize them from `cam.c` `rtw89_cam_get_sec_key_cmd` (pairwise sec
     cam idx 0, group idx 1, CCMP128). The group key's id must be patched into
     the ADDR_CAM and DCTL commands (`GTK_IDX_HI2`): diff the commands before
     and after each install to find the byte.
  - Exclude from all segments: TX index registers `0x1058..=0x107c` and
    their reads, plus everything `w2-seqgen.py` already drops (IRQ, RX idx).
- `up.seq` writes no ring bases; it un-stops every TX channel at the end.
  So TX needs its own ring memory for the channels used (management, VO,
  BE) — extend `bringup::Dma` so `pre_init` can point those channels at real
  rings instead of the shared idle ring (keep the golden trace test passing:
  ring bases are excluded from comparison).
- TX descriptors: decode the `txd` probe records (struct `rtw89_tx_desc_info`
  in `core.h`) per frame type — auth/assoc, EAPOL, encrypted data — and build
  the WD page as `pci.c` `rtw89_pci_txwd_submit` + `core.c`
  `rtw89_core_fill_txdesc_v1` + `rtw89_pci_fill_txaddr_info_v1` do for 8852C.
  Keep frame DMA below 4 GiB. Host-test it against those decoded records.

## Remaining work, in order

(The list below is the original plan; the state table above says which of
its items are already done.)

1. ~~`tx.rs` in `akuma-rtw89` (+ `Dma` TX rings), host tests.~~
2. Seqgen extension + `join{1..4}.seq`, privacy check, parse tests like
   `recorded_up_sequence_parses_to_the_end`.
3. `amd64/src/rtw89.rs`: keep the card up after `init` as the real
   `/dev/wifi0` backend (`Backend::Rtw89` in `wifi.rs`); a
   `sched::spawn_daemon` task (pattern: `net.rs` `netpoll_daemon`) polls RX,
   runs: scan ch1 → J1/J2 → auth → assoc → J3 → `akuma_wpa::eapol::Supplicant`
   → msg4 → J4 → keys. Status lines into `akuma_wifi::status::Status`. Shut
   the card down on the reboot path (no live bus master across warm reset).
4. Stage the PSK on p3 from the Mac (never printed), add a ryzen grub entry
   (next free index) running herd with `wifi auto` + `autoreboot`, rehearse,
   cycle. Iterate on `[rtw]` lines.
5. Then data path: DHCP via `ExternalDevice` in `akuma-net-nic` (W5) or a
   minimal DHCP probe first.
6. Docs: survey doc § 5.4, `crates/akuma-rtw89/README.md`,
   `docs/reference/subsystems/wifi.md`, `overlays/ryzen/README.md` menu.

Host tests: `cargo test -p akuma-wpa -p akuma-ieee80211 -p akuma-rtw89 --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)`;
clippy the same crates. Allocation-free, `forbid(unsafe_code)` in crates,
console output only via the existing `serial::puts`/`put_hex` helpers.
