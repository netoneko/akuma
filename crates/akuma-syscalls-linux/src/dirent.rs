//! `struct linux_dirent64` — the record `getdents64(2)` streams into userspace.
//!
//! This is a *variable-length* wire record, which is why it is offsets and an
//! encoder here rather than a `#[repr(C)]` struct like everything in
//! [`stat`](crate::stat): `d_name` is a flexible array member, and the header in
//! front of it is deliberately **not** a whole number of its own alignment.
//!
//! That last point is the trap this module exists to close. The natural header
//! — `{ u64 d_ino, i64 d_off, u16 d_reclen, u8 d_type }` — has
//! `size_of == 24` under `repr(C)`, because C pads a struct out to its 8-byte
//! alignment. But `d_name` begins at **19**, immediately after `d_type`, with no
//! padding. Reaching for `size_of::<Header>()` as the name offset therefore
//! silently clobbers the first five bytes of every filename, and `ls` shows
//! garbage. `src/syscall/fs.rs` used to spell 0/8/16/18/19 as five bare literals
//! in a raw-pointer cursor for exactly this reason.

/// Byte offset of `d_ino` — the inode number.
pub const D_INO: usize = 0;
/// Byte offset of `d_off` — the seek offset of the *next* record.
pub const D_OFF: usize = 8;
/// Byte offset of `d_reclen` — the total length of this record, padding included.
pub const D_RECLEN: usize = 16;
/// Byte offset of `d_type` — one of the `DT_*` values.
pub const D_TYPE: usize = 18;
/// Byte offset of `d_name` — a NUL-terminated name, then zero padding to `d_reclen`.
pub const D_NAME: usize = 19;

/// `d_reclen` is rounded up to this so every record starts 8-byte aligned.
pub const ALIGN: usize = 8;

// The offsets are a chain, not five independent facts: each is the previous one
// plus that field's width. Asserted so a future edit that widens a field cannot
// leave the rest stale.
const _: () = assert!(D_OFF == D_INO + core::mem::size_of::<u64>());
const _: () = assert!(D_RECLEN == D_OFF + core::mem::size_of::<i64>());
const _: () = assert!(D_TYPE == D_RECLEN + core::mem::size_of::<u16>());
const _: () = assert!(D_NAME == D_TYPE + core::mem::size_of::<u8>());
// The whole point of the module header: the header is 19 bytes, not 24.
const _: () = assert!(D_NAME == 19);
const _: () = assert!(D_NAME < 24);

/// The `d_reclen` a record holding a `name_len`-byte name needs: the 19-byte
/// header, the name, its NUL terminator, rounded up to [`ALIGN`].
#[must_use]
pub const fn reclen(name_len: usize) -> usize {
    (D_NAME + name_len + 1 + (ALIGN - 1)) & !(ALIGN - 1)
}

/// Encode one complete record into `rec`, which must be exactly
/// [`reclen(name.len())`](reclen) bytes.
///
/// Writes the header, the name, its NUL, and zeroes the pad — it does not assume
/// the caller's buffer arrived zeroed, because "the pad happens to be zero
/// already" is a property of one call site, not of the format.
///
/// Returns `false` and writes nothing if `rec` is the wrong length, so a sizing
/// slip upstream cannot produce a half-written record that the reader would then
/// walk off the end of.
#[must_use]
pub fn encode(rec: &mut [u8], ino: u64, off: i64, d_type: u8, name: &[u8]) -> bool {
    let want = reclen(name.len());
    if rec.len() != want {
        return false;
    }
    let name_end = D_NAME + name.len();
    rec[D_INO..D_OFF].copy_from_slice(&ino.to_ne_bytes());
    rec[D_OFF..D_RECLEN].copy_from_slice(&off.to_ne_bytes());
    // `want` is bounded by the buffer the caller sized, and a name long enough to
    // overflow a u16 cannot fit one: `getdents64` buffers are far below 64 KB.
    rec[D_RECLEN..D_TYPE].copy_from_slice(&(want as u16).to_ne_bytes());
    rec[D_TYPE] = d_type;
    rec[D_NAME..name_end].copy_from_slice(name);
    rec[name_end..].fill(0);
    true
}

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this module is named for: `d_name` is at 19, but a
    /// `repr(C)` header struct measures 24. Anything that uses the struct size as
    /// the name offset eats the first five characters of every filename.
    #[test]
    fn name_offset_is_not_the_padded_header_size() {
        #[repr(C)]
        struct Header {
            d_ino: u64,
            d_off: i64,
            d_reclen: u16,
            d_type: u8,
        }
        assert_eq!(core::mem::size_of::<Header>(), 24);
        assert_eq!(D_NAME, 19);
        assert_eq!(core::mem::offset_of!(Header, d_ino), D_INO);
        assert_eq!(core::mem::offset_of!(Header, d_off), D_OFF);
        assert_eq!(core::mem::offset_of!(Header, d_reclen), D_RECLEN);
        assert_eq!(core::mem::offset_of!(Header, d_type), D_TYPE);
    }

    #[test]
    fn reclen_rounds_up_to_eight_and_leaves_room_for_the_nul() {
        // 19 header + 1 name + 1 NUL = 21 -> 24
        assert_eq!(reclen(1), 24);
        // 19 + 4 + 1 = 24, already aligned
        assert_eq!(reclen(4), 24);
        // 19 + 5 + 1 = 25 -> 32
        assert_eq!(reclen(5), 32);
        assert_eq!(reclen(0), 24);
        for n in 0..512 {
            let r = reclen(n);
            assert_eq!(r % ALIGN, 0, "reclen({n}) = {r} is not 8-aligned");
            assert!(r >= D_NAME + n + 1, "reclen({n}) = {r} has no room for the NUL");
            assert!(r < D_NAME + n + 1 + ALIGN, "reclen({n}) = {r} over-pads");
        }
    }

    #[test]
    fn encode_writes_every_field_and_nul_terminates() {
        let name = b"allocstress";
        let mut rec = alloc::vec![0xAAu8; reclen(name.len())];
        assert!(encode(&mut rec, 0x1122_3344_5566_7788, 0x42, 8, name));

        assert_eq!(
            u64::from_ne_bytes(rec[D_INO..D_OFF].try_into().unwrap()),
            0x1122_3344_5566_7788
        );
        assert_eq!(i64::from_ne_bytes(rec[D_OFF..D_RECLEN].try_into().unwrap()), 0x42);
        assert_eq!(
            u16::from_ne_bytes(rec[D_RECLEN..D_TYPE].try_into().unwrap()) as usize,
            rec.len()
        );
        assert_eq!(rec[D_TYPE], 8);
        assert_eq!(&rec[D_NAME..D_NAME + name.len()], name);
        assert_eq!(rec[D_NAME + name.len()], 0, "name is not NUL-terminated");
        // Poison must be gone from the pad — `encode` zeroes it rather than
        // trusting the caller's buffer to have arrived clean.
        assert!(
            rec[D_NAME + name.len()..].iter().all(|&b| b == 0),
            "pad still holds poison: {:?}",
            &rec[D_NAME + name.len()..]
        );
    }

    #[test]
    fn encode_refuses_a_wrong_sized_record_without_writing() {
        let name = b"x";
        let mut rec = alloc::vec![0xAAu8; reclen(name.len()) - 1];
        assert!(!encode(&mut rec, 1, 1, 8, name));
        assert!(rec.iter().all(|&b| b == 0xAA), "refused encode still wrote");

        let mut big = alloc::vec![0xAAu8; reclen(name.len()) + 8];
        assert!(!encode(&mut big, 1, 1, 8, name));
        assert!(big.iter().all(|&b| b == 0xAA), "refused encode still wrote");
    }

    /// A chain of records encoded back to back must walk cleanly by `d_reclen`,
    /// which is what `abi_write_probe`'s getdents64 arm checks on the guest.
    #[test]
    fn a_chain_of_records_walks_by_reclen() {
        let names: [&[u8]; 4] = [b"a", b"bb", b"cccccccc", b"ddddddddddddddddd"];
        let total: usize = names.iter().map(|n| reclen(n.len())).sum();
        let mut buf = alloc::vec![0u8; total];
        let mut at = 0;
        for (i, n) in names.iter().enumerate() {
            let rl = reclen(n.len());
            assert!(encode(&mut buf[at..at + rl], i as u64 + 1, 1, 8, n));
            at += rl;
        }
        assert_eq!(at, total);

        let mut off = 0;
        let mut seen = 0;
        while off < total {
            let rl = u16::from_ne_bytes(
                buf[off + D_RECLEN..off + D_TYPE].try_into().unwrap(),
            ) as usize;
            assert_eq!(rl % ALIGN, 0);
            let name_bytes = &buf[off + D_NAME..off + rl];
            let nul = name_bytes.iter().position(|&b| b == 0).expect("no NUL in record");
            assert_eq!(&name_bytes[..nul], names[seen]);
            off += rl;
            seen += 1;
        }
        assert_eq!(seen, names.len());
        assert_eq!(off, total, "the reclen chain did not land on the buffer end");
    }
}
