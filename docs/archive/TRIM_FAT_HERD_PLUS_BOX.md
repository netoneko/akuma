# herd + box: one box library, two clients

**Status: implemented 2026-08-12.** All four steps under "Order of work" landed
except the deferred one (OCI types, explicitly gated on a second consumer that
still does not exist). `boxlib::sys` holds `SpawnOptions`, the box syscall
numbers, and the `register_box`/`kill_box`/`set_box_stack_rump`/`spawn_ext`
wrappers; herd depends on `box` (package `boxlib`) with `default-features =
false` and its own `akuma` feature gating `boxlib/akuma`, so herd's host-test
build still never links `libakuma`. `box_id_for` and `boxlib::json` are shared
the same way. The ABI is pinned on both sides: a host test in
`userspace/box/src/sys.rs` and a `const` assertion beside the kernel's
`SpawnOptions` in `src/syscall/proc.rs`. Verified by booting a private QEMU
instance and confirming `box ps`'s id for a herd-started boxed service matches
an independently-computed `box_id_for` hash, plus `box open`/`use`/`show`/
`close` round-tripping through the same `boxlib::sys` calls herd uses.

The question: `userspace/herd` and `userspace/box` both manage boxes. If shared
types live in one place, does **herd depend on box**, box on herd, or neither?

**Answer: herd depends on box.** `boxlib` becomes the box library and `box` the
CLI over it, matching what `userspace/tar` already did. The rest of this doc is
why, and what specifically moves.

## What is duplicated today

Not OCI types — the *box ABI*. `SpawnOptions` is a `#[repr(C)]` struct the
kernel reads out of userspace memory by layout, and it is written out three
times, independently:

| | Definition | `#[repr(C)]` | Part of this consolidation |
|---|---|---|---|
| Kernel (the definer) | `src/syscall/proc.rs:1316` | yes | **no** — see below |
| box | `userspace/box/src/main.rs:34` | yes | yes |
| herd | `userspace/herd/src/main.rs:820` | yes | yes |

All three agree today, field for field. Nothing checks that they still will.
There is no test anywhere asserting the size or field offsets of this struct
against the kernel's.

**The kernel's copy stays put.** The two userspace copies collapse into one;
the count goes 3 → 2, not 3 → 1. That is not a compromise, it is the only
possible shape: the kernel workspace is `members = [".", "crates/*"]` with
`exclude = ["userspace"]`, userspace is a separate workspace, and there is not
one dependency edge between `crates/` and `userspace/` in either direction
today. A kernel that depended on `userspace/boxlib` would be a kernel that
cannot build without the programs it runs.

The tempting middle option — a tiny `crates/akuma-box-abi` holding the struct
and the syscall numbers, depended on by both sides — is also rejected here. It
would introduce the **first** userspace→`crates/` edge, which is a bigger
architectural commitment than one struct justifies, and it would put the kernel
tree on the critical path of an in-VM userspace build, which the self-hosting
work has spent a long time keeping narrow. A syscall ABI is a contract between
two independently built programs; the way to hold it is an assertion on each
side of the boundary, not a shared type across it (see step 2 below).

The syscall numbers are written out five times across two binaries — box states
them twice **in the same crate**:

| Constant | box | herd |
|---|---|---|
| `SPAWN_EXT` 315 | `main.rs:46` | `main.rs:833` |
| `REGISTER_BOX` 316 | `main.rs:47`, `run.rs:19` | `main.rs:834` |
| `KILL_BOX` 317 | `main.rs:48`, `run.rs:20` | — |
| `SET_BOX_STACK` 324 | — | `main.rs:835` |
| `MOUNT_IN_NS` 325 | via `libakuma` | `main.rs:954` |
| `CORE_INIT` 327 | — | `main.rs:836` |

And the box id — the one that matters most:

```rust
// userspace/herd/src/main.rs:883          // userspace/box/src/spec.rs:82
fn generate_box_id(name: &str) -> u64      pub fn box_id_for(name: &str) -> u64
    box_id.wrapping_mul(31).wrapping_add(*b as u64)   // identical
    if box_id == 0 { box_id = 1; }                    // identical
```

Character for character the same algorithm, in two binaries, naming boxes in
the **same kernel registry**. They must agree — `box ps` lists what herd
registered, `box use <name>` resolves against it — and only one of them has
tests. This is the failure that is one well-meaning "improvement" away: change
the multiplier on one side and the two tools quietly stop meaning the same box
by the same name, with no error anywhere. Nothing crashes; `box close web`
simply kills a different box than herd started.

Below that, each binary carries its own thin wrappers over the same syscalls —
`register_box` (`herd:892`), `spawn_in_box` (`herd:904`), `spawn_ext`
(`box/main.rs:50`) — and its own JSON parser
(`herd:216-403`, ~180 lines, no tests; box's is now a path-addressed layer over
`picojson`).

## Why they look like siblings — and why that reading loses

The first reading of the code says these are peers, not layers: **herd never
invokes box**. It registers boxes, sets their network stack, mounts `/proc` into
their namespaces and spawns into them entirely on its own, straight through
`libakuma::syscall`. Two independent clients of one kernel API. On that
reading, neither should depend on the other and the shared code belongs
underneath both — in `libakuma`, which every userspace binary already links.

That is wrong for one reason: **`libakuma` is the libc layer, and `box_id_for`
is policy.** The name→id hash is not an ABI the kernel defines — the kernel
takes whatever `u64` it is handed. It is a *convention* that box and herd agreed
on, and conventions belong to the domain that owns them, not to the syscall
wrapper library that every unrelated binary (`hello`, `httpd`, `paws`) also
links. Putting it in `libakuma` spreads a container concept across the whole
userspace tree to avoid one dependency edge.

"They are siblings" is a description of the current code, not an argument that
it should stay that way. One of the two *is* the box tool.

## Why herd → box and not box → herd

- **Precedent, and the bug behind it.** `userspace/tar` was made
  `[lib] akuma_tar` + a thin CLI so `box` could depend on the library instead of
  spawning `/bin/tar`. That was not aesthetics: `/bin/tar` was silently a
  busybox applet, and the resulting hardlink expansion turned a 1.9 MB layer
  into 467.7 MB of mode-less copies (`BOX_DOCKER_COMPAT.md`). "A dependency
  cannot be swapped out from under us by a symlink" is the lesson, and the same
  lesson applies to an algorithm copy-pasted between two binaries: the copy can
  drift, and nothing tells you when it has.
- **Stability gradient.** box's types are pinned by things outside this repo —
  the OCI image and distribution specs — and by the kernel's syscall ABI. They
  change rarely and for external reasons. herd's types (`ServiceConfig`,
  restart policy, log rotation, core pinning) are ours and churn with whatever
  we need this month. Depend on the slow side.
- **Direction of knowledge.** A supervisor that starts services can reasonably
  know what a box is. A container tool has no business knowing about a service
  table, restart budgets, or log files.
- **No cycle to argue about.** box references herd nowhere today.

## Rejected alternatives

- **An external OCI crate.** `oci-spec` 0.10.0 (Apache-2.0) is the canonical
  Rust implementation and cannot build here: serde + `serde_json` + thiserror +
  `derive_builder` + getset + strum + regex across ~10.5k lines, `std::` in
  `lib.rs`/`error.rs`, no `no_std` feature. `oci-client` / `oci-distribution`
  are further out — async tokio + reqwest. Nothing on crates.io targets
  `aarch64-unknown-none`.
- **`crates/oci-runtime/`.** `crates/` is for what the **kernel** shares, and
  the kernel has no OCI concept: it takes a `box_id`, a `root_dir`, and an
  overlay `lowerdir=…` string. The precedent is `akuma-ssh-crypto`, which was
  *moved out* of `crates/` into `userspace/` once its only consumer turned out
  to be `userspace/sshd`.
- **A shared OCI *types* crate, now.** The overlap is smaller than the name
  suggests. box reads the **image** spec (`config.Entrypoint/Cmd/WorkingDir`)
  and the **distribution** spec (manifests, indexes); herd reads the **runtime**
  spec (`config.json`: `root.path`, `process.args/env/cwd`, `mounts`,
  `herd:188-403`). Different documents, near-zero shared types. Building the
  abstraction now means maintaining it with one caller per type.

## The shape

`boxlib` (box's library half, already split out for host testing) grows a
feature-gated `sys` module holding what currently sits in the binary, and box's
single `akuma` feature splits in two so herd does not inherit the registry
client:

```toml
# userspace/box/Cargo.toml
default = ["akuma", "pull"]
akuma = ["dep:libakuma"]                                  # boxlib::sys
pull  = ["akuma", "dep:libakuma-tls", "dep:akuma-tar"]    # the registry side
```

```toml
# userspace/herd/Cargo.toml
boxlib = { package = "box", path = "../box", default-features = false, features = ["akuma"] }
```

| boxlib module | What it owns | herd wants it |
|---|---|---|
| `sys` *(new, `akuma`-gated)* | `SpawnOptions`, syscall numbers, `register_box`, `spawn_ext`, `kill_box`, `set_box_stack`, `mount_in_ns` | **yes** |
| `spec` | `box_id_for`, image config → argv, `box run` flag parsing | `box_id_for` |
| `boxes` | `/proc/boxes` parsing, name/id resolution | probably |
| `json` | path-addressed reads over `picojson` | **yes** |
| `paths` | `/var/lib/box` layout, store names, overlay order | no |
| `oci_ref`, `manifest` | references, registry URLs, manifest lists | not yet |

herd pulling in modules it does not call is not a cost worth restructuring for —
LTO drops them — but the feature split does keep TLS, tar and the whole pull
pipeline out of herd's build graph, which is the part that would have been real.

### What herd deletes

`generate_box_id` (`:883`), `SpawnOptions` (`:820`), the `SYSCALL_*` constants
(`:833-836`, `:954`), `register_box` (`:892`), the spawn plumbing in
`spawn_in_box` (`:904`), and — once it is on `boxlib::json` — the hand-rolled
parser at `:216-403`. Roughly 250 lines, all of it a second copy of something.

### Mechanics, verified 2026-08-12

`box` is a reserved Rust keyword, so the package cannot be imported under its
own name; the lib target is `boxlib` and the dependency is declared by package.
Probed by temporarily adding the dependency and a test to herd:

```toml
boxlib = { package = "box", path = "../box", default-features = false }
```

```rust
assert_eq!(boxlib::spec::box_id_for("web"), boxlib::spec::box_id_for("web"));
```

`cargo test -p herd --lib --no-default-features --target <host>` — 8 passed,
including the probe. herd's own host-test suite still links, i.e. the dependency
did not drag `libakuma` into herd's std build. Probe reverted.

## Order of work

1. **`box_id_for` first.** It is the smallest change and closes the only
   duplication here that can silently produce *wrong behaviour* rather than a
   compile error.
2. **`boxlib::sys`** — move `SpawnOptions` and the wrappers out of box's
   `main.rs`/`run.rs` (which also collapses box's own duplicate constants), add
   the feature split, move herd across. Then pin the ABI on **both** sides of
   the workspace boundary, against the same written-down numbers rather than a
   shared type: 72 bytes, nine 8-byte fields at offsets 0, 8, 16, 24, 32, 40,
   48, 56, 64 on LP64 aarch64. A host test in `boxlib` (`size_of` +
   `core::mem::offset_of!` per field) and a `const` assertion beside the kernel's
   definition in `src/syscall/proc.rs`. Either side changing shape then fails to
   build on that side, instead of silently handing the kernel a struct whose
   `box_id` is where it expects `stdin_len`.
3. **`boxlib::json`** — move herd off its hand-rolled parser. This is where the
   known bugs are: `json_get_str_array` truncates at the first escaped quote, so
   any ordinary `sh -c "…"` OCI entrypoint loses its arguments, and
   `json_get_object`'s brace counter is not string-aware, so a `}` inside any
   string value under `process` truncates the object and silently drops `args`
   (`TRIM_FAT_HAND_ROLLED_JSON.md`).
4. **OCI types — only when a second consumer exists.** If herd ever launches a
   pulled image, `oci_ref`/`manifest`/`spec` are already sitting in the crate it
   depends on.

## What this does not do

- It does not unify the runtime spec with the image spec. herd's `config.json`
  reader and box's image-config reader stay separate readers of separate
  documents; they just stop having separate *parsers*.
- It does not make herd able to run OCI images, and does not add a `box`
  dependency to anything else. sshd, httpd, paws are unaffected.
- It does not touch the kernel, beyond adding a `const` assertion next to a
  struct definition that already exists. The box ABI belongs to the kernel;
  this only stops *userspace* from restating it once per binary. Nothing here
  makes the kernel aware that `boxlib` exists.

## Background

- `docs/archive/BOX_DOCKER_COMPAT.md` — the `/bin/tar` symlink bug and why
  `userspace/tar` became a library with a thin CLI; the precedent this doc
  follows.
- `docs/archive/TRIM_FAT_HAND_ROLLED_JSON.md` — the audit that found herd's two
  JSON bugs and counted the hand-rolled parsers across the tree.
- `docs/archive/BUILTIN_SSH_REMOVAL.md` — `akuma-ssh-crypto` moving out of
  `crates/` into `userspace/` when its only consumer was a userspace binary.
- `docs/reference/subsystems/containers.md` — the kernel side: box ids, root
  dirs, mount namespaces, the overlay root.
- `userspace/box/README.md` — what of OCI is currently supported, and the
  crate's own layout.
