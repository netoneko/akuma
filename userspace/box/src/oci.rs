use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::{print, println, print_dec, unlink};
use libakuma_tls::{https_get, download_file_with_headers, HttpHeaders};

use crate::json;
use crate::images;

pub struct ImageRef {
    pub registry: String,
    pub name: String,
    pub tag: String,
}

pub fn parse_image_ref(s: &str) -> ImageRef {
    let (name_part, tag) = match s.rfind(':') {
        Some(pos) => {
            let after = &s[pos + 1..];
            if after.contains('/') {
                (s, "latest")
            } else {
                (&s[..pos], after)
            }
        }
        None => (s, "latest"),
    };

    let (registry, name) = if let Some(slash_pos) = name_part.find('/') {
        let first = &name_part[..slash_pos];
        if first.contains('.') || first.contains(':') {
            let reg = if first == "docker.io" {
                "registry-1.docker.io"
            } else {
                first
            };
            (String::from(reg), String::from(&name_part[slash_pos + 1..]))
        } else {
            (String::from("registry-1.docker.io"), String::from(name_part))
        }
    } else {
        (String::from("registry-1.docker.io"), format!("library/{}", name_part))
    };

    ImageRef {
        registry,
        name,
        tag: String::from(tag),
    }
}

fn fetch_token(image: &ImageRef) -> Result<String, String> {
    if image.registry != "registry-1.docker.io" {
        return Ok(String::new());
    }

    print("  Fetching auth token...\n");
    let url = format!(
        "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
        image.name
    );
    let headers = HttpHeaders::new();
    let body = https_get(&url, &headers)
        .map_err(|e| format!("token fetch failed: {:?}", e))?;
    let body_str = core::str::from_utf8(&body)
        .map_err(|_| String::from("invalid token response"))?;

    json::extract_string(body_str, "token")
        .ok_or_else(|| String::from("no token in auth response"))
}

struct Manifest {
    config_digest: String,
    layer_digests: Vec<String>,
}

fn fetch_manifest(image: &ImageRef, token: &str) -> Result<Manifest, String> {
    print("  Fetching manifest...\n");

    let url = format!(
        "https://{}/v2/{}/manifests/{}",
        image.registry, image.name, image.tag
    );
    let mut headers = HttpHeaders::new();
    headers.add(
        "Accept",
        "application/vnd.docker.distribution.manifest.v2+json, \
         application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.list.v2+json, \
         application/vnd.oci.image.index.v1+json",
    );
    if !token.is_empty() {
        headers.bearer_auth(token);
    }

    let body = https_get(&url, &headers)
        .map_err(|e| format!("manifest fetch failed: {:?}", e))?;
    let body_str = core::str::from_utf8(&body)
        .map_err(|_| String::from("invalid manifest response"))?;

    print("  Response length: ");
    print_dec(body_str.len());
    print("\n");
    let preview_len = core::cmp::min(body_str.len(), 300);
    print("  Response start: ");
    println(&body_str[..preview_len]);

    let media_type = json::extract_string(body_str, "mediaType").unwrap_or_default();
    let has_manifests = json::extract_array(body_str, "manifests").is_some();

    if media_type.contains("manifest.list") || media_type.contains("image.index")
        || has_manifests
    {
        return resolve_platform_manifest(body_str, image, token);
    }

    parse_manifest(body_str)
}

fn resolve_platform_manifest(list_json: &str, image: &ImageRef, token: &str) -> Result<Manifest, String> {
    let manifests_arr = json::extract_array(list_json, "manifests")
        .ok_or_else(|| String::from("no manifests array in manifest list"))?;

    for obj in json::iter_array_objects(manifests_arr) {
        if let Some(platform) = json::extract_object(obj, "platform") {
            let arch = json::extract_string(platform, "architecture").unwrap_or_default();
            let os = json::extract_string(platform, "os").unwrap_or_default();
            if (arch == "arm64" || arch == "aarch64") && os == "linux" {
                let digest = json::extract_string(obj, "digest")
                    .ok_or_else(|| String::from("no digest in platform manifest entry"))?;
                return fetch_manifest_by_digest(image, &digest, token);
            }
        }
    }

    Err(String::from("no linux/arm64 manifest found in manifest list"))
}

fn fetch_manifest_by_digest(image: &ImageRef, digest: &str, token: &str) -> Result<Manifest, String> {
    print("  Fetching platform manifest...\n");

    let url = format!(
        "https://{}/v2/{}/manifests/{}",
        image.registry, image.name, digest
    );
    let mut headers = HttpHeaders::new();
    headers.add(
        "Accept",
        "application/vnd.docker.distribution.manifest.v2+json, \
         application/vnd.oci.image.manifest.v1+json",
    );
    if !token.is_empty() {
        headers.bearer_auth(token);
    }

    let body = https_get(&url, &headers)
        .map_err(|e| format!("platform manifest fetch failed: {:?}", e))?;
    let body_str = core::str::from_utf8(&body)
        .map_err(|_| String::from("invalid platform manifest response"))?;

    parse_manifest(body_str)
}

fn parse_manifest(json_str: &str) -> Result<Manifest, String> {
    let config_obj = json::extract_object(json_str, "config")
        .ok_or_else(|| String::from("no config in manifest"))?;
    let config_digest = json::extract_string(config_obj, "digest")
        .ok_or_else(|| String::from("no digest in manifest config"))?;

    let layers_arr = json::extract_array(json_str, "layers")
        .ok_or_else(|| String::from("no layers in manifest"))?;

    let mut layer_digests = Vec::new();
    for obj in json::iter_array_objects(layers_arr) {
        let digest = json::extract_string(obj, "digest")
            .ok_or_else(|| String::from("no digest in layer entry"))?;
        layer_digests.push(digest);
    }

    Ok(Manifest {
        config_digest,
        layer_digests,
    })
}

fn fetch_config(image: &ImageRef, digest: &str, token: &str) -> Result<String, String> {
    print("  Fetching config...\n");

    let url = format!(
        "https://{}/v2/{}/blobs/{}",
        image.registry, image.name, digest
    );
    let mut headers = HttpHeaders::new();
    if !token.is_empty() {
        headers.bearer_auth(token);
    }

    let tmp_path = "/tmp/oci-config.json";
    download_file_with_headers(&url, tmp_path, &headers)
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
    let url = format!(
        "https://{}/v2/{}/blobs/{}",
        image.registry, image.name, digest
    );
    let mut headers = HttpHeaders::new();
    if !token.is_empty() {
        headers.bearer_auth(token);
    }

    download_file_with_headers(&url, dest_path, &headers)
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
    let image = parse_image_ref(image_str);
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
