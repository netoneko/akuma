# Crate safety: which crates forbid `unsafe`

**Grade: A** — the list below was measured on 2026-08-28 by grepping every
`crates/*/src/` tree and confirmed by building each crate with the ban in force
(`cargo clippy -p <crate> --target $HOST -- -D warnings`, 0 errors).

Ten of the eighteen extracted crates are **unsafe-free and enforced so**. Each
carries a crate-level attribute in its `lib.rs`:

```rust
#![forbid(unsafe_code)]
```

`forbid`, not `deny`: `deny` can be switched back off by a module-local
`#[allow(unsafe_code)]`, `forbid` cannot. Adding an `unsafe` block to any crate
in the first table is a hard compile error, which is the point — these are the
crates whose whole value is that they are pure logic you can reason about and
host-test.

## Enforced unsafe-free

| crate | what it holds |
|---|---|
| `akuma-boot` | Linux `reboot(2)` ABI decode |
| `akuma-isolation` | box/namespace path confinement (`subdir_fs`) |
| `akuma-kacho` | the shared observe/decide/hysteresis layer for self-tuning policies |
| `akuma-net-yarn` | the socket readiness wait loop as a pure state machine |
| `akuma-rump` | device-independent orchestration for the rump raw-L2 path |
| `akuma-scheduler` | discrete-event simulator for placement / netpoll wake policy |
| `akuma-terminal` | terminal/line-discipline state |
| `akuma-syscalls-time` | time syscalls + the boot-time SNTP client |
| `akuma-vfs` | the `Filesystem` trait and common FS types |

`akuma-isolation` joined this list on 2026-08-28 rather than being born into it.
It had exactly one `unsafe`: a `core::str::from_utf8_unchecked` over a path
buffer built by concatenating two `&str`s. That is valid UTF-8 by construction,
so the unchecked call bought only the skipped validation pass — a walk over a
few tens of bytes. Replacing it with a checked `from_utf8(...).unwrap_or("")`
costs nothing measurable and removed the crate's last `unsafe`.

## Not enforceable, and why

These crates contain `unsafe` that is not an artifact of convenience — removing
it would mean removing the crate's reason to exist. Counts are `unsafe` sites as
of 2026-08-28.

| crate | sites | why it is irreducible |
|---|---:|---|
| `akuma-exec` | ~216 | process/thread/address-space internals: trap frames, raw page tables, context switch |
| `akuma-net` | ~43 | DMA-visible buffers and virtio descriptor rings |
| `akuma-virtio` | ~38 | MMIO and DMA by definition |
| `akuma-ext2` | ~19 | `repr(C)` on-disk structures read through raw byte buffers |
| `akuma-primitives` | ~18 | the dependency-free leaf: IRQ masking, per-CPU registers, the console writer |
| `akuma-syscalls-linux` | ~12 | `transmute` layout assertions that pin `repr(C)` ABI types against Linux headers |
| `akuma-timer` | ~8 | `mrs`/`msr` on CNTV/PL031 |
| `akuma-pmm` | ~6 | volatile reads/writes to physical frames |
| `akuma-firecracker` | ~2 | `pub unsafe fn describe_ptr` takes a raw FDT pointer from the caller — the unsafety is the *contract*, not the body |

**Two of these are worth re-checking as they shrink**, but neither is close
today: `akuma-firecracker`'s two sites are one genuinely-unsafe public signature
plus its own body, and `akuma-syscalls-linux`'s are layout assertions that could
in principle move to a checked byte-comparison.

## Why the ban lives in `lib.rs` and not `Cargo.toml`

Cargo's `[lints]` table cannot express it per-crate here. Every crate inherits
the workspace lint set with:

```toml
[lints]
workspace = true
```

and Cargo rejects mixing that with crate-local lints outright:

```
cannot override `workspace.lints` in `lints`, either remove the overrides
or `lints.workspace = true` and manually specify the lints
```

So spelling the ban in `Cargo.toml` would mean dropping the inherit and copying
the whole ~45-entry `[workspace.lints]` table into each of ten manifests — ten
copies to drift out of step, which is the exact class of duplication
[`../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](../archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md)
exists to remove. The crate-level attribute is one line, is scoped to the crate,
and composes with the inherited lints.

If a future Cargo grows additive crate lints, moving the ban into the manifests
is mechanical.

## Adding a crate to the list

1. `grep -rn '\bunsafe\b' crates/<name>/src/ --include='*.rs'` — check the hits
   are code, not doc comments (`akuma-rump`'s only hit was a doc comment).
2. Add `#![forbid(unsafe_code)]` after any existing `#![...]` attributes.
3. `cargo clippy -p <name> --target $HOST -- -D warnings` and
   `cargo test -p <name> --target $HOST`.
4. Add a row above.
