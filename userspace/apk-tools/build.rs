use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const APK_STATIC_URL: &str =
    "https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64/apk-tools-static-3.0.5-r0.apk";
const ALPINE_KEYS_URL: &str =
    "https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64/alpine-keys-2.6-r0.apk";

fn download_if_missing(url: &str, dest: &Path) {
    if dest.exists() {
        return;
    }
    let name = dest.file_name().unwrap().to_str().unwrap();
    println!("cargo:warning=Downloading {}...", name);
    let status = Command::new("curl")
        .arg("-L")
        .arg(url)
        .arg("-o")
        .arg(dest)
        .status()
        .expect("Failed to execute curl");
    if !status.success() {
        panic!("Failed to download {}", url);
    }
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor_dir = manifest_dir.join("vendor");
    let dist_dir = manifest_dir.join("dist");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let staging_dir = out_dir.join("staging");
    let bootstrap = manifest_dir.join("../../bootstrap");

    for dir in [&vendor_dir, &dist_dir, &staging_dir] {
        fs::create_dir_all(dir).unwrap();
    }

    // ── Download ────────────────────────────────────────────────────────────
    let apk_pkg = vendor_dir.join("apk-tools-static.apk");
    let keys_pkg = vendor_dir.join("alpine-keys.apk");
    download_if_missing(APK_STATIC_URL, &apk_pkg);
    download_if_missing(ALPINE_KEYS_URL, &keys_pkg);

    // ── Extract apk.static binary, rename to apk, place in bin/ ────────────
    let staging_bin = staging_dir.join("bin");
    fs::create_dir_all(&staging_bin).unwrap();

    let status = Command::new("tar")
        .arg("xzf")
        .arg(&apk_pkg)
        .arg("-C")
        .arg(&staging_dir)
        .arg("sbin/apk.static")
        .status()
        .expect("Failed to extract apk-tools-static");
    if !status.success() {
        panic!("Failed to extract apk.static");
    }

    fs::rename(staging_dir.join("sbin/apk.static"), staging_bin.join("apk")).unwrap();
    let _ = fs::remove_dir(staging_dir.join("sbin"));

    // ── Extract signing keys ───────────────────────────────────────────────
    let status = Command::new("tar")
        .arg("xzf")
        .arg(&keys_pkg)
        .arg("-C")
        .arg(&staging_dir)
        .arg("--include=etc/apk/keys/*")
        .arg("--include=usr/share/apk/keys/*")
        .status()
        .expect("Failed to extract alpine-keys");
    if !status.success() {
        panic!("Failed to extract alpine-keys");
    }

    // ── Create archive ─────────────────────────────────────────────────────
    let archive_path = dist_dir.join("apk-tools.tar");
    println!("cargo:warning=Creating apk-tools.tar...");
    let status = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("--no-xattrs")
        .arg("--format=ustar")
        .arg("-cf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&staging_dir)
        .arg("bin")
        .arg("etc")
        .arg("usr")
        .status()
        .expect("Failed to create tar");
    if !status.success() {
        panic!("tar creation failed");
    }

    let bootstrap_archives = bootstrap.join("archives");
    fs::create_dir_all(&bootstrap_archives).unwrap();
    fs::copy(&archive_path, bootstrap_archives.join("apk-tools.tar"))
        .expect("Failed to copy apk-tools.tar to bootstrap/archives");
    println!(
        "cargo:warning=apk-tools archive ready: {}",
        archive_path.display()
    );

    // ── Bootstrap: copy apk binary to bin/ ─────────────────────────────────
    let bootstrap_bin = bootstrap.join("bin");
    fs::create_dir_all(&bootstrap_bin).unwrap();
    fs::copy(staging_bin.join("apk"), bootstrap_bin.join("apk"))
        .expect("Failed to copy apk to bootstrap/bin");

    // ── Bootstrap: APK config ──────────────────────────────────────────────
    let apk_etc = bootstrap.join("etc/apk");
    let apk_keys = apk_etc.join("keys");
    let apk_cache = bootstrap.join("var/cache/apk");
    let apk_db = bootstrap.join("lib/apk/db");

    for dir in [&apk_etc, &apk_keys, &apk_cache, &apk_db] {
        fs::create_dir_all(dir).unwrap();
    }

    fs::write(
        apk_etc.join("repositories"),
        "http://dl-cdn.alpinelinux.org/alpine/latest-stable/main\n\
         http://dl-cdn.alpinelinux.org/alpine/latest-stable/community\n",
    )
    .unwrap();

    fs::write(apk_etc.join("arch"), "aarch64\n").unwrap();

    // Copy signing keys into bootstrap
    let staging_keys = staging_dir.join("etc/apk/keys");
    if staging_keys.is_dir() {
        for entry in fs::read_dir(&staging_keys).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), apk_keys.join(entry.file_name())).unwrap();
        }
    }

    // .gitkeep for empty dirs
    for dir in [&apk_cache, &apk_db] {
        let gitkeep = dir.join(".gitkeep");
        if !gitkeep.exists() {
            fs::write(&gitkeep, "").unwrap();
        }
    }

    println!("cargo:rerun-if-changed=build.rs");
}
