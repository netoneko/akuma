use akuma_net::socket::libc_errno;

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

    if !crate::ramfb::is_initialized() {
        return (-libc_errno::EIO as i64) as u64;
    }

    let src = unsafe { core::slice::from_raw_parts(buf_ptr as *const u8, buf_len) };

    let copied = crate::ramfb::draw(src);
    if copied == 0 {
        (-libc_errno::EIO as i64) as u64
    } else {
        copied as u64
    }
}

pub(super) fn sys_fb_info(info_ptr: u64) -> u64 {
    if info_ptr == 0 {
        return (-libc_errno::EINVAL as i64) as u64;
    }

    match crate::ramfb::info() {
        Some(info) => {
            unsafe {
                core::ptr::write(info_ptr as *mut crate::ramfb::FBInfo, info);
            }
            0
        }
        None => (-libc_errno::EIO as i64) as u64,
    }
}
