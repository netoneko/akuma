# ext3/ext4 implementation notes

These notes describe the independently written extension of `akuma-ext2` on
`more-improvements`. They record implemented behavior and current limits, not
a claim of complete ext3/ext4 support. The on-disk layouts are based on the
published [ext4 format documentation](https://docs.kernel.org/filesystems/ext4/)
and [virtio 1.2 specification](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html).
Linux filesystem and e2fsprogs implementation code was not reused; e2fsprogs
is used as an image generator and independent test oracle.

## Mount and I/O contracts

The `ext3` and `ext4` Cargo features are additive and off by default; `ext4`
implies `ext3`. The original constructor requests read-write access and still
only accepts the legacy ext2 feature mask. `MountMode::ReadOnly` is explicit,
enforced at the filesystem entry points and retained in the VFS mount table;
remounting a read-only instance writable fails. Runtime mount names `ext2`,
`ext3` and `ext4` share the same on-disk capability checks.

Clean internal-journal ext3/ext4 filesystems can be read without replay. The
filesystem state must be clean, no orphan processing may be pending, and the
journal must be internal. Unknown feature bits and unsupported
combinations are rejected before writes. In particular, there is no journal
on-device replay or journaled write implementation; requesting writable access
to a journaled image fails. A narrow unclean ext3 profile can instead be
recovered in memory on a read-only mount, as described below.

The `journal::plan_replay` parser reads a bounded, classic
32-bit unchecksummed journal into a read-only plan, handling completed
transactions, revocations, escaped blocks and ring/sequence wrap. It is
applied to a read-only in-memory device overlay through `recover_read_only`;
the normal read-only guest mount path tries that recovery after an unsupported
clean mount. The backing device is never written. This path only supports
classic ext3 with no pending orphan recovery; it rejects ext4 layout features,
checksummed/64-bit journals and journals larger than 64 MiB. It rejects home
blocks overlapping the journal and checks the resulting superblock before
publishing the mount. A host test constructs a committed transaction on a real
ext3 image and compares the recovered contents with `e2fsck` journal replay.

The independent classic JBD transaction writer and block-level staging buffer
are available as building blocks, but are not connected to a writable mount.
The writer sets and flushes the recovery marker, writes and flushes staged log
blocks, publishes and flushes the journal start, commits, then checkpoints and
clears the journal and recovery marker with barriers. Superblock after-images
retain the recovery bit until the final marker update. It requires an exclusive
device with a truthful flush and atomic persistence for the sectors carrying
recovery metadata; this contract has not yet been established for every device.

`JournaledDevice` stages writes and commits them with this writer. It requires
an explicit `JournalStorage` implementation that promises both stable barriers
and atomic sector writes; a generic virtio device does not claim the latter.
The device poisons itself after staging or commit failures. It cannot stop a
filesystem from serving already cached data after an abort, detect a killed
caller, or provide per-operation serialization: the future VFS integration
must handle those conditions. A host-only test brackets existing filesystem
operations manually and checks the committed image with `e2fsck`.

A host-only adapter stages the existing ext2 mutation paths against an ext3
image and commits successful operations. Create, rename, unlink and truncate
are interrupted at each I/O and barrier; each image is journal-replayed by
`e2fsck`, then checked without repairs and compared against the pre- and
post-operation content. This does not yet address rollback of in-memory state,
thread death, concurrent operations, oversized transactions, or writable VFS
mounts.

Virtio-blk negotiates the flush feature and submits a two-descriptor FLUSH
request after preceding writes complete. Filesystem sync propagates failure;
when the device does not offer flush the driver returns an error rather than
claiming a durability guarantee. This is a prerequisite for journaling, not a
transaction implementation. USB and NVMe retain their own barriers.

## Ext4 read path

Inode block mapping selects the legacy map or an extent tree based on the inode
flag. The extent walker checks headers, entry bounds, depth, ordering, lengths,
physical ranges and high address bits; holes and unwritten extents read as
zero. External extent nodes are read through the block cache. A 64-byte group
descriptor and `flex_bg` are accepted, but block numbers beyond `u32` are not.

For `metadata_csum`, a small independently implemented reflected CRC32C routine
validates the superblock at mount, group descriptors and inodes on access,
directory leaf and HTree index blocks on traversal, and external extent nodes
on traversal. The inode number is transient in-memory state and is not encoded
in the on-disk inode. The implementation supports the UUID-derived seed and
checksum type 1; alternate seeds and unsupported checksum or incompatible
features remain gated. The read-only HTree path scans directory blocks linearly
rather than implementing hash lookup. Directory and extent checks are performed
on blocks actually visited, not as a whole-volume scrub. Data blocks themselves
are not checksummed by ext4 metadata checksums.

The deferred-free slot array lives on the heap. An inline 16 KiB array was
copied through the runtime mount path, creating a 43 KiB stack frame and
corrupting the amd64 guest's 32 KiB kernel stack. The QEMU guest caught this;
the mount regression probe checks for the stack canary as well as its verdict.

## Verification and remaining work

Host tests generate images with `mke2fs`, populate them with `debugfs`, and
exercise clean ext3 and default ext4 images, sparse and multi-level extent
maps, HTree directories and metadata corruption. `scripts/probes/ext_ro_qemu.sh`
builds the guest, boots it with disposable ext2 root and ext3/ext4 data images,
checks `fsync`, read, write rejection and remount rejection, and runs `e2fsck
-fn` on each data image after shutdown. The script uses QEMU software emulation
and does not touch an existing VM image.

Still required: on-device journal recovery (including checksums and orphan
handling), transaction-aware cache
and eviction, ordered barriers and write-error handling, ext3 write support,
ext4 extent/metadata allocation and checksum updates, failure injection at
every durable boundary, and interoperability and power-cut testing. New
features must be accepted only when all relevant read or write paths support
them; a Cargo flag is not a promise to accept every filesystem carrying that
name.
