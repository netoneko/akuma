# Box fresh-root (`SubdirFs`) limitations

A box created with a non-`/` `box_root` (e.g. `box_root = /srv/rumpbox`, or
`box open --root <dir>`) gets an isolated, chroot-like filesystem view: the kernel
wraps the box's mount namespace in a `SubdirFs` (`crates/akuma-isolation/src/subdir_fs.rs`)
mounted at `/`, which **prepends `box_root` to every path** the box resolves
(`create_box_namespace`, `src/vfs/mod.rs`). This is cheap and needs no kernel page-table
work, but it is *only* a VFS-layer remap. The following do **not** come for free —
learned while putting `userspace/sshd` in the rumpnet box for `acceptance/11`.

## 1. Special device paths bypass the namespace (and that's why they work)

`/dev/net/tap0`, `/dev/zero`, `/dev/null`, `/dev/urandom` are matched by **literal
path before namespace resolution** in the open handler (`src/syscall/fs.rs`, the
`path == "/dev/..."` checks). So a fresh-root box *can* open them with no per-box
mount — the rumpnet box opens `/dev/net/tap0` this way. The flip side: **any new
device implemented purely in the VFS layer** (resolved through `with_fs` →
namespace) would resolve under `box_root` (`/srv/rumpbox/dev/...`) and break inside a
fresh-root box. New devices must either be literal-matched pre-namespace like the
above, or be mounted into the box namespace.

## 2. `/proc` is absent unless explicitly mounted into the box namespace

There is no global `/proc` fallback once the box has a `/` `SubdirFs` mount (the
namespace match wins; `with_fs` only falls back to the global mount table when the
namespace has *no* match). So `/proc` must be mounted into the box namespace
(`SYSCALL_MOUNT_IN_NS` → `ProcFilesystem`). This is why `sshd.conf` carries
`mount = proc`: sshd's interactive shell bridge forwards the child shell's stdin via
`/proc/<pid>/fd/0` (`userspace/sshd/src/protocol.rs`), which would otherwise resolve
to `/srv/rumpbox/proc/...` and fail. herd mounts it via `setup_fs_mounts`
(`userspace/herd/src/main.rs`).

## 3. Re-`register_box` is idempotent (so it doesn't drop mounts)

herd calls `register_box` twice — once with a placeholder pid, then with the real
pid. `create_box_namespace` is now **idempotent**: if the box already has a
namespace it returns the existing one rather than recreating it
(`src/vfs/mod.rs`). Without this, the second register would replace the namespace
and silently drop any mounts added in between (e.g. the `/proc` above). A service
that *joins* an existing box (`join_box`) must NOT call `register_box` at all.

## 4. No symlinks assumed

The box rootfs is populated by copying files (`populate_disk.sh` copies the
`bootstrap/` tree). busybox is **copied** as `/bin/sh` (so argv[0] dispatch runs the
`ash` applet), not symlinked — don't assume symlink support in a box rootfs.

## 5. Binaries are not shared with the host root

`SubdirFs` is not a union/overlay mount. Every binary the box runs
(`/bin/rump_server`, `/bin/sshd`, `/bin/sh`, …) must physically exist under
`box_root` (e.g. `bootstrap/srv/rumpbox/bin/...`). This duplicates them on disk.

## Future direction

A real per-box devfs + procfs auto-mounted at box creation would remove the
pre-namespace special-casing (item 1) and the manual `mount = proc` (item 2), and
an overlay/union backing would remove the binary duplication (item 5).
