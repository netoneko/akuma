use akuma_net::socket::libc_errno;
use akuma_exec::mmu::user_access::{copy_from_user_safe, copy_to_user_safe};
use super::validate_user_ptr;
use super::EFAULT;

pub(super) fn sys_fb_init(width: u32, height: u32) -> u64 {
    if width == 0 || height == 0 || width > 1920 || height > 1080 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    match crate::ramfb::init(width, height) {
        Ok(()) => 0,
        Err(_) => (-libc_errno::EIO as i64) as u64,
    }
}

pub(super) fn sys_fb_draw(buf_ptr: u64, buf_len: usize) -> u64 {
    if buf_ptr == 0 || buf_len == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }
    if !validate_user_ptr(buf_ptr, buf_len) { return EFAULT; }

    if !crate::ramfb::is_initialized() {
        return (-libc_errno::EIO as i64) as u64;
    }

    // Use a large kernel buffer for FB drawing (e.g. 1MB chunk)
    let chunk_size = buf_len.min(1024 * 1024);
    let mut kernel_buf = alloc::vec![0u8; chunk_size];
    let mut total_copied = 0;

    while total_copied < buf_len {
        let this_chunk = (buf_len - total_copied).min(chunk_size);
        if unsafe { copy_from_user_safe(kernel_buf.as_mut_ptr(), (buf_ptr as usize + total_copied) as *const u8, this_chunk).is_err() } {
            if total_copied > 0 { return total_copied as u64; }
            return EFAULT;
        }
        let copied = crate::ramfb::draw(&kernel_buf[..this_chunk]);
        if copied == 0 {
            if total_copied > 0 { return total_copied as u64; }
            return (-libc_errno::EIO as i64) as u64;
        }
        total_copied += this_chunk;
    }
    total_copied as u64
}

pub(super) fn sys_fb_info(info_ptr: u64) -> u64 {
    if info_ptr == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }
    if !validate_user_ptr(info_ptr, core::mem::size_of::<crate::ramfb::FBInfo>()) { return EFAULT; }

    match crate::ramfb::info() {
        Some(info) => {
            if unsafe { copy_to_user_safe(info_ptr as *mut u8, &info as *const crate::ramfb::FBInfo as *const u8, core::mem::size_of::<crate::ramfb::FBInfo>()).is_err() } {
                return EFAULT;
            }
            0
        }
        None => (-libc_errno::EIO as i64) as u64,
    }
}
