# Hand-rolled parsers (non-JSON): an inventory

Audit of `src/`, `crates/`, and `userspace/*/src` for hand-rolled or duplicated
parsing code, excluding JSON (already covered in
`docs/archive/TRIM_FAT_HAND_ROLLED_JSON.md`) and excluding
`userspace/nca/native-cli-ai/**` (vendored submodule, out of scope). Read as-is
at commit `a19baa1`; nothing here was edited.

Searched for: `fn parse*`, `fn from_str`, `fn decode*`, `split`/`splitn`/
`split_whitespace`, `from_str_radix`, `find_headers_end`, `BASE64_ALPHABET`,
and SSH wire-format helpers (`read_string`/`write_string`/`parse_key_blob`).

## Resolution log

- **sshd crypto dedup — DONE (commit `7e3f5b2`, same day as the audit).** The
  critical finding (§ "What actually breaks" item 1: sshd's local
  `parse_key_blob` accepted Ed25519 identity / low-order points the shared
  crypto crate rejects) was fixed immediately. `sshd/src/auth.rs` now
  re-exports `akuma_ssh_crypto::auth::AuthResult` and delegates the entire
  `publickey` verification path to `akuma_ssh_crypto::auth::handle_publickey_auth`;
  `sshd/src/keys.rs` re-exports `encode_public_key_ssh`/`parse_public_key_ssh`
  from `akuma_ssh_crypto::keys`. What remains in sshd is genuinely sshd-specific:
  the `handle_userauth_request` SSH-envelope dispatcher (async, reads
  user/service/method, honors `disable_key_verification`), and the async fs
  glue (`load_or_generate_host_key`, `load_authorized_keys`, `HOST_KEY`
  spinlock). Verified end-to-end: host-built sshd for `aarch64-unknown-none`,
  injected into a devbox-smoltcp QEMU instance with `disable_key_verification=false`
  + a real ed25519 key in `authorized_keys`, `ssh -o PreferredAuthentications=publickey`
  connected and ran commands. Crypto-crate host tests (`cargo test -p
  akuma-ssh-crypto`) — including `parse_key_blob_rejects_zero_key` and
  `parse_key_blob_rejects_low_order_points` — pass (30/30). `sshd` wire.rs
  host tests pass (11/11).
- **HTTP(S) parsing cluster — meow's sites DONE, 2026-08-12 (branch `box-run`).**
  `libakuma-tls::http` had the canonical `parse_url`/`ParsedUrl` and
  `parse_status_line` all along, but both were module-private — only
  `find_headers_end` was actually `pub`, so nothing outside the crate could
  reuse them despite the doc's consolidation table assuming they were
  reusable. Made `parse_url`/`ParsedUrl`/`parse_status_line` `pub` and
  re-exported them from `libakuma_tls`'s crate root alongside
  `find_headers_end`; changed `parse_status_line`'s signature from
  `(headers: &str) -> u16` (0 = unparsed) to `(headers: &str) ->
  Option<u16>`, updating its two existing internal callers
  (`download_redirects_{tcp,tls}`) to `.unwrap_or(0)`.
  `userspace/meow/src/tools/net.rs` — deleted its local `ParsedUrl`/
  `parse_http_url`, `find_headers_end`, and the status-line half of
  `parse_http_response`, all now calling the shared functions; also fixed a
  real bug found while touching this code: the plain-HTTP GET request
  builder used a multi-line string literal *without* backslash
  continuations, so it sent literal newlines and leading whitespace instead
  of `\r\n` between header lines — a malformed request most servers would
  either reject or misparse. `userspace/meow/src/api/client.rs` — deleted
  its own third copy of `find_headers_end` (named `find_header_end`, and
  with different semantics: returned the offset *before* `\r\n\r\n` instead
  of past it, requiring a `+ 4` at every call site) and its crude
  `header_str.contains(" 200 ")` status check, both replaced with the shared
  `find_headers_end`/`parse_status_line`. Bonus finds while in
  `libakuma-tls::http` itself, beyond what the original audit caught:
  `HttpStream::process_pending_data` and `HttpStreamTls::process_pending_data`
  each had their *own* third and fourth inline copies of the status-line
  scan (identical to each other and to the standalone `parse_status_line`
  the file already had) — collapsed both to call the shared function.
  Verified end-to-end in a devbox-smoltcp QEMU instance against a real
  OpenAI-compatible server (mlx-server, plain HTTP not HTTPS — so this
  exercises `client.rs`'s non-TLS streaming path directly): a tool-calling
  chat turn streamed correctly, and `HttpFetch` correctly fetched a file
  from a second plain-HTTP test server, both round-trips text-exact.
  `scratch`'s and `httpd`'s sites (URL/status-line/header duplicates,
  `find_crlf` × 2, Content-Length/Location scanners × 3) are unchanged —
  out of scope for this pass, which was scoped to meow.
- **IPv4 parsers × 2 — DONE, 2026-08-12.** `libakuma::SocketAddrV4` gained a
  `parse_ip(s: &str) -> Option<[u8; 4]>` associated fn (`splitn(5, '.')` +
  reject-a-5th-octet, same shape `meow/linux_net.rs::parse_ipv4` already had
  right), used internally by `SocketAddrV4::parse` for the IP half. Fixes the
  silent-5th-octet-acceptance bug documented in "What actually breaks" item 3
  as a side effect — real call sites (`libakuma::net`'s `resolve`-address
  path, `meow/tools/pretend_shell.rs`) now inherit the fix. `libakuma::parse_u8`/
  `parse_u16` (the port half) deleted, replaced with `str::parse` — folds into
  the decimal-integer item below. `meow/linux_net.rs::parse_ipv4` deleted,
  now calls `SocketAddrV4::parse_ip` at its three call sites (nameserver
  parsing, `/etc/hosts`, and the resolve() IP-literal fast path).
- **Decimal integer parsers × 5 — DONE, 2026-08-12.** All five hand-rolled
  `parse_u32`/`parse_u64`/`parse_u8`/`parse_u16` deleted (`herd`, `hello`,
  `stackstress`, `libakuma`'s pair) — every call site now does
  `s.parse::<uN>().ok()` directly, `core`'s own `FromStr`. One real behavior
  change worth flagging: the hand-rolled versions treated an **empty string**
  as `Some(0)` (the digit loop just never ran); `str::parse` treats `""` as
  `Err`. Every call site already does `.unwrap_or(DEFAULT)`, so this changes
  "empty CLI arg silently becomes 0" into "empty CLI arg falls back to the
  documented default" (`herd`'s `start_delay`/`core` keys default to 0
  either way — no change there; `stackstress`'s iteration/mode and `hello`'s
  outputs/delay defaults are non-zero, so this is a real if obscure behavior
  change for a deliberately-empty argument, arguably a correctness
  improvement — an empty string isn't a valid count).
- **HTTP(S) parsing cluster — `scratch` DONE, 2026-08-12 (branch `box-run`).**
  `scratch/src/http.rs::Url::parse_internal` now calls `libakuma_tls::parse_url`
  for the scheme/host/port/path split, keeping only its `.git`-suffix
  normalization on top — its hand-rolled copy deleted. `scratch/http.rs`'s
  and `scratch/stream.rs`'s duplicate `parse_status_line`s both deleted,
  replaced with `libakuma_tls::parse_status_line` (called with the whole
  header block — it already does `.lines().next()` internally, so this is a
  drop-in replacement for both "single already-extracted line" and "full
  block" callers). `find_crlf` (`http.rs`) and `find_crlf_slice` (`stream.rs`)
  collapsed to one `pub(crate) fn find_crlf` in `http.rs`, used by both.
  `http.rs::parse_response` and `stream.rs::parse_headers` — near-identical
  per the original audit — factored into one shared `pub(crate) fn
  parse_header_block(header_str) -> Result<(u16, Vec<(String,String)>,
  bool)>` in `http.rs` that both files call; `parse_response` layers its
  full-buffer `decode_chunked` on top, `stream.rs` layers its incremental
  `ChunkedState` state machine on top — the streaming-vs-buffered chunked
  *decoding* genuinely differs and stays separate, only the *header* parsing
  was actually duplicated. Verified via `cargo check`/`clippy` (workspace-wide,
  clean) — **not** re-verified live in QEMU against a real git remote this
  round (time-boxed); worth an `acceptance/`-style clone/fetch smoke test
  before trusting this in production if it hasn't had one since.
- **`httpd/src/main.rs:extract_post_body` — DONE, 2026-08-12.** Collapsed its
  two near-identical `Content-Length:` / `content-length:` branches (checking
  the same header twice for casing) into one `eq_ignore_ascii_case` check.
  Scoped narrowly: this is httpd parsing a *request* it received as a server,
  a different direction from `libakuma-tls::http`'s Content-Length scanners
  (parsing a *response* as a client) — left those alone, matching the
  original audit's own "Maybe / low priority" call on that trio.
- Config-file skeleton × 4 remains open (explicitly deferred by the original
  audit — shared part is ~5 lines, not worth a generic helper today).


## Summary

- **One security-relevant partial dedup.** `userspace/sshd/src/{auth.rs,keys.rs}`
  duplicate `akuma-ssh-crypto`'s `parse_key_blob`/`parse_signature_blob`/
  `base64_*`/`encode_public_key_ssh`/`parse_public_key_ssh` nearly verbatim —
  and the sshd copies are weaker (see § "What actually breaks"). The dedup
  pattern was already applied to `sshd/src/crypto.rs` (24 lines, just re-exports
  `akuma_ssh_crypto::crypto::*`) and `sshd/src/wire.rs`, but `auth.rs`/`keys.rs`
  were missed.
- **HTTP(S) parsing is the biggest cluster by raw LOC** — ~6 sites, ~3 of them
  near-verbatim copies of the same URL/status-line/header parser. `scratch`,
  `meow/tools/net`, and `libakuma-tls` all independently parse the same
  `http(s)://host[:port]/path` shape; `scratch/http.rs` and `scratch/stream.rs`
  each carry their own byte-identical `parse_status_line` and `find_crlf`;
  `meow/tools/net.rs` reimplements `libakuma_tls::find_headers_end` despite
  that function being a public export that `scratch` already imports correctly.
- **Integer parsing: 5 copies of "checked_mul/checked_add over ASCII digits".**
  Trivial code, but it's already in `core` as `str::parse::<uN>()`, and the
  five copies have already drifted (two use `is_ascii_digit`, three use
  `>= b'0' && <= b'9'`).
- **IPv4 octet parsing: 2 copies** (`meow/linux_net.rs::parse_ipv4` and
  `libakuma::SocketAddrV4::parse`), doing the same split-on-`.`-take-4-octets.
- **Key=value config-file parsing skeleton is duplicated 4x** (`herd`,
  `meow`, `sshd`, `scratch`) but the shared part is ~5 lines and the per-key
  dispatch + section semantics differ enough that this is borderline — flagged
  as low priority, not a real bug source today.
- **Not duplicated** (checked, single-purpose): `tar/src/format.rs::parse_octal`,
  `scratch/src/pktline.rs::parse_hex_u16`, the `scratch/src/pack.rs`
  delta/copy instruction parsers, `box/src/boxes.rs::parse_id` (handles
  hex+decimal), `meow/src/util.rs::parse_query_param`,
  `httpd/src/main.rs::parse_path_and_query`, the ELF header parsers in
  `crates/akuma-exec/src/elf/`, and the ext2 directory parser in
  `crates/akuma-ext2/src/ext2.rs`.

## Sites

### `userspace/sshd/src/{auth.rs, keys.rs}` — forked SSH crypto helpers — **RESOLVED**

The dedup pattern was applied to `crypto.rs` (24 lines re-exporting
`akuma_ssh_crypto::crypto::*`) and `wire.rs` (`use akuma_ssh_crypto::crypto::…`)
but stopped there. Still locally defined in sshd:

- `sshd/src/auth.rs:19-21` redeclares `SSH_MSG_USERAUTH_*` constants (same
  values as `akuma_ssh_crypto::auth::{SSH_MSG_USERAUTH_FAILURE, _SUCCESS,
  _PK_OK}`).
- `sshd/src/auth.rs:208 parse_key_blob` — copy of
  `akuma_ssh_crypto::auth::parse_key_blob` (`auth.rs:41`), **without the
  low-order-point / identity-point rejection** the crypto crate added at
  `auth.rs:56` (`is_low_order_point`, `LOW_ORDER_POINTS` at `auth.rs:66`).
- `sshd/src/auth.rs:225 parse_signature_blob` — copy of
  `akuma_ssh_crypto::auth::parse_signature_blob` (`auth.rs:90`).
- `sshd/src/auth.rs:107 handle_publickey_auth` — `async` fork of
  `akuma_ssh_crypto::auth::handle_publickey_auth` (`auth.rs:167`); structurally
  the same function, calls sshd's weaker local `parse_key_blob` at line 145 on
  the client-supplied wire blob.
- `sshd/src/auth.rs:242 build_signed_data`, `:build_success_response`,
  `:build_failure_response`, `:build_pk_ok_response` — copies of the
  response builders in `akuma_ssh_crypto::auth` (lines 108, 144, 149, 157).
- `sshd/src/keys.rs:28-113 BASE64_ALPHABET`, `base64_encode`, `base64_decode`
  — byte-identical (modulo `as u32` vs `u32::from`) to
  `akuma_ssh_crypto::keys.rs:17-104`.
- `sshd/src/keys.rs:138 encode_public_key_ssh`, `:154 parse_public_key_ssh`
  — copies of `akuma_ssh_crypto::keys.rs::{114, 127}`. Note: **neither** the
  sshd copy nor the crypto-crate copy of `parse_public_key_ssh` does the
  low-order-point check (only `parse_key_blob` in the crypto crate does); see
  § "What actually breaks" item 2.

The host-testable crate `akuma-ssh-crypto` exists *specifically* so the kernel
(`crates/akuma-ssh`) and userspace sshd share one implementation —
`sshd/src/crypto.rs`'s doc comment says so outright. These copies defeat that
for the auth/keys half.

### HTTP(S) parsing cluster — `libakuma-tls`, `scratch`, `meow/tools/net`, `httpd`

**URL parsing, 3 copies** of strip-scheme → split host_port from path on first
`/` → split host:port on `rfind(':')`:

- `userspace/libakuma-tls/src/http.rs:321 parse_url` (canonical; returns
  borrowed `ParsedUrl`).
- `userspace/scratch/src/http.rs:36 Url::parse_internal` (returns owned `Url`;
  adds a `.git` suffix normalization in strict mode but otherwise identical
  control flow).
- `userspace/meow/src/tools/net.rs:127 parse_http_url` (copy of #1, identical
  except for the struct name).

**HTTP status-line parsing, 4 copies** of `split_whitespace().nth(1)?.parse::<u16>()`:

- `userspace/scratch/src/http.rs:489 parse_status_line`
- `userspace/scratch/src/stream.rs:340 parse_status_line` (byte-identical to
  the above — same crate, two files, two copies)
- `userspace/libakuma-tls/src/http.rs:1034 parse_status_line`
- inlined into `userspace/meow/src/tools/net.rs:155 parse_http_response`

**HTTP response/header parsing, 2 copies** (find header boundary, walk lines,
split on `:`, trim, detect `Transfer-Encoding: chunked` case-insensitively):

- `userspace/scratch/src/http.rs:378 parse_response`
- `userspace/scratch/src/stream.rs:306 parse_headers`

Both push `(String, String)` header tuples into a `Vec`; same Transfer-Encoding
check, same loop shape.

**`find_crlf`, 2 copies** — both `(0..data.len().saturating_sub(1)).find(|&i| data[i] == b'\r' && data[i + 1] == b'\n')`:

- `userspace/scratch/src/http.rs:483 find_crlf`
- `userspace/scratch/src/stream.rs:300 find_crlf_slice`

**`find_headers_end` reimplemented** in
`userspace/meow/src/tools/net.rs:167` — even though the canonical version at
`userspace/libakuma-tls/src/http.rs:698` is exported by
`libakuma_tls::find_headers_end` (`lib.rs:27`) and `scratch/src/{http,stream}.rs`
already import it correctly (`use libakuma_tls::{find_headers_end, …}`).
`meow/tools/net.rs` reimplements it instead of `use`-ing it.

**Content-Length / Location header extraction, 3 copies** in
`userspace/libakuma-tls/src/http.rs` alone — `parse_content_length` (`:551`),
`parse_cl_header` (`:1043`), `extract_location_header` (`:1023`) — all share
the same "take N bytes, lowercase, prefix-match" shape. (Same file, three
near-identical scanners; the N=9 / N=16 byte-budget-and-lowercase trick is
duplicated rather than factored into one `header_matches(name, line)` helper.)
Plus `httpd/src/main.rs:212` and `:216` each open-code their own
`Content-Length` lookup inline.

### Decimal integer parsers — 5 copies

All do `checked_mul(10)?.checked_add(digit)?` over ASCII digits, returning
`Option<uN>`:

- `userspace/herd/src/main.rs:659 parse_u64`, `:670 parse_u32` (via
  `parse_u64` + `u32::try_from`). Uses `c.is_ascii_digit()`.
- `userspace/hello/src/main.rs:98 parse_u32`, `:110 parse_u64`. Uses
  `c >= b'0' && c <= b'9'`.
- `userspace/stackstress/src/main.rs:128 parse_u32`. Uses
  `c >= b'0' && c <= b'9'`.
- `userspace/libakuma/src/lib.rs:702 parse_u8`, `:713 parse_u16`. Uses
  `c.is_ascii_digit()`. Also consumed by `SocketAddrV4::parse` (`:682`) for
  the port and the four IP octets.

The two stylistic variants are semantically identical — divergence for no
reason. Every one of these replaces what `str::parse::<u8/u16/u32/u64>()` from
`core` already does (and returns `Option` if you go through `Result::ok()`).

### IPv4 octet parsing — 2 copies

- `userspace/meow/src/linux_net.rs:19 parse_ipv4` — uses
  `parts.next()?.parse::<u8>().ok()?` (i.e. delegates to `core`), plus
  `splitn(5, '.')` so a 5th octet is rejected.
- `userspace/libakuma/src/lib.rs:682 SocketAddrV4::parse` — uses its own
  hand-rolled `parse_u8` (see above), via `split('.')` with no 5th-octet
  rejection (a 5-octet string silently takes the first 4 and ignores the rest).

### Config file parsing skeleton — 4 copies, low priority

The shared shape is "trim line, skip blank/`#`-comment, `split_once('=')` or
`find('=')`, trim key + value, dispatch on key":

- `userspace/herd/src/main.rs:589 parse_service_config` (flat key=value,
  typed fields).
- `userspace/meow/src/config.rs:264 Config::parse` (adds `[provider:name]`
  section headers).
- `userspace/sshd/src/config.rs:83 SshdConfig::parse_line` (flat key=value,
  lowercases the key).
- `userspace/scratch/src/config.rs:93 GitConfig::parse` (Git's
  `[section "subsection"]` INI format, the most structurally different of the
  four).

The shared skeleton is ~5 lines; the per-key dispatch and section semantics
differ enough that consolidating saves little today. Worth a tiny
`parse_kv_lines(content) -> impl Iterator<Item = (&str, &str)>` helper only if
a 5th config file shows up.

## Consolidation

| Site | Move / consolidate? | Cost / why |
|---|---|---|
| `sshd/src/auth.rs` + `sshd/src/keys.rs` duplicates of `akuma_ssh_crypto::{auth,keys}` | **DONE — commit `7e3f5b2`.** | The pattern was already established in the same crate (`sshd/src/crypto.rs` and `sshd/src/wire.rs` re-export `akuma_ssh_crypto`). Fix was mechanical: `pub use akuma_ssh_crypto::auth::AuthResult`, delegate `publickey` verification to `akuma_ssh_crypto::auth::handle_publickey_auth`, re-export `encode_public_key_ssh`/`parse_public_key_ssh` from `akuma_ssh_crypto::keys`. The crypto crate's `handle_publickey_auth` is sync; sshd's `handle_userauth_request` stayed `async` because it `await`s `load_authorized_keys()` — it loads the keys first, then calls the sync helper with `&authorized_keys` as a parameter. Fixed the low-order-point regression for free. QEMU-verified. |
| URL parsers × 3 | **`meow` DONE — 2026-08-12; `scratch` open.** | `libakuma-tls::http::{ParsedUrl, parse_url}` made `pub` and re-exported from the crate root (they existed all along but were module-private, so nothing outside the crate could actually reuse them). `meow/tools/net.rs` now calls `libakuma_tls::parse_url`, its local copy deleted. `scratch/http.rs::Url::parse_internal` still has its own copy (keeps its `.git`-suffix logic on top) — not touched. |
| `parse_status_line` × 4 (+ 2 more found in-file, see "What actually breaks") | **`meow` + `libakuma-tls`'s own internal dupes DONE — 2026-08-12; `scratch` open.** | `libakuma-tls::http::parse_status_line` made `pub` (signature changed `u16` → `Option<u16>`) and re-exported. `meow/tools/net.rs` and `meow/api/client.rs` (a 5th site the original audit missed — see below) both now call it. `HttpStream`/`HttpStreamTls::process_pending_data` in `libakuma-tls` itself also had their own inline copies (2 more the audit missed) — collapsed to call the same function. `scratch/http.rs` and `scratch/stream.rs`'s copies untouched. |
| `parse_response` / `parse_headers` × 2 (both in `scratch`) | **Open — not touched.** | Same crate, same file shape, byte-identical helpers (`parse_status_line`, `find_crlf`). One `parse_http_response(data) -> Result<{status, headers, body}>` in `scratch` (or move up to `libakuma-tls`); both call sites use it. |
| `find_crlf` × 2 (both in `scratch`) | **Open — not touched.** | Same crate, same one-liner. |
| `meow/tools/net.rs::find_headers_end` (+ `meow/api/client.rs`'s `find_header_end`, a 5th site the audit missed) | **DONE — 2026-08-12.** | Both now `use libakuma_tls::find_headers_end;` like scratch already did. `client.rs`'s copy was also a *different* function despite the near-identical name: it returned the offset *before* `\r\n\r\n` (every call site did `+ 4`), where the canonical one returns the offset *after* — worth checking call-site math when consolidating a "same name, different semantics" duplicate like this one. |
| `libakuma-tls::http` Content-Length / Location scanners × 3 | **Maybe.** | One `header_line_matches(name: &[u8], line: &str) -> bool` helper replaces the three `take-N-bytes-and-lowercase` blocks. Low priority — all three are in the same file and already local to one consumer. |
| Decimal integer parsers × 5 | **DONE — 2026-08-12.** | Deleted all five; every call site now does `s.parse::<uN>().ok()` directly. |
| IPv4 parsers × 2 | **DONE — 2026-08-12.** | Added `SocketAddrV4::parse_ip(s) -> Option<[u8; 4]>` (4-octet, reject 5th); `SocketAddrV4::parse` and `meow/linux_net.rs` (3 call sites) both use it now. Fixed `SocketAddrV4::parse`'s silent 5th-octet acceptance as a side effect. |
| Config-file skeleton × 4 | **No / defer.** | Shared part is ~5 lines; per-file dispatch and section semantics differ enough that a generic helper saves little. Revisit if a 5th config format appears. |
| Anything in `src/` or `crates/` | **N/A — nothing to move.** | Kernel and host-testable crates have no hand-rolled parsers in the categories audited (URL, HTTP, integer, SSH wire, IP, base64, config). ELF/ext2/pack scanners are purpose-built and not duplicated. None of these sites interact with the `safe_print!`/no-allocation-on-console-paths rule from `CLAUDE.md` — all of them are `userspace/*` binaries already using `alloc`. |

## What actually breaks

Ranked by severity, most severe first:

1. **`sshd/src/auth.rs:208 parse_key_blob` accepts Ed25519 identity / low-order
   points that `akuma_ssh_crypto::auth::parse_key_blob` rejects — RESOLVED.**
   This is the client-supplied wire key blob (read at `auth.rs:127-130`, passed
   to `parse_key_blob` at `:145`) — attacker-controlled. The crypto crate added
   `is_low_order_point`/`LOW_ORDER_POINTS` (`akuma-ssh-crypto/src/auth.rs:56,
   :66`) explicitly because relying on `ed25519_dalek::VerifyingKey::from_bytes`
   to reject small-order points is backend-version-fragile; the comment at
   `auth.rs:53-55` says so outright ("could allow signature forgery depending
   on the Ed25519 backend's verification strictness"). Whether the current
   `ed25519_dalek` version rejects them at `from_bytes` is irrelevant — sshd's
   copy was one backend bump away from accepting a degenerate key, in a path
   that decides whether to authorize an SSH session. **Fix (commit `7e3f5b2`):
   deleted sshd's local `parse_key_blob`/`parse_signature_blob`/
   `build_signed_data`/`build_*_response`/`handle_publickey_auth` and
   delegated to `akuma_ssh_crypto::auth::handle_publickey_auth`, which uses
   the canonical low-order-point-rejecting `parse_key_blob`.** Verified with
   QEMU (see Resolution log above).

2. **Neither `parse_public_key_ssh` (sshd *or* crypto crate) does the
   low-order-point check** — only `parse_key_blob` does. The authorized_keys
   file is root-controlled, so direct attacker control of `parse_public_key_ssh`
   input is lower-risk than `parse_key_blob`, but the *semantic* gap remains: a
   degenerate key written to `/etc/sshd/authorized_keys` (by mistake, by a
   future tool, by anyone who gets root) is silently accepted as a valid
   authorized key. Fix: add the same `is_low_order_point` guard to
   `akuma_ssh_crypto::keys::parse_public_key_ssh` when consolidating; sshd's
   copy disappears for free.

3. **`SocketAddrV4::parse` (`libakuma/src/lib.rs:682`) silently accepts a
   5-octet IPv4 string.** It uses `ip_str.split('.')` and writes exactly four
   octets into a fixed `[u8; 4]`, ignoring any trailing components —
   `"1.2.3.4.5"` parses as `1.2.3.4` with no error. `meow/linux_net.rs::parse_ipv4`
   gets this right (`splitn(5, '.')` + `parts.next().is_some()` check). Low
   impact today (callers pass trusted config strings), but a real divergence
   between the two copies, and exactly the kind of "trusted input becomes
   untrusted later" gap that bites in a refactor.

4. **Five copies of decimal-integer parsing have already drifted.** Two
   (`herd`, `libakuma`) use `c.is_ascii_digit()`; three (`hello`, `stackstress`,
   and implicitly `libakuma::parse_u8`'s sibling style) use `c >= b'0' && c <= b'9'`.
   Semantically identical today, but the divergence is the smell that says
   "these were written by copy-paste and not maintained together." The next
   bug — accepting a leading `+`, rejecting whitespace, handling overflow
   differently — will land in one copy and not the others. Replacing all five
   with `str::parse::<uN>()` (or one shared helper) removes the drift surface
   entirely.

5. **`scratch/src/http.rs::parse_response` and `scratch/src/stream.rs::parse_headers`
   are structurally identical HTTP response parsers in the same crate** — same
   `find_headers_end` import, same status-line parser, same header-tuple
   accumulation, same Transfer-Encoding detection. Any bug fix (a new
   transfer-encoding, a header-folding case, a UTF-8 BOM) has to be applied
   twice and won't be. No known live bug today, but this is the textbook
   "duplicate code rots" shape.

6. **`meow/tools/net.rs::find_headers_end` is a from-scratch reimplementation
   of `libakuma_tls::find_headers_end`** in the same workspace, with the
   canonical version one `use` statement away. No known bug in the reimpl
   (it's a simple byte scan), but its existence means a future fix to header
   termination detection (e.g. tolerating a lone LF for broken servers) has to
   land in two places, and only one of them is covered by
   `box/src/tests.rs::test_http_find_headers_end`. — **RESOLVED 2026-08-12.**
7. **`meow/tools/net.rs::tool_http_fetch`'s plain-HTTP GET request was
   malformed — found while consolidating the URL/status-line parsing above,
   not by the original audit.** The request template was a multi-line Rust
   string literal *without* the `\` line-continuations every other request
   builder in the tree uses (`libakuma-tls::http::build_http_request` and
   friends, `meow/api/client.rs`'s own POST builders) — so instead of
   `\r\n`-joined header lines it sent literal `\n` plus the source
   indentation as leading whitespace on each "header" line: `GET /path
   HTTP/1.0\n\n             Host: ...\n\n             User-Agent: ...`. A
   strict HTTP/1.0 parser would either reject this outright or, since a
   blank line ends the header block, could interpret the request as
   header-less (`GET /path HTTP/1.0` followed immediately by an empty line)
   and silently drop `Host`/`User-Agent`/`Connection`. **Fixed** alongside
   the `parse_url`/`find_headers_end` consolidation in the same function.
   QEMU-verified: `HttpFetch` correctly round-tripped a real file from a
   plain-HTTP test server after the fix.

## Background

- `docs/archive/TRIM_FAT_HAND_ROLLED_JSON.md` — sibling audit, same format,
  same scope rules. Established the per-site / consolidation-table / what-
  actually-breaks shape this doc follows.
- `docs/archive/TRIM_FAT_SSHD.md` — prior art on sshd-specific trimming; the
  partial dedup of sshd's crypto module (re-export instead of fork) was done
  there, this doc is the follow-up noting it didn't reach `auth.rs`/`keys.rs`.
- `docs/archive/BUILTIN_SSH_REMOVAL.md` — context for why all SSH lives in
  `userspace/sshd` + `userspace/akuma-ssh-crypto` now (kernel SSH server
  removed 2026-08-10), which is why the crypto crate exists at all as a
  shared host-testable boundary.
- `docs/reference/subsystems/console.md` § "Printing rules" — noted as
  inapplicable: every site in this audit is a `userspace/*` binary already
  using `alloc::String`/`Vec`; none are kernel/console paths.
- `CLAUDE.md` § "Testing" — `cargo test -p akuma-ssh-crypto --target $HOST`
  is the host-test path the crypto-crate consolidation would route through;
  the sshd-local copies currently have no host-test coverage at all (the
  `sshd` crate's only host-testable half is `wire.rs`, per CLAUDE.md).
