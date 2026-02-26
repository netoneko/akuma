use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── Toolchain detection ──────────────────────────────────────────────────────

fn find_gmake() -> &'static str {
    let homebrew = "/opt/homebrew/opt/make/libexec/gnubin/make";
    if Path::new(homebrew).exists() { homebrew } else { "make" }
}

fn cross_ar() -> &'static str {
    // Prefer the musl cross-archiver: its archive index format is
    // compatible with aarch64-linux-musl-ld. llvm-ar produces a
    // different index that the cross-linker can't read.
    if Command::new("aarch64-linux-musl-ar").arg("--version").output().is_ok() {
        "aarch64-linux-musl-ar"
    } else {
        let homebrew = "/opt/homebrew/opt/llvm/bin/llvm-ar";
        if Path::new(homebrew).exists() { homebrew } else { "ar" }
    }
}

fn cross_ranlib() -> &'static str {
    if Command::new("aarch64-linux-musl-ranlib").arg("--version").output().is_ok() {
        "aarch64-linux-musl-ranlib"
    } else {
        let homebrew = "/opt/homebrew/opt/llvm/bin/llvm-ranlib";
        if Path::new(homebrew).exists() { homebrew } else { "ranlib" }
    }
}

/// The build host triplet (e.g. "aarch64-apple-darwin" on Apple Silicon).
/// Used as --build= in configure scripts to signal cross-compilation.
fn host_triplet() -> String {
    env::var("HOST").unwrap_or_else(|_| "x86_64-unknown-linux-gnu".to_string())
}

// ── Download helpers ─────────────────────────────────────────────────────────

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

fn extract_tarball(tarball: &Path, dest_dir: &Path) {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(dest_dir)
        .status()
        .expect("Failed to execute tar");
    if !status.success() {
        // Remove the tarball so it gets re-downloaded next time
        let _ = fs::remove_file(tarball);
        panic!("Failed to extract {}. Bad download? Removed tarball to force re-download.", tarball.display());
    }
}

// ── Build: zlib ──────────────────────────────────────────────────────────────

fn build_zlib(vendor_dir: &Path, deps_dir: &Path, cc: &str, gmake: &str) {
    if deps_dir.join("lib/libz.a").exists() {
        println!("cargo:warning=zlib already built, skipping.");
        return;
    }
    let tarball = vendor_dir.join("zlib-1.3.1.tar.gz");
    download_if_missing("https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz", &tarball);

    let src_dir = vendor_dir.join("zlib-1.3.1");
    if src_dir.exists() { fs::remove_dir_all(&src_dir).unwrap(); }
    extract_tarball(&tarball, vendor_dir);

    println!("cargo:warning=Configuring zlib...");
    let status = Command::new("./configure")
        .current_dir(&src_dir)
        .env("CC", cc)
        .env("CFLAGS", "-Os -fPIC")
        .arg(format!("--prefix={}", deps_dir.display()))
        .arg("--static")
        .status()
        .expect("Failed to configure zlib");
    if !status.success() { panic!("zlib configure failed"); }

    // zlib configure on macOS sets AR=libtool, which cannot handle ELF objects.
    // Patch the generated Makefile to use the cross-archiver instead.
    let makefile_path = src_dir.join("Makefile");
    let content = fs::read_to_string(&makefile_path).unwrap();
    let patched = content
        .replace("AR=libtool", &format!("AR={}", cross_ar()))
        .replace("ARFLAGS=-o", "ARFLAGS=rcs");
    fs::write(&makefile_path, patched).unwrap();

    println!("cargo:warning=Building zlib...");
    let status = Command::new(gmake)
        .current_dir(&src_dir)
        .env("AR", cross_ar())
        .env("RANLIB", cross_ranlib())
        .arg("install")
        .status()
        .expect("Failed to build zlib");
    if !status.success() { panic!("zlib build failed"); }
}

// ── Build: lz4 ───────────────────────────────────────────────────────────────

fn build_lz4(vendor_dir: &Path, deps_dir: &Path, cc: &str, gmake: &str) {
    if deps_dir.join("lib/liblz4.a").exists() {
        println!("cargo:warning=lz4 already built, skipping.");
        return;
    }
    let tarball = vendor_dir.join("lz4-1.9.4.tar.gz");
    download_if_missing(
        "https://github.com/lz4/lz4/releases/download/v1.9.4/lz4-1.9.4.tar.gz",
        &tarball,
    );

    let src_dir = vendor_dir.join("lz4-1.9.4");
    if src_dir.exists() { fs::remove_dir_all(&src_dir).unwrap(); }
    extract_tarball(&tarball, vendor_dir);

    // lz4's `lib` target always builds both static and shared; the shared lib
    // uses macOS dylib flags (-install_name etc.) that the cross-compiler
    // rejects. Build only the static archive explicitly, then install manually.
    println!("cargo:warning=Building lz4...");
    let status = Command::new(gmake)
        .current_dir(src_dir.join("lib"))
        .env("CC", cc)
        .env("AR", cross_ar())
        .env("RANLIB", cross_ranlib())
        .env("CFLAGS", "-Os -fPIC")
        .arg("liblz4.a")
        .status()
        .expect("Failed to build lz4");
    if !status.success() { panic!("lz4 build failed"); }

    // Install static lib, headers, and .pc file manually
    let lz4_lib = src_dir.join("lib");
    let deps_lib = deps_dir.join("lib");
    let deps_inc = deps_dir.join("include");
    let deps_pc = deps_lib.join("pkgconfig");
    fs::create_dir_all(&deps_lib).unwrap();
    fs::create_dir_all(&deps_inc).unwrap();
    fs::create_dir_all(&deps_pc).unwrap();
    fs::copy(lz4_lib.join("liblz4.a"), deps_lib.join("liblz4.a")).unwrap();
    for h in &["lz4.h", "lz4hc.h", "lz4frame.h", "lz4frame_static.h"] {
        fs::copy(lz4_lib.join(h), deps_inc.join(h)).unwrap();
    }
    fs::write(deps_pc.join("liblz4.pc"), format!(
        "prefix={prefix}\nexec_prefix=${{prefix}}\nlibdir=${{prefix}}/lib\nincludedir=${{prefix}}/include\n\nName: lz4\nDescription: extremely fast lossless compression\nVersion: 1.9.4\nLibs: -L${{libdir}} -llz4\nLibs.private: -L${{libdir}} -llz4\nCflags: -I${{includedir}}\n",
        prefix = deps_dir.display()
    )).unwrap();
}

// ── Build: zstd ──────────────────────────────────────────────────────────────

fn build_zstd(vendor_dir: &Path, deps_dir: &Path, cc: &str, gmake: &str) {
    if deps_dir.join("lib/libzstd.a").exists() {
        println!("cargo:warning=zstd already built, skipping.");
        return;
    }
    let tarball = vendor_dir.join("zstd-1.5.5.tar.gz");
    download_if_missing(
        "https://github.com/facebook/zstd/releases/download/v1.5.5/zstd-1.5.5.tar.gz",
        &tarball,
    );

    let src_dir = vendor_dir.join("zstd-1.5.5");
    if src_dir.exists() { fs::remove_dir_all(&src_dir).unwrap(); }
    extract_tarball(&tarball, vendor_dir);

    // The default `install` target includes `install-shared` which uses macOS
    // dylib flags that the cross-compiler rejects. Build only the targets we need.
    println!("cargo:warning=Building zstd...");
    for target in &["install-static", "install-pc", "install-includes"] {
        let status = Command::new(gmake)
            .current_dir(src_dir.join("lib"))
            .env("CC", cc)
            .env("AR", cross_ar())
            .env("RANLIB", cross_ranlib())
            .env("CFLAGS", "-Os -fPIC")
            .arg(target)
            .arg(format!("PREFIX={}", deps_dir.display()))
            .status()
            .unwrap_or_else(|e| panic!("Failed to run make {}: {}", target, e));
        if !status.success() { panic!("zstd {} failed", target); }
    }
}

// ── Build: LibreSSL ──────────────────────────────────────────────────────────

fn build_libressl(vendor_dir: &Path, deps_dir: &Path, cc: &str, gmake: &str) {
    if deps_dir.join("lib/libssl.a").exists() {
        println!("cargo:warning=LibreSSL already built, skipping.");
        return;
    }
    let tarball = vendor_dir.join("libressl-3.9.2.tar.gz");
    download_if_missing(
        "https://ftp.openbsd.org/pub/OpenBSD/LibreSSL/libressl-3.9.2.tar.gz",
        &tarball,
    );

    let src_dir = vendor_dir.join("libressl-3.9.2");
    if src_dir.exists() { fs::remove_dir_all(&src_dir).unwrap(); }
    extract_tarball(&tarball, vendor_dir);

    println!("cargo:warning=Configuring LibreSSL...");
    let status = Command::new("./configure")
        .current_dir(&src_dir)
        .env("CC", cc)
        .env("AR", cross_ar())
        .env("RANLIB", cross_ranlib())
        .env("CFLAGS", "-Os -fPIC")
        .arg(format!("--build={}", host_triplet()))
        .arg("--host=aarch64-linux-musl")
        .arg(format!("--prefix={}", deps_dir.display()))
        .arg("--disable-shared")
        .arg("--enable-static")
        .arg("--disable-tests")
        .arg("--without-openssldir")
        .status()
        .expect("Failed to configure LibreSSL");
    if !status.success() { panic!("LibreSSL configure failed"); }

    println!("cargo:warning=Building LibreSSL...");
    let status = Command::new(gmake)
        .current_dir(&src_dir)
        .env("AR", cross_ar())
        .env("RANLIB", cross_ranlib())
        .arg("install")
        .status()
        .expect("Failed to build LibreSSL");
    if !status.success() { panic!("LibreSSL build failed"); }
}

// ── Build: libarchive ────────────────────────────────────────────────────────

fn build_libarchive(vendor_dir: &Path, deps_dir: &Path, cc: &str, gmake: &str) {
    if deps_dir.join("lib/libarchive.a").exists()
        && deps_dir.join("include/archive.h").exists()
        && deps_dir.join("include/archive_entry.h").exists()
    {
        println!("cargo:warning=libarchive already built, skipping.");
        return;
    }
    let tarball = vendor_dir.join("libarchive-3.7.4.tar.gz");
    download_if_missing(
        "https://github.com/libarchive/libarchive/releases/download/v3.7.4/libarchive-3.7.4.tar.gz",
        &tarball,
    );

    let src_dir = vendor_dir.join("libarchive-3.7.4");
    if src_dir.exists() { fs::remove_dir_all(&src_dir).unwrap(); }
    extract_tarball(&tarball, vendor_dir);

    let pkgconfig_dir = deps_dir.join("lib/pkgconfig");
    let include = deps_dir.join("include");
    let lib_dir = deps_dir.join("lib");

    println!("cargo:warning=Configuring libarchive...");
    let status = Command::new("./configure")
        .current_dir(&src_dir)
        .env("CC", cc)
        .env("AR", cross_ar())
        .env("RANLIB", cross_ranlib())
        .env("CFLAGS", format!("-Os -fPIC -I{}", include.display()))
        .env("LDFLAGS", format!("-L{}", lib_dir.display()))
        // Tell pkg-config to only look in our deps dir so it finds lz4/zstd
        .env("PKG_CONFIG_LIBDIR", &pkgconfig_dir)
        .arg(format!("--build={}", host_triplet()))
        .arg("--host=aarch64-linux-musl")
        .arg(format!("--prefix={}", deps_dir.display()))
        .arg("--disable-shared")
        .arg("--enable-static")
        .arg("--with-zlib")
        .arg("--with-lz4")
        .arg("--with-zstd")
        .arg("--without-bz2lib")
        .arg("--without-libb2")
        .arg("--without-iconv")
        .arg("--without-lzma")
        .arg("--without-lzo2")
        .arg("--without-xml2")
        .arg("--without-expat")
        .arg("--without-openssl")
        .arg("--without-cng")
        .arg("--disable-bsdtar")
        .arg("--disable-bsdcpio")
        .arg("--disable-bsdcat")
        .arg("--disable-bsdunzip")
        .status()
        .expect("Failed to configure libarchive");
    if !status.success() { panic!("libarchive configure failed"); }

    println!("cargo:warning=Building libarchive...");
    let status = Command::new(gmake)
        .current_dir(&src_dir)
        .env("AR", cross_ar())
        .env("RANLIB", cross_ranlib())
        .arg("install")
        .status()
        .expect("Failed to build libarchive");
    if !status.success() { panic!("libarchive build failed"); }

    // libarchive's generated .pc file omits zlib from Libs.private when
    // zlib is detected via the system rather than pkg-config Requires.
    // Patch it so `pkg-config --libs --static libarchive` includes -lz.
    let pc_path = deps_dir.join("lib/pkgconfig/libarchive.pc");
    if pc_path.exists() {
        let pc = fs::read_to_string(&pc_path).unwrap();
        if !pc.contains("-lz") {
            let patched = pc.replace(
                "Libs.private:",
                "Libs.private: -lz",
            );
            fs::write(&pc_path, patched).unwrap();
            println!("cargo:warning=Patched libarchive.pc to include -lz");
        }
    }
}

fn reindex_archives(deps_dir: &Path) {
    let lib_dir = deps_dir.join("lib");
    if let Ok(entries) = fs::read_dir(&lib_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "a").unwrap_or(false) {
                let name = path.file_name().unwrap().to_str().unwrap();
                println!("cargo:warning=Re-indexing {} with aarch64-linux-musl-ranlib", name);
                let _ = Command::new("aarch64-linux-musl-ranlib")
                    .arg(&path)
                    .status();
            }
        }
    }
}

// ── Build: xbps ──────────────────────────────────────────────────────────────

fn build_xbps(xbps_src: &Path, deps_dir: &Path, staging_dir: &Path, cc: &str, gmake: &str) {
    if !xbps_src.exists() {
        panic!(
            "xbps submodule not found at {}. Run: git submodule update --init",
            xbps_src.display()
        );
    }

    let pkgconfig_dir = deps_dir.join("lib/pkgconfig");
    let extra_ldflags = format!("-L{} -Wl,--entry=_start", deps_dir.join("lib").display());

    println!("cargo:warning=Configuring xbps...");
    let status = Command::new("./configure")
        .current_dir(xbps_src)
        .env("CC", cc)
        .env("PKG_CONFIG_LIBDIR", &pkgconfig_dir)
        .env("PKG_CONFIG_PATH", "")
        .arg("--prefix=/usr")
        .arg(format!("--build={}", host_triplet()))
        .arg("--host=aarch64-unknown-linux-musl")
        .arg("--sysconfdir=/etc")
        .arg("--enable-static")
        .status()
        .expect("Failed to configure xbps");
    if !status.success() { panic!("xbps configure failed"); }

    // Patch config.mk to inject our LDFLAGS (deps path, entry point),
    // remove PIE flags (conflict with -static), and tune CFLAGS
    patch_config_mk(xbps_src, &extra_ldflags);
    // Patch lib/Makefile to only build static library (libxbps.so fails
    // or is useless when cross-compiling for a static-only target)
    patch_lib_makefile(xbps_src);
    // Patch mk/prog.mk so only .static binaries are built and installed
    // (renamed to the base name since Akuma has no shared libs)
    patch_prog_mk(xbps_src);

    println!("cargo:warning=Building xbps...");
    let status = Command::new(gmake)
        .current_dir(xbps_src)
        .env("CC", cc)
        .env("AR", cross_ar())
        .env("RANLIB", cross_ranlib())
        .env("DESTDIR", staging_dir)
        .env("PKG_CONFIG_LIBDIR", &pkgconfig_dir)
        .arg("install")
        .status()
        .expect("Failed to build xbps");
    if !status.success() { panic!("xbps build failed"); }
}

fn patch_config_mk(dir: &Path, extra_ldflags: &str) {
    let path = dir.join("config.mk");
    if !path.exists() { return; }
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let patched: String = content
        .lines()
        .map(|line| {
            if line.starts_with("LDFLAGS =") || line.starts_with("LDFLAGS=") {
                let existing = line.split_once('=').map(|(_, v)| v.trim()).unwrap_or("");
                format!("LDFLAGS = {} {}", extra_ldflags, existing)
            } else if line.starts_with("LDFLAGS +=") || line.starts_with("LDFLAGS+=") {
                // Strip -l* library flags from LDFLAGS appends — they cause
                // link order issues with static linking. Libraries are already
                // listed in STATIC_LIBS in the correct order.
                let val = line.split_once("+=").map(|(_, v)| v.trim()).unwrap_or("");
                let filtered: String = val.split_whitespace()
                    .filter(|tok| !tok.starts_with("-l"))
                    .collect::<Vec<_>>()
                    .join(" ");
                if filtered.is_empty() {
                    String::new()
                } else {
                    format!("LDFLAGS += {}", filtered)
                }
            } else if line.starts_with("CFLAGS =") || line.starts_with("CFLAGS=") {
                let existing = line.split_once('=').map(|(_, v)| v.trim()).unwrap_or("");
                format!("CFLAGS = -Os {}", existing)
            } else if line.contains("-fPIE") || line.contains("-pie") {
                String::new()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if patched != content {
        println!("cargo:warning=Patched config.mk");
        fs::write(&path, patched).expect("Failed to patch config.mk");
    }
}

fn patch_lib_makefile(xbps_src: &Path) {
    let path = xbps_src.join("lib/Makefile");
    let content = fs::read_to_string(&path).expect("Failed to read lib/Makefile");
    let patched = content
        // Only build the static archive — shared lib uses -shared which
        // conflicts with the static cross-compilation target
        .replace("all: libxbps.so libxbps.a", "all: libxbps.a")
        // Install only the static archive
        .replace(
            "install: all\n\tinstall -d $(DESTDIR)$(LIBDIR)\n\tinstall -m 644 libxbps.a $(DESTDIR)$(LIBDIR)\n\tinstall -m 755 $(LIBXBPS_SHLIB) $(DESTDIR)$(LIBDIR)\n\tcp -a libxbps.so $(DESTDIR)$(LIBDIR)\n\tcp -a libxbps.so.$(LIBXBPS_MAJOR) $(DESTDIR)$(LIBDIR)",
            "install: all\n\tinstall -d $(DESTDIR)$(LIBDIR)\n\tinstall -m 644 libxbps.a $(DESTDIR)$(LIBDIR)",
        );
    if patched != content {
        println!("cargo:warning=Patched lib/Makefile for static-only build");
        fs::write(&path, patched).unwrap();
    }
}

fn patch_prog_mk(xbps_src: &Path) {
    let path = xbps_src.join("mk/prog.mk");
    let content = fs::read_to_string(&path).expect("Failed to read mk/prog.mk");
    let patched = content
        // Only build the static binary
        .replace(
            "BINS = $(BIN)\nMANSECTION ?= 1",
            "BINS =\nMANSECTION ?= 1",
        )
        .replace(
            "BINS += $(BIN).static",
            "BINS = $(BIN).static",
        )
        // Install .static binary as the base name
        .replace(
            "\tinstall -m 755 $(BIN) $(DESTDIR)$(SBINDIR)\nifdef BUILD_STATIC\n\tinstall -m 755 $(BIN).static $(DESTDIR)$(SBINDIR)\nendif",
            "\tinstall -m 755 $(BIN).static $(DESTDIR)$(SBINDIR)/$(BIN)",
        )
        // Wrap STATIC_LIBS with --start-group/--end-group so the linker
        // resolves circular deps between libxbps, libarchive, libssl, zlib
        .replace(
            "$(STATIC_LIBS) -o $@",
            "-Wl,--start-group $(STATIC_LIBS) -Wl,--end-group -o $@",
        );
    if patched != content {
        println!("cargo:warning=Patched mk/prog.mk for static-only install");
        fs::write(&path, patched).unwrap();
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Persistent build directories (survive `cargo clean` — downloads and
    // compiled deps are expensive to rebuild)
    let vendor_dir = manifest_dir.join("vendor");
    let deps_dir = manifest_dir.join("build/deps");
    let dist_dir = manifest_dir.join("dist");

    // Staging area (in OUT_DIR)
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let staging_dir = out_dir.join("staging");

    let xbps_src = manifest_dir.join("xbps");
    let bootstrap_archives = manifest_dir.join("../../bootstrap/archives");

    let cc = "aarch64-linux-musl-gcc";
    let gmake = find_gmake();

    for dir in &[&vendor_dir, &deps_dir, &dist_dir, &staging_dir] {
        fs::create_dir_all(dir).unwrap();
    }

    // Dependency chain — order matters
    build_zlib(&vendor_dir, &deps_dir, cc, gmake);
    build_lz4(&vendor_dir, &deps_dir, cc, gmake);
    build_zstd(&vendor_dir, &deps_dir, cc, gmake);
    build_libressl(&vendor_dir, &deps_dir, cc, gmake);
    build_libarchive(&vendor_dir, &deps_dir, cc, gmake);

    // Re-index all static archives with the cross-ranlib so the archive
    // symbol table is in a format the cross-linker can read. Some deps
    // (zlib, libarchive) use autotools libtool which generates archives
    // with an index incompatible with aarch64-linux-musl-ld.
    reindex_archives(&deps_dir);

    build_xbps(&xbps_src, &deps_dir, &staging_dir, cc, gmake);

    // Package
    let archive_path = dist_dir.join("xbps.tar");
    println!("cargo:warning=Creating xbps.tar...");
    let status = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("--no-xattrs")
        .arg("--format=ustar")
        .arg("-cf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&staging_dir)
        .arg("usr")
        .status()
        .expect("Failed to execute tar");
    if !status.success() { panic!("tar creation failed"); }

    fs::create_dir_all(&bootstrap_archives).unwrap();
    fs::copy(&archive_path, bootstrap_archives.join("xbps.tar"))
        .expect("Failed to copy xbps.tar to bootstrap/archives");

    println!("cargo:warning=xbps package ready: {}", archive_path.display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=xbps/configure");
}
