# Wifi stack: fuzzing, a simulated bad link, and what they found — 2026-10-12

The station joins the home network and stays up for 40 minutes at a time
(`AKUMA_AMD64_ON_RYZEN_LAPTOP.md` § 5), on a link that loses frames often
enough that the access point resends message 3 two or three times per join
and the link drops now and then. This session put the stack's pure parts
under a deterministic fuzzer and a simulated bad link on the host, read the
kernel station against them, and fixed what turned up. Nothing here has run
on the metal yet; the last section says what needs to.

## What was built

| where | what | runs |
|---|---|---|
| `crates/akuma-ieee80211/tests/fuzz.rs` | every frame parser (beacon, RSN, auth, assoc, deauth, data, A-MSDU, CCMP header, elements) over 200k mutations of valid frames and random bytes | 0.1 s |
| `crates/akuma-wpa/tests/fuzz.rs` | `Key::parse`, `Supplicant::handle` in all three states, `aes::unwrap`, `find_gtk`, over 100k mutations; garbage between the real messages | 1.5 s |
| `crates/akuma-wpa/tests/lossy.rs` | **the bad link**: a hostapd-like access point (retries with the next replay counter, `AP_TRIES` then deauth, new ANonce per handshake, protected EAPOL readable only under its installed key), a channel that loses, duplicates, delays, corrupts and injects spoofed message 1s, and a station driver doing exactly what `rtw89_sta.rs` does with each `Action`. Six scenarios, 40–400 seeds each, loss up to 50 %, rekeys, a replay, "a long bad day" | 0.3 s |
| `crates/akuma-rtw89/tests/fuzz_rx.rs` | the RX buffer parser and ring arithmetic over 200k mutated buffers and any index register value | 0.01 s |
| `crates/akuma-wifi/tests/fuzz.rs` | the `/dev/wifi0` grammar (what any root process writes), the status parser over random text, every representable status through 4 KiB and back, hex, the simulated radio under any command sequence | 0.2 s |
| `akuma_wifi::sim` **lossy mode** (`wifisim=lossy`) | the simulated radio with a seeded fault model: scan misses, slow and failed joins, link drops that come back `connected` or `failed`, a wandering signal; ryzen menu entry 15 (`wifisoak`) runs `wifi auto` against it for 5 minutes | QEMU / metal |
| `userspace/wifi` | `auto_step` / `auto_backoff`: the `wifi auto` policy as a library function, run on the host against the lossy radio for 20 seeds × 2000 looks; `parse_config` over random text | 0.1 s |

All deterministic (seeded xorshift in each file), no dependencies, run by
the ordinary host test command. A failure names its seed and scenario.

## The audit: what was read, and what held up

Everything a frame or a `write(2)` reaches, end to end, judged against "what
does a malformed, spoofed, lost or duplicated input do here":

| read | verdict |
|---|---|
| `akuma-ieee80211::frame` (`Fc`, `Hdr`, `Ies`) | sound: every access bounds-checked; the element walk stops at a truncated element. 200k mutations, no finding |
| `akuma-ieee80211::beacon` (`parse`, `Rsn::parse`) | sound: suite counts of 65535 walk `get` and bail. No finding |
| `akuma-ieee80211::sta` (`Auth`, `AssocResp`, `Goodbye`, `Data`, `Amsdu`) | sound: the A-MSDU subframe walk cannot loop (each step advances by at least 14) nor overrun (`get`). `AssocResp::parse` accepts subtype 3 (reassoc) too — intended. No finding |
| `akuma-ieee80211::ccmp` (`header`, `Replay`) | sound; lacked a pairwise reset for a rekey (added, `pairwise_rekeyed`) |
| `akuma-wpa::eapol::Key::parse` | **finding 1** (panic) |
| `akuma-wpa::eapol::Supplicant` | **findings 3, 4, 5**; otherwise the MIC/nonce/flag order is right (nonce before MIC, so a MIC failure means "this key" and the kernel's `auth-failed` inference stands) |
| `akuma-wpa::aes::unwrap` / `find_gtk` | sound: length checks before any index; 20k random inputs. No finding |
| `akuma-rtw89::rx::parse`, `Ring` | sound: the `written` and `pkt_size` claims are both checked against the buffer; the ring's modulo keeps every index inside. 200k mutations, no finding |
| `akuma-wifi::cmd` | **finding 7** (empty SSID); the parser itself survives 200k random lines |
| `akuma-wifi::status`, `hex` | sound: every representable status round-trips through 4 KiB; a truncated read parses or fails, never panics. `Status::parse` fails the whole read on one unparsable `bss` field — strict, acceptable, noted |
| `amd64/src/wifi.rs` (the device) | sound: 512-byte cap, all lines parsed before any applied, cursors fixed-size and LRU. Not host-testable; read only |
| `amd64/src/rtw89_sta.rs`: `attempt` (the handshake loop) | one EAPOL message per lap and the rest left to the AP's retransmission: correct and now simulated. `Goodbye` mid-handshake, `Rekey` before keys, MIC counting: fine |
| `rtw89_sta.rs`: `idle` | **finding 6** (pairwise rekey); the from-AP filter, the clear/undecrypted/replay triage, and the beacon-loss grace are right. `trace_tcp`: **finding 2** |
| `rtw89_sta.rs`: `join_wanted`, `link_lost`, `find`, `BAD_BSS`, `sweep` | reasonable; `BAD_BSS` is never cleared on success (harmless with one AP, alternates with two); a stale cached channel costs a ~12 s triple attempt before `forget_network` rescans — could be one attempt then rescan. Left as is, noted |
| `userspace/wifi` `cmd_auto` | **finding 8**; `join()` returning at the kernel's first `failed` while the kernel keeps retrying is fine with the new wait on `associating` |
| `userspace/wifi` `parse_config` | sound over 20k random texts; `passphrase` costs a PBKDF2 per parse, by design |

## What it found

Ordered by how it was found: the fuzzers first, the simulation next, reading
against them last.

### 1. `eapol::Key::parse` panicked on a short body length — a kernel abort from the air (fuzz)

The parser checked the frame was ≥ 99 bytes, cut it to the 802.1X body
length the sender claims, then indexed bytes 97–98 of the cut frame. A body
length under 95 indexed past the end. The frame is a data frame to the
station's address with ethertype `0x888e` — message 1 has no MIC and
addresses are not authenticated, so anyone in range can send it, before or
after the join. Fixed: the cut frame must still hold a header
(`eapol.rs`, `Key::parse`; pinned by `short_body_length_is_refused_not_a_panic`).

### 2. `trace_tcp` panicked on an IPv4 total length under the header — a kernel abort from any peer (reading)

`amd64/src/rtw89_sta.rs` logs the first 40 TCP SYN/RST segments with their
checksums. It sliced the packet to the IP total length, then `p[12..20]` and
`p[ihl..]` — a total length of 0 (or any value under `ihl`) panicked. The
packet comes through the access point from anyone on the LAN, or from the
internet if the router forwards it. Fixed with bounds checks. Not
host-testable (it lives in the kernel file); it would be worth moving the
IP/TCP trace helpers into a crate.

### 3. The replay counter was recorded on message 1 — one spoofed frame locked the real access point out (simulation)

IEEE 802.11-2020 12.7.2: *"the local Key Replay Counter field should not be
updated until after the EAPOL-Key MIC is checked and found to be valid"* —
message 1 has no MIC. The supplicant recorded it anyway, so a message 1 with
a large counter (a spoof, or a stale frame from the access point's previous
life) made every real frame after it "replayed" until the access point gave
up and deauthenticated. The simulation's spoof scenario lost **241 of 300**
joins to this. Fixed: only MIC'd frames move the counter.

### 4. One candidate key — one spoofed message 1 inside the retransmission window failed the join (simulation)

After fix 3 the same scenario still lost 114 of 300. A spoofed message 1
between the real message 1 and message 3 replaced the candidate key and
nonce, message 3 then failed its nonce check until the access point gave up.
`wpa_supplicant` has the same single `tptk`. Fixed with a ring of the last
**four** candidates (`CANDIDATES`), each `(ANonce, PTK)`: message 3 is
matched to a candidate by nonce. It now takes four forgeries inside one
retransmission window; the scenario costs **10 deauthentications in 300
runs, against 2 with no spoofs at all** (loss alone), printed by the test.

### 5. Message 1 replaced the installed pairwise key at once (reading, then the unit test)

`msg1` derived the PTK into the one `ptk` field the group rekey and the
retransmitted-message-3 paths verify with. After the keys were in, any
message 1 — spoofed, stale, or the access point starting a real rekey —
left the station unable to verify the access point's next frame; the access
point would then deauthenticate it. Fixed by the candidate ring above: the
installed key changes only when a message 3 verifies under a candidate.

### 6. A pairwise rekey was ignored, so a network that rekeys dropped the link every time (reading)

`idle()` handled `Action::Complete` (a retransmitted message 3) and
`Action::Rekey` (group) and nothing else. An access point starting a new
4-way handshake on the live link (`wpa_ptk_rekey`, hourly on some firmware)
got message 1 ignored, retried, and deauthenticated the station; the kernel
then rejoined, a few seconds of outage and a new DHCP lease per rekey.
`Action::Complete` now carries `fresh`: the first handshake and a rekey hand
out new keys (install after message 4, under the old key, is on the air;
packet number and pairwise replay counters start over); a retransmitted
message 3 does not (answer again, in the clear, as measured on the metal).
`idle()` answers message 1 under the installed key and installs a fresh
completion's keys by replaying `JOIN4`. **The simulation runs this policy
through rekeys under 40 % loss; the metal has not run it** (below).

### 7. The command grammar could not carry an empty SSID (fuzz)

`cmd::write` of a `connect` with an empty SSID produced two blanks where
the hex went; `cmd::parse` skips blank words and read the key as the SSID.
No network is joinable by an empty name, so the writer now refuses it
rather than emit a line the parser cannot read back.

### 8. `wifi auto` fought the kernel's own rejoin (reading)

The rtw89 station rejoins a lost link by itself, reporting `associating`
meanwhile. The tool treated anything but `connected` as "scan and join",
and its `scan` made the daemon sweep all thirteen channels (~5 s) in the
middle of the rejoin, then its `connect` restarted the three-attempt join.
`auto_step` now waits on `scanning`/`associating` and steps in only on
`down`/`failed`; the backoff is `auto_backoff`. Both are host-tested, and
the whole loop runs against the lossy simulated radio in
`auto_keeps_the_lossy_radio_joined`.

## What the simulation says about the shipped policy

With the fixes, over the loss settings the real link is nowhere near:

| channel | joins that needed a deauth + rejoin | message 3 retransmitted |
|---|---|---|
| 10 % loss | ~0 of 400 | yes |
| 30 % loss, 10 % dup, 10 % delay, 5 % corrupt | some | yes |
| 50 % loss, 20 % dup, 20 % delay, 10 % corrupt | many, every one converged within budget | yes |
| 25 % loss + 15 % spoofs + reboots, 30 rounds of rekeys | every seed ends keyed in step with the access point | — |

The station never ends up holding keys the access point does not, a
replayed message 3 is refused before and after a group rekey, and a MIC
failure never happens on a frame the right key signed (checked at 0 %
corruption; with corruption on, the channel's bit flips are what the FCS
would catch on a real link).

## Not done, and what must run on the metal

- **Fix 6 on the metal**: a pairwise rekey has never been seen on this
  network (the home AP may not rekey the pairwise key at all). What is new
  on the kernel path: message 2 and message 4 of a rekey go out protected
  under the installed key (group message 2 already did, and the AP took it:
  "a group rekey ran", boot 47), and `JOIN4` is replayed a second time on a
  live association (it is replayed on every rejoin on the running card
  already, after `JOIN2`/`JOIN3`; whether it is safe without them is the
  open question). Watch for `[rtw] eapol: pairwise rekey:` lines in
  `klog-N.all`; a rekey that goes wrong ends in `sent away by the access
  point` and the usual rejoin, which is what happened before anyway.
- **Entries 7 and 15 were rehearsed** the same evening, in QEMU/KVM on ryzen
  (Pop side) with this kernel built on the Mac and copied over, in a private
  rig (`/home/netoneko/akuma-metal-wifi`: its own `out/`, `grub.cfg` and
  root image with the `wifisoak` service and the new `wifi` layered in) so
  the shared rig and the other agent's trees were not touched. Entry 7's
  transcript is identical to 2026-10-06's. Entry 15: `wifi auto` alive at
  all 30 samples, three link drops each recovered to `connected` by the
  radio alone, one join issued by the tool in five minutes, QEMU exited on
  the guest's own reboot, p3 fsck clean. **Not run on the metal.** The
  radio's `failed`-after-drop arm (1 in 4 drops) did not fire in this seed;
  the tool's recovery from it is covered by the host test only.
- Entry 7's run left p3 with `Deleted inode 351 has zero dtime` (fsck
  rc 4, no other error): `wifi forget` unlinks a file and the guest then
  hard-reboots; the ext2 driver's `unlink` does not stamp `dtime`. Not
  wifi's, not new (no earlier rehearsal deleted a file), worth a look in
  `akuma-ext2`.
- `trace_tcp` is still in the kernel file, untested on the host.
- The station daemon's timing policies (dwell, `MGMT_TRIES`, `JOIN_ATTEMPTS`,
  the beacon-loss grace, the rejoin backoff, `BAD_BSS`) are still only in
  `rtw89_sta.rs`. The simulation covers the EAPOL layer; a `Radio` trait
  over `Card` would let the whole join run on the host against the same
  channel model. That is the next extraction.
- The kernel infers `auth-failed` (and stops retrying) from a MIC failure
  followed by a timeout or deauthentication. A forged message 3 with the
  real ANonce (sniffable) would trigger it; `wifi auto` retries within a
  minute, so it is a nuisance, not a lockout.
