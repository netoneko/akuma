# Trimming userspace sshd's binary size

Follow-up investigation after `docs/archive/SSHD_IMPLEMENTATION_PLAN.md` turned
out to rest on a false premise. This doc records what's actually true and what
was done about it (branch `rewrite-sshd-libc`).

## The original plan was wrong

`SSHD_IMPLEMENTATION_PLAN.md` proposed dropping `libakuma` for the crates.io
`libc` crate plus hand-rolled `extern "C"` syscall wrappers, to save the ~17 KB
libakuma was assumed to cost.

`userspace/sshd` targets `aarch64-unknown-none` (bare metal, no OS), not a
hosted musl/glibc target. On that target:

- `libakuma` isn't a thin syscall shim — it supplies the `#[global_allocator]`,
  the `#[panic_handler]`, and the process entry glue. There is no other source
  for those on `aarch64-unknown-none`.
- The upstream `libc` crate provides FFI *signatures* for a real C library to
  link against on hosted targets (`*-linux-musl`, `*-linux-gnu`, …). It has no
  meaningful bindings for `target_os = "none"` — there's no libc to bind to.

Confirmed empirically: a prior session had already half-applied step 1 of the
plan to `userspace/sshd/Cargo.toml` (dropped the `akuma` feature, added
`libc = "0.2"`) without touching any source file. `cargo check -p sshd`
immediately failed with:

```
error: no global memory allocator found but one is required
error: `#[panic_handler]` function required, but not found
error[E0432]: unresolved import `libakuma`   (config.rs, main.rs, protocol.rs, auth.rs)
```

That Cargo.toml edit was reverted — `libakuma` stays.

Akuma *does* have a real path to run genuine musl/libc binaries: it implements
the Linux AArch64 syscall ABI well enough to run unmodified musl-static ELF
binaries (`docs/reference/abi/musl.md`) — that's how apk/Alpine binaries,
rustc, git etc. run on it. `userspace/meow`'s `linux-net` feature already
cross-builds for `aarch64-unknown-linux-musl` as an alternative build path.
Porting `sshd` to that target/toolchain (real musl libc, real allocator, real
panic infra, real `libc` crate bindings) is a legitimate *alternative* project
to what's described below, but a much bigger one — it means giving up the
bare-metal `no_std` build entirely, cross-linking against a musl sysroot, and
losing today's build ergonomics (`build.sh --sshd-only`, one Cargo workspace).
Out of scope here; noted for whoever wants to chase it.

## What actually made sshd big: a shared crate default, not sshd itself

Baseline size (`aarch64-unknown-none`, `--release`, workspace's existing
`opt-level=z` + `lto=true` + `codegen-units=1` + `strip=true` +
`overflow-checks=false`):

```
152120 bytes total, 145792 bytes .text
```

Building with `--config profile.release.strip=false` and inspecting with
`llvm-nm --print-size --size-sort` (from the rustup toolchain — no extra
tooling needed) turned up a single 30 KB symbol:

```
0x7800 (30720 bytes)  ED25519_BASEPOINT_TABLE_INNER_DOC_HIDDEN
```

That's `curve25519-dalek`'s precomputed ed25519 basepoint-multiplication
table (`precomputed-tables` feature) — ~21% of the whole binary's `.text`,
there purely to make basepoint scalar multiplication fast. sshd only signs
once per connection (the host-key signature during key exchange); that speed
is not worth 30 KB.

Neither `sshd`'s own `ed25519-dalek` dependency line nor `x25519-dalek`'s
requested this. `cargo tree -e features -i curve25519-dalek` traced it to
**`crates/akuma-ssh-crypto/Cargo.toml`** — the host-testable SSH-2
wire/crypto crate shared between userspace `sshd` and the *in-kernel* SSH
server (`crates/akuma-ssh`, compiled into the kernel via the root
`Cargo.toml`). Its dependency line was:

```toml
ed25519-dalek = { version = "2", default-features = false, features = ["alloc", "zeroize", "fast"] }
```

`"fast"` and `"zeroize"` were **unconditional** — not gated behind any Cargo
feature of `akuma-ssh-crypto` itself — so every consumer, including userspace
`sshd`, paid for both regardless of whether it wanted them. `"fast"` is what
turns on `curve25519-dalek/precomputed-tables`.

This is exactly the kind of fat the original plan was trying to cut, just in
the wrong place — and one that can't be fixed from `sshd`'s own `Cargo.toml`
at all: Cargo feature unification is additive-only, so nothing `sshd` requests
can *remove* a feature something else in the graph unconditionally turns on.

## The fix

`crates/akuma-ssh-crypto/Cargo.toml` now gates both behind Cargo features,
**defaulted on** — so the in-kernel SSH server's build (root `Cargo.toml` →
`crates/akuma-ssh` → `akuma-ssh-crypto`, both still requesting default
features, unmodified) is byte-for-byte unaffected. The kernel was
intentionally left untouched throughout this work.

```toml
[features]
default = ["fast", "zeroize"]
fast = ["ed25519-dalek/fast"]
zeroize = ["ed25519-dalek/zeroize"]

[dependencies]
ed25519-dalek = { version = "2", default-features = false, features = ["alloc"] }
```

`userspace/sshd/Cargo.toml` opts out of both:

```toml
akuma-ssh-crypto = { path = "../../crates/akuma-ssh-crypto", default-features = false }
...
ed25519-dalek = { version = "2", default-features = false, features = ["alloc"] }
```

(sshd's own `ed25519-dalek` line also had `"zeroize"` dropped — same reasoning,
no direct `zeroize`/`Zeroize` usage anywhere in `userspace/sshd/src/`.)

## Result

```
                text     data   bss    total
before:       145792      64   768   152120
after:        107240      64   768   115256
saved:         38552       0     0    36864   (~24%)
```

Bigger than the ~30 KB table alone — dropping `zeroize` also removes the
`zeroize` crate's `Drop`/scrubbing glue and some `subtle`-adjacent code paths
that came along with it.

## Verification performed

- `cargo check --release --target aarch64-unknown-none` at the repo root
  (kernel + `akuma-ssh` + `akuma-ssh-crypto` with **default** features) —
  unaffected, confirms the kernel path still gets `fast`+`zeroize` unchanged.
- `cargo tree -e features -i ed25519-dalek` at the repo root — confirms
  `akuma`/`akuma-net`/`akuma-ssh` still resolve `akuma-ssh-crypto`'s `default`
  feature (`fast`+`zeroize` both present).
- Host unit tests, `akuma-ssh-crypto` with `--no-default-features` (i.e.
  sshd's exact feature set) — all 30 pass, including
  `auth_tests::verify_signature_valid`, confirming ed25519 verification is
  still correct without the precomputed table.
- Host unit tests, `sshd --lib --no-default-features` (the `wire.rs` suite) —
  all 11 pass.
- `cargo clippy` on both touched packages — `akuma-ssh-crypto` clean; `sshd`
  has 13 pre-existing warnings in code untouched by this change (not
  introduced here).
- Live boot (`cargo run --release`) + real SSH client connection over the
  actual key-exchange/auth/channel path (`ssh -p 2222 root@localhost 'echo …'`)
  — command executed and its output round-tripped correctly.

One live-test observation, **not investigated further and not attributed to
this change**: the OpenSSH client reported "Connection to localhost closed by
remote host" (exit 255) even though the remote command's output came through
correctly. This looks like a pre-existing exit-status/channel-teardown
behavior of this custom `sshd` (see `sshd_drain_fix` and the exit-status
wire-format code in `src/protocol.rs`/`src/wire.rs`) rather than anything
related to the crypto feature change — this diff never touched protocol
logic — but it wasn't cross-checked against the pre-change binary, so treat
that attribution as a hypothesis, not a confirmed fact.

## Ideas for further size reduction (not attempted)

Found while eyeballing `llvm-nm --size-sort` output, biggest-first, none
pursued this round:

- `alloc::str::to_lowercase` — 2712 bytes. Unicode case-folding tables pulled
  in by *something* doing a case-insensitive string comparison somewhere in
  the auth/config path; worth finding what and whether an ASCII-only compare
  would do.
- `core::char::methods::escape_debug_ext` — 1304 bytes. Likely reachable via a
  `#[derive(Debug)]` somewhere (e.g. `AuthResult` in `src/auth.rs`, which
  clippy already flags for an unused field) formatting a `char`/`str`.
- `sha2::sha512::compress512` (5004 bytes) / `compress256` (1832 bytes) — both
  compiled in because ed25519 uses SHA-512 internally and the SSH transport
  uses SHA-256 for HMAC/KEX. Legitimately needed; not a target.
- The AES `fixslice` software backend (`aes4soft`) tables — several hundred
  bytes each for `sub_bytes`/`aes128_encrypt`/`bitslice`. Also legitimately
  needed (packet encryption); only worth revisiting if a hardware-AES path
  ever becomes available on this target.

## The in-kernel SSH server is a candidate for removal, not just trimming

> **Outcome (2026-08-10, branch `trim-fat-sshd`).** This is what happened: the
> built-in server is now compiled out of every profile except `extreme-size`,
> behind `cfg(kernel_builtin_ssh)`. Measured 217 KB off the extreme image,
> 1.26 MB off default `--release`, and +308 KB of free RAM at the 4 MB floor.
> The section below is the original argument, kept verbatim; the measurement,
> the gating, and the two defects it surfaced are in
> [`BUILTIN_SSH_REMOVAL.md`](BUILTIN_SSH_REMOVAL.md). The pre-auth panic
> described below was closed by deletion. Harness fallout — the
> `[SSH Server] Listening` boot marker no longer exists and every wait loop
> had to change — is documented there under "Boot-readiness marker: harness
> divergence".


Everything above trims `userspace/sshd`. But there are **two** independent SSH-2
server implementations in this codebase: the userspace one (`userspace/sshd` +
`crates/akuma-ssh-crypto`) and a separate **built-in, in-kernel** one
(`crates/akuma-ssh`, compiled into the kernel binary itself, started by
`[Main] Spawning built-in SSH server thread...` on port 22). The
`devbox-smoltcp` profile — already the *documented default devbox* per
`overlays/devbox/README.md` — drops the built-in server entirely in favor of
the userspace one (`userspace-sshd` meta-feature): "built-in SSH dropped in
favour of the userspace `/bin/sshd`."

That's the direction this should keep going. The in-kernel server is a second,
independently-maintained copy of the same packet framing / KEX / auth logic
(it even shares `akuma-ssh-crypto` with the userspace one), permanently
resident in every kernel build that doesn't explicitly opt out — kernel
attack surface and maintenance burden for a feature the recommended runtime
configuration already doesn't use. It should probably be scheduled for
removal (or made an opt-in feature nobody enables by default) rather than
kept in sync forever: it's "a lot of support for things we don't need" once
`devbox-smoltcp`/userspace `sshd` is the standard path.

One live data point for why that matters, found while testing the load
behavior of userspace `sshd` for a follow-up doc
(`userspace/sshd/docs/PROTOCOL_UNDER_LOAD.md`): the **in-kernel** server has
the same unauthenticated-crash bug userspace `sshd` has (see that doc) — a
single malformed pre-auth packet panics `crates/akuma-ssh/src/packet.rs:83`
(`process_unencrypted_packet`, unguarded `packet_len - padding_len - 1`) —
except because it runs *inside the kernel*, the panic doesn't just kill one
process, it takes the **entire kernel** down: `panic=abort` at EL1 has no
process boundary to contain it, and the panic handler's own unwind path hit a
second fault (`WARNING: Kernel accessing user-space address! ... stale
TTBR0`) and wedged the VM completely (100% CPU, unresponsive to new
connections, confirmed with a 10-byte crafted packet against a fresh boot).
That's strictly worse than the userspace equivalent, which — per the
concurrency model in `PROTOCOL_UNDER_LOAD.md` — only kills the shared
`sshd` process. Notably, the in-kernel server's *encrypted*-path sibling
(`process_encrypted_packet`, `crates/akuma-ssh/src/packet.rs`) already has a
bounds check the unencrypted path (and both of userspace `sshd`'s paths)
lack:
```rust
let payload_len = packet_len - padding_len - 1;
if 5 + payload_len > decrypted.len() { return None; }
```
— so even the in-kernel implementation's own author(s) already knew this
needed guarding in one spot and missed the other. Two divergent copies of the
same logic, only one of the four total call sites fixed: exactly the kind of
drift that argues for deleting the redundant copy rather than patching both
forever.

Per this session's scope (userspace-only, kernel untouched), no kernel code
was changed to investigate or confirm this — it's flagged here as a reason to
prioritize retiring `crates/akuma-ssh` rather than as something fixed.

## Background

- Superseded plan: `docs/archive/SSHD_IMPLEMENTATION_PLAN.md` (kept verbatim,
  do not follow literally — see "The original plan was wrong" above).
- musl/Linux syscall ABI compatibility: `docs/reference/abi/musl.md`,
  `docs/reference/abi/linux-compat.md`.
- Prior sshd fixes: `sshd_drain_fix` memory / `docs/archive/` history around
  `src/protocol.rs`'s interactive-shell bridge and exit-status handling.
