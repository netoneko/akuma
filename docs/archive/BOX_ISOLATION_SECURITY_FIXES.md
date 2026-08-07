# Box isolation was advisory: nine unenforced boundaries (2026-08-08)

**Status: FIXED.** All nine landed together and are covered by host tests
(`akuma-isolation`, `akuma-exec`) plus a 10-case boot-suite self-test
(`test_box_isolation_syscall_guards` in `src/process_tests.rs`).

Prompted by a note that "there was some stuff related to boxes which were a bit
insecure". The pointer was
[`BOX_CONTAINERS.md`](BOX_CONTAINERS.md) §"Nested Namespaces" item 3 —
*"`..` traversal must be sanitized to ensure it never ascends above the virtual
root"* — and §"Reattachment" — *"Security: reattachment is only permitted within
the same box hierarchy or from the host (Box 0)"*. The second one turned out to
be the only box rule that **was** enforced
(`reattach_process_ext`, `crates/akuma-exec/src/process/exec.rs`). Auditing the
rest of the box surface found that essentially none of it was.

## 1. The shape of the problem

`crates/akuma-exec/src/box_mod/access.rs` contains `can_access_box`,
`can_kill_box` and `cascade_kill_order`; `box_mod/hierarchy.rs` contains
`get_ancestry_chain`, `is_ancestor`, `get_descendants` and
`validate_nested_root`. All of it is pure, all of it had unit tests, and all of
it passed.

**None of it had a single caller.** `grep -rn 'can_access_box\|can_kill_box\|
validate_nested_root' src crates userspace` returned only the definitions and
their own tests. The permission model existed as a tested library that the
syscall layer never consulted, so at the ABI boundary a boxed process was
subject to no box checks at all beyond `reattach`.

That is the worst failure mode for this kind of code: the tests are green, the
model is documented, and the enforcement is absent.

## 2. The escapes

### 2a. Two syscalls to a full host-filesystem view

`sys_register_box` (`src/syscall/container.rs`) took an arbitrary `id`, `name`
and `root_dir` from any caller in any box and wrote them straight into the
registry. `create_box_namespace` (`src/vfs/mod.rs`) then built the box's
namespace — and for `root_dir == "/"` it installs **no** `SubdirFs` mount at
all, leaving the namespace empty:

```rust
if root_dir != "/"
    && let Some(root_fs) = get_root_fs() {
        let subdir = Arc::new(SubdirFs::new(root_fs, root_dir));
        let _ = ns.mount.lock().mount("/", subdir);
    }
```

An empty namespace is not a closed door. `with_fs` tries the process namespace
first and then **falls back to the global mount table**, so every path in such a
box resolves against the host's ext2 root, read and write. Combined with 2b:

```
register_box(id = <any unused>, name = "esc", root_dir = "/", primary_pid = self)
spawn_ext("/bin/sh", SpawnOptions { box_id: <that id>, .. })
```

...and the child is out, with no jail and full write access to the host rootfs.

The same syscall also let any box **overwrite an existing box's entry**,
including box 0's — resetting the host box's `root_dir`, `creator_pid` and
`primary_pid`.

### 2b. Spawning into somebody else's box

`sys_spawn_ext` passed `o.box_id` through to `spawn_process_with_channel_ext`
unchecked, and `spawn.rs` applies it as:

```rust
if box_id != 0 {
    process.box_id = box_id;
    if let Some(ns) = (runtime().get_box_namespace)(box_id) {
        process.namespace = ns;      // <- the target box's mounts, wholesale
    }
```

So a boxed process could spawn a child directly into a **sibling's** box: its
`box_id`, its mount namespace, and (via `box_id`-keyed routing) its network
stack. No registration needed; just name the box.

### 2c. `parent_box_id` was hardcoded `None`

`sys_register_box` always wrote `parent_box_id: None`, so no box ever recorded a
parent and `get_ancestry_chain` treated every box as a direct child of the host.
Every ancestry-based rule in `access.rs` was therefore permanently blind — which
is *why* wiring the checks up required this fix first, not just alongside it.

### 2d. `validate_nested_root` accepted siblings

```rust
if child_root_dir.starts_with(parent_root) { Ok(()) }
```

A bare prefix test. `/containers/box10` "starts with" `/containers/box1`, so a
box could claim a jail rooted inside a *sibling's* subtree. It also accepted
unresolved `..` (`/containers/box1/../box3` passes `starts_with` and then
resolves elsewhere on disk) and relative paths.

### 2e. Killing any box

`sys_kill_box` ran no check whatsoever. Any process could kill every process in
any other box. Worse, it dropped the victim's namespace *first*:

```rust
crate::vfs::remove_box_namespace(box_id);
if kill_box(box_id).is_ok() { 0 } else { ESRCH }
```

`kill_box(0)` is refused inside `akuma-exec`, but `remove_box_namespace(0)` had
already run by then — a rejected call could still strand a box without its
mounts.

### 2f. Repointing another box's network stack

`sys_set_box_stack(box_id, 1)` marked *any* box as rump, routing its AF_INET
syscalls at a `rump_server` the caller controls. Unchecked.

### 2g. Unmounting your own jail floor

`sys_umount2` let a boxed process unmount **`/`** from its own namespace. That
namespace is the box: its single `/` mount is the `SubdirFs` jail. Remove it and
the namespace is empty, and — per 2a — `with_fs` falls back to the global mount
table. A one-syscall escape, needing no registry access at all.

### 2h. `SubdirFs` never sanitized `..`

`full_path!` (`crates/akuma-isolation/src/subdir_fs.rs`) concatenated
`prefix + path` with no normalization, exactly the thing `BOX_CONTAINERS.md`
called out. In practice `with_fs` canonicalizes before dispatching, so this was
not a *live* escape through the normal `open` path — but `SubdirFs` is a public
crate type reachable from the mount table by any caller, and "the layer above
happens to sanitize" is not containment.

### 2i. Mount targets were never canonicalized

`MountNamespace::{mount,unmount,resolve}` (`akuma-isolation/src/mount.rs`)
compare mount points **literally** — only `trim_end_matches('/')`. An
un-normalized target (`/proc/`, `/a/../proc`) registers a mount point no lookup
can ever match, and side-steps the duplicate check that stops a box shadowing
its own root.

## 3. The fixes

| # | Fix | Where |
|---|---|---|
| 1 | `can_register_box` — new pure rule: re-registration needs `can_access_box`; a new box becomes a **child of the caller** and its root must lie inside the caller's root; box 0 can never be created | `box_mod/access.rs` |
| 2 | `validate_nested_root` matches on a **component boundary**, requires an absolute path, and rejects unresolved `.`/`..` | `box_mod/hierarchy.rs` |
| 3 | `sys_register_box` canonicalizes `root_dir`, gates on `can_register_box`, and records the returned `parent_box_id` (preserving the existing parent on re-register, so a box cannot re-parent itself) | `src/syscall/container.rs` |
| 4 | `sys_kill_box` gates on `can_kill_box`, and drops the namespace only **after** the kill succeeds | `src/syscall/container.rs` |
| 5 | `sys_spawn_ext` gates a non-zero `box_id` on `can_access_box` (`box_id == 0` still means "inherit the caller's box" and needs no check) | `src/syscall/proc.rs` |
| 6 | `sys_set_box_stack` gates on `can_access_box` | `src/syscall/proc.rs` |
| 7 | `sys_umount2` refuses to unmount `/` — a box may drop a mount it added, never the floor it stands on | `src/syscall/container.rs` |
| 8 | `SubdirFs` confines `.`/`..` before prefixing, clamping at the virtual root | `akuma-isolation/src/subdir_fs.rs` |
| 9 | `sys_mount` / `sys_umount2` / `sys_mount_in_ns` canonicalize their target | `src/syscall/container.rs` |

The caller's identity is resolved in exactly one place,
`container::caller_box_and_pid()`, which maps "no `Process`" (kernel thread —
the built-in shell, the boot path) to box 0.

### Cost of the confinement fast path

`confine()` scans for a `.`/`..` component and returns `None` when there is
none, so a canonical path allocates nothing and takes the original stack-buffer
path unchanged. Only a path actually containing a dot component pays for a
`canonicalize_path` `String`.

## 4. What is deliberately unchanged

- **`can_access_box`'s `creator_pid` fallback** stays. It is how a supervisor
  that created a box keeps reach over it after the pid moves between boxes.
- **A box may still create nested boxes.** Nesting is a supported use; the fix
  constrains *where* (inside the caller's own root) and *whose child* it is, not
  whether.
- **Symlinks are not re-confined.** `SubdirFs::create_symlink` stores the target
  verbatim, matching chroot semantics: the target is re-resolved through the
  box's own namespace (`vfs::resolve_symlinks` → `read_symlink` → `with_fs`), so
  a symlink cannot be used to leave the box from inside it. A symlink planted by
  a box *is* followed by the host if the host walks into `box_root` — same as
  Linux, and the same reason you do not walk into a container's rootfs.
- **`with_fs`'s global-mount-table fallback** stays. Making it strict for boxed
  processes was considered and rejected: with fix 7 in place a jailed box always
  has its `/` mount, so the fallback is unreachable for it, and the fallback is
  load-bearing for boxes whose `box_root` is `/`
  (see [`BOX_SUBDIR_FS_LIMITATIONS.md`](BOX_SUBDIR_FS_LIMITATIONS.md) §2).

## 5. Compatibility

Both in-tree callers of these syscalls — `userspace/herd` and `userspace/box` —
run in box 0, which `can_access_box` short-circuits to "allowed". herd's
deliberate double `register_box` (placeholder pid, then real pid) still works:
it takes the re-registration branch, which preserves the recorded parent. A
`box`/`herd` run from *inside* a box is now constrained to its own subtree,
which is the intended new behaviour.

One semantic change: `kill_box` on an unknown box id returns `EPERM` rather than
`ESRCH` when the caller is boxed. That is deliberate — the old code let a box
probe for the existence of boxes it cannot see.

## 6. Tests

| Layer | What it pins |
|---|---|
| `akuma-isolation::subdir_fs::tests` (7) | every path-taking `Filesystem` method is confined, including both `rename` operands and the heap fallback for paths over `FS_MAX_PATH_SIZE`; `..` clamps at the root; canonical paths are byte-identical to before |
| `akuma-exec::box_mod::access::tests` (+6) | `can_register_box`: host may register anything, a new box is the caller's child, roots outside the caller's jail are refused, box 0's entry cannot be hijacked, an unrelated box cannot be re-registered, an unregistered caller box gets nothing |
| `akuma-exec::box_mod::hierarchy::tests` (+3) | the sibling-prefix bug (`/containers/box10`), unresolved `..`/`.`, relative roots |
| `src/process_tests.rs::test_box_isolation_syscall_guards` (10 cases) | the real `handle_syscall` paths, from a process registered into box A: sibling/host `kill_box`, `register_box` with `/` and with `..`, the legitimate own-subtree registration **and** its recorded parent, `spawn_ext` into a sibling, `set_box_stack` on a sibling, `umount2("/")`, `SubdirFs` `..` against the live ext2 root, and a host-box positive control so the suite cannot pass by blanket denial |

Boot-verified: `[Test] box isolation syscall guards PASSED (10 cases)`.

## Background

- [`BOX_CONTAINERS.md`](BOX_CONTAINERS.md) — the original proposal, including
  the `..` sanitization requirement this closes.
- [`BOX_SUBDIR_FS_LIMITATIONS.md`](BOX_SUBDIR_FS_LIMITATIONS.md) — what the
  `SubdirFs` jail does and does not give you.
- [`CONTAINERS_STAGE_1_PLAN.md`](CONTAINERS_STAGE_1_PLAN.md),
  [`CONTAINERS_STAGE_2_PLAN.md`](CONTAINERS_STAGE_2_PLAN.md).
- Current state: [`../reference/subsystems/containers.md`](../reference/subsystems/containers.md).
