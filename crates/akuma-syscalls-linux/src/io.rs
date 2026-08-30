//! The scatter/gather and readiness wire types: `struct iovec`,
//! `struct pollfd`, `struct epoll_event`, and the `aio_ring` header.

/// Linux `struct iovec`.
///
/// `iov_base` is kept as a `u64` rather than a pointer because it is a *user*
/// address that has not been validated yet — giving it a pointer type would
/// invite dereferencing it before `validate_user_ptr` runs.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct IoVec {
    pub iov_base: u64,
    pub iov_len: usize,
}

/// Linux `struct pollfd`.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

/// Linux `struct epoll_event`.
///
/// **On aarch64 this is NOT `__attribute__((packed))`**, unlike x86-64, where
/// the same struct is 12 bytes. The explicit `_pad` word is what makes the Rust
/// definition agree with the C one; drop it and every event after the first is
/// read from the wrong offset by userspace.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct EpollEvent {
    pub events: u32,
    /// aarch64 ABI padding — see the type's doc comment.
    pub _pad: u32,
    pub data: u64,
}

/// The `struct aio_ring` header `io_setup(2)` maps into userspace.
///
/// Only the header is ABI here; the ring's sizing policy (one page) stays in
/// `src/syscall/aio.rs`. `bun` dereferences the context pointer immediately
/// after `io_setup` returns, which is why this has to be a real mapped ring and
/// not a small integer handle.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct AioRingHeader {
    pub id: u32,
    pub nr: u32,
    pub head: u32,
    pub tail: u32,
    pub magic: u32,
    pub compat_features: u32,
    pub incompat_features: u32,
    pub header_length: u32,
}

/// `AIO_RING_MAGIC` — what glibc checks before it will read `head`/`tail` out
/// of the ring itself instead of calling `io_getevents`.
pub const AIO_RING_MAGIC: u32 = 0xa10a_10a1;

/// `sizeof(struct aio_ring)`.
///
/// Taken from the type rather than restated as `32`, which is how
/// `src/syscall/aio.rs` used to spell it: the value it writes into the ring's
/// own `header_length` field and the value it uses to size the event area were
/// two independent literals that both had to match this struct.
pub const AIO_RING_HEADER_SIZE: u32 = core::mem::size_of::<AioRingHeader>() as u32;

/// `sizeof(struct io_event)` — the stride of the event array that follows the
/// header. Not a struct here because nothing in this kernel fills one yet.
pub const AIO_RING_EVENT_SIZE: usize = 32;

const _: () = assert!(core::mem::size_of::<IoVec>() == 16);
const _: () = assert!(core::mem::offset_of!(IoVec, iov_len) == 8);
const _: () = assert!(core::mem::size_of::<PollFd>() == 8);
const _: () = assert!(core::mem::offset_of!(PollFd, events) == 4);
const _: () = assert!(core::mem::offset_of!(PollFd, revents) == 6);
// 16, not the 12 an x86-64 `struct epoll_event` occupies.
const _: () = assert!(core::mem::size_of::<EpollEvent>() == 16);
const _: () = assert!(core::mem::offset_of!(EpollEvent, data) == 8);
const _: () = assert!(core::mem::size_of::<AioRingHeader>() == 32);

#[cfg(test)]
mod tests {
    use super::*;

    /// `revents` is the field the kernel writes back, and it sits in the top
    /// half of the second word. An `events: i32` would silently make the kernel
    /// clobber `fd` of the *next* entry in the array.
    #[test]
    fn pollfd_revents_is_the_last_two_bytes() {
        let p = PollFd { fd: -1, events: 0x0102, revents: 0x0304 };
        assert_eq!(core::mem::offset_of!(PollFd, fd), 0);
        assert_eq!(core::mem::size_of_val(&p.fd), 4);
        assert_eq!(core::mem::offset_of!(PollFd, events), 4);
        assert_eq!(core::mem::size_of_val(&p.events), 2, "an i32 here clobbers the next fd");
        assert_eq!(core::mem::offset_of!(PollFd, revents), 6);
        assert_eq!(core::mem::size_of_val(&p.revents), 2);
        assert_eq!(core::mem::size_of::<PollFd>(), 8, "no tail padding after revents");
    }

    /// The aarch64-vs-x86-64 difference, stated as a test because it is the one
    /// fact about this struct anybody gets wrong: an array of two events has the
    /// second one at byte 16.
    #[test]
    fn epoll_event_array_stride_is_16_not_12() {
        assert_eq!(core::mem::size_of::<EpollEvent>(), 16, "the array stride");
        assert_eq!(core::mem::size_of::<[EpollEvent; 2]>(), 32, "so events[1] starts at 16");
        // `_pad` is what buys the stride: without it `data` would sit at 4 and
        // the struct would be 12.
        assert_eq!(core::mem::offset_of!(EpollEvent, events), 0);
        assert_eq!(core::mem::offset_of!(EpollEvent, _pad), 4);
        assert_eq!(core::mem::offset_of!(EpollEvent, data), 8);
    }

    /// `iov_len` is `size_t`, so the pair is 16 bytes and a `readv` of N
    /// vectors strides by 16.
    #[test]
    fn iovec_stride_is_16() {
        assert_eq!(core::mem::size_of::<[IoVec; 3]>(), 48);
    }
}
