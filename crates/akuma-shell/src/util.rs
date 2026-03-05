//! Byte-level string utilities used throughout the shell.

/// Trim leading and trailing ASCII whitespace from a byte slice.
#[must_use]
pub fn trim_bytes(data: &[u8]) -> &[u8] {
    let start = data
        .iter()
        .position(|&b| !b.is_ascii_whitespace())
        .unwrap_or(data.len());
    let end = data
        .iter()
        .rposition(|&b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &data[start..end]
}

/// Split at first whitespace, returning (`first_word`, `rest_trimmed`).
#[must_use]
pub fn split_first_word(data: &[u8]) -> (&[u8], &[u8]) {
    data.iter()
        .position(|&b| b.is_ascii_whitespace())
        .map_or((data, &[] as &[u8]), |pos| {
            (&data[..pos], trim_bytes(&data[pos..]))
        })
}

/// Translate terminal escape sequences to simpler byte equivalents.
///
/// `\x1b[3~` (Delete key) is mapped to `\x7f` so apps like neatvi
/// delete in insert mode.
#[must_use]
pub fn translate_input_keys(data: &[u8]) -> alloc::vec::Vec<u8> {
    let mut result = alloc::vec::Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 3 < data.len()
            && data[i] == 0x1b
            && data[i + 1] == b'['
            && data[i + 2] == b'3'
            && data[i + 3] == b'~'
        {
            result.push(0x7f);
            i += 4;
        } else {
            result.push(data[i]);
            i += 1;
        }
    }
    result
}
