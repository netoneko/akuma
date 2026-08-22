# Security audit: the `ssh` client (and one `sshd` finding)

A self-review pass over `userspace/sshd/src/client/` and
`userspace/sshd/src/client_wire.rs` (the new `ssh` client, see
[`SSH_CLIENT.md`](SSH_CLIENT.md)), done immediately after writing the code
and before it ever ran against a real server. Three of these were caught by
the same host-test suite written to cover them — the tests failed against
the first version of the fix, not just the original bug, which is worth
noting since it means the *fixed* behavior is what's pinned down, not just
"doesn't panic on this one input."

## Fixed

### 1. Remote crash on a 5-byte packet, pre-authentication

`take_unencrypted_packet` (the pre-`NEWKEYS` framing, no MAC) decoded
`packet_len` from an attacker-controlled 4-byte field and then unconditionally
indexed `input_buffer[5]` (the message type) without checking that
`packet_len` was large enough for that index to exist. `packet_len = 1` with
`padding_len = 0` passes the existing `padding_len >= packet_len` guard (`0`
is not `>= 1`) and then panics on `input_buffer[5]` — one byte past a 5-byte
buffer. Since `panic = "abort"` (this workspace's profile), that panic kills
the whole process.

**Reachable by any TCP peer, unauthenticated, with 5 crafted bytes**, since
this framing has no MAC yet. `take_encrypted_packet` had the identical gap
one level in (`decrypted[4]`/`decrypted[5]`), reachable post-auth by a
malicious server (it only needs a packet that MACs correctly under keys it
already holds — which, from the client's position, it does).

**Fix:** both functions now reject `packet_len < 2` before touching either
index (`client_wire.rs:195`, `:261`) — a packet needs at least a
`padding_len` byte and one payload byte (the message type) to exist at all.

**Regression tests:**
`encrypted_packet_rejects_packet_len_too_small_to_hold_a_message_without_panicking`,
`unencrypted_packet_rejects_packet_len_too_small_to_hold_a_message_without_panicking`.

### 2. The same crash class, one arithmetic step later

While adding a regression test for #1, a **second, separate** instance of
the same class turned up: `payload_len = packet_len - padding_len - 1` can
legitimately compute to `0` (e.g. `packet_len=2, padding_len=1`), and both
functions then sliced `[6..5+payload_len]` — with `payload_len=0` that's
`[6..5]`, a slice with `start > end`, which panics unconditionally in Rust
regardless of what's actually in the buffer.

Every *real* SSH payload carries at least the message-type byte, so
`payload_len == 0` only arises from a peer that's lying about its own
framing — same reachability as #1 (pre-auth for the unencrypted path,
post-auth-malicious-server for the encrypted one).

**Fix:** both functions now reject `payload_len < 1` (`client_wire.rs:223`,
`:273`).

**Regression tests:** `encrypted_packet_rejects_payload_len_zero_without_panicking`,
`unencrypted_packet_rejects_payload_len_zero_without_panicking`.

### 3. "Malformed" was being treated as "not enough data yet" — a hang, not a crash

The original `Option<(u8, Vec<u8>)>` return type conflated two genuinely
different situations under one `None`: *fewer bytes are buffered than this
packet needs* (true "come back later") and *all the bytes this packet needs
are already buffered, and they don't parse* (bad MAC, or an out-of-range
`padding_len`/`payload_len`). The second case can **never** resolve by
waiting for more bytes — the packet in question is already fully in hand and
already invalid. A caller that treats both as "keep waiting" (which the
first version of `take_encrypted_packet`/`take_unencrypted_packet` did, and
which `sshd`'s own `process_encrypted_packet`/`process_unencrypted_packet`
still do — see below) spins forever re-parsing the same stuck bytes: not a
crash, but a hang, and a **pre-auth** one for a MITM that corrupts a single
byte during the unencrypted phase.

**Fix:** replaced the return type with a three-state `TakePacket` enum
(`Ready`/`Incomplete`/`Malformed`); `Connection::try_take_packet` in
`protocol.rs` now turns `Malformed` into a `ClientError` that actually
disconnects, instead of a `None` the pump loop reinterprets as "nothing to
do this tick."

**Regression test:** `encrypted_packet_rejects_a_bad_mac_as_malformed_not_incomplete`
(the name states the behavior the earlier version got backwards).

**Not fixed here, flagged for follow-up:** `userspace/sshd/src/protocol.rs`
(the *server*, `process_encrypted_packet`/`process_unencrypted_packet`) has
the same `None`-conflates-both-cases shape, inherited by the client when this
code was first written by mirroring it. Fixing the server was out of scope
for a client-focused audit — it's shipped, in-use code with its own test
history, and changing its error-handling contract deserves its own pass, not
a drive-by edit — but it has the same hang class described above.

### 4. Ephemeral X25519 secret and the long-term identity key both drew from a 64-bit PRNG

`akuma_ssh_crypto::crypto::SimpleRng` is an xorshift64 generator: 64 bits of
internal state, seeded once from 8 bytes of hardware entropy. It's the
RNG both `sshd` and (originally) this client used for *everything* —
including, in the first version of this client, the X25519 ephemeral secret
generated fresh per connection and the Ed25519 identity key generated once
and persisted to disk.

That's fine for what it was designed for in `sshd` (the KEXINIT cookie,
anti-fingerprinting padding — neither secret, neither security-load-bearing)
but wrong for key material: a 256-bit key generated from a 64-bit PRNG state
doesn't have 256 bits of effective security, it has (at most) 64 — the real
security margin collapses to whatever it costs to brute-force the PRNG's
internal state, which is a solved problem for a plain xorshift. For the
ephemeral KEX secret this would undermine forward secrecy for that session;
for the persisted identity key, every future connection authenticating with
it.

**Fix:** both call sites now pull directly from `getrandom()` (real hardware
entropy, no PRNG in between) instead of `conn.rng.fill_bytes()`:

- `protocol.rs:168` — the X25519 ephemeral secret. Failure aborts the
  connection (`ClientError`) rather than falling back to `SimpleRng`.
- `client/keys.rs:84` — the generated identity key. `generate_and_save` and
  `load_identity` both had to become `Option`-returning so a `getrandom`
  failure surfaces as "no usable identity key" instead of silently
  proceeding with weak (or, previously, zeroed-then-xorshifted) material.

`SimpleRng` is still used for the KEXINIT cookie and AES-CTR packet padding
— both cosmetic/anti-fingerprinting, neither secret.

### 5. No cap on buffered bytes — a memory-exhaustion DoS

Nothing bounded how large `Connection::input_buffer` could grow.
`take_*_packet` correctly wait for `total_needed` bytes before parsing, but
`total_needed` is derived from an attacker-supplied `packet_len` — a peer
(or a MITM) that claims an enormous `packet_len` and then trickles bytes (or
never finishes) makes the client buffer without limit, chasing a "packet"
that may never complete.

**Fix:** `MAX_INPUT_BUFFER` (1 MiB — generous over the 16 KiB `MAX_PACKET`
this client ever advertises, while still bounding the worst case) is checked
after every socket read, both in the blocking handshake path
(`recv_packet`, `protocol.rs:439`) and the non-blocking interactive pump
(`protocol.rs:628`). Exceeding it disconnects with a clear error rather than
continuing to grow.

### 6. Unbounded pre-version-line loop

RFC 4253 §4.2 allows a server to send banner lines before its actual
`SSH-2.0-...` version line, and `read_version_line` printed and kept reading
any non-`SSH-` line forever. A broken or hostile peer could hold the
handshake open indefinitely by never sending a line that starts with
`SSH-`, each individual line already bounded (1024 bytes) but the *count* of
lines wasn't.

**Fix:** `MAX_BANNER_LINES = 100` (`protocol.rs:769`, checked at `:796`);
exceeding it aborts with an error instead of looping forever.

### 7. Ignored `Result`s around identity/`known_hosts` persistence

`generate_and_save` (new identity key) and `add_known_host` (TOFU
acceptance) both discarded the `Result` from `mkdir_p`/file writes. A
silent failure here isn't cosmetic:

- If saving a freshly generated identity key silently fails, the function
  still returns it as if persisted — the *next* invocation generates a
  **different** key with no indication why a server that accepted the
  previous connection suddenly rejects this one.
- If recording a TOFU-accepted host key silently fails, the next connection
  re-runs the TOFU prompt with no memory of the previous acceptance — which
  reads exactly like "the host key changed" to anyone not watching closely.

**Fix:** both now check the `Result` and `eprintln!` a specific warning
naming the path and errno on failure (`client/keys.rs`), rather than
proceeding as if nothing happened. (A background-task-only
`SSH_MSG_REQUEST_FAILURE` reply and a best-effort `CHANNEL_CLOSE` on the way
out remain deliberately unchecked — both are documented inline as
low-stakes: a broken connection there surfaces on the very next
read/write in the same loop, which *does* propagate.)

### 8. `disable_key_verification` is now a compile-time opt-in, not just a config flag

Pre-existing in `sshd`, found while auditing the client's auth path for
comparison: `sshd.conf`'s `disable_key_verification = true` bypasses
`publickey` auth entirely and accepts any client. That's a legitimate
dev/demo knob, but a config-file boolean is one typo or one copy-pasted dev
config away from shipping with authentication effectively off, in a binary
nobody built with that in mind.

**Fix:** gated behind a new Cargo feature,
`insecure-disable-key-verification` (off by default). Without it, the config
flag still parses (no hard error on a config file that carries it) but is
**ignored, loudly** — `auth.rs` logs a warning naming exactly why — instead
of silently taking effect. See `Cargo.toml` and `src/auth.rs`.

### 9. Missing channel-recipient validation

`CHANNEL_DATA`/`CHANNEL_EXTENDED_DATA`/`CHANNEL_WINDOW_ADJUST`/
`CHANNEL_REQUEST`/`CHANNEL_CLOSE` handling in the interactive pump read and
acted on the payload without checking the `recipient channel` field against
`LOCAL_CHANNEL`. Since this client only ever opens one channel, exploiting a
mismatch would require a peer already inside the encrypted session
misaddressing its own messages — low severity — but it's a correctness gap
worth closing as defense-in-depth rather than assuming a well-behaved peer.
Now checked explicitly; a mismatched frame is skipped (`continue`s the
packet-drain loop) rather than acted on.

### 10. Key material wasn't zeroized on drop

Originally listed below as an accepted limitation; revisited and fixed.
`akuma-ssh-crypto`'s `zeroize` feature (zeroes `ed25519-dalek` key material
on drop) was off for both binaries via `sshd/Cargo.toml`'s
`default-features = false` on that dependency, alongside `fast`
(curve25519-dalek's precomputed basepoint table). Those two features don't
belong in the same bucket: `fast` is a pure speed/size trade with no
security content, but `zeroize` is exactly the property that keeps `sshd`'s
host key and `ssh`'s identity key / ephemeral KEX secret from lingering in
freed heap memory after use, readable by anything with later access to that
memory.

**Fix:** `sshd/Cargo.toml` now re-enables `zeroize` explicitly
(`features = ["zeroize"]`) while leaving `fast` off. Applies to both
binaries, since they share the one dependency declaration. `fast` stays a
deliberate opt-in, not because it's insecure but because signing/verifying
once per connection never gets close to paying back a 30 KB table.

## Reduced allocation churn (not a security finding, but part of the same pass)

`take_encrypted_packet`/`take_unencrypted_packet` originally reallocated and
copied the **entire remaining input buffer** on every single packet
consumed (`*input_buffer = input_buffer[n..].to_vec()`), and separately
copied the returned payload out of an already-owned buffer a second time.
For a long interactive session pushing many small packets (exactly the
`late.sh` use case — a TUI redrawing on every keystroke), this is real,
avoidable churn. Both functions now use `Vec::drain` to shift the
unconsumed tail in place (no reallocation) and reuse the already-owned
decrypted/consumed buffer for the returned payload instead of copying it
out again — one allocation per received packet instead of up to three.

## Accepted limitations (documented, not fixed)

These were considered and deliberately left as-is, with the reasoning
below — flagging them here is the point, not silently living with them.

- **No Terrapin (CVE-2023-48795) / "strict KEX" mitigation.** This client
  transparently discards `SSH_MSG_IGNORE`/`DEBUG`/`UNIMPLEMENTED` at any
  point in the handshake (RFC 4253 permits them anywhere), which is exactly
  the class of behavior the `kex-strict-c-v00@openssh.com` extension exists
  to lock down against a prefix-truncation attacker. A real fix means
  advertising and honoring that pseudo-algorithm (reject early-phase
  IGNORE/DEBUG/UNIMPLEMENTED once negotiated, reset sequence numbers to 0
  after `NEWKEYS`) — implementable, but enough independent moving parts
  (interacts with KEXINIT parsing, packet framing, and the NEWKEYS
  boundary all at once) that getting it subtly wrong under time pressure
  felt worse than being honest that it's missing. The exposure is narrower
  than for a general-purpose client, though: no extension negotiation, no
  rekeying, and exactly one hard-coded algorithm suite means there's very
  little for a Terrapin-style attack to actually downgrade.
- **`SimpleRng`'s 64-bit state, generally.** Fixed at the two call sites
  that generate real key material (#4 above); still used for the KEXINIT
  cookie and CTR padding bytes, both non-secret. The underlying generator
  lives in the shared `akuma-ssh-crypto` crate (also used by `sshd`) and
  replacing it with a real CSPRNG is a crate-level change affecting the
  server too — out of scope for a client-focused pass.
- **No file-permission enforcement (0600) on identity keys/`known_hosts`.**
  Real `ssh` refuses a world-readable private key. Akuma's `open()` syscall
  wrapper has no mode argument, and this codebase shows no evidence of a
  local multi-user/permission-boundary threat model (root-only,
  single-tenant) — so there's no local attacker this would actually be
  defending against here. Noted rather than silently assumed away.
- **DNS resolution has no built-in validation.** `resolve_target` trusts
  whatever `resolve_host()` returns. This is true of essentially every SSH
  client on every OS; what makes it acceptable here specifically is that a
  spoofed resolution answer just becomes "connecting to the wrong host",
  which the host-key verification + TOFU `known_hosts` (mismatch = hard
  refuse) is exactly the mechanism designed to catch.
