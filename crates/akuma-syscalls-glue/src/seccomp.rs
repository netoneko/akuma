// SPDX-License-Identifier: BSD-2-Clause
//! The seccomp syscall's user copies and process-state changes.

use super::{copy_from_user, read_user_into, validate_user_ptr};
use akuma_exec::process::{current_creds, current_process_shared};
use akuma_primitives::errno::Errno;
use akuma_seccomp::{Instruction, MAX_INSNS, Program, action_available};
use akuma_syscalls_linux::seccomp::{
    GET_ACTION_AVAIL, MODE_FILTER, PR_GET_NO_NEW_PRIVS, PR_GET_SECCOMP, PR_SET_NO_NEW_PRIVS,
    PR_SET_SECCOMP, SET_MODE_FILTER, SockFprog,
};
use alloc::vec::Vec;

fn install(addr: u64) -> Result<u64, Errno> {
    let mut fprog = SockFprog::default();
    read_user_into(&mut fprog, addr)?;
    let len = usize::from(fprog.len);
    if !(1..=MAX_INSNS).contains(&len) {
        return Err(Errno::EINVAL);
    }

    let proc = current_process_shared().ok_or(Errno::ESRCH)?;
    // capget exposes the effective CAP_SYS_ADMIN bit for euid 0. Akuma has
    // no independently mutable capability sets or user namespaces.
    if !proc.seccomp.no_new_privs() && !current_creds().is_root() {
        return Err(Errno::EACCES);
    }
    if fprog.filter == 0 {
        return Err(Errno::EINVAL);
    }
    if !validate_user_ptr(fprog.filter, len * Instruction::SIZE) {
        return Err(Errno::EFAULT);
    }
    if !proc.seccomp.has_room(len) {
        return Err(Errno::ENOMEM);
    }

    let mut instructions = Vec::new();
    instructions
        .try_reserve_exact(len)
        .map_err(|_| Errno::ENOMEM)?;
    let mut chunk = [[0u8; Instruction::SIZE]; 32];
    for start in (0..len).step_by(chunk.len()) {
        let count = (len - start).min(chunk.len());
        let buf = &mut chunk[..count];
        let addr = fprog.filter + (start * Instruction::SIZE) as u64;
        copy_from_user(akuma_pod::as_bytes_mut(buf), addr)?;
        instructions.extend(buf.iter().copied().map(Instruction::from_ne_bytes));
    }
    let program = Program::new(instructions).map_err(|_| Errno::EINVAL)?;
    proc.seccomp.install(program)?;
    Ok(0)
}

pub(crate) fn syscall(op: u32, flags: u32, addr: u64) -> u64 {
    let result = match (op, flags) {
        (SET_MODE_FILTER, 0) => install(addr),
        (GET_ACTION_AVAIL, 0) => {
            let mut action = 0u32;
            read_user_into(&mut action, addr).and_then(|()| {
                if action_available(action) {
                    Ok(0)
                } else {
                    Err(Errno::EOPNOTSUPP)
                }
            })
        }
        _ => Err(Errno::EINVAL),
    };
    result.unwrap_or_else(Errno::ret)
}

pub(crate) fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> Option<u64> {
    let result = match option {
        PR_SET_SECCOMP if arg2 == MODE_FILTER => install(arg3),
        PR_SET_SECCOMP => Err(Errno::EINVAL),
        PR_GET_SECCOMP => Ok(current_process_shared()
            .filter(|p| p.seccomp.has_filter())
            .map_or(0, |_| MODE_FILTER)),
        PR_SET_NO_NEW_PRIVS if (arg2, arg3, arg4, arg5) == (1, 0, 0, 0) => {
            current_process_shared().ok_or(Errno::ESRCH).map(|p| {
                p.seccomp.set_no_new_privs();
                0
            })
        }
        PR_GET_NO_NEW_PRIVS if (arg2, arg3, arg4, arg5) == (0, 0, 0, 0) => Ok(u64::from(
            current_process_shared().is_some_and(|p| p.seccomp.no_new_privs()),
        )),
        PR_SET_NO_NEW_PRIVS | PR_GET_NO_NEW_PRIVS => Err(Errno::EINVAL),
        _ => return None,
    };
    Some(result.unwrap_or_else(Errno::ret))
}
