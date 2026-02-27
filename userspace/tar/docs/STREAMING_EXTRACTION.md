# Streaming Tar Extraction

## Problem

The original `tar` implementation loaded entire archives into memory using `read_file_to_vec()`, then passed the full byte slice to `TarArchiveRef` for parsing. For large archives (e.g. xbps.tar at ~56 MB), this exceeded the userspace heap budget (~33 MB) and caused OOM.

## Solution: Dual-Path Extraction

The `untar()` function now selects between two extraction strategies based on whether gzip decompression is needed:

| Path | Flag | Function | Memory usage |
|------|------|----------|-------------|
| Streaming | `-xvf` (no `-z`) | `untar_streaming()` | ~4 KB buffer + 512 B header |
| In-memory | `-xzvf` (gzip) | `untar_in_memory()` | Full archive + decompressed copy |

### Streaming path (`untar_streaming`)

Parses the tar format directly from a file descriptor:

1. Read a 512-byte header block
2. Validate the header checksum
3. Parse filename, size, and type flag
4. For regular files: read data in 4 KB chunks, writing each chunk to the output file immediately
5. Read past any padding to the next 512-byte boundary
6. Repeat until two consecutive zero blocks (end of archive) or EOF

Peak memory: one 512-byte header + one 4 KB I/O buffer. The archive contents never accumulate in memory.

### In-memory path (`untar_in_memory`)

Used only for `.tar.gz` archives where the full compressed payload must be available for decompression. Still improved from the original: the compressed `raw_data` is `drop()`-ed immediately after decompression so only one copy (the decompressed data) is held at a time.

## Tar Format Details

The streaming parser handles the following tar header layout (offsets in bytes):

| Offset | Length | Field |
|--------|--------|-------|
| 0 | 100 | Filename |
| 124 | 12 | File size (octal, null/space terminated) |
| 148 | 8 | Header checksum (octal) |
| 156 | 1 | Type flag (`'0'`/`'\0'` = file, `'5'` = directory) |
| 257 | 6 | USTAR magic (`"ustar\0"`) |
| 345 | 155 | USTAR filename prefix |

### USTAR prefix handling

The prefix field at offset 345 is only read when the magic at offset 257 is exactly `"ustar\0"`. GNU-format archives use `"ustar "` (trailing space) and repurpose those bytes for other data. Reading the prefix unconditionally from non-USTAR archives produces garbage filenames.

### Checksum validation

Every header is validated before use. The checksum is the sum of all 512 header bytes, treating the 8-byte checksum field (offset 148-155) as ASCII spaces (`0x20`). Both unsigned and signed byte arithmetic are checked for compatibility. A failed checksum indicates the parser has read file data as a header (misalignment) and extraction stops immediately.

### Extended header types

The following type flags are recognized and skipped (their data blocks are consumed but not extracted):

- `'x'` — pax extended header (per-entry metadata)
- `'g'` — pax global extended header
- `'L'` — GNU long filename
- `'K'` — GNU long link name

### Padding and alignment

Tar entries are padded to 512-byte boundaries. After writing file data, the parser reads and discards the remaining padding bytes using `read_skip()` (read-based, not `lseek`). This avoids depending on `lseek(SEEK_CUR)` which may not be reliable for all file descriptor types in the kernel.

## Kernel-Side Streaming Download (`pkg install --streaming`)

For the `pkg install` command in the kernel shell (`src/shell/commands/net.rs`), a `--streaming` flag was added that downloads archive files directly to disk without buffering the entire HTTP response body in memory.

```
pkg install --streaming xbps
```

The `http_get_streaming_to_file()` function:

1. Sends an HTTP/1.0 GET request
2. Reads headers into a small buffer (up to 16 KB)
3. Opens the destination file via `AsyncFile`
4. Writes each TCP read (4 KB chunks) directly to disk
5. Prints progress every 512 KB

Without `--streaming`, the original in-memory download path is used (suitable for small packages).

## End-to-End Flow for Large Packages

```
pkg install --streaming xbps
```

1. HTTP body streams to `/tmp/xbps.tar` in 4 KB chunks (never in memory)
2. `tar -xvf /tmp/xbps.tar -C /` uses streaming extraction (4 KB + 512 B memory)
3. `/tmp/xbps.tar` is deleted after extraction
