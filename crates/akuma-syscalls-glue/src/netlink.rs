//! A `NETLINK_ROUTE` socket that acknowledges and does nothing.
//!
//! bubblewrap's `--unshare-net` brings `lo` up in its new network namespace with two netlink requests
//! (`RTM_NEWADDR`, `RTM_NEWLINK`) and reads an `NLMSG_ERROR` acknowledgement for each. Akuma has no network
//! namespace and `lo` is already up, so there is nothing to configure; this answers the acknowledgements so the
//! sandbox setup can finish. It implements no other netlink request: a request without `NLM_F_ACK` gets no reply and
//! no data is ever returned. What it means for `--unshare-net` is in `docs/archive/BWRAP_ON_BOXES.md`.
//!
//! Gated by `sc-private-ns`.
use super::*;
use akuma_exec::process::{FileDescriptor, NetlinkAcks};
use alloc::sync::Arc;

const AF_NETLINK: i32 = 16;
const SOCK_RAW: i32 = 3;
const NETLINK_ROUTE: i32 = 0;
const NLMSG_ERROR: u16 = 2;
const NLM_F_ACK: u16 = 4;
/// `sizeof(struct nlmsghdr)`.
const HDR: usize = 16;
/// An acknowledgement: the header, `int error`, and the request's header echoed back.
const ACK_LEN: usize = HDR + 4 + HDR;

/// `socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE)`. Other netlink families and types are refused.
pub(super) fn socket(sock_type: i32, proto: i32) -> u64 {
    if sock_type & 0xf != SOCK_RAW || proto != NETLINK_ROUTE {
        return Errno::EINVAL.ret();
    }
    let Some(proc) = akuma_exec::process::current_process_shared() else { return ESRCH };
    let fd = proc.alloc_fd(FileDescriptor::NetlinkRoute(Arc::new(NetlinkAcks::new())));
    if sock_type & 0x8_0000 != 0 {
        proc.set_cloexec(fd);
    }
    if sock_type & 0x800 != 0 {
        proc.set_nonblock(fd);
    }
    u64::from(fd)
}

/// Whether `domain` is the one this module serves.
pub(super) const fn is_netlink(domain: i32) -> bool {
    domain == AF_NETLINK
}

fn acks_of(fd: u32) -> Option<Arc<NetlinkAcks>> {
    match akuma_exec::process::current_process_shared()?.get_fd(fd)? {
        FileDescriptor::NetlinkRoute(a) => Some(a),
        _ => None,
    }
}

/// `Some(result)` when `fd` is a netlink socket. `bind` just succeeds.
pub(super) fn bind(fd: u32) -> Option<u64> {
    acks_of(fd).map(|_| 0)
}

/// `sendto`/`sendmsg` on a netlink socket: queue an acknowledgement for each message that asked for one.
pub(super) fn send(fd: u32, buf_ptr: u64, len: usize) -> Option<u64> {
    let acks = acks_of(fd)?;
    if len > 4096 {
        return Some(Errno::EMSGSIZE.ret());
    }
    let mut buf = [0u8; 4096];
    if copy_from_user(&mut buf[..len], buf_ptr).is_err() {
        return Some(EFAULT);
    }
    let mut at = 0;
    while at + HDR <= len {
        let msg_len = u32::from_ne_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]) as usize;
        if msg_len < HDR || at + msg_len > len {
            return Some(EINVAL);
        }
        let flags = u16::from_ne_bytes([buf[at + 6], buf[at + 7]]);
        if flags & NLM_F_ACK != 0 {
            let mut hdr = [0u8; HDR];
            hdr.copy_from_slice(&buf[at..at + HDR]);
            if !acks.push(hdr) {
                return Some(Errno::ENOBUFS.ret());
            }
        }
        at += (msg_len + 3) & !3;
    }
    Some(len as u64)
}

/// `recvfrom`/`recvmsg` on a netlink socket: the oldest acknowledgement, or `EAGAIN` when there is none (this stub
/// never blocks: an ack is queued by the send that earns it).
pub(super) fn recv(fd: u32, buf_ptr: u64, len: usize) -> Option<u64> {
    let acks = acks_of(fd)?;
    let Some(req) = acks.pop() else { return Some(EAGAIN) };
    let mut ack = [0u8; ACK_LEN];
    ack[0..4].copy_from_slice(&(ACK_LEN as u32).to_ne_bytes());
    ack[4..6].copy_from_slice(&NLMSG_ERROR.to_ne_bytes());
    // flags 0; the sequence and port id of the request; error 0 (success); the request's header.
    ack[8..16].copy_from_slice(&req[8..16]);
    ack[HDR + 4..].copy_from_slice(&req);
    let n = ACK_LEN.min(len);
    if copy_to_user(buf_ptr, &ack[..n]).is_err() {
        return Some(EFAULT);
    }
    Some(n as u64)
}
