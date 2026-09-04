use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// musl HEADERS come from the Alpine apk, not an in-tree musl build. We only need
// the headers to cross-compile tcc here; the libc/crt that compiled programs link
// against is supplied on Akuma by `apk add musl-dev` (same package, same version)
// — except on amd64, where nothing supplies a libc yet at all (see that arm's
// comment below). Pinned like apk-tools' downloads — bump in lockstep with the
// Akuma sysroot. The two architectures are not on the same Alpine package
// revision (`r2` for x86_64, `r23`→`r2` for aarch64 as Alpine's own revision
// history moved between when this was first pinned and 2026-09-04, when
// `latest-stable` stopped serving the old aarch64 revision at all — a `curl -L`
// with no `-f` doesn't fail on the 404 that followed, so the "download" used to
// silently cache an HTML error page as if it were the apk) — Alpine cuts
// per-arch revisions independently, so pinning one shared version string here
// would just be wrong for whichever arch didn't get that revision, and staying
// pinned to a revision this URL can 404 on is exactly what just happened.
const MUSL_DEV_URL_AARCH64: &str =
    "https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64/musl-dev-1.2.6-r2.apk";
const MUSL_DEV_URL_X86_64: &str =
    "https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/x86_64/musl-dev-1.2.6-r2.apk";

fn download_if_missing(url: &str, dest: &Path) {
    if dest.exists() {
        return;
    }
    let name = dest.file_name().unwrap().to_str().unwrap();
    println!("cargo:warning=Downloading {}...", name);
    let status = Command::new("curl")
        .arg("-L")
        // Fail on a 4xx/5xx response instead of writing the error page's HTML
        // to `dest` and reporting success — see this const's own comment for
        // the exact way that used to go wrong silently (a `tar xzf` on the
        // resulting "apk" a build or two later, far from this call, with an
        // error that named the tar step rather than the real cause here).
        .arg("-f")
        .arg(url)
        .arg("-o")
        .arg(dest)
        .status()
        .expect("Failed to execute curl");
    if !status.success() {
        panic!("Failed to download {}", url);
    }
}

/// Extract a single path (e.g. `usr/include`) out of an Alpine `.apk` (a gzipped,
/// multi-segment tar) into `dest`.
fn extract_apk_path(apk: &Path, dest: &Path, path: &str) {
    let status = Command::new("tar")
        .arg("xzf")
        .arg(apk)
        .arg("-C")
        .arg(dest)
        .arg(path)
        .status()
        .expect("Failed to run tar on apk");
    if !status.success() {
        panic!("Failed to extract '{}' from {}", path, apk.display());
    }
}

fn main() {
    println!("cargo:rerun-if-changed=tinycc/tcc.c");
    println!("cargo:rerun-if-changed=tinycc/libtcc.c");
    println!("cargo:rerun-if-changed=src/libc_stubs.c");
    println!("cargo:rerun-if-changed=src/setjmp_aarch64.S");
    println!("cargo:rerun-if-changed=src/setjmp_x86_64.S");
    println!("cargo:rerun-if-changed=src/config.h");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target = env::var("TARGET").unwrap();
    let host = env::var("HOST").unwrap();

    // Every other arch-specific choice in this file branches on this one
    // boolean. `userspace/tcc` has exactly two consumers today — the
    // `aarch64-unknown-linux-musl` build `userspace/build.sh` drives, and the
    // `x86_64-unknown-none` one `amd64/mkdisk.sh` drives — so a target string
    // no build here recognises is a configuration bug worth failing loudly
    // on, not a silent fall-through to whichever arch happened to be checked
    // first.
    let is_x86_64 = target.starts_with("x86_64");
    if !is_x86_64 && !target.starts_with("aarch64") {
        panic!(
            "userspace/tcc's build.rs only knows aarch64 and x86_64 targets; got '{}'. \
             Add that arch's branch here rather than guessing which existing one it's closest to.",
            target
        );
    }

    // ── musl headers from apk ───────────────────────────────────────────────
    let vendor_dir = manifest_dir.join("vendor");
    fs::create_dir_all(&vendor_dir).unwrap();
    let (musl_url, musl_apk_name) = if is_x86_64 {
        (MUSL_DEV_URL_X86_64, "musl-dev-x86_64.apk")
    } else {
        (MUSL_DEV_URL_AARCH64, "musl-dev-aarch64.apk")
    };
    let musl_apk = vendor_dir.join(musl_apk_name);
    download_if_missing(musl_url, &musl_apk);

    let musl_sysroot = out_dir.join("musl-sysroot");
    let _ = fs::remove_dir_all(&musl_sysroot);
    fs::create_dir_all(&musl_sysroot).unwrap();
    extract_apk_path(&musl_apk, &musl_sysroot, "usr/include");
    let musl_include = musl_sysroot.join("usr/include");
    if !musl_include.join("stdio.h").exists() {
        panic!(
            "musl headers not found at {} after extracting {}",
            musl_include.display(),
            musl_apk.display()
        );
    }

    // 1. Build TCC compiler itself
    //
    // `TCC_TARGET_X86_64` needs no runtime support tinycc doesn't already
    // vendor: upstream's own `lib/Makefile` builds x86_64's `libtcc1.a` from
    // `libtcc1.o` alone (`X86_64_O = libtcc1.o $(COMMON_O)`), where arm64
    // additionally pulls in `lib-arm64.o` (`ARM64_O = lib-arm64.o
    // $(COMMON_O)`) for helpers x86_64's calling convention does not need.
    // Neither arch here builds `$(COMMON_O)` (atomics/alloca/builtin helpers)
    // — that was already true of the AArch64 port before x86_64 existed, so
    // this is matching existing scope, not narrowing it further.
    let mut build = cc::Build::new();
    build
        .define(if is_x86_64 { "TCC_TARGET_X86_64" } else { "TCC_TARGET_ARM64" }, "1")
        .define("ONE_SOURCE", "1")
        .define("CONFIG_TCC_STATIC", "1")
        .define("CONFIG_TCC_SEMLOCK", "0")
        .flag("-ffreestanding")
        .flag("-fno-builtin")
        .flag("-nostdinc")
        .flag("-w")
        .include("tinycc")
        .include("tinycc/include")
        .include("src")
        .include(&musl_include)
        .target(&target)
        .host(&host);

    // Build the tcc compiler for SIZE on size/extreme kernels. A smaller tcc
    // image means a smaller demand-paged working set, which is what sets the
    // low-memory compile floor — so forward cargo's "s"/"z" OPT_LEVEL straight
    // to the C compiler (-Os/-Oz) instead of remapping it to -O3.
    let opt_level_str = env::var("OPT_LEVEL").unwrap();
    match opt_level_str.as_str() {
        "s" | "z" => { build.opt_level_str(&opt_level_str); }
        other => { build.opt_level(other.parse().unwrap_or(0)); }
    }
    // Emit one section per function/datum so the linker can garbage-collect the
    // tcc codegen paths that are never reached (pairs with --gc-sections below).
    build
        .flag("-ffunction-sections")
        .flag("-fdata-sections")
        .out_dir(&out_dir);

    // Apply -Dmain=tcc_main only to compiler sources
    let setjmp_src = if is_x86_64 { "src/setjmp_x86_64.S" } else { "src/setjmp_aarch64.S" };
    build
        .file("tinycc/tcc.c")
        .file("src/libc_stubs.c")
        .file(setjmp_src)
        .define("main", "tcc_main")
        .compile("tcc_all_objs");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=tcc_all_objs");
    // Garbage-collect unreferenced sections at link time (pairs with the
    // -ffunction-sections/-fdata-sections above) to shrink the final binary.
    println!("cargo:rustc-link-arg=--gc-sections");

    // 2. Build runtime objects for the sysroot
    let mut sysroot_build = cc::Build::new();
    sysroot_build.target(&target).host(&host);
    let compiler = sysroot_build.get_compiler();

    // The bare-metal clang target for the *runtime helper* objects — deliberately
    // not the same string as `cc::Build::target` fed the main compiler build
    // above (which lets `cc`'s own Rust-triple-to-clang-triple mapping pick),
    // because these two objects need to link into a `libtcc1.a` a real linker
    // consumes standalone, not just compile under whatever `cc` guessed. On
    // aarch64 that is `aarch64-none-elf` (unchanged from before x86_64
    // existed); x86_64 mirrors the same "-none-elf" bare-metal shape rather
    // than reaching for a hosted x86_64-*-linux-* triple this freestanding
    // build has no libc or startup files for.
    let sysroot_clang_target = if is_x86_64 { "x86_64-none-elf" } else { "aarch64-none-elf" };

    let run_cc = |src: &str, obj: &str, extra_args: &[&str]| {
        let mut cmd = compiler.to_command();
        cmd.arg("-target").arg(sysroot_clang_target);
        cmd.arg("-ffreestanding").arg("-fno-builtin").arg("-nostdinc").arg("-O3");
        cmd.args(extra_args);
        cmd.arg("-c").arg(src).arg("-o").arg(out_dir.join(obj));
        let status = cmd.status().expect("Failed to run compiler");
        if !status.success() {
            panic!("Compiler failed for src: {}", src);
        }
    };

    // 2. Build TCC runtime objects
    let musl_inc = musl_include.to_str().unwrap();
    run_cc("tinycc/lib/libtcc1.c", "libtcc1_base.o", &["-I", "tinycc", "-I", "tinycc/include", "-I", musl_inc]);
    // `lib-arm64.c` is ARM64-only (varargs/`__clear_cache` helpers x86_64's
    // codegen backend never calls into) — see the comment on `X86_64_O` above.
    // `objs` collects what actually goes in the archive so this stays a single
    // `run_ar` call either way, rather than two divergent ones.
    let mut objs = vec![out_dir.join("libtcc1_base.o")];
    if !is_x86_64 {
        run_cc(
            "tinycc/lib/lib-arm64.c",
            "lib-arm64.o",
            &["-D__arm64_clear_cache=__clear_cache", "-I", "tinycc", "-I", "tinycc/include", "-I", musl_inc],
        );
        objs.push(out_dir.join("lib-arm64.o"));
    }

    // Create archives manually
    let find_tool = |name: &str| {
        if Command::new(name).arg("--version").status().is_ok() {
            return Some(name.to_string());
        }
        let homebrew_path = format!("/opt/homebrew/opt/llvm/bin/{}", name);
        if Command::new(&homebrew_path).arg("--version").status().is_ok() {
            return Some(homebrew_path);
        }
        None
    };

    let ar_bin = find_tool("llvm-ar").unwrap_or_else(|| "ar".to_string());
    let ranlib_bin = find_tool("llvm-ranlib").unwrap_or_else(|| "ranlib".to_string());

    let ar_bin_clone = ar_bin.clone();
    let ranlib_bin_clone = ranlib_bin.clone();
    let run_ar = move |archive: &Path, objs: &[&Path]| {
        let mut cmd = Command::new(&ar_bin_clone);
        if ar_bin_clone.contains("llvm-ar") {
            cmd.arg("--format=gnu");
        }
        cmd.arg("rcs").arg(archive);
        for obj in objs {
            cmd.arg(obj);
        }
        let status = cmd.status().expect("Failed to run ar");
        if !status.success() {
            panic!("ar failed for archive: {:?}", archive);
        }

        let mut cmd = Command::new(&ranlib_bin_clone);
        cmd.arg(archive);
        let status = cmd.status().expect("Failed to run ranlib");
        if !status.success() {
            panic!("ranlib failed for archive: {:?}", archive);
        }
    };

    let obj_refs: Vec<&Path> = objs.iter().map(PathBuf::as_path).collect();
    run_ar(&out_dir.join("libtcc1.a"), &obj_refs);

    // 3. Stage + pack libtcc1.tar — the ONLY sysroot artifact we ship.
    //
    // It carries tcc's compiler-helper archive (libtcc1.a) AND tcc's internal
    // headers (tccdefs.h, stddef.h, stdarg.h, …). Combined with `apk add
    // musl-dev` on Akuma — which provides crt1.o/crti.o/crtn.o, libc.a and the
    // POSIX headers — this is everything our tcc needs. We deliberately no
    // longer build or ship a full musl sysroot (the old libc.tar); musl is
    // sourced from apk on both sides (headers here, libc on Akuma).
    //
    // amd64 has no `apk` and no musl `libc.a`/crt objects on its disk image at
    // all yet — this archive still ships for it (a smaller, self-contained
    // `-static -nostdlib`-shaped link is the plan there, not `apk add
    // musl-dev`), but that gap is real and open, not resolved by this file.
    let libtcc1_staging = out_dir.join("libtcc1_staging");
    if libtcc1_staging.exists() {
        fs::remove_dir_all(&libtcc1_staging).unwrap();
    }
    let libtcc1_tcc_dir = libtcc1_staging.join("usr/lib/tcc");
    let libtcc1_inc_dir = libtcc1_tcc_dir.join("include");
    fs::create_dir_all(&libtcc1_inc_dir).unwrap();
    fs::copy(out_dir.join("libtcc1.a"), libtcc1_tcc_dir.join("libtcc1.a")).unwrap();
    copy_dir_recursive(Path::new("tinycc/include"), &libtcc1_inc_dir).unwrap();

    let libtcc1_archive_path = out_dir.join("libtcc1.tar");
    let status = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("--no-xattrs")
        .arg("--format=ustar")
        .arg("-cf")
        .arg(&libtcc1_archive_path)
        .arg("-C")
        .arg(&libtcc1_staging)
        .arg("usr")
        .status()
        .expect("Failed to execute tar for libtcc1.tar");
    if !status.success() {
        panic!("tar command failed for libtcc1.tar");
    }

    // 4. Copy to dist directory
    //
    // `dist/libtcc1.tar` (no arch suffix) is a name `userspace/build.sh` reads
    // literally — it is the AArch64 pipeline's only consumer, so that exact
    // path must keep meaning "the aarch64 archive" and never get overwritten
    // by an x86_64 build run afterwards. x86_64's own copy gets its own name;
    // `amd64/mkdisk.sh` reads it by that name.
    let dist_dir = manifest_dir.join("dist");
    fs::create_dir_all(&dist_dir).unwrap();
    let dist_name = if is_x86_64 { "libtcc1-x86_64.tar" } else { "libtcc1.tar" };
    fs::copy(&libtcc1_archive_path, dist_dir.join(dist_name)).unwrap();
    println!("cargo:warning=libtcc1 archive created at {}", dist_dir.join(dist_name).display());
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.exists() {
        fs::create_dir_all(dst)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}
