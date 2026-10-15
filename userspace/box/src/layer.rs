//! Streaming layer encoder: tar bytes in, `tar.gz` bytes out, with both digests.
//!
//! An OCI layer has two identities. The **diff_id** is the sha256 of the
//! *uncompressed* tar (what `rootfs.diff_ids` in the config lists); the
//! **digest** is the sha256 of the *compressed* blob (what the manifest lists,
//! and what `box` keys `/var/lib/box/layers/` by). One pass computes both and
//! holds only the compressor's window — never the layer — which is what lets a
//! multi-hundred-MB layer commit in bounded memory.
//!
//! The gzip header carries no timestamp and no name, so the same tar always
//! compresses to the same bytes: a rebuild of unchanged content gets the same
//! digest, which is what makes layer reuse real.

use alloc::vec::Vec;
use miniz_oxide::deflate::core::{
    compress, create_comp_flags_from_zip_params, CompressorOxide, TDEFLFlush, TDEFLStatus,
};
use sha2::{Digest, Sha256};

/// zlib-style level 6, raw deflate (no zlib wrapper — gzip has its own).
const LEVEL: i32 = 6;
const OUT_CHUNK: usize = 64 * 1024;

pub struct LayerWriter<F: FnMut(&[u8])> {
    sink: F,
    comp: CompressorOxide,
    diff: Sha256,
    blob: Sha256,
    crc: crc32fast::Hasher,
    isize: u32,
    uncompressed: u64,
    compressed: u64,
    out: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// `sha256:<hex>` of the uncompressed tar.
    pub diff_id: alloc::string::String,
    /// `sha256:<hex>` of the gzip blob.
    pub digest: alloc::string::String,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
}

fn hex_digest(h: Sha256) -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::from("sha256:");
    for b in h.finalize() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl<F: FnMut(&[u8])> LayerWriter<F> {
    /// `sink` receives the gzip stream in order, as it is produced.
    pub fn new(sink: F) -> Self {
        let mut w = Self {
            sink,
            // strategy 0 = default; window_bits -15 = raw deflate.
            comp: CompressorOxide::new(create_comp_flags_from_zip_params(LEVEL, -15, 0)),
            diff: Sha256::new(),
            blob: Sha256::new(),
            crc: crc32fast::Hasher::new(),
            isize: 0,
            uncompressed: 0,
            compressed: 0,
            out: alloc::vec![0u8; OUT_CHUNK],
        };
        // magic, CM=deflate, FLG=0, MTIME=0, XFL=0, OS=255 (unknown).
        w.emit_raw(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff]);
        w
    }

    fn emit_raw(&mut self, bytes: &[u8]) {
        self.blob.update(bytes);
        self.compressed += bytes.len() as u64;
        (self.sink)(bytes);
    }

    /// Feed uncompressed tar bytes.
    pub fn write(&mut self, mut data: &[u8]) {
        self.diff.update(data);
        self.crc.update(data);
        self.isize = self.isize.wrapping_add(data.len() as u32);
        self.uncompressed += data.len() as u64;
        while !data.is_empty() {
            let (_, consumed, produced) = compress(&mut self.comp, data, &mut self.out, TDEFLFlush::None);
            if produced > 0 {
                let chunk = self.out[..produced].to_vec();
                self.emit_raw(&chunk);
            }
            data = &data[consumed..];
        }
    }

    /// Flush the compressor, write the gzip trailer, return both digests.
    pub fn finish(mut self) -> Finished {
        loop {
            let (status, _, produced) = compress(&mut self.comp, &[], &mut self.out, TDEFLFlush::Finish);
            if produced > 0 {
                let chunk = self.out[..produced].to_vec();
                self.emit_raw(&chunk);
            }
            if status == TDEFLStatus::Done {
                break;
            }
        }
        let crc = self.crc.clone().finalize();
        let mut trailer = [0u8; 8];
        trailer[..4].copy_from_slice(&crc.to_le_bytes());
        trailer[4..].copy_from_slice(&self.isize.to_le_bytes());
        self.emit_raw(&trailer);
        Finished {
            diff_id: hex_digest(self.diff),
            digest: hex_digest(self.blob),
            compressed_size: self.compressed,
            uncompressed_size: self.uncompressed,
        }
    }
}

/// `sha256:<hex>` of a byte slice (config and manifest digests).
pub fn sha256_digest(data: &[u8]) -> alloc::string::String {
    let mut h = Sha256::new();
    h.update(data);
    hex_digest(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn encode(data: &[u8], pieces: usize) -> (Vec<u8>, Finished) {
        let mut out = Vec::new();
        let mut w = LayerWriter::new(|b| out.extend_from_slice(b));
        for chunk in data.chunks(pieces.max(1)) {
            w.write(chunk);
        }
        let fin = w.finish();
        (out, fin)
    }

    fn gunzip(gz: &[u8]) -> Vec<u8> {
        assert_eq!(&gz[..4], &[0x1f, 0x8b, 8, 0]);
        let body = &gz[10..gz.len() - 8];
        let raw = miniz_oxide::inflate::decompress_to_vec(body).expect("valid deflate");
        let crc = u32::from_le_bytes(gz[gz.len() - 8..gz.len() - 4].try_into().unwrap());
        let isize = u32::from_le_bytes(gz[gz.len() - 4..].try_into().unwrap());
        assert_eq!(crc, crc32fast::hash(&raw));
        assert_eq!(isize as usize, raw.len());
        raw
    }

    fn sample() -> Vec<u8> {
        // Compressible but not trivially so, and bigger than OUT_CHUNK's worth
        // of output for the incompressible tail.
        let mut v = Vec::new();
        for i in 0..400_000u32 {
            v.push((i.wrapping_mul(2_654_435_761) >> 24) as u8 & if i % 7 == 0 { 0xff } else { 0x0f });
        }
        v
    }

    #[test]
    fn round_trips_and_digests_match_the_inputs() {
        let data = sample();
        let (gz, fin) = encode(&data, 8192);
        assert_eq!(gunzip(&gz), data);
        assert_eq!(fin.diff_id, sha256_digest(&data));
        assert_eq!(fin.digest, sha256_digest(&gz));
        assert_eq!(fin.compressed_size, gz.len() as u64);
        assert_eq!(fin.uncompressed_size, data.len() as u64);
    }

    #[test]
    fn output_does_not_depend_on_how_input_is_chunked() {
        let data = sample();
        let (a, fa) = encode(&data, 512);
        let (b, fb) = encode(&data, 100_000);
        let (c, _) = encode(&data, 1);
        assert_eq!(a, b);
        assert_eq!(a, c, "same tar must give the same blob and so the same digest");
        assert_eq!(fa, fb);
    }

    #[test]
    fn an_empty_layer_is_a_valid_gzip() {
        let (gz, fin) = encode(&[], 1);
        assert_eq!(gunzip(&gz), Vec::<u8>::new());
        // sha256 of the empty string.
        assert_eq!(fin.diff_id, "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn incompressible_data_still_round_trips() {
        let mut x = 0x1234_5678u32;
        let data: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let (gz, _) = encode(&data, 4096);
        assert_eq!(gunzip(&gz), data);
        let _ = vec![0u8; 0];
    }
}
