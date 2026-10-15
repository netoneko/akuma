//! `box push`, `box login`, `box logout` — the HTTP half.
//!
//! What to send and how to read the answers is in `boxlib::registry`; this file
//! opens sockets and files. The flow is the distribution spec's monolithic
//! upload: for the config and every layer, `HEAD` the blob and skip it if the
//! registry has it, else `POST …/blobs/uploads/` and `PUT` the bytes to the
//! `Location` it returns with `?digest=`; then `PUT` the manifest.
//!
//! **Security:** `libakuma-tls` does not verify server certificates, so a push
//! to an `https://` registry is protected against passive eavesdropping and
//! nothing else — credentials and layers can be read or replaced by anyone on the
//! path. That is the pull side's exposure too, but pulls send no secrets.
//! `box push` says so every time it sends credentials.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use libakuma::{close, exit, open, open_flags, print, println, read_fd};
use libakuma_tls::{http_request, parse_url, HttpHeaders, HttpResponse, RequestBody};

use boxlib::imgconfig::{manifest_json, MEDIA_MANIFEST};
use boxlib::json;
use boxlib::layer::sha256_digest;
use boxlib::oci_ref::{parse_image_ref, ImageRef};
use boxlib::registry::{
    self, parse_challenge, push_scope, resolve_location, token_from_body, token_request_url,
    with_digest, Challenge, Credentials, AUTH_FILE,
};

use crate::{fsx, images};

/// Responses here are acknowledgements and error documents, never payloads.
const MAX_RESPONSE: usize = 1024 * 1024;

/// Docker's own file, read (never written) so an existing `docker login` works.
const DOCKER_CONFIG: &str = "/root/.docker/config.json";

// ── credentials ──

/// `BOX_REGISTRY_USER` + `BOX_REGISTRY_PASSWORD`, else `box login`'s file, else
/// Docker's.
fn find_credentials(registry: &str) -> Option<Credentials> {
    if let (Some(user), Some(password)) = (libakuma::env("BOX_REGISTRY_USER"), libakuma::env("BOX_REGISTRY_PASSWORD")) {
        return Some(Credentials { user: String::from(user), password: String::from(password) });
    }
    for file in [AUTH_FILE, DOCKER_CONFIG] {
        if let Some(doc) = fsx::read_text(file) {
            if let Some(c) = registry::lookup_credentials(&doc, registry) {
                return Some(c);
            }
        }
    }
    None
}

/// Write `AUTH_FILE` without ever exposing it: created empty, made `0600`, then
/// filled — there is no moment a world-readable file holds a password.
fn write_auth_file(doc: &str) -> Result<(), String> {
    if !libakuma::mkdir_p("/var/lib/box") {
        return Err(String::from("cannot create /var/lib/box"));
    }
    fsx::write_file(AUTH_FILE, b"")?;
    if libakuma::chmod(AUTH_FILE, 0o600) < 0 {
        return Err(format!("cannot restrict {AUTH_FILE} to its owner"));
    }
    fsx::write_file(AUTH_FILE, doc.as_bytes())
}

fn read_stdin_line() -> String {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    while read_fd(0, &mut b) == 1 && b[0] != b'\n' {
        out.push(b[0]);
    }
    if out.last() == Some(&b'\r') {
        out.pop();
    }
    String::from_utf8(out).unwrap_or_default()
}

/// `box login [--insecure] -u <user> (-p <password> | --password-stdin) <registry>`
///
/// `--insecure` is accepted for symmetry with `push` and changes nothing: login
/// only stores the credential; the first request that uses it is a push.
pub fn cmd_login(args: libakuma::Args) -> ! {
    let argv: Vec<&str> = args.collect();
    let (mut user, mut password, mut stdin) = (None, None, false);
    let mut registry_arg = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i] {
            "-u" | "--username" => {
                i += 1;
                user = argv.get(i).copied();
            }
            "-p" | "--password" => {
                i += 1;
                password = argv.get(i).map(|s| String::from(*s));
            }
            "--password-stdin" => stdin = true,
            "--insecure" => {}
            a => registry_arg = Some(a),
        }
        i += 1;
    }
    let (Some(user), Some(reg)) = (user, registry_arg) else {
        print("Usage: box login -u <user> (-p <password> | --password-stdin) <registry>\n");
        exit(1);
    };
    if stdin {
        password = Some(read_stdin_line());
    }
    let Some(password) = password.filter(|p| !p.is_empty()) else {
        print("box login: no password (-p <password> or --password-stdin)\n");
        exit(1);
    };
    // `docker.io` and a bare `localhost:5000` both mean what `box push` will
    // resolve them to; reuse its parser so the stored key is the one it looks up.
    let registry = parse_image_ref(&format!("{reg}/x")).registry;
    let doc = fsx::read_text(AUTH_FILE).unwrap_or_default();
    let doc = registry::with_login(&doc, &registry, &Credentials { user: String::from(user), password });
    match write_auth_file(&doc) {
        Ok(()) => {
            print("Login credentials for ");
            print(&registry);
            print(" saved in ");
            println(AUTH_FILE);
            exit(0);
        }
        Err(e) => {
            print("box login: ");
            println(&e);
            exit(1);
        }
    }
}

/// `box logout <registry>`
pub fn cmd_logout(mut args: libakuma::Args) -> ! {
    let Some(reg) = args.next() else {
        print("Usage: box logout <registry>\n");
        exit(1);
    };
    let registry = parse_image_ref(&format!("{reg}/x")).registry;
    let doc = fsx::read_text(AUTH_FILE).unwrap_or_default();
    let (doc, removed) = registry::without_login(&doc, &registry);
    if !removed {
        print("box logout: no stored credentials for ");
        println(&registry);
        exit(1);
    }
    if let Err(e) = write_auth_file(&doc) {
        print("box logout: ");
        println(&e);
        exit(1);
    }
    print("Removed credentials for ");
    println(&registry);
    exit(0);
}

// ── a registry session ──

enum Body<'a> {
    None,
    Bytes(&'a [u8]),
    /// A file and how much of it to send; reopened for every attempt so a retry
    /// after a refreshed token starts from the first byte.
    File(&'a str, u64),
}

struct Session {
    image: ImageRef,
    creds: Option<Credentials>,
    /// The `Authorization` value in force (`Basic …` or `Bearer …`), if any.
    auth: Option<String>,
    scope: String,
}

impl Session {
    fn new(image: ImageRef) -> Self {
        let creds = find_credentials(&image.registry);
        let scope = push_scope(&image.name);
        Self { image, creds, auth: None, scope }
    }

    /// Ask the registry what it wants, and do it. `Ok` means the next request
    /// is as authorized as it is going to get.
    fn authenticate(&mut self) -> Result<(), String> {
        self.auth = None;
        let url = format!("{}/v2/", self.image.base_url());
        let r = http_request("GET", &url, &HttpHeaders::new(), RequestBody::Empty, MAX_RESPONSE)
            .map_err(|e| format!("cannot reach {}: {e:?}", self.image.registry))?;
        match r.status {
            200 => Ok(()),
            401 => {
                let challenge = r.header("WWW-Authenticate").and_then(parse_challenge);
                match challenge {
                    Some(Challenge::Basic) => {
                        let c = self.creds.as_ref().ok_or_else(|| self.login_hint())?;
                        self.auth = Some(c.basic_header());
                        Ok(())
                    }
                    Some(Challenge::Bearer { realm, service, .. }) => {
                        let mut h = HttpHeaders::new();
                        if let Some(c) = &self.creds {
                            h.add("Authorization", &c.basic_header());
                        }
                        let turl = token_request_url(&realm, service.as_deref(), &self.scope);
                        let t = http_request("GET", &turl, &h, RequestBody::Empty, MAX_RESPONSE)
                            .map_err(|e| format!("token request failed: {e:?}"))?;
                        if t.status == 401 || t.status == 403 {
                            return Err(if self.creds.is_some() {
                                format!("{} refused the stored credentials", self.image.registry)
                            } else {
                                self.login_hint()
                            });
                        }
                        if t.status != 200 {
                            return Err(format!("token endpoint answered {}", t.status));
                        }
                        let body = core::str::from_utf8(&t.body).unwrap_or("");
                        let tok = token_from_body(body).ok_or("token endpoint sent no token")?;
                        self.auth = Some(format!("Bearer {tok}"));
                        Ok(())
                    }
                    None => Err(String::from("registry demands an authentication scheme box does not speak")),
                }
            }
            s => Err(format!("{} does not look like a registry: GET /v2/ answered {s}", self.image.registry)),
        }
    }

    fn login_hint(&self) -> String {
        format!(
            "{} needs credentials: box login -u <user> --password-stdin {}  (or set BOX_REGISTRY_USER and BOX_REGISTRY_PASSWORD)",
            self.image.registry, self.image.registry
        )
    }

    /// One request, with credentials, retried once after re-authenticating if the
    /// registry says 401 (a bearer token lasts minutes; a layer upload can last
    /// longer). `Authorization` goes only to the registry's own host: an upload
    /// `Location` may name a CDN, and the token is not theirs.
    fn call(&mut self, method: &str, url: &str, extra: &[(&str, &str)], body: &Body) -> Result<HttpResponse, String> {
        for attempt in 0..2 {
            let mut h = HttpHeaders::new();
            for (n, v) in extra {
                h.add(n, v);
            }
            if let (Some(a), true) = (&self.auth, self.same_host(url)) {
                h.add("Authorization", a);
            }
            let r = match body {
                Body::None => http_request(method, url, &h, RequestBody::Empty, MAX_RESPONSE),
                Body::Bytes(b) => http_request(method, url, &h, RequestBody::Bytes(b), MAX_RESPONSE),
                Body::File(path, len) => {
                    let fd = open(path, open_flags::O_RDONLY);
                    if fd < 0 {
                        return Err(format!("cannot open {path}"));
                    }
                    let r = http_request(method, url, &h, RequestBody::Fd { fd, len: *len }, MAX_RESPONSE);
                    close(fd);
                    r
                }
            }
            .map_err(|e| format!("{method} {url}: {e:?}"))?;
            if r.status == 401 && attempt == 0 && self.same_host(url) {
                self.authenticate()?;
                continue;
            }
            return Ok(r);
        }
        Err(String::from("unreachable"))
    }

    fn same_host(&self, url: &str) -> bool {
        match (parse_url(url), parse_url(&self.image.base_url())) {
            (Some(a), Some(b)) => a.host == b.host && a.port == b.port,
            _ => false,
        }
    }

    /// Does the registry already hold this blob?
    fn has_blob(&mut self, digest: &str) -> Result<bool, String> {
        let r = self.call("HEAD", &self.image.blob_url(digest), &[], &Body::None)?;
        match r.status {
            200 => Ok(true),
            404 => Ok(false),
            401 | 403 => Err(self.denied()),
            s => Err(format!("HEAD blob {digest}: status {s}")),
        }
    }

    fn denied(&self) -> String {
        if self.creds.is_some() {
            format!("{} refused the credentials for {} (push needs write access)", self.image.registry, self.image.name)
        } else {
            self.login_hint()
        }
    }

    fn upload_blob(&mut self, digest: &str, body: &Body) -> Result<(), String> {
        let start = self.call("POST", &self.image.upload_start_url(), &[], &Body::None)?;
        match start.status {
            202 => {}
            401 | 403 => return Err(self.denied()),
            s => return Err(format!("starting an upload: status {s}{}", detail(&start))),
        }
        let loc = start.header("Location").ok_or("registry sent no upload Location")?;
        let url = with_digest(&resolve_location(&self.image.base_url(), loc), digest);
        let put = self.call("PUT", &url, &[("Content-Type", "application/octet-stream")], body)?;
        match put.status {
            201 | 204 => Ok(()),
            s => Err(format!("uploading {digest}: status {s}{}", detail(&put))),
        }
    }
}

/// `: <registry's own message>` from an error document, if it sent one.
fn detail(r: &HttpResponse) -> String {
    let body = core::str::from_utf8(&r.body).unwrap_or("");
    let msgs = json::strings_at(body, &["errors", "*", "message"]);
    if msgs.is_empty() { String::new() } else { format!(": {}", msgs.join("; ")) }
}

fn short(digest: &str) -> &str {
    digest.strip_prefix("sha256:").map_or(digest, |h| &h[..h.len().min(12)])
}

// ── box push ──

/// `box push [--insecure] <image>`
pub fn cmd_push(args: libakuma::Args) -> ! {
    let argv: Vec<&str> = args.collect();
    let insecure = argv.contains(&"--insecure");
    let Some(image) = argv.iter().find(|a| !a.starts_with("--")) else {
        print("Usage: box push [--insecure] <registry/name:tag>\n");
        exit(1);
    };
    match push(image, insecure) {
        Ok(()) => exit(0),
        Err(e) => {
            print("box push: ");
            println(&e);
            exit(1);
        }
    }
}

fn push(image_str: &str, insecure: bool) -> Result<(), String> {
    let mut image = parse_image_ref(image_str);
    image.insecure = insecure;

    let store = images::sanitize_name(image_str);
    if !images::image_exists(&store) {
        return Err(format!("image '{image_str}' not found"));
    }
    let config = images::load_config(&store).ok_or("unreadable config")?;
    let layers = images::load_layers(&store);
    let mut sized: Vec<(String, u64)> = Vec::new();
    for d in &layers {
        let n = images::blob_size(d).ok_or_else(|| {
            format!("layer {d} has no stored blob (the image was pulled before blobs were kept) — re-pull it")
        })?;
        sized.push((d.clone(), n));
    }
    let config_digest = sha256_digest(config.as_bytes());
    let manifest = manifest_json(&config_digest, config.len() as u64, &sized);
    let manifest_digest = sha256_digest(manifest.as_bytes());

    print("The push refers to repository [");
    print(&image.registry);
    print("/");
    print(&image.name);
    println("]");
    if image.scheme() == "http" {
        println("  warning: plain HTTP — credentials and layers cross the network unprotected");
    } else {
        println("  note: server certificates are not verified; do not push secrets over an untrusted network");
    }

    let mut s = Session::new(image);
    s.authenticate()?;

    for (d, n) in &sized {
        if s.has_blob(d)? {
            print("  ");
            print(short(d));
            println(": Layer already exists");
            continue;
        }
        print("  ");
        print(short(d));
        print(": Pushing ");
        libakuma::print_dec(*n as usize);
        println(" bytes");
        s.upload_blob(d, &Body::File(&images::blob_path(d), *n))?;
    }
    if s.has_blob(&config_digest)? {
        println("  config: already exists");
    } else {
        print("  ");
        print(short(&config_digest));
        println(": Pushing config");
        s.upload_blob(&config_digest, &Body::Bytes(config.as_bytes()))?;
    }

    let url = s.image.manifest_url(&s.image.tag);
    let put = s.call("PUT", &url, &[("Content-Type", MEDIA_MANIFEST)], &Body::Bytes(manifest.as_bytes()))?;
    match put.status {
        201 | 200 => {}
        401 | 403 => return Err(s.denied()),
        st => return Err(format!("pushing the manifest: status {st}{}", detail(&put))),
    }
    // The registry hashes what it received. If it disagrees with the digest we
    // computed, something between us rewrote the manifest.
    if let Some(got) = put.header("Docker-Content-Digest") {
        if got != manifest_digest {
            return Err(format!("registry stored {got}, expected {manifest_digest}"));
        }
    }
    print(&s.image.tag);
    print(": digest: ");
    print(&manifest_digest);
    print(" size: ");
    libakuma::print_dec(manifest.len());
    println("");
    Ok(())
}
