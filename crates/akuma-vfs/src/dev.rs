//! The `/dev` device table.
//!
//! One table replacing the name/major/minor/mode knowledge that used to live
//! as independent `if path == "/dev/..."` blocks in `sys_newfstatat` and
//! `sys_statx` — the drift `docs/archive/DEVFS_MISSING.md` was written to
//! describe (`/dev/random` `open()`ed fine but `stat()`ed `ENOENT`, because
//! only two of the five devices had ever been copy-pasted into the stat path).
//!
//! This module answers **"what exists and what does `stat` say about it"** and
//! nothing else. It deliberately does not carry `open()` behavior: each
//! device's `open()` is genuinely different (a PRNG read loop, a PCM sink, a
//! socket-backed fd), so `sys_openat`'s dispatch stays where it is and this
//! table stays pure data — see `DEVFS_MISSING.md` §3.
//!
//! Everything here is a pure function of a [`DevProbe`], so the whole table is
//! host-unit-testable without a boot: the caller supplies what the kernel
//! probed (sound device present? which block slots filled?) instead of this
//! module reaching into `crate::block` / `crate::audio` itself.

/// `S_IFCHR` — character device.
const S_IFCHR: u32 = 0o20000;
/// `S_IFBLK` — block device.
const S_IFBLK: u32 = 0o60000;

/// `getdents64` `d_type` for a character device.
pub const DT_CHR: u8 = 2;
/// `getdents64` `d_type` for a block device.
pub const DT_BLK: u8 = 6;

/// Highest block-device slot the table knows how to name.
///
/// Matches `akuma_virtio::block::MAX_BLOCK_DEVICES`, but kept as its own
/// constant rather than a dependency: this crate is `no_std` and driver-free by
/// design, and a mismatch is caught by [`DevProbe::block_slots`] simply having
/// no bit set above what the driver registered.
pub const MAX_BLOCK_SLOTS: usize = 4;

/// One entry in the device table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevNode {
    /// Name relative to `/dev` — `"null"`, `"vda"`. Never contains a slash.
    pub name: &'static str,
    /// `S_IFBLK` when set, `S_IFCHR` when clear.
    pub is_block: bool,
    /// Permission bits only; [`DevNode::mode`] adds the type.
    pub perm: u32,
    pub major: u32,
    pub minor: u32,
    /// Stable synthetic inode. Device `st_dev` is 0 while real files report 1
    /// (`sys_newfstatat`), so these never collide with an ext2 inode number.
    pub ino: u64,
}

impl DevNode {
    /// Full `st_mode`: file type plus permission bits.
    #[must_use]
    pub const fn mode(&self) -> u32 {
        (if self.is_block { S_IFBLK } else { S_IFCHR }) | self.perm
    }

    /// `getdents64` `d_type`.
    #[must_use]
    pub const fn d_type(&self) -> u8 {
        if self.is_block { DT_BLK } else { DT_CHR }
    }
}

/// Always-present nodes: pure kernel constructs with no hardware behind them,
/// so they exist on every build and in every namespace. Major/minor follow
/// Linux's `Documentation/admin-guide/devices.txt`; `null`'s and `zero`'s
/// inodes (1 and 5) are the values the deleted `sys_newfstatat` /
/// `sys_statx` blocks already reported, preserved so nothing that cached a
/// device inode sees it change.
const STATIC_NODES: &[DevNode] = &[
    DevNode { name: "null",    is_block: false, perm: 0o666, major: 1, minor: 3, ino: 1 },
    DevNode { name: "zero",    is_block: false, perm: 0o666, major: 1, minor: 5, ino: 5 },
    DevNode { name: "random",  is_block: false, perm: 0o666, major: 1, minor: 8, ino: 8 },
    DevNode { name: "urandom", is_block: false, perm: 0o666, major: 1, minor: 9, ino: 9 },
    // The controlling terminal. `open()` is special-cased in `sys_openat`
    // (it resolves the CALLER's channel, which a static table cannot), so this
    // entry exists for `stat`/`ls` only — same shape as tap0, opposite reason.
    // Major 5 minor 0 is Linux's TTY_MAJOR/0.
    DevNode { name: "tty",     is_block: false, perm: 0o666, major: 5, minor: 0, ino: 16 },
];

/// The two nodes that exist only when a virtio-sound device was found. Both
/// name the same PCM sink, matching `sys_openat`'s `"/dev/dsp" || "/dev/audio"`.
const AUDIO_NODES: &[DevNode] = &[
    DevNode { name: "dsp",   is_block: false, perm: 0o666, major: 14, minor: 3, ino: 14 },
    DevNode { name: "audio", is_block: false, perm: 0o666, major: 14, minor: 4, ino: 15 },
];

/// `vda`..`vdd`, gated per-slot on [`DevProbe::block_slots`]. Spelled out
/// rather than computed so the whole table is static data the lookup can
/// borrow from. `254` is virtio-blk's major; the minor spacing of 16 mirrors
/// Linux reserving 16 minors per disk for partitions this kernel does not have.
/// Inodes start at 32, above every static node's.
const BLOCK_NODES: &[DevNode; MAX_BLOCK_SLOTS] = &[
    DevNode { name: "vda", is_block: true, perm: 0o660, major: 254, minor: 0,  ino: 32 },
    DevNode { name: "vdb", is_block: true, perm: 0o660, major: 254, minor: 16, ino: 33 },
    DevNode { name: "vdc", is_block: true, perm: 0o660, major: 254, minor: 32, ino: 34 },
    DevNode { name: "vdd", is_block: true, perm: 0o660, major: 254, minor: 48, ino: 35 },
];

/// Live kernel state the table needs, plus who is asking.
///
/// Passed in rather than probed here so the table is a pure function — see the
/// module docs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DevProbe {
    /// A virtio-sound device was found at boot (`audio::is_available()`).
    pub audio: bool,
    /// Bit `i` set means block slot `i` is populated, i.e. `vd{a+i}` exists.
    pub block_slots: u8,
    /// The caller runs inside a box (`box_id != 0`).
    ///
    /// **Boxes get no synthetic `/dev`.** This is a deliberate scope decision
    /// (2026-08-25), not an oversight: a box has its own rootfs, and showing it
    /// the host's disks and sound card in `ls /dev` is a containment leak with
    /// no consumer asking for it. Expand later if something needs it.
    ///
    /// Two things survive the carve-out:
    ///
    /// * `null` and `zero` still answer [`lookup`] in a box, because they
    ///   already did before this table existed and silently turning
    ///   `stat("/dev/null")` into `ENOENT` inside a box would be a regression.
    ///   They are still absent from [`list`] — the asymmetry preserves today's
    ///   behavior exactly rather than inventing new behavior for boxes.
    /// * `/dev/net/tap0` is unaffected because it was never in this table
    ///   (`DEVFS_MISSING.md` §3: nested path, directly-openable only). A
    ///   `stack = rump` box's `rump_server` opens it through `sys_openat`,
    ///   which this module does not touch, so rump boxes keep their NIC.
    pub in_box: bool,
}

/// Whether `node` is visible to the caller described by `probe`.
fn visible(probe: DevProbe, node: &DevNode, listing: bool) -> bool {
    if !probe.in_box {
        return true;
    }
    // In-box: nothing lists, and only the two pre-existing stat targets look up.
    !listing && matches!(node.name, "null" | "zero")
}

/// Every node that exists for `probe`, in a stable order.
///
/// Borrows from the three static tables — no allocation, so `lookup` on a hot
/// `stat` path costs a walk of at most ten `&'static DevNode`s and nothing else.
fn all_nodes(probe: DevProbe) -> impl Iterator<Item = &'static DevNode> {
    STATIC_NODES
        .iter()
        .chain(AUDIO_NODES.iter().filter(move |_| probe.audio))
        .chain(
            BLOCK_NODES
                .iter()
                .enumerate()
                .filter(move |(idx, _)| probe.block_slots & (1u8 << idx) != 0)
                .map(|(_, node)| node),
        )
}

/// The node named by `name` (a `/dev/`-relative name, no slash), if it exists
/// and is visible to the caller.
///
/// This is the single lookup behind `stat`, `statx`, `faccessat2` and `access`
/// on a device path.
#[must_use]
pub fn lookup(probe: DevProbe, name: &str) -> Option<DevNode> {
    all_nodes(probe)
        .find(|n| n.name == name && visible(probe, n, false))
        .copied()
}

/// Every node that should appear in `ls /dev`, in a stable order.
///
/// Yields nothing inside a box — see [`DevProbe::in_box`]. Returns an iterator
/// rather than a collection so a caller that only needs "is there anything
/// here?" pays nothing; the one caller that needs a real listing
/// (`vfs::dev_entries`) allocates the `DirEntry` vector it was going to build
/// anyway.
pub fn list(probe: DevProbe) -> impl Iterator<Item = &'static DevNode> {
    all_nodes(probe).filter(move |n| visible(probe, n, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// A host probe: sound device present, two disks, not in a box.
    fn host() -> DevProbe {
        DevProbe { audio: true, block_slots: 0b0011, in_box: false }
    }

    /// Collecting is the test's own convenience — the crate itself never does.
    fn names(probe: DevProbe) -> Vec<&'static str> {
        list(probe).map(|n| n.name).collect()
    }

    fn block_names(probe: DevProbe) -> Vec<&'static str> {
        names(probe).into_iter().filter(|n| n.starts_with("vd")).collect()
    }



    #[test]
    fn the_gap_this_table_closes() {
        // The bug from DEVFS_MISSING.md §1.2: these three `open()`ed fine but
        // `stat()`ed ENOENT because only null/zero were in the stat path.
        for name in ["random", "urandom", "dsp"] {
            assert!(lookup(host(), name).is_some(), "{name} must stat");
        }
    }

    #[test]
    fn audio_nodes_gated_on_the_probe() {
        let mut p = host();
        assert!(lookup(p, "dsp").is_some());
        assert!(lookup(p, "audio").is_some());
        p.audio = false;
        assert!(lookup(p, "dsp").is_none());
        assert!(lookup(p, "audio").is_none());
    }

    #[test]
    fn block_nodes_track_populated_slots() {
        assert_eq!(block_names(host()), ["vda", "vdb"]);
        assert!(lookup(host(), "vdc").is_none());

        // A gap in the middle still names the right letters: slot index, not
        // registration order, picks the name (matching `device_name(idx)`).
        let sparse = DevProbe { block_slots: 0b1001, ..Default::default() };
        assert_eq!(block_names(sparse), ["vda", "vdd"]);
    }

    #[test]
    fn block_nodes_are_block_type_with_partition_spaced_minors() {
        let p = DevProbe { block_slots: 0b1111, ..Default::default() };
        let vdc = lookup(p, "vdc").unwrap();
        assert!(vdc.is_block);
        assert_eq!(vdc.d_type(), DT_BLK);
        assert_eq!(vdc.mode(), 0o60660);
        assert_eq!((vdc.major, vdc.minor), (254, 32));
    }

    #[test]
    fn null_and_zero_keep_the_values_the_old_hardcoded_blocks_reported() {
        let null = lookup(host(), "null").unwrap();
        assert_eq!((null.mode(), null.ino, null.major, null.minor), (0o20666, 1, 1, 3));
        let zero = lookup(host(), "zero").unwrap();
        assert_eq!((zero.mode(), zero.ino, zero.major, zero.minor), (0o20666, 5, 1, 5));
    }

    #[test]
    fn inodes_are_unique() {
        let p = DevProbe { audio: true, block_slots: 0b1111, in_box: false };
        let mut inos: Vec<u64> = list(p).map(|n| n.ino).collect();
        let total = inos.len();
        inos.sort_unstable();
        inos.dedup();
        assert_eq!(inos.len(), total);
    }

    /// Every table entry must be reachable by name, or it can never be `stat`ed.
    #[test]
    fn every_listed_node_is_also_lookupable() {
        let p = DevProbe { audio: true, block_slots: 0b1111, in_box: false };
        for node in list(p) {
            assert_eq!(lookup(p, node.name).as_ref(), Some(node), "{}", node.name);
        }
    }

    #[test]
    fn a_box_gets_no_listing_at_all() {
        let p = DevProbe { audio: true, block_slots: 0b1111, in_box: true };
        assert_eq!(list(p).count(), 0);
    }

    #[test]
    fn a_box_never_sees_host_hardware() {
        let p = DevProbe { audio: true, block_slots: 0b1111, in_box: true };
        for name in ["vda", "vdb", "vdc", "vdd", "dsp", "audio", "random", "urandom"] {
            assert!(lookup(p, name).is_none(), "{name} must not leak into a box");
        }
    }

    #[test]
    fn a_box_keeps_the_two_devices_that_already_stat_today() {
        let p = DevProbe { in_box: true, ..Default::default() };
        assert_eq!(lookup(p, "null").map(|n| n.mode()), Some(0o20666));
        assert_eq!(lookup(p, "zero").map(|n| n.mode()), Some(0o20666));
    }

    #[test]
    fn unknown_names_and_paths_never_match() {
        for name in ["", "vde", "nul", "null/", "net/tap0", "console"] {
            assert!(lookup(host(), name).is_none(), "{name:?} must not resolve");
        }
    }
}
