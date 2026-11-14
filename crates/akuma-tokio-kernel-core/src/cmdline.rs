//! The boot command line's `token=value` words. The machine half finds the line (PVH start
//! info on amd64, the DTB's `/chosen/bootargs` on aarch64); this reads it.

/// The value of the first `prefix` word on `line` (`prefix` includes the `=`).
#[must_use]
pub fn token<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.split_ascii_whitespace().find_map(|t| t.strip_prefix(prefix))
}

/// [`token`], copied into `buf` (truncated to it).
pub fn token_into<'a>(line: &str, prefix: &str, buf: &'a mut [u8]) -> Option<&'a str> {
    let value = token(line, prefix)?;
    let n = value.len().min(buf.len());
    buf[..n].copy_from_slice(&value.as_bytes()[..n]);
    core::str::from_utf8(&buf[..n]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_first_token() {
        assert_eq!(token("console=ttyS0 smp=4 smp=2", "smp="), Some("4"));
        assert_eq!(token("console=ttyS0", "smp="), None);
        assert_eq!(token("xsmp=4", "smp="), None);
    }

    #[test]
    fn copies_truncated() {
        let mut b = [0u8; 3];
        assert_eq!(token_into("epoch=123456", "epoch=", &mut b), Some("123"));
    }
}
