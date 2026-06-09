use alloc::string::String;
use alloc::vec::Vec;

use crate::{access, close, fstat, open, open_flags, read_fd, write_fd};

pub fn read(path: &str) -> Result<Vec<u8>, i32> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(-fd);
    }
    let stat = match fstat(fd) {
        Ok(s) => s,
        Err(e) => {
            close(fd);
            return Err(e);
        }
    };
    let size = stat.st_size as usize;
    let mut buf = alloc::vec![0u8; size];
    let mut pos = 0;
    while pos < size {
        let n = read_fd(fd, &mut buf[pos..]);
        if n <= 0 {
            break;
        }
        pos += n as usize;
    }
    close(fd);
    Ok(buf)
}

pub fn read_to_string(path: &str) -> Result<String, i32> {
    let bytes = read(path)?;
    String::from_utf8(bytes).map_err(|_| 22) // EINVAL
}

pub fn write(path: &str, data: &[u8]) -> Result<(), i32> {
    let fd = open(
        path,
        open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC,
    );
    if fd < 0 {
        return Err(-fd);
    }
    let mut pos = 0;
    while pos < data.len() {
        let n = write_fd(fd, &data[pos..]);
        if n <= 0 {
            break;
        }
        pos += n as usize;
    }
    close(fd);
    Ok(())
}

pub fn exists(path: &str) -> bool {
    access(path, 0) == 0 // F_OK = 0
}
