use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let src_dir = manifest_dir.join("llama.cpp");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let build_dir = out_dir.join("llama-build");
    let target_bin_dir = manifest_dir.join("../../bootstrap/bin");

    if !src_dir.exists() {
        panic!(
            "llama.cpp source not found at {}. Did you forget to initialize submodules?",
            src_dir.display()
        );
    }

    fs::create_dir_all(&build_dir).expect("Failed to create build directory");

    // CMake configure
    println!("cargo:warning=Configuring llama.cpp with CMake...");
    let status = Command::new("cmake")
        .current_dir(&build_dir)
        .arg(&src_dir)
        .arg("-DCMAKE_C_COMPILER=aarch64-linux-musl-gcc")
        .arg("-DCMAKE_CXX_COMPILER=aarch64-linux-musl-g++")
        .arg("-DCMAKE_C_FLAGS=-march=armv8.2-a+fp16+dotprod")
        .arg("-DCMAKE_CXX_FLAGS=-march=armv8.2-a+fp16+dotprod")
        .arg("-DCMAKE_EXE_LINKER_FLAGS=-static -Wl,--entry=_start")
        .arg("-DCMAKE_SYSTEM_NAME=Linux")
        .arg("-DCMAKE_SYSTEM_PROCESSOR=aarch64")
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg("-DGGML_NATIVE=OFF")
        .arg("-DGGML_CPU_AARCH64=ON")
        .arg("-DGGML_OPENMP=OFF")
        .arg("-DGGML_BLAS=OFF")
        .arg("-DGGML_CUDA=OFF")
        .arg("-DGGML_METAL=OFF")
        .arg("-DGGML_VULKAN=OFF")
        .arg("-DGGML_RPC=OFF")
        .arg("-DBUILD_SHARED_LIBS=OFF")
        .arg("-DLLAMA_CURL=OFF")
        .arg("-DLLAMA_OPENSSL=OFF")
        .arg("-DLLAMA_BUILD_EXAMPLES=OFF")
        .arg("-DLLAMA_BUILD_TESTS=OFF")
        .status()
        .expect("Failed to execute cmake — is cmake installed?");

    if !status.success() {
        panic!("cmake configure failed");
    }

    let num_jobs = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "4".to_string());

    fs::create_dir_all(&target_bin_dir).expect("Failed to create bootstrap/bin");

    // Build and install llama-cli
    println!("cargo:warning=Building llama-cli...");
    let status = Command::new("cmake")
        .arg("--build")
        .arg(&build_dir)
        .arg("--target")
        .arg("llama-cli")
        .arg("-j")
        .arg(&num_jobs)
        .status()
        .expect("Failed to execute cmake --build");

    if !status.success() {
        panic!("Failed to build llama-cli");
    }

    let compiled_cli = build_dir.join("bin/llama-cli");
    if !compiled_cli.exists() {
        panic!("llama-cli binary not found at {}", compiled_cli.display());
    }
    let dest_cli = target_bin_dir.join("llama-cli");
    fs::copy(&compiled_cli, &dest_cli).expect("Failed to copy llama-cli");
    let _ = Command::new("aarch64-linux-musl-strip").arg(&dest_cli).status();
    println!("cargo:warning=Installed llama-cli to bootstrap/bin/llama-cli");

    // Build and install llama-server
    println!("cargo:warning=Building llama-server...");
    let status = Command::new("cmake")
        .arg("--build")
        .arg(&build_dir)
        .arg("--target")
        .arg("llama-server")
        .arg("-j")
        .arg(&num_jobs)
        .status()
        .expect("Failed to execute cmake --build");

    if !status.success() {
        panic!("Failed to build llama-server");
    }

    let compiled_server = build_dir.join("bin/llama-server");
    if !compiled_server.exists() {
        panic!("llama-server binary not found at {}", compiled_server.display());
    }
    let dest_server = target_bin_dir.join("llama-server");
    fs::copy(&compiled_server, &dest_server).expect("Failed to copy llama-server");
    let _ = Command::new("aarch64-linux-musl-strip").arg(&dest_server).status();
    println!("cargo:warning=Installed llama-server to bootstrap/bin/llama-server");

    // Build and install llama-bench
    println!("cargo:warning=Building llama-bench...");
    let status = Command::new("cmake")
        .arg("--build")
        .arg(&build_dir)
        .arg("--target")
        .arg("llama-bench")
        .arg("-j")
        .arg(&num_jobs)
        .status()
        .expect("Failed to execute cmake --build");

    if !status.success() {
        panic!("Failed to build llama-bench");
    }

    let compiled_bench = build_dir.join("bin/llama-bench");
    if !compiled_bench.exists() {
        panic!("llama-bench binary not found at {}", compiled_bench.display());
    }
    let dest_bench = target_bin_dir.join("llama-bench");
    fs::copy(&compiled_bench, &dest_bench).expect("Failed to copy llama-bench");
    let _ = Command::new("aarch64-linux-musl-strip").arg(&dest_bench).status();
    println!("cargo:warning=Installed llama-bench to bootstrap/bin/llama-bench");

    println!("cargo:rerun-if-changed=build.rs");
}
