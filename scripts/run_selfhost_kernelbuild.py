import subprocess, sys
BUILD = ("/bin/busybox env PATH=/usr/local/bin:/usr/bin:/bin HOME=/root CARGO_HOME=/root/.cargo "
         "RUSTC=/usr/local/bin/rustc CARGO_BUILD_TARGET=aarch64-unknown-none "
         "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS=-Clink-arg=-T/root/akuma/linker.ld "
         "/usr/bin/cargo build --release -p akuma --manifest-path /root/akuma/Cargo.toml -j1")
with open("logs/inv_kernelbuild.log","w") as f:
    p = subprocess.Popen(["ssh","-o","StrictHostKeyChecking=no","-o","UserKnownHostsFile=/dev/null",
                          "-o","ConnectTimeout=10","-o","ServerAliveInterval=30",
                          "-p","2322","root@localhost",BUILD],
                         stdout=f, stderr=subprocess.STDOUT)
    p.wait()
    f.write(f"\n=== BUILD SSH EXIT {p.returncode} ===\n")
