# RSA TLS Verification — Feature Gate & Size/Memory Gains

Status: **done** (branch `purge-rsa`). Measured 2026-06-05.

## What changed

RSA signature verification for TLS server certificates is now behind a Cargo
feature, `tls-rsa`, instead of being unconditionally compiled in.

- `tls-rsa` is **on by default** (`Cargo.toml` `[features].default`), so
  `cargo build`/`run`/`test` and the `release` profile keep full RSA support.
- The **`size` and `extreme-size` profiles drop it** — their build scripts use
  `--no-default-features`, and `tls-rsa` is simply not re-added
  (`scripts/build_size.sh`, `scripts/build_extreme_size.sh`).
- With the feature off, the `rsa` crate (and its `num-bigint-dig` bignum
  arithmetic) is not linked, and RSA-signed certs are rejected as
  `InvalidSignatureScheme`.

### Why RSA is safe to drop in the small profiles

- **SSH does not use RSA at all.** Host key, client pubkey auth, and KEX are
  Ed25519 / Curve25519 only (`crates/akuma-ssh/src/constants.rs:30`,
  `crates/akuma-ssh-crypto/src/auth.rs`). The `ssh-rsa` key type is explicitly
  rejected. Dropping RSA costs SSH nothing.
- The **only** consumer of RSA is outbound HTTPS/TLS server-cert verification
  (`crates/akuma-net/src/tls_verifier.rs`). ECDSA-P256 and Ed25519 cert
  verification stay available. With `tls-rsa` off, the kernel can still reach
  any host that offers an ECDSA cert (most CDN-fronted traffic, and the
  increasingly-common dual-cert servers); it can only *not* verify hosts that
  present an **RSA-only** cert.

### Code touchpoints

- `Cargo.toml` — `tls-rsa` feature in `default`; maps to
  `["akuma-net/tls-rsa", "dep:rsa"]`; top-level `rsa` dep made `optional`.
- `crates/akuma-net/Cargo.toml` — `tls-rsa = ["dep:rsa"]`; `rsa` made `optional`.
- `crates/akuma-net/src/tls_verifier.rs` — the 6 RSA verify fns and their 6
  dispatch arms are `#[cfg(feature = "tls-rsa")]`.

## Size impact (flat `.bin`, the image QEMU loads)

| Profile | rsa ON | rsa OFF | Saved |
|---|---:|---:|---:|
| `release` | 2,888,800 B | 2,573,408 B | **308 KB (−10.9%)** |
| `size` | 884,256 B | 855,584 B | **28 KB (−3.2%)** |
| `extreme-size` | 826,896 B | 794,128 B | **32 KB (−4.0%)** |

### Why `release` saves 10× more than the size profiles

The RSA path is fully *live* in every build (no dead code) — the difference is
purely how compactly two profile knobs compile the same bignum-heavy code.
Isolated by forcing one knob at a time on the `size` profile (`.text` delta of
rsa-on minus rsa-off):

| Config | opt-level | LTO | RSA `.text` cost |
|---|---|---|---:|
| `size`/`extreme` (as shipped) | `z` | on | **31 KB** |
| `size`, opt-level forced to 3 | 3 | on | **90 KB** |
| `release` | 3 | off | **345 KB** |

1. **opt-level `z` vs `3` → ~3×.** RSA verification is `num-bigint-dig` modular
   arithmetic. `-O3` unrolls/inlines those big-integer loops and the per-hash
   generic instantiations; `-Oz` keeps them as compact un-unrolled calls.
2. **LTO off vs on → another ~3.8×.** LTO merges the six hash-generic
   monomorphizations (`VerifyingKey<Sha256/384/512>` × PKCS#1/PSS), does bounded
   cross-crate inlining, and eliminates the unreached parts of `rsa` +
   `num-bigint-dig` + `const-oid`. `[profile.release]` has **no LTO**, so all of
   it lands in `.text` whole and duplicated.

So `size`/`extreme` already squeeze RSA to its irreducible ~30 KB — which is both
why the saving there is modest and why ~30 KB is the honest budget for the
low-RAM goal. (The bigger lever for the `release` image specifically would be
enabling `lto`, which shrinks far more than just RSA.)

## Turning the saving into freed RAM: dynamic boot-stack reservation

Shrinking the binary alone does **not** free runtime RAM — the kernel reserves a
region for the boot stack (`STACK_BOTTOM = load + IMAGE_SIZE`), and both images
fit inside the old reservation. To realize the gain, the reservation must shrink
with the binary.

This was first done by hand-tightening the `extreme-size` `IMAGE_SIZE` 880 KB →
848 KB, but that meant editing a **3-way lockstep** constant (build.rs +
boot.rs + main.rs — and missing main.rs leaves the heap reservation on the stale
offset so the freed pages never reach the pool; observed in practice).

That manual constant is now **gone**. `linker.ld` derives the reservation from
the *actual* linked size and exports it as absolute symbols, so it auto-tracks
the binary on every build with no per-profile constant to maintain:

```ld
_kernel_phys_end = .;
STACK_BOTTOM  = ALIGN(_kernel_phys_end, 0x1000) + 0x2000;  /* image end + 2-page guard */
STACK_TOP     = STACK_BOTTOM + 0x100000;                   /* 1 MB boot stack */
IMAGE_RESERVE = STACK_BOTTOM - KERNEL_PHYS_BASE;           /* ARM64 Image header */
```

- `src/boot.rs` asm loads `STACK_TOP` for the initial SP and emits `IMAGE_RESERVE`
  in the Image header — no Rust-injected `stack_top`/`image_size` constants.
- `src/main.rs` and `src/exceptions.rs` read `STACK_BOTTOM`/`STACK_TOP` as extern
  absolute symbols (the same trick already used for `_kernel_phys_end`).
- `build.rs` no longer injects `--defsym=STACK_BOTTOM` and has no `IMAGE_SIZE`.
- Boot self-test `test_boot_stack_reservation_invariants` (`src/process_tests.rs`)
  asserts STACK_BOTTOM > image, page-aligned, 1 MB stack, sane guard.

For `extreme-size` after the rsa-off shrink, `_kernel_phys_end` = 0x402cd580
(**821 KB**), so the derivation yields `STACK_BOTTOM` = 0x402d0000 →
**`IMAGE_RESERVE` = 832 KB** — i.e. the reservation auto-tracked to 832 KB, 16 KB
tighter than the hand-tuned 848 KB, with no constant to touch. `size` likewise
went 944 KB → 892 KB automatically. See
`docs/LOW_MEMORY_ENVIRONMENT.md` *"Dynamic boot-stack reservation"*.

## Measured runtime gain (`extreme-size`)

There is no `/proc/meminfo`, so busybox `free` is unavailable; the kernel's
periodic `[Mem]` line and the boot `PMM stats` line are the equivalent. RAM-free
is MB-granular, but `PMM stats` is page-granular (4 KB) and shows the gain.

Same kernel, same disk, before vs after (rsa-off + `IMAGE_SIZE` 848 KB):

| MEM | Kernel binary | PMM total | allocated | **free pages** |
|---|---:|---:|---:|---:|
| 8 MB (before: rsa-on, 880 KB) | 853 KB | 2048 | 753 | 1295 |
| 8 MB (after) | 821 KB | 2048 | 745 | **1303** |
| 5 MB (before) | 853 KB | 1280 | 657 | 623 |
| 5 MB (after) | 821 KB | 1280 | 649 | **631** |

**+8 pages = +32 KB of user-page pool at every memory size.** Boot reaches
`[SSH Server] Listening` cleanly in all cases; heap and thread counts unchanged.

## New extreme-size boot baseline (boot-to-SSH)

Sweep of the tightened `extreme-size` kernel, fresh disk snapshot per boot:

| MEM | Result | PMM (total / alloc / free) |
|---|---|---|
| 7 MB | boots, SSH up | 1792 / 713 / 1079 |
| 6 MB | boots, SSH up | 1536 / 681 / 855 |
| **5 MB** | **boots, SSH up (floor)** | 1280 / 649 / 631 |
| 4 MB | fails | — |

**Boot-to-SSH floor: 5 MB.** The 4 MB failure is *not* a kernel OOM — QEMU
aborts with `Not enough space for DTB after kernel/initrd`, i.e. a QEMU
guest-memory-layout limit (kernel loads at +2 MB, leaving too little room for the
DTB at 4 MB). So the 32 KB RSA/`IMAGE_SIZE` win adds free pages at every size but
does not break through the 5 MB→4 MB wall, which is QEMU-imposed.

Logs: `logs/rsa-purge/`.
