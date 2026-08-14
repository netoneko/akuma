use super::validate_user_ptr;
use super::{EFAULT, EINVAL, EIO, copy_from_user, write_user_val};

pub(super) fn sys_fb_init(width: u32, height: u32) -> u64 {
    if width == 0 || height == 0 || width > 1920 || height > 1080 {
        return EINVAL;
    }

    let _drv_bkl = super::fs::DriverBklGuard::new();
    match crate::ramfb::init(width, height) {
        Ok(()) => 0,
        Err(_) => EIO,
    }
}

pub(super) fn sys_fb_draw(buf_ptr: u64, buf_len: usize) -> u64 {
    if buf_ptr == 0 || buf_len == 0 {
        return EINVAL;
    }
    if !validate_user_ptr(buf_ptr, buf_len) { return EFAULT; }

    if !crate::ramfb::is_initialized() {
        return EIO;
    }

    let _drv_bkl = super::fs::DriverBklGuard::new();
    // Use a large kernel buffer for FB drawing (e.g. 1MB chunk)
    let chunk_size = buf_len.min(1024 * 1024);
    let mut kernel_buf = alloc::vec![0u8; chunk_size];
    let mut total_copied = 0;

    while total_copied < buf_len {
        let this_chunk = (buf_len - total_copied).min(chunk_size);
        if copy_from_user(&mut kernel_buf[..this_chunk], buf_ptr + total_copied as u64).is_err() {
            if total_copied > 0 { return total_copied as u64; }
            return EFAULT;
        }
        let copied = crate::ramfb::draw(&kernel_buf[..this_chunk]);
        if copied == 0 {
            if total_copied > 0 { return total_copied as u64; }
            return EIO;
        }
        total_copied += this_chunk;
    }
    total_copied as u64
}

pub(super) fn sys_fb_info(info_ptr: u64) -> u64 {
    if info_ptr == 0 {
        return EINVAL;
    }
    let _drv_bkl = super::fs::DriverBklGuard::new();
    match crate::ramfb::info() {
        Some(info) => {
            if write_user_val(info_ptr, &info).is_err() {
                return EFAULT;
            }
            0
        }
        None => EIO,
    }
}
