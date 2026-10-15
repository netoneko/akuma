//! The decisions behind `box push` and `box login` — pure string work.
//!
//! What to ask a registry, how it asks for credentials, where an upload went,
//! and how credentials are written down. `push.rs` owns the HTTP; nothing here
//! touches a socket or a file, so all of it is host-tested.
//!
//! The credential file is Docker's `config.json` shape on purpose
//! (`{"auths":{"<registry>":{"auth":"base64(user:password)"}}}`): the same file
//! can be read from a Docker install, and nothing new has to be documented.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::imgconfig::q;
use crate::json::{self, Seg, Value};
use crate::oci_ref::{ImageRef, DOCKER_HUB};

/// Where `box login` keeps credentials. Written `0600`.
pub const AUTH_FILE: &str = "/var/lib/box/auth.json";

// ── base64 (RFC 4648, with padding) ──

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

#[must_use]
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut pad = 0;
    for c in s.trim().bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            _ => return None,
        };
        if pad > 0 {
            return None; // data after padding
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ── credentials ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

impl Credentials {
    /// The value of an `Authorization:` header for HTTP Basic.
    #[must_use]
    pub fn basic_header(&self) -> String {
        format!("Basic {}", base64_encode(format!("{}:{}", self.user, self.password).as_bytes()))
    }

    fn to_auth(&self) -> String {
        base64_encode(format!("{}:{}", self.user, self.password).as_bytes())
    }

    /// `base64(user:password)` as stored. The first `:` splits, so a password
    /// may contain colons and a user name may not — the same rule Docker has.
    fn from_auth(b64: &str) -> Option<Self> {
        let raw = String::from_utf8(base64_decode(b64)?).ok()?;
        let (user, password) = raw.split_once(':')?;
        if user.is_empty() {
            return None;
        }
        Some(Self { user: String::from(user), password: String::from(password) })
    }
}

/// The `auths` object keys a credential for `registry` may be stored under: the
/// host as typed, with a scheme, and for Docker Hub the legacy names Docker
/// itself writes.
fn key_variants(registry: &str) -> Vec<String> {
    let mut v = alloc::vec![String::from(registry), format!("https://{registry}"), format!("http://{registry}")];
    if registry == DOCKER_HUB {
        for k in ["index.docker.io", "docker.io", "https://index.docker.io/v1/", "https://index.docker.io/v1"] {
            v.push(String::from(k));
        }
    }
    v
}

/// Every `(registry key, base64 auth)` in an `auths` document, in order.
/// Entries that carry no `auth` string (a credential-helper stub) are skipped.
#[must_use]
pub fn auth_entries(doc: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let _ = json::walk(doc, |path, value| {
        if let Value::Str(s) = value {
            if path.matches(&["auths", "*", "auth"]) {
                if let Some(Seg::Key(k)) = path.segments().get(1) {
                    out.push((k.clone(), String::from(s)));
                }
            }
        }
    });
    out
}

/// The stored credentials for `registry`, if any.
#[must_use]
pub fn lookup_credentials(doc: &str, registry: &str) -> Option<Credentials> {
    let entries = auth_entries(doc);
    for key in key_variants(registry) {
        if let Some((_, auth)) = entries.iter().find(|(k, _)| *k == key) {
            return Credentials::from_auth(auth);
        }
    }
    None
}

fn render_auths(entries: &[(String, String)]) -> String {
    let mut o = String::from("{\"auths\":{");
    for (i, (k, a)) in entries.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        o.push_str(&format!("{}:{{\"auth\":{}}}", q(k), q(a)));
    }
    o.push_str("}}\n");
    o
}

/// `doc` with `registry`'s credentials set to `c` (added, or replaced in place).
/// Other registries' entries are kept as they were.
#[must_use]
pub fn with_login(doc: &str, registry: &str, c: &Credentials) -> String {
    let mut entries = auth_entries(doc);
    let keys = key_variants(registry);
    entries.retain(|(k, _)| !keys.contains(k));
    entries.push((String::from(registry), c.to_auth()));
    render_auths(&entries)
}

/// `doc` without any credentials for `registry`, and whether there were any.
#[must_use]
pub fn without_login(doc: &str, registry: &str) -> (String, bool) {
    let mut entries = auth_entries(doc);
    let keys = key_variants(registry);
    let before = entries.len();
    entries.retain(|(k, _)| !keys.contains(k));
    let removed = entries.len() != before;
    (render_auths(&entries), removed)
}

// ── authentication challenges ──

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Challenge {
    Basic,
    Bearer { realm: String, service: Option<String>, scope: Option<String> },
}

/// Parse a `WWW-Authenticate` header: `Basic realm="…"` or
/// `Bearer realm="…",service="…",scope="…"`. Values may be quoted (and then may
/// contain commas) or bare. Unknown schemes and a `Bearer` without a `realm`
/// are `None`: there is nothing to do with them.
#[must_use]
pub fn parse_challenge(header: &str) -> Option<Challenge> {
    let header = header.trim();
    let (scheme, rest) = header.split_once(char::is_whitespace).unwrap_or((header, ""));
    if scheme.eq_ignore_ascii_case("basic") {
        return Some(Challenge::Basic);
    }
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let (mut realm, mut service, mut scope) = (None, None, None);
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i] == b',' || b[i].is_ascii_whitespace()) {
            i += 1;
        }
        let ks = i;
        while i < b.len() && b[i] != b'=' && b[i] != b',' {
            i += 1;
        }
        let key = rest[ks..i].trim();
        if i >= b.len() || b[i] != b'=' {
            continue;
        }
        i += 1; // '='
        let val = if i < b.len() && b[i] == b'"' {
            i += 1;
            let vs = i;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            let v = &rest[vs..i];
            i += 1; // closing quote
            v
        } else {
            let vs = i;
            while i < b.len() && b[i] != b',' {
                i += 1;
            }
            rest[vs..i].trim()
        };
        match key.to_ascii_lowercase().as_str() {
            "realm" => realm = Some(String::from(val)),
            "service" => service = Some(String::from(val)),
            "scope" => scope = Some(String::from(val)),
            _ => {}
        }
    }
    Some(Challenge::Bearer { realm: realm?, service, scope })
}

/// The scope a push needs: pull too, because pushing starts with a `HEAD` and a
/// cross-check of what the repository already has.
#[must_use]
pub fn push_scope(repository: &str) -> String {
    format!("repository:{repository}:pull,push")
}

/// Percent-encode a query value (RFC 3986 unreserved stay; `:` and `/` are kept
/// readable because every registry accepts them in `scope`).
fn query_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.bytes() {
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'/' | b',' => o.push(c as char),
            _ => o.push_str(&format!("%{c:02X}")),
        }
    }
    o
}

/// Where to ask for a bearer token. `scope` overrides the challenge's own — the
/// `/v2/` ping carries none, and the one a failed request carries may be pull
/// only.
#[must_use]
pub fn token_request_url(realm: &str, service: Option<&str>, scope: &str) -> String {
    let mut url = String::from(realm);
    url.push(if realm.contains('?') { '&' } else { '?' });
    if let Some(s) = service {
        url.push_str(&format!("service={}&", query_escape(s)));
    }
    url.push_str(&format!("scope={}", query_escape(scope)));
    url
}

/// The token in a token endpoint's reply: `token`, or OAuth2's `access_token`.
#[must_use]
pub fn token_from_body(body: &str) -> Option<String> {
    json::string_at(body, &["token"]).or_else(|| json::string_at(body, &["access_token"]))
}

// ── uploads ──

impl ImageRef {
    /// `POST` here starts a blob upload.
    #[must_use]
    pub fn upload_start_url(&self) -> String {
        format!("{}/v2/{}/blobs/uploads/", self.base_url(), self.name)
    }
}

/// The URL a registry's `Location` header names: absolute as sent, or a path on
/// the registry that sent it.
#[must_use]
pub fn resolve_location(base_url: &str, location: &str) -> String {
    if location.starts_with("http://") || location.starts_with("https://") {
        String::from(location)
    } else if location.starts_with('/') {
        format!("{base_url}{location}")
    } else {
        format!("{base_url}/{location}")
    }
}

/// The monolithic-upload `PUT` URL: the upload's own `Location` plus
/// `digest=`. That location usually carries a query already (`?_state=…`), and
/// the digest is appended with `&` then — never a second `?`.
#[must_use]
pub fn with_digest(upload_url: &str, digest: &str) -> String {
    format!("{}{}digest={}", upload_url, if upload_url.contains('?') { '&' } else { '?' }, digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci_ref::parse_image_ref;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (plain, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64_encode(plain.as_bytes()), enc);
            assert_eq!(base64_decode(enc).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn base64_decode_refuses_garbage() {
        assert!(base64_decode("Zm9v!").is_none());
        assert!(base64_decode("Zg==Zg==").is_none());
    }

    #[test]
    fn basic_header_is_user_colon_password() {
        let c = Credentials { user: "aladdin".into(), password: "open sesame".into() };
        assert_eq!(c.basic_header(), "Basic YWxhZGRpbjpvcGVuIHNlc2FtZQ==");
    }

    #[test]
    fn a_password_may_contain_colons() {
        let c = Credentials { user: "u".into(), password: "a:b:c".into() };
        let doc = with_login("", "r.example", &c);
        assert_eq!(lookup_credentials(&doc, "r.example"), Some(c));
    }

    #[test]
    fn login_adds_replaces_and_keeps_the_others() {
        let a = Credentials { user: "a".into(), password: "1".into() };
        let b = Credentials { user: "b".into(), password: "2".into() };
        let a2 = Credentials { user: "a".into(), password: "3".into() };
        let doc = with_login(&with_login("", "one.example", &a), "two.example", &b);
        assert_eq!(lookup_credentials(&doc, "one.example"), Some(a));
        let doc = with_login(&doc, "one.example", &a2);
        assert_eq!(lookup_credentials(&doc, "one.example"), Some(a2));
        assert_eq!(lookup_credentials(&doc, "two.example"), Some(b));
        assert_eq!(auth_entries(&doc).len(), 2);
    }

    #[test]
    fn logout_removes_only_that_registry() {
        let c = Credentials { user: "a".into(), password: "1".into() };
        let doc = with_login(&with_login("", "one.example", &c), "two.example", &c);
        let (doc, removed) = without_login(&doc, "one.example");
        assert!(removed);
        assert_eq!(lookup_credentials(&doc, "one.example"), None);
        assert!(lookup_credentials(&doc, "two.example").is_some());
        assert!(!without_login(&doc, "one.example").1);
    }

    #[test]
    fn reads_a_docker_config_json() {
        // What `docker login` writes, extra fields and all.
        let doc = r#"{"auths":{"https://index.docker.io/v1/":{"auth":"YWxhZGRpbjpvcGVu"},
                       "ghcr.io":{"auth":"dTpw","email":"x@y"},
                       "helper.example":{}},
                      "credsStore":"desktop"}"#;
        let hub = lookup_credentials(doc, DOCKER_HUB).unwrap();
        assert_eq!((hub.user.as_str(), hub.password.as_str()), ("aladdin", "open"));
        assert_eq!(lookup_credentials(doc, "ghcr.io").unwrap().user, "u");
        assert!(lookup_credentials(doc, "helper.example").is_none());
        assert!(lookup_credentials(doc, "nowhere.example").is_none());
    }

    #[test]
    fn a_registry_with_a_port_is_its_own_key() {
        let c = Credentials { user: "u".into(), password: "p".into() };
        let doc = with_login("", "10.0.0.5:5000", &c);
        assert!(lookup_credentials(&doc, "10.0.0.5:5000").is_some());
        assert!(lookup_credentials(&doc, "10.0.0.5").is_none());
    }

    #[test]
    fn parses_a_hub_style_bearer_challenge() {
        let h = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:me/app:pull,push""#;
        assert_eq!(
            parse_challenge(h),
            Some(Challenge::Bearer {
                realm: "https://auth.docker.io/token".into(),
                service: Some("registry.docker.io".into()),
                scope: Some("repository:me/app:pull,push".into()),
            })
        );
    }

    #[test]
    fn parses_ghcr_style_and_basic_challenges() {
        assert_eq!(
            parse_challenge(r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:user/image:pull""#),
            Some(Challenge::Bearer {
                realm: "https://ghcr.io/token".into(),
                service: Some("ghcr.io".into()),
                scope: Some("repository:user/image:pull".into()),
            })
        );
        assert_eq!(parse_challenge(r#"Basic realm="Registry Realm""#), Some(Challenge::Basic));
        assert_eq!(parse_challenge("basic"), Some(Challenge::Basic));
    }

    #[test]
    fn a_bearer_challenge_needs_a_realm_and_other_schemes_are_refused() {
        assert_eq!(parse_challenge(r#"Bearer service="x""#), None);
        assert_eq!(parse_challenge("Negotiate"), None);
        assert_eq!(parse_challenge(""), None);
    }

    #[test]
    fn unquoted_and_reordered_parameters_parse() {
        assert_eq!(
            parse_challenge("Bearer scope=repository:a/b:pull, realm=https://t.example/tok"),
            Some(Challenge::Bearer {
                realm: "https://t.example/tok".into(),
                service: None,
                scope: Some("repository:a/b:pull".into()),
            })
        );
    }

    #[test]
    fn token_url_carries_service_and_a_push_scope() {
        assert_eq!(
            token_request_url("https://auth.docker.io/token", Some("registry.docker.io"), &push_scope("me/app")),
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:me/app:pull,push"
        );
        assert_eq!(
            token_request_url("https://t.example/tok?x=1", None, "repository:a:pull"),
            "https://t.example/tok?x=1&scope=repository:a:pull"
        );
    }

    #[test]
    fn token_url_escapes_what_must_be_escaped() {
        assert_eq!(token_request_url("https://t/tok", Some("a b&c"), "s"), "https://t/tok?service=a%20b%26c&scope=s");
    }

    #[test]
    fn a_token_reply_may_say_token_or_access_token() {
        assert_eq!(token_from_body(r#"{"token":"t1","expires_in":300}"#).as_deref(), Some("t1"));
        assert_eq!(token_from_body(r#"{"access_token":"t2"}"#).as_deref(), Some("t2"));
        assert_eq!(token_from_body(r#"{"error":"denied"}"#), None);
    }

    #[test]
    fn locations_resolve_against_the_registry_that_sent_them() {
        let base = "https://reg.example:5000";
        assert_eq!(resolve_location(base, "/v2/a/blobs/uploads/u1?_state=x"), "https://reg.example:5000/v2/a/blobs/uploads/u1?_state=x");
        assert_eq!(resolve_location(base, "https://cdn.example/up/1"), "https://cdn.example/up/1");
        assert_eq!(resolve_location(base, "v2/a/blobs/uploads/u1"), "https://reg.example:5000/v2/a/blobs/uploads/u1");
    }

    #[test]
    fn the_digest_joins_with_the_right_separator() {
        assert_eq!(with_digest("http://r/v2/a/blobs/uploads/u", "sha256:ab"), "http://r/v2/a/blobs/uploads/u?digest=sha256:ab");
        assert_eq!(with_digest("http://r/v2/a/blobs/uploads/u?_state=z", "sha256:ab"), "http://r/v2/a/blobs/uploads/u?_state=z&digest=sha256:ab");
    }

    #[test]
    fn upload_start_url_follows_the_scheme() {
        let mut r = parse_image_ref("localhost:5000/me/app:1");
        assert_eq!(r.upload_start_url(), "https://localhost:5000/v2/me/app/blobs/uploads/");
        r.insecure = true;
        assert_eq!(r.upload_start_url(), "http://localhost:5000/v2/me/app/blobs/uploads/");
    }
}
