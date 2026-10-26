# OPEN: the host reports a 1 GiB guest disk as 856 MiB (2026-10-26)

**Status: open, not investigated.** Written down so another agent can reproduce it locally. Nothing here is a verified cause; hypotheses are marked.

## Symptom

After the Taoxichuan guest root image was grown from 16 MiB to 1 GiB, the first boot on it printed, in `/var/log/herd/vm-dev.log`:

```
[KVM] linux: guest disk: /boot/akuma-dev-root-ssh.img, 856 MiB
```

The file it names was 1,073,741,824 bytes (`ls -l`) and fully allocated (`ls -s`: 1,048,576 KB) when checked afterwards. 856 MiB is 897,581,056 bytes. The boots before the grow printed `16 MiB` for the same path, correctly.

The guest still booted (`BOOT OK ... ready_in=45s`), because everything the battery touches sits in the low block groups. A guest that believes it has 1 GiB (the superblock says 262,144 blocks) on a device that is 856 MiB wide would fail on any block past the end.

## How the image was made (reproduce)

1. `apk add e2fsprogs e2fsprogs-extra` in the Akuma box's own root (`resize2fs`, `debugfs`, `e2fsck`, `dumpe2fs`).
2. `scripts/kvm/goose/grow_dev_image.sh` (new): copies `/boot/akuma-dev-root-apk.img.orig.16M`, `truncate -s 1G`, `resize2fs`, `e2fsck -fn` must exit 0, then moves it over `/boot/akuma-dev-root-apk.img.orig`.
   - The 16 MiB original is kept as `.orig.16M`, sha256 `f3aa35d6f84460bd34cb...` (full value in `/root/dev-cycle/orig16.sha256` on the box).
   - Result: 262,144 blocks of 4 KiB, 32,768 inodes, `e2fsck -fn` rc 0, 104 files, 3,857 blocks used; `/etc/resolv.conf`, `/etc/apk/keys`, `/pkgs`, `/bin/busybox` present via `debugfs`.
3. `scripts/kvm/goose/boot_dev.sh` (unchanged): `cp` of the `.orig` to `/boot/akuma-dev-root-ssh.img`, then `devroot-ensure` rewrites it through a tmp file and a rename, then `herd start vm-dev`. Boot to ready took 45 s with the 1 GiB copy included.

## Where the number comes from (code, read-only)

- `amd64/src/kvm/linux/machine.rs:1054` prints `d.sectors() >> 11` as MiB.
- `amd64/src/kvm/linux.rs:683-690`: for a file-backed disk, `sectors()` is `akuma_vfs_glue::file_size(path) / SECTOR`.

So `file_size()` returned about 897.6 MB at the moment the VM started, for a file whose size was 1 GiB seconds earlier on every other view.

## Hypotheses (unverified)

1. `file_size()` reads the on-disk inode, which lagged behind the cache after the 1 GiB `cp` and the `devroot-ensure` tmp+rename rewrite; a `sync` before `herd start vm-dev` would then fix it. The 16 MiB image never showed it because there was no meaningful lag at that size.
2. `file_size()` or the ext2 inode size path mishandles a size past some threshold or a partially extended file.
3. The copy was still in flight in a way `ls` hides (the size seen by `ls` came from a cache).

## Not done yet

- No second boot on the same image (to see whether 856 MiB repeats or becomes 1024 MiB).
- No `sync` before `herd start vm-dev` (the cheapest test of hypothesis 1).
- No direct call of `file_size()` on the grown file after a `sync`.
- The gate battery on the 1 GiB image (`/root/dev-cycle/gates1g.out` on the box) had G1 G2 G3 G4 H1 H2 PASS when this was written; the rest had not finished.

## Related (not the same issue)

`taoxichuan/src/main.rs` `fill_statfs` answers `statfs` with fixed numbers for a 16 MiB filesystem (4,096 blocks, 2,053 free); the 1 GiB image makes those wrong, and `W1b` (`df /`) hashes that output. M4 (`apk add curl`, 12.6 MB installed) needs the statfs answer to come from the real superblock and the W1b baseline to be re-recorded on purpose.

## Background

`docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md` (stage 8 sessions L and the image grow), `scripts/kvm/goose/grow_dev_image.sh`.
