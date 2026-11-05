# RTW89_WIFI_DORMANCY_AUDIT.md

Date: 2026-11-05. Scope: the ryzen box's RTL8852CE link-drop ("wifi dormancy,
~5 s of ping before ssh") seen under both Pop!_OS and akuma. Status:
root-caused from logs on both OSes; Pop-side fix applied; akuma-side fixes
planned (not yet implemented).

## Why

ssh to ryzen kept dying mid-kex; `docs/archive/GOOSE_GLM_SERVE_ON_RYZEN.md`
documents the workaround ("wifi needs ~5 s ping before ssh"). Same box, two
OSes, one NIC — worth one audit.

## Method

- Pop side: `journalctl` for `wpa_supplicant`/`NetworkManager`/kernel, this
  boot and `journalctl -b -1`; `iw dev wlp2s0 link`; NM conf.d.
- akuma side: the `klog` herd daemon (kernel log to p3 every 5 s,
  `etc/ryzen/klog.sh`) — mount `/dev/nvme0n1p3` (label AKUMA-RYZEN)
  read-only from Pop and read `/var/log/ryzen/klog-*.all`. Boots 262–266
  (Nov 2–5) inspected; 266 is the session that ended at 20:06 when the box
  was rebooted to Pop.
- Source: `amd64/src/rtw89.rs`, `amd64/src/rtw89_sta.rs`,
  `crates/akuma-rtw89/src/script.rs`.

## Hardware

Realtek RTL8852CE (`rtw89_8852ce`), Lenovo ideapad platform, AP `Onyxia`
(a4:91:b1:e6:84:b1), observed at 2.4 GHz chan 1 then chan 11, RSSI about
-65 dBm (akuma's link_report sees "0 dBm" — its RSSI scaling is not
calibrated; see fix 5). Firmware in use everywhere:
`rtw89/rtw8852c_fw-1.bin` 0.27.122.0. Pop cannot load `rtw8852c_fw-2.bin`
("Direct firmware load ... failed with error -2" — linux-firmware too old);
akuma deliberately parses only the fw-1 format (`amd64/src/rtw89.rs:74-78`).

## Findings — Pop (Linux)

Every drop is `CTRL-EVENT-DISCONNECTED bssid=... reason=4
locally_generated=1` — 802.11 "disassociated due to inactivity" raised by the
laptop — followed by reassociation to the same BSSID within ~4 s:

- previous boot: **60** drops, all `reason=4 locally_generated=1`
  (plus 1 `reason=3` = deliberate leave);
- this boot: 1 drop at 20:18:06, ten minutes after boot;
- `wifi.powersave = 3` (NM default = on): the chip dozes, misses
  beacons/AP traffic, supplicant declares inactivity. Known rtw89/8852CE
  behavior, amplified by marginal 2.4 GHz RF.

### Fix applied on Pop (2026-11-05 20:22)

`/etc/NetworkManager/conf.d/70-wifi-powersave-off.conf`:

```
[connection]
wifi.powersave = 2
```

plus `iw dev wlp2s0 set power_save off` for the live connection.
Zero disconnects in the first hour after (counter checked hourly below).
Reversible: delete the conf.d file.

## Findings — akuma

The `wifi`/`wifiauto` herd services already run the chip **without power
save** (no power-save H2C anywhere in the rtw89 driver), and the firmware's
beacon-filter report (`RTW89_MAC_C2H_FUNC_BCNFLTR_RPT`, bits 9:8 = beacon
loss) is triaged by `BEACON_GRACE_MS = 5000`
(`amd64/src/rtw89_sta.rs:105-107`): a loss report is believed only when
nothing from the AP arrived for 5 s; otherwise
"beacon loss ignored: the access point's frames still arrive".

Per boot 262–266, real loss / ignored reports: 1/1, 0/1, 0/1, 0/2, 1/2 —
about one hard loss per boot (vs Pop's ~3/hour). The losses are acted on by
`link_lost()` + wifiauto full rescan/rejoin ("wifi: joining home (Onyxia)").
The 5 s grace + rejoin is exactly the "~5 s of ping" dormancy.

Two more facts from klog-266:

- Onyxia changed channel mid-session (joined `chan 1`, later `chan 11`);
  each hop costs a full rejoin because CSA (channel-switch announcement) is
  not parsed.
- The `[rtw] link:` stats line printed
  `max gap 18446744073708616 ms` — a u64 underflow in the lap-gap
  computation that hides the real worst-case gap.

## The firmware bin is not the fix

Same blob on both OSes: Pop's drop loop is power-save-driven and stops when
power save is off; akuma already runs power-save-free and sees ~1 loss per
boot. Moving to `rtw8852c_fw-2.bin` would require teaching akuma's parser a
new fw format (`amd64/src/rtw89.rs:76`) for no measured benefit. Skip.

## akuma fix plan (ranked; not yet started)

1. **Fast reassociate**: on a believed beacon loss with healthy beacon RSSI,
   re-associate to the current BSSID first (the join script steps exist in
   `crates/akuma-rtw89/src/script.rs`); full rescan only if that fails.
   Expected: dormancy 5 s + rescan → ~1 s.
2. **CSA**: parse the channel-switch announcement element in beacons
   (`akuma_ieee80211::beacon`) and retune the card instead of dropping the
   link on channel change.
3. **Adaptive grace**: with report beacon RSSI (bits 23:16, −110 dBm bias)
   stronger than about −60 dBm, shrink `BEACON_GRACE_MS` to ~2 s; the
   firmware over-reports at this AP and the grace is pure added latency.
4. **Stats underflow**: the `max gap` lap computation needs
   saturating/checked subtraction (the wrapped value printed is
   `18446744073708616`).
5. **RSSI calibration + 5 GHz preference**: `link_report` shows `0 dBm`
   (scaling off) — fix the bias math; then prefer a 5 GHz BSSID of Onyxia in
   network selection if present. -65 dBm on 2.4 GHz is the shared RF root
   cause; 5 GHz is the real robustness win.

## References

- `docs/archive/GOOSE_GLM_SERVE_ON_RYZEN.md` — the "~5 s of ping" symptom.
- `docs/archive/LITTER_TRASHCAN_RYZEN_JOIN.md` — the RTL8169 Silent-stall
  arm: the precedent shape for "one detection, one kick, measured".
- Pop evidence: `journalctl -b -1 -u wpa_supplicant` (60× reason=4);
  akuma evidence: `/var/log/ryzen/klog-266.all` on p3.
