// SPDX-License-Identifier: BSD-2-Clause
//! Linux seccomp and prctl wire ABI for the two native 64-bit targets.

pub const SET_MODE_FILTER: u32 = 1;
pub const GET_ACTION_AVAIL: u32 = 2;
pub const MODE_FILTER: u64 = 2;

pub const PR_GET_SECCOMP: i32 = 21;
pub const PR_SET_SECCOMP: i32 = 22;
pub const PR_SET_NO_NEW_PRIVS: i32 = 38;
pub const PR_GET_NO_NEW_PRIVS: i32 = 39;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SockFprog {
    pub len: u16,
    pub padding: [u8; 6],
    pub filter: u64,
}

akuma_pod::pod!(SockFprog {
    len: u16,
    padding: [u8; 6],
    filter: u64
});

const _: () = {
    assert!(core::mem::size_of::<SockFprog>() == 16);
    assert!(core::mem::offset_of!(SockFprog, filter) == 8);
};
