use alloc::format;
use alloc::string::String;
use libakuma::{print, println};

use crate::json;
use crate::oci;

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

fn assert_eq_str(a: &str, b: &str, ctx: &str) -> Result<(), String> {
    if a != b {
        Err(format!("{}: '{}' != '{}'", ctx, a, b))
    } else {
        Ok(())
    }
}

fn assert_some<T>(opt: &Option<T>, ctx: &str) -> Result<(), String> {
    if opt.is_none() { Err(format!("{}: expected Some, got None", ctx)) } else { Ok(()) }
}

fn assert_none<T>(opt: &Option<T>, ctx: &str) -> Result<(), String> {
    if opt.is_some() { Err(format!("{}: expected None, got Some", ctx)) } else { Ok(()) }
}

fn assert_true(cond: bool, ctx: &str) -> Result<(), String> {
    if !cond { Err(format!("{}: expected true", ctx)) } else { Ok(()) }
}

// ---- JSON parser tests ----

fn test_json_extract_string() -> Result<(), String> {
    let j = r#"{"name": "hello", "version": "1.0"}"#;
    assert_eq_str(&json::extract_string(j, "name").unwrap(), "hello", "name")?;
    assert_eq_str(&json::extract_string(j, "version").unwrap(), "1.0", "version")?;
    assert_none(&json::extract_string(j, "missing"), "missing")?;
    Ok(())
}

fn test_json_extract_string_escapes() -> Result<(), String> {
    let j = r#"{"path": "foo\/bar", "msg": "he said \"hi\""}"#;
    assert_eq_str(&json::extract_string(j, "path").unwrap(), "foo/bar", "path")?;
    assert_eq_str(&json::extract_string(j, "msg").unwrap(), "he said \"hi\"", "msg")?;
    Ok(())
}

fn test_json_extract_object() -> Result<(), String> {
    let j = r#"{"config": {"digest": "sha256:abc"}, "other": 1}"#;
    let obj = json::extract_object(j, "config").ok_or("no config")?;
    assert_eq_str(obj, r#"{"digest": "sha256:abc"}"#, "config obj")?;
    assert_eq_str(&json::extract_string(obj, "digest").unwrap(), "sha256:abc", "digest")?;
    Ok(())
}

fn test_json_extract_array() -> Result<(), String> {
    let j = r#"{"layers": [{"digest": "a"}, {"digest": "b"}]}"#;
    let arr = json::extract_array(j, "layers").ok_or("no layers")?;
    assert_true(arr.starts_with('['), "starts with [")?;
    assert_true(arr.ends_with(']'), "ends with ]")?;
    Ok(())
}

fn test_json_iter_array_objects() -> Result<(), String> {
    let arr = r#"[{"a": 1}, {"b": 2}, {"c": 3}]"#;
    let objs = json::iter_array_objects(arr);
    assert_true(objs.len() == 3, "3 objects")?;
    assert_eq_str(objs[0], r#"{"a": 1}"#, "first")?;
    assert_eq_str(objs[2], r#"{"c": 3}"#, "third")?;
    Ok(())
}

fn test_json_string_array() -> Result<(), String> {
    let j = r#"{"Cmd": ["/bin/sh", "-c", "echo hello"]}"#;
    let arr = json::extract_string_array(j, "Cmd").ok_or("no Cmd")?;
    assert_true(arr.len() == 3, "3 elements")?;
    assert_eq_str(&arr[0], "/bin/sh", "0")?;
    assert_eq_str(&arr[1], "-c", "1")?;
    assert_eq_str(&arr[2], "echo hello", "2")?;
    Ok(())
}

fn test_json_string_array_empty() -> Result<(), String> {
    let j = r#"{"Cmd": []}"#;
    let arr = json::extract_string_array(j, "Cmd").ok_or("no Cmd")?;
    assert_true(arr.is_empty(), "empty")?;
    Ok(())
}

fn test_json_manifest_list_detection() -> Result<(), String> {
    let j = r#"{"manifests":[{"digest":"sha256:abc","mediaType":"application/vnd.oci.image.manifest.v1+json","platform":{"architecture":"arm64","os":"linux"},"size":610}]}"#;
    let media_type = json::extract_string(j, "mediaType").unwrap_or_default();
    assert_true(!media_type.contains("manifest.list"), "not manifest.list")?;
    assert_true(!media_type.contains("image.index"), "not image.index")?;
    assert_some(&json::extract_array(j, "manifests"), "has manifests array")?;
    Ok(())
}

fn test_json_real_manifest() -> Result<(), String> {
    let manifest = r#"{
        "schemaVersion": 2,
        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
        "config": {
            "mediaType": "application/vnd.docker.container.image.v1+json",
            "size": 1471,
            "digest": "sha256:cd9176cd36f99ec4cae927afa38448e8a9592e5277252f62926defa6aafad0f5"
        },
        "layers": [
            {
                "mediaType": "application/vnd.docker.image.rootfs.diff.tar.gzip",
                "size": 2295859,
                "digest": "sha256:b85757a5ca1a383b3aea14e0fae4eee32fedfbc8dc91ab477a399ea83e427b10"
            }
        ]
    }"#;

    let config = json::extract_object(manifest, "config").ok_or("no config")?;
    let digest = json::extract_string(config, "digest").ok_or("no digest")?;
    assert_eq_str(&digest, "sha256:cd9176cd36f99ec4cae927afa38448e8a9592e5277252f62926defa6aafad0f5", "config digest")?;

    let layers = json::extract_array(manifest, "layers").ok_or("no layers")?;
    let layer_objs = json::iter_array_objects(layers);
    assert_true(layer_objs.len() == 1, "1 layer")?;
    let ld = json::extract_string(layer_objs[0], "digest").ok_or("no layer digest")?;
    assert_eq_str(&ld, "sha256:b85757a5ca1a383b3aea14e0fae4eee32fedfbc8dc91ab477a399ea83e427b10", "layer digest")?;
    Ok(())
}

fn test_json_platform_matching() -> Result<(), String> {
    let manifest_list = r#"{"manifests":[
        {"digest":"sha256:amd64","platform":{"architecture":"amd64","os":"linux"}},
        {"digest":"sha256:arm64","platform":{"architecture":"arm64","os":"linux"}},
        {"digest":"sha256:unknown","platform":{"architecture":"unknown","os":"unknown"}}
    ]}"#;

    let arr = json::extract_array(manifest_list, "manifests").ok_or("no manifests")?;
    let objs = json::iter_array_objects(arr);
    let mut arm64_digest = None;
    for obj in &objs {
        if let Some(platform) = json::extract_object(obj, "platform") {
            let arch = json::extract_string(platform, "architecture").unwrap_or_default();
            let os = json::extract_string(platform, "os").unwrap_or_default();
            if (arch == "arm64" || arch == "aarch64") && os == "linux" {
                arm64_digest = json::extract_string(obj, "digest");
                break;
            }
        }
    }
    assert_eq_str(&arm64_digest.ok_or("no arm64")?, "sha256:arm64", "arm64 digest")?;
    Ok(())
}

// ---- OCI image ref parser tests ----

fn test_oci_ref_simple() -> Result<(), String> {
    let r = oci::parse_image_ref("busybox");
    assert_eq_str(&r.registry, "registry-1.docker.io", "registry")?;
    assert_eq_str(&r.name, "library/busybox", "name")?;
    assert_eq_str(&r.tag, "latest", "tag")?;
    Ok(())
}

fn test_oci_ref_with_tag() -> Result<(), String> {
    let r = oci::parse_image_ref("ubuntu:22.04");
    assert_eq_str(&r.registry, "registry-1.docker.io", "registry")?;
    assert_eq_str(&r.name, "library/ubuntu", "name")?;
    assert_eq_str(&r.tag, "22.04", "tag")?;
    Ok(())
}

fn test_oci_ref_with_user() -> Result<(), String> {
    let r = oci::parse_image_ref("myuser/myapp:v1");
    assert_eq_str(&r.registry, "registry-1.docker.io", "registry")?;
    assert_eq_str(&r.name, "myuser/myapp", "name")?;
    assert_eq_str(&r.tag, "v1", "tag")?;
    Ok(())
}

fn test_oci_ref_custom_registry() -> Result<(), String> {
    let r = oci::parse_image_ref("ghcr.io/owner/repo:sha-abc");
    assert_eq_str(&r.registry, "ghcr.io", "registry")?;
    assert_eq_str(&r.name, "owner/repo", "name")?;
    assert_eq_str(&r.tag, "sha-abc", "tag")?;
    Ok(())
}

fn test_oci_ref_docker_io_rewrite() -> Result<(), String> {
    let r = oci::parse_image_ref("docker.io/library/alpine:3.19");
    assert_eq_str(&r.registry, "registry-1.docker.io", "registry")?;
    assert_eq_str(&r.name, "library/alpine", "name")?;
    assert_eq_str(&r.tag, "3.19", "tag")?;
    Ok(())
}

fn test_oci_ref_registry_with_port() -> Result<(), String> {
    let r = oci::parse_image_ref("localhost:5000/myimage:dev");
    assert_eq_str(&r.registry, "localhost:5000", "registry")?;
    assert_eq_str(&r.name, "myimage", "name")?;
    assert_eq_str(&r.tag, "dev", "tag")?;
    Ok(())
}

// ---- HTTP header parsing tests (uses libakuma_tls) ----

fn test_http_find_headers_end() -> Result<(), String> {
    let data = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
    let end = libakuma_tls::find_headers_end(data).ok_or("no end found")?;
    assert_true(&data[end..] == b"hello", "body after headers")?;
    Ok(())
}

fn test_http_find_headers_end_missing() -> Result<(), String> {
    let data = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n";
    assert_none(&libakuma_tls::find_headers_end(data), "no end")?;
    Ok(())
}

// ---- Download integration test (runs on Akuma with libakuma-tls) ----

fn test_download_busybox_manifest() -> Result<(), String> {
    use libakuma_tls::{https_get, HttpHeaders};

    let url = "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/busybox:pull";
    let headers = HttpHeaders::new();
    let body = https_get(url, &headers)
        .map_err(|e| format!("token fetch: {:?}", e))?;
    let body_str = core::str::from_utf8(&body)
        .map_err(|_| String::from("invalid utf8"))?;
    let token = json::extract_string(body_str, "token")
        .ok_or_else(|| String::from("no token"))?;

    let manifest_url = "https://registry-1.docker.io/v2/library/busybox/manifests/latest";
    let mut mh = HttpHeaders::new();
    mh.bearer_auth(&token);
    mh.add("Accept",
        "application/vnd.docker.distribution.manifest.v2+json, \
         application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.list.v2+json, \
         application/vnd.oci.image.index.v1+json");
    let mbody = https_get(manifest_url, &mh)
        .map_err(|e| format!("manifest fetch: {:?}", e))?;
    let mstr = core::str::from_utf8(&mbody)
        .map_err(|_| String::from("invalid manifest utf8"))?;

    assert_some(&json::extract_array(mstr, "manifests"), "has manifests array")?;

    let arr = json::extract_array(mstr, "manifests").unwrap();
    let objs = json::iter_array_objects(arr);
    let mut found_arm64 = false;
    for obj in &objs {
        if let Some(platform) = json::extract_object(obj, "platform") {
            let arch = json::extract_string(platform, "architecture").unwrap_or_default();
            let os = json::extract_string(platform, "os").unwrap_or_default();
            if (arch == "arm64" || arch == "aarch64") && os == "linux" {
                found_arm64 = true;
                break;
            }
        }
    }
    assert_true(found_arm64, "arm64 platform in manifest list")?;
    Ok(())
}

fn test_download_busybox_layer_size() -> Result<(), String> {
    use libakuma_tls::{https_get, download_file_with_headers, HttpHeaders};

    let url = "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/busybox:pull";
    let headers = HttpHeaders::new();
    let body = https_get(url, &headers)
        .map_err(|e| format!("token fetch: {:?}", e))?;
    let body_str = core::str::from_utf8(&body)
        .map_err(|_| String::from("invalid utf8"))?;
    let token = json::extract_string(body_str, "token")
        .ok_or_else(|| String::from("no token"))?;

    // Get manifest list -> arm64 digest -> platform manifest -> layer digest+size
    let manifest_url = "https://registry-1.docker.io/v2/library/busybox/manifests/latest";
    let mut mh = HttpHeaders::new();
    mh.bearer_auth(&token);
    mh.add("Accept",
        "application/vnd.docker.distribution.manifest.v2+json, \
         application/vnd.oci.image.manifest.v1+json, \
         application/vnd.docker.distribution.manifest.list.v2+json, \
         application/vnd.oci.image.index.v1+json");
    let mbody = https_get(manifest_url, &mh)
        .map_err(|e| format!("manifest fetch: {:?}", e))?;
    let mstr = core::str::from_utf8(&mbody)
        .map_err(|_| String::from("invalid manifest utf8"))?;

    let arr = json::extract_array(mstr, "manifests")
        .ok_or("no manifests")?;
    let mut arm64_digest = None;
    for obj in json::iter_array_objects(arr) {
        if let Some(platform) = json::extract_object(obj, "platform") {
            let arch = json::extract_string(platform, "architecture").unwrap_or_default();
            let os = json::extract_string(platform, "os").unwrap_or_default();
            if (arch == "arm64" || arch == "aarch64") && os == "linux" {
                arm64_digest = json::extract_string(obj, "digest");
                break;
            }
        }
    }
    let arm64_digest = arm64_digest.ok_or("no arm64 digest")?;

    let plat_url = format!(
        "https://registry-1.docker.io/v2/library/busybox/manifests/{}",
        arm64_digest
    );
    let mut ph = HttpHeaders::new();
    ph.bearer_auth(&token);
    ph.add("Accept",
        "application/vnd.docker.distribution.manifest.v2+json, \
         application/vnd.oci.image.manifest.v1+json");
    let pbody = https_get(&plat_url, &ph)
        .map_err(|e| format!("platform manifest fetch: {:?}", e))?;
    let pstr = core::str::from_utf8(&pbody)
        .map_err(|_| String::from("invalid platform manifest utf8"))?;

    let layers_arr = json::extract_array(pstr, "layers")
        .ok_or("no layers in platform manifest")?;
    let layer_objs = json::iter_array_objects(layers_arr);
    assert_true(!layer_objs.is_empty(), "at least 1 layer")?;

    let layer_digest = json::extract_string(layer_objs[0], "digest")
        .ok_or("no layer digest")?;
    let expected_size = extract_number(layer_objs[0], "size")
        .ok_or("no layer size")?;

    assert_true(expected_size > 500_000, &format!("layer size {} > 500KB", expected_size))?;

    // Download the blob
    let blob_url = format!(
        "https://registry-1.docker.io/v2/library/busybox/blobs/{}",
        layer_digest
    );
    let tmp = "/tmp/box-test-layer.tar.gz";
    let mut bh = HttpHeaders::new();
    bh.bearer_auth(&token);
    download_file_with_headers(&blob_url, tmp, &bh)
        .map_err(|e| format!("layer download: {:?}", e))?;

    // Measure file size
    let fd = libakuma::open(tmp, 0);
    if fd < 0 {
        return Err(String::from("failed to open downloaded layer"));
    }
    let mut total: usize = 0;
    let mut buf = [0u8; 8192];
    loop {
        let n = libakuma::read_fd(fd, &mut buf);
        if n <= 0 { break; }
        total += n as usize;
    }
    libakuma::close(fd);
    libakuma::unlink(tmp);

    if total != expected_size {
        return Err(format!(
            "downloaded {} bytes, expected {} (truncation!)",
            total, expected_size
        ));
    }
    assert_true(total > 500_000, &format!("downloaded size {} > 500KB", total))?;
    Ok(())
}

fn extract_number(json: &str, key: &str) -> Option<usize> {
    let pattern = alloc::format!("\"{}\"", key);
    let key_pos = json.find(&pattern)?;
    let after_key = &json[key_pos + pattern.len()..];
    let colon_pos = after_key.find(':')?;
    let rest = &after_key[colon_pos + 1..].trim_start();
    let mut n: usize = 0;
    for b in rest.as_bytes() {
        if *b >= b'0' && *b <= b'9' {
            n = n * 10 + (*b - b'0') as usize;
        } else {
            break;
        }
    }
    if n > 0 { Some(n) } else { None }
}

pub fn run_all(network: bool) -> bool {
    let mut t = TestRunner::new();

    println("--- JSON parser ---");
    t.run("extract_string", test_json_extract_string);
    t.run("extract_string_escapes", test_json_extract_string_escapes);
    t.run("extract_object", test_json_extract_object);
    t.run("extract_array", test_json_extract_array);
    t.run("iter_array_objects", test_json_iter_array_objects);
    t.run("string_array", test_json_string_array);
    t.run("string_array_empty", test_json_string_array_empty);
    t.run("manifest_list_detection", test_json_manifest_list_detection);
    t.run("real_manifest", test_json_real_manifest);
    t.run("platform_matching", test_json_platform_matching);

    println("\n--- OCI ref parser ---");
    t.run("simple_name", test_oci_ref_simple);
    t.run("name_with_tag", test_oci_ref_with_tag);
    t.run("name_with_user", test_oci_ref_with_user);
    t.run("custom_registry", test_oci_ref_custom_registry);
    t.run("docker_io_rewrite", test_oci_ref_docker_io_rewrite);
    t.run("registry_with_port", test_oci_ref_registry_with_port);

    println("\n--- HTTP header parsing ---");
    t.run("find_headers_end", test_http_find_headers_end);
    t.run("find_headers_end_missing", test_http_find_headers_end_missing);

    if network {
        println("\n--- Download integration (network) ---");
        t.run("busybox_manifest", test_download_busybox_manifest);
        t.run("busybox_layer_size", test_download_busybox_layer_size);
    } else {
        println("\n(skipping network tests, use 'box test --net' to run them)");
    }

    t.summary()
}
