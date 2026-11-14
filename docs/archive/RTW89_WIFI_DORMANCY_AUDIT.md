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
- Source: `crates/akuma-kernel-amd64/src/rtw89.rs`, `crates/akuma-kernel-amd64/src/rtw89_sta.rs`,
  `crates/akuma-rtw89/src/script.rs`.

## Hardware

Realtek RTL8852CE (`rtw89_8852ce`), Lenovo ideapad platform, AP `Onyxia`
(a4:91:b1:e6:84:b1), observed at 2.4 GHz chan 1 then chan 11, RSSI about
-65 dBm (akuma's link_report sees "0 dBm" — its RSSI scaling is not
calibrated; see fix 5). Firmware in use everywhere:
`rtw89/rtw8852c_fw-1.bin` 0.27.122.0. Pop cannot load `rtw8852c_fw-2.bin`
("Direct firmware load ... failed with error -2" — linux-firmware too old);
akuma deliberately parses only the fw-1 format (`crates/akuma-kernel-amd64/src/rtw89.rs:74-78`).

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
Zero disconnects in the first ~15 minutes after (the earlier cadence was one
drop every 10–18 min; counter re-checked before the akuma fix branch).
Reversible: delete the conf.d file.

## Findings — akuma

The `wifi`/`wifiauto` herd services already run the chip **without power
save** (no power-save H2C anywhere in the rtw89 driver), and the firmware's
beacon-filter report (`RTW89_MAC_C2H_FUNC_BCNFLTR_RPT`, bits 9:8 = beacon
loss) is triaged by `BEACON_GRACE_MS = 5000`
(`crates/akuma-kernel-amd64/src/rtw89_sta.rs:105-107`): a loss report is believed only when
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
new fw format (`crates/akuma-kernel-amd64/src/rtw89.rs:76`) for no measured benefit. Skip.

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

## 2026-11-14 outage: silent channel change (11 -> 6), no recovery (observation, cause not proven)

Written down at the user's request so the stability work can pick it up later; nothing was changed.

**What happened.** The ryzen box (KVM-herd kernel, ~17.5 h uptime, no reboot) became unreachable from the LAN at
about 19:30 UTC while its console still said `wlan0: connected to Onyxia (a4:91:b1:e6:84:b1), chan 11`. It stayed
that way until the user ran `wifi disconnect` + `wifi connect` at ~19:46:55 UTC, after which it joined the **same
BSSID on chan 6** and worked. The scan list shows that BSSID on chan 6 and at least four other networks on chan 11.

**Evidence (all from the persisted `/var/log/ryzen/klog-290.all`, 91,917 lines; the in-kernel dmesg ring of 595
lines had already overwritten the moment).** Times are estimates (+-1 min) reconstructed from herd `PSTATS`
uptime stamps, not from a wall clock. The once-a-minute `[rtw] link:` line:

| time (UTC) | rx data | tx | tx `retry-limit` | note |
|---|---|---|---|---|
| 19:29:25 | 1,333,500 | 1,128,077 | 4 | normal traffic |
| 19:30:25 | 1,334,674 | 1,128,944 | 4 | last rx advance |
| 19:31 - 19:39 | 1,334,674 (frozen) | 1,128,944 (frozen) | 4 | nothing in, nothing out for ~10 min |
| 19:40 - 19:46 | frozen | +16..+45 | 10 -> 38 -> 46 -> 48 | transmits start, all fail at the retry limit |
| 19:46:55 | | | | `[rtw] join4 (keys)` = the user's reconnect |

- **No** `disconnected`, `deauth` or `beacon loss` line anywhere in that window. The only `beacon loss ignored: the
  access point's frames still arrive` in the boot is the one at join time (log line 802). The driver never noticed.
- `rx data` stopping dead while the link believes it is up is the signature of the radio sitting on a channel the AP
  left, not of a rekey or a lock problem (a group rekey ran normally at 19:26:25 and again at 19:56:25).
- The `wifi` daemon had `Onyxia` marked `auto` and did not reconnect, because from its side nothing dropped.

**Hypothesis, not confirmed:** the AP changed channel 11 -> 6 (auto-channel on a crowded 2.4 GHz band) without a
usable CSA, and the station neither followed nor detected the loss. This is the second channel hop seen on this AP
(chan 1 -> 11 above), and the cost this time was a manual rejoin after ~17 minutes dark. Not ruled out: a firmware
state wedge that only a rejoin clears. A beacon capture on chan 11 vs 6 at the time of the drop would settle it.

**Corrects the first read of this outage:** it was reported that nothing persisted the log. `klogd` does persist
(`/var/log/ryzen/klog-<boot>.all`); what was missing is any line at the moment of the drop, because the driver
logged none.

**Candidate work, in the plan's own order (no new ranking implied):**
1. A stall detector: `rx data` unchanged for N minutes while associated (or tx `retry-limit` rising with no rx) is a
   dead link; log it loudly and kick a rescan/rejoin. This one needs no CSA parsing and covers every cause above.
2. The CSA path already listed under "akuma fix plan" item 2, plus a `[rtw]` line when a beacon's channel differs
   from the joined one.
3. Have the `wifi` daemon's `auto` mode treat a stalled link as disconnected, so it rejoins by itself.
4. The 5 GHz BSSID preference (item 5) would take most of the exposure away.

## References

- `docs/archive/GOOSE_GLM_SERVE_ON_RYZEN.md` — the "~5 s of ping" symptom.
- `docs/archive/LITTER_TRASHCAN_RYZEN_JOIN.md` — the RTL8169 Silent-stall
  arm: the precedent shape for "one detection, one kick, measured".
- Pop evidence: `journalctl -b -1 -u wpa_supplicant` (60× reason=4);
  akuma evidence: `/var/log/ryzen/klog-266.all` on p3.
