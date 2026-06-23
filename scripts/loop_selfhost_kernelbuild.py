import subprocess, time, re, sys
BUILD = ("/bin/busybox env PATH=/usr/local/bin:/usr/bin:/bin HOME=/root CARGO_HOME=/root/.cargo "
         "RUSTC=/usr/local/bin/rustc CARGO_BUILD_TARGET=aarch64-unknown-none "
         "CARGO_TARGET_AARCH64_UNKNOWN_NONE_RUSTFLAGS=-Clink-arg=-T/root/akuma/linker.ld "
         "/usr/bin/cargo build --release -p akuma --manifest-path /root/akuma/Cargo.toml -j1")
def ssh_build(logf):
    with open(logf,"w") as f:
        p=subprocess.Popen(["ssh","-o","StrictHostKeyChecking=no","-o","UserKnownHostsFile=/dev/null",
            "-o","ConnectTimeout=10","-o","ServerAliveInterval=30","-p","2322","root@localhost",BUILD],
            stdout=f,stderr=subprocess.STDOUT)
        p.wait(); return p.returncode
ansi=re.compile(r'\x1b\[[0-9;]*m')
def last_step(txt):
    m=re.findall(r'(\d+)/147: ([a-z0-9_-]+)',txt); return m[-1] if m else ("0","?")
MAX=12; prev_fail=None; same=0
for attempt in range(1,MAX+1):
    lf=f"logs/loopbuild_{attempt}.log"
    print(f"[attempt {attempt}] starting", flush=True)
    rc=ssh_build(lf)
    txt=ansi.sub('',open(lf).read())
    step,crate=last_step(txt)
    if "Finished" in txt and "release" in txt.split("Finished")[-1][:30] or re.search(r'Finished `release`',txt):
        print(f"[attempt {attempt}] *** FINISHED at {step}/147 ***", flush=True); break
    # find the crate that failed to compile
    fm=re.search(r"could not compile `([^`]+)`",txt)
    failcrate=fm.group(1) if fm else None
    sig11 = "signal: 11" in txt or "SIGSEGV" in txt
    print(f"[attempt {attempt}] rc={rc} step={step}/147 ({crate}) fail={failcrate} sig11={sig11}", flush=True)
    if failcrate and failcrate==prev_fail:
        same+=1
        if same>=3:
            print(f"[attempt {attempt}] DETERMINISTIC fail on {failcrate} ({same}x) — bailing", flush=True); break
    else:
        same=0
    prev_fail=failcrate
    time.sleep(2)
print("LOOP DONE", flush=True)
