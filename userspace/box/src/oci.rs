//! `box pull` — fetch an OCI image from a registry onto disk.
//!
//! The decisions (which reference, which platform, which digests) live in the
//! host-testable `boxlib` half; this file is the I/O: HTTPS, blobs, extraction.

use alloc::format;
use alloc::string::String;
use libakuma::{print, println, print_dec, unlink};
use libakuma_tls::{https_get, download_file_with_headers, HttpHeaders};

use boxlib::json;
use boxlib::manifest::{self, Manifest};
use boxlib::oci_ref::ImageRef;

use crate::images;

/// Accept every manifest media type a registry might answer with. Without the
/// list types the registry hands back a single-platform manifest chosen for
/// *its* idea of our architecture.
const ACCEPT_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.oci.image.index.v1+json";

fn auth_headers(token: &str) -> HttpHeaders {
    let mut headers = HttpHeaders::new();
    if !token.is_empty() {
        headers.bearer_auth(token);
    }
    headers
}

fn fetch_token(image: &ImageRef) -> Result<String, String> {
    if !image.needs_token() {
        return Ok(String::new());
    }

    print("  Fetching auth token...\n");
    let headers = HttpHeaders::new();
    let body = https_get(&image.token_url(), &headers)
        .map_err(|e| format!("token fetch failed: {:?}", e))?;
    let body_str = core::str::from_utf8(&body)
        .map_err(|_| String::from("invalid token response"))?;

    json::string_at(body_str, &["token"])
        .ok_or_else(|| String::from("no token in auth response"))
}

fn fetch_manifest(image: &ImageRef, token: &str) -> Result<Manifest, String> {
    print("  Fetching manifest...\n");

    let mut headers = auth_headers(token);
    headers.add("Accept", ACCEPT_MANIFEST);

    let body = https_get(&image.manifest_url(&image.tag), &headers)
        .map_err(|e| format!("manifest fetch failed: {:?}", e))?;
    let body_str = core::str::from_utf8(&body)
        .map_err(|_| String::from("invalid manifest response"))?;

    if manifest::is_manifest_list(body_str) {
        let digest = manifest::select_platform_digest(body_str)?;
        return fetch_manifest_by_digest(image, &digest, token);
    }

    manifest::parse_manifest(body_str)
}

fn fetch_manifest_by_digest(image: &ImageRef, digest: &str, token: &str) -> Result<Manifest, String> {
    print("  Fetching platform manifest...\n");

    let mut headers = auth_headers(token);
    headers.add("Accept", ACCEPT_MANIFEST);

    let body = https_get(&image.manifest_url(digest), &headers)
        .map_err(|e| format!("platform manifest fetch failed: {:?}", e))?;
    let body_str = core::str::from_utf8(&body)
        .map_err(|_| String::from("invalid platform manifest response"))?;

    manifest::parse_manifest(body_str)
}

fn fetch_config(image: &ImageRef, digest: &str, token: &str) -> Result<String, String> {
    print("  Fetching config...\n");

    let headers = auth_headers(token);
    let tmp_path = "/tmp/oci-config.json";
    download_file_with_headers(&image.blob_url(digest), tmp_path, &headers)
        .map_err(|e| format!("config fetch failed: {:?}", e))?;

    let fd = libakuma::open(tmp_path, 0);
    if fd < 0 {
        return Err(String::from("failed to open downloaded config"));
    }
    let mut buf = alloc::vec![0u8; 64 * 1024];
    let n = libakuma::read_fd(fd, &mut buf);
    libakuma::close(fd);
    unlink(tmp_path);

    if n <= 0 {
        return Err(String::from("failed to read config file"));
    }
    let body_str = core::str::from_utf8(&buf[..n as usize])
        .map_err(|_| String::from("invalid config response"))?;

    Ok(String::from(body_str))
}

fn download_layer(
    image: &ImageRef,
    digest: &str,
    token: &str,
    dest_path: &str,
) -> Result<(), String> {
    let headers = auth_headers(token);
    download_file_with_headers(&image.blob_url(digest), dest_path, &headers)
        .map_err(|e| format!("layer download failed: {:?}", e))
}

/// Unpack one gzipped layer tarball into `rootfs_path`.
///
/// Linked in rather than shelled out to: `/bin/tar` is a path, and for the whole
/// life of `box pull` that path was a busybox applet symlink whose hardlink
/// handling turned a 1.9 MB layer into 467 MB of copies with their mode bits
/// lost. A dependency cannot be swapped out from under us by a symlink.
fn extract_layer(layer_path: &str, rootfs_path: &str) -> Result<(), String> {
    let opts = akuma_tar::ExtractOptions { gzip: true, verbose: false, ..Default::default() };
    match akuma_tar::extract_file(layer_path, rootfs_path, &opts) {
        Ok(stats) => {
            if stats.rejected > 0 {
                return Err(format!("layer contains {} entries outside the target directory", stats.rejected));
            }
            Ok(())
        }
        Err(e) => Err(format!("extract failed: {}", e.describe())),
    }
}

pub fn pull_image(image_str: &str) -> Result<(), String> {
    let image = boxlib::oci_ref::parse_image_ref(image_str);
    let store_name = images::sanitize_name(image_str);

    print("box: pulling ");
    print(&image.registry);
    print("/");
    print(&image.name);
    print(":");
    println(&image.tag);

    let token = fetch_token(&image)?;
    let manifest = fetch_manifest(&image, &token)?;

    print("  Config: ");
    println(&manifest.config_digest);
    print("  Layers: ");
    print_dec(manifest.layer_digests.len());
    print("\n");

    let config_json = fetch_config(&image, &manifest.config_digest, &token)?;

    images::prepare_image_dir(&store_name)?;

    let total = manifest.layer_digests.len();
    for (i, digest) in manifest.layer_digests.iter().enumerate() {
        let short = if digest.len() > 19 { &digest[7..19] } else { digest.as_str() };
        let dest = images::layer_dir(digest);

        // Layers are content-addressed, so one already on disk is byte-identical
        // to the one this manifest names — skip the download entirely.
        if images::dir_exists(&dest) {
            print("  Layer ");
            print_dec(i + 1);
            print("/");
            print_dec(total);
            print(" (");
            print(short);
            print(") already present\n");
            continue;
        }

        print("  Downloading layer ");
        print_dec(i + 1);
        print("/");
        print_dec(total);
        print(" (");
        print(short);
        print(")...\n");

        let tmp_path = format!("/tmp/oci-layer-{}.tar.gz", i);
        download_layer(&image, digest, &token, &tmp_path)?;

        print("  Extracting layer ");
        print_dec(i + 1);
        print("/");
        print_dec(total);
        print("...\n");

        // Extract into a staging directory and rename it into place, so an
        // interrupted pull can never leave a half-populated directory that the
        // check above would then accept as complete.
        let staging = format!("{}.tmp", dest);
        if !libakuma::mkdir_p(&staging) {
            return Err(format!("failed to create {}", staging));
        }
        extract_layer(&tmp_path, &staging)?;
        unlink(&tmp_path);

        let rc = libakuma::rename(&staging, &dest);
        if rc < 0 {
            return Err(format!("failed to publish layer {}: errno {}", short, -rc));
        }
    }

    images::save_config(&store_name, &config_json)?;
    images::save_layers(&store_name, &manifest.layer_digests)?;

    print("  Image stored as '");
    print(&store_name);
    print("' at ");
    println(&images::image_dir(&store_name));

    Ok(())
}
