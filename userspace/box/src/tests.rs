//! `box test` — the on-target suite.
//!
//! Deliberately small. Everything that is pure logic (JSON, image references,
//! manifests, paths, argv composition) is unit-tested on the **host** against
//! `boxlib`, where the whole test suite runs in milliseconds and a failure
//! points at a line:
//!
//! ```text
//! cargo test -p box --lib --no-default-features --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```
//!
//! What is left here is what a host cannot answer: that the same code, compiled
//! for `aarch64-unknown-none` and linked against `libakuma`, still works — and
//! that the TLS download path returns whole files. See `docs/TESTING.md`.

use alloc::format;
use alloc::string::String;
use libakuma::{print, println};

use boxlib::{json, manifest, oci_ref, paths, spec};

struct TestRunner {
    passed: u32,
    failed: u32,
}

impl TestRunner {
    fn new() -> Self {
        Self { passed: 0, failed: 0 }
    }

    fn run(&mut self, name: &str, f: fn() -> Result<(), String>) {
        print("[test] ");
        print(name);
        print("... ");
        match f() {
            Ok(()) => {
                println("ok");
                self.passed += 1;
            }
            Err(msg) => {
                print("FAILED: ");
                println(&msg);
                self.failed += 1;
            }
        }
    }

    fn summary(&self) -> bool {
        print("\n");
        libakuma::print_dec((self.passed + self.failed) as usize);
        print(" tests, ");
        libakuma::print_dec(self.passed as usize);
        print(" passed, ");
        libakuma::print_dec(self.failed as usize);
        println(" failed");
        self.failed == 0
    }
}

fn check(cond: bool, ctx: &str) -> Result<(), String> {
    if cond {
        Ok(())
    } else {
        Err(String::from(ctx))
    }
}

// ---- Smoke tests: the host-tested logic, compiled for the target ----

/// The JSON parser needs a heap for its scratch buffer and its results. On the
/// target that heap is libakuma's, not the host allocator the unit tests use.
fn test_json_on_target() -> Result<(), String> {
    let doc = r#"{"config":{"digest":"sha256:abc"},"layers":[{"digest":"sha256:l0"}]}"#;
    let m = manifest::parse_manifest(doc)?;
    check(m.config_digest == "sha256:abc", "config digest")?;
    check(m.layer_digests == ["sha256:l0"], "layer digests")?;
    check(
        json::string_at(doc, &["config", "digest"]).as_deref() == Some("sha256:abc"),
        "path lookup",
    )
}

fn test_image_ref_on_target() -> Result<(), String> {
    let r = oci_ref::parse_image_ref("busybox");
    check(r.registry == "registry-1.docker.io", "registry")?;
    check(r.name == "library/busybox", "name")?;
    check(r.tag == "latest", "tag")?;
    check(paths::sanitize_name("docker.io/library/busybox") == "busybox", "store name")
}

fn test_argv_on_target() -> Result<(), String> {
    let p = spec::image_process_from_config(r#"{"config":{"Cmd":["sh"]}}"#);
    check(p.argv_with(&[]) == ["sh"], "image cmd")
}

/// The HTTP header split the download pipeline depends on, exercised where the
/// TLS stack actually runs.
fn test_http_find_headers_end() -> Result<(), String> {
    let data = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
    let end = libakuma_tls::find_headers_end(data).ok_or("no header end found")?;
    check(&data[end..] == b"hello", "body after headers")?;
    check(
        libakuma_tls::find_headers_end(b"HTTP/1.1 200 OK\r\n").is_none(),
        "incomplete headers must not parse",
    )
}

// ---- Network integration (requires --net) ----

fn busybox_token() -> Result<String, String> {
    use libakuma_tls::{https_get, HttpHeaders};

    let image = oci_ref::parse_image_ref("busybox");
    let body = https_get(&image.token_url(), &HttpHeaders::new())
        .map_err(|e| format!("token fetch: {:?}", e))?;
    let body_str = core::str::from_utf8(&body).map_err(|_| String::from("invalid utf8"))?;
    json::string_at(body_str, &["token"]).ok_or_else(|| String::from("no token"))
}

fn busybox_manifest_list(token: &str) -> Result<String, String> {
    use libakuma_tls::{https_get, HttpHeaders};

    let image = oci_ref::parse_image_ref("busybox");
    let mut headers = HttpHeaders::new();
    headers.bearer_auth(token);
    headers.add(
        "Accept",
        "application/vnd.docker.distribution.manifest.v2+json, \
         application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.list.v2+json, \
         application/vnd.oci.image.index.v1+json",
    );
    let body = https_get(&image.manifest_url(&image.tag), &headers)
        .map_err(|e| format!("manifest fetch: {:?}", e))?;
    core::str::from_utf8(&body)
        .map(String::from)
        .map_err(|_| String::from("invalid manifest utf8"))
}

/// Docker Hub still answers with a manifest list that has a linux/arm64 entry.
/// Catches a registry-side change of shape, which no host test can see.
fn test_download_busybox_manifest() -> Result<(), String> {
    let token = busybox_token()?;
    let list = busybox_manifest_list(&token)?;
    check(manifest::is_manifest_list(&list), "response is a manifest list")?;
    manifest::select_platform_digest(&list)?;
    Ok(())
}

/// Full download pipeline: the layer that arrives is exactly the size the
/// manifest declared.
///
/// This is the test that catches TLS truncation — `box pull busybox` once
/// stopped at ~217 KB of a 1.9 MB layer because a full-size TLS 1.3 record did
/// not fit in `TLS_RECORD_SIZE`. Nothing reported an error; the file was just
/// short.
fn test_download_busybox_layer_size() -> Result<(), String> {
    use libakuma_tls::{download_file_with_headers, https_get, HttpHeaders};

    let token = busybox_token()?;
    let list = busybox_manifest_list(&token)?;
    let arm64_digest = manifest::select_platform_digest(&list)?;

    let image = oci_ref::parse_image_ref("busybox");
    let mut headers = HttpHeaders::new();
    headers.bearer_auth(&token);
    headers.add(
        "Accept",
        "application/vnd.docker.distribution.manifest.v2+json, \
         application/vnd.oci.image.manifest.v1+json",
    );
    let body = https_get(&image.manifest_url(&arm64_digest), &headers)
        .map_err(|e| format!("platform manifest fetch: {:?}", e))?;
    let platform_manifest =
        core::str::from_utf8(&body).map_err(|_| String::from("invalid platform manifest utf8"))?;

    let m = manifest::parse_manifest(platform_manifest)?;
    let expected = manifest::layer_size(platform_manifest, 0).ok_or("no layer size")? as usize;
    check(expected > 500_000, "declared layer size looks too small")?;

    let tmp = "/tmp/box-test-layer.tar.gz";
    let mut blob_headers = HttpHeaders::new();
    blob_headers.bearer_auth(&token);
    download_file_with_headers(&image.blob_url(&m.layer_digests[0]), tmp, &blob_headers)
        .map_err(|e| format!("layer download: {:?}", e))?;

    let fd = libakuma::open(tmp, 0);
    if fd < 0 {
        return Err(String::from("failed to open downloaded layer"));
    }
    let mut total: usize = 0;
    let mut buf = [0u8; 8192];
    loop {
        let n = libakuma::read_fd(fd, &mut buf);
        if n <= 0 {
            break;
        }
        total += n as usize;
    }
    libakuma::close(fd);
    libakuma::unlink(tmp);

    if total != expected {
        return Err(format!(
            "downloaded {} bytes, expected {} (truncation!)",
            total, expected
        ));
    }
    Ok(())
}

pub fn run_all(network: bool) -> bool {
    let mut t = TestRunner::new();

    println("--- on-target smoke ---");
    t.run("json_parse", test_json_on_target);
    t.run("image_ref", test_image_ref_on_target);
    t.run("image_argv", test_argv_on_target);
    t.run("http_find_headers_end", test_http_find_headers_end);

    if network {
        println("\n--- Download integration (network) ---");
        t.run("busybox_manifest", test_download_busybox_manifest);
        t.run("busybox_layer_size", test_download_busybox_layer_size);
    } else {
        println("\n(skipping network tests, use 'box test --net' to run them)");
    }

    println("\n(logic tests run on the host: see docs/TESTING.md)");
    t.summary()
}
