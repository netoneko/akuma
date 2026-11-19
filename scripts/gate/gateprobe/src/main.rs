//! gateprobe: what a Linux userspace needs from a kernel, one line per capability.
//!
//! Output (stdout, one per capability, in order): `GATE <name> PASS <detail>` or `GATE <name> FAIL <why>`,
//! then `GATE RESULT PASS` / `GATE RESULT FAIL n`. A capability that panics or hangs is reported as FAIL
//! (each runs on its own thread with a deadline); the process exits 0 only on RESULT PASS. Runs as pid 1
//! on the platform kernels and as an ordinary program on Linux (`gateprobe` with no arguments), which is
//! how the probe itself is checked. `gateprobe --child ...` is the re-exec target of the spawn checks.
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

type Check = fn() -> Result<String, String>;

fn threads() -> Result<String, String> {
    // clone + futex: N workers contend on a mutex and hand a token round via a condvar.
    let n = 4usize;
    let state = Arc::new((Mutex::new((0usize, 0u64)), Condvar::new()));
    let mut hs = Vec::new();
    for id in 0..n {
        let s = state.clone();
        hs.push(thread::spawn(move || {
            for _ in 0..200 {
                let (m, cv) = &*s;
                let mut g = m.lock().unwrap();
                while g.0 % n != id {
                    g = cv.wait(g).unwrap();
                }
                g.0 += 1;
                g.1 += id as u64;
                cv.notify_all();
            }
        }));
    }
    for h in hs {
        h.join().map_err(|_| "worker panicked".to_string())?;
    }
    let g = state.0.lock().unwrap();
    if g.0 != n * 200 {
        return Err(format!("token ring ended at {}", g.0));
    }
    Ok(format!("{n} workers, {} handoffs", g.0))
}

fn spawn_wait() -> Result<String, String> {
    // fork/clone + execve + pipe + wait4 + exit status, with stdout and stderr captured separately.
    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let out = Command::new(&me)
        .args(["--child", "echo"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("spawn: {e}"))?;
    if String::from_utf8_lossy(&out.stdout).trim() != "child-says-hi" {
        return Err(format!("stdout {:?}", String::from_utf8_lossy(&out.stdout)));
    }
    if String::from_utf8_lossy(&out.stderr).trim() != "child-err" {
        return Err(format!("stderr {:?}", String::from_utf8_lossy(&out.stderr)));
    }
    let st = Command::new(&me).args(["--child", "exit7"]).status().map_err(|e| format!("spawn: {e}"))?;
    if st.code() != Some(7) {
        return Err(format!("exit status {:?}", st.code()));
    }
    Ok("stdout, stderr, exit status 7".into())
}

fn spawn_many() -> Result<String, String> {
    // The fork/wait loop that once wedged SMP kernels: sixteen children, several alive at once.
    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut kids = Vec::new();
    for _ in 0..16 {
        kids.push(
            Command::new(&me)
                .args(["--child", "echo"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("spawn: {e}"))?,
        );
    }
    for mut k in kids {
        let mut s = String::new();
        k.stdout.take().unwrap().read_to_string(&mut s).map_err(|e| format!("read: {e}"))?;
        let st = k.wait().map_err(|e| format!("wait: {e}"))?;
        if !st.success() || s.trim() != "child-says-hi" {
            return Err(format!("child {st:?} {s:?}"));
        }
    }
    Ok("16 children".into())
}

fn files() -> Result<String, String> {
    // The root on a kernel image (pid 1 is root); the temp dir when run as an ordinary user on Linux.
    let mut dir = format!("/gate-{}", std::process::id());
    if fs::create_dir(&dir).is_err() {
        dir = format!("{}/gate-{}", std::env::temp_dir().display(), std::process::id());
        fs::create_dir(&dir).map_err(|e| format!("mkdir {dir}: {e}"))?;
    }
    let r = (|| -> Result<String, String> {
        let a = format!("{dir}/a");
        let b = format!("{dir}/b");
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&a, &payload).map_err(|e| format!("write: {e}"))?;
        fs::rename(&a, &b).map_err(|e| format!("rename: {e}"))?;
        if fs::metadata(&a).is_ok() {
            return Err("old name still exists after rename".into());
        }
        let back = fs::read(&b).map_err(|e| format!("read: {e}"))?;
        if back != payload {
            return Err("content differs after rename".into());
        }
        let mut f = fs::OpenOptions::new().append(true).open(&b).map_err(|e| format!("append: {e}"))?;
        f.write_all(b"tail").map_err(|e| format!("append write: {e}"))?;
        drop(f);
        if fs::metadata(&b).map_err(|e| format!("stat: {e}"))?.len() != 100_004 {
            return Err("size after append".into());
        }
        for i in 0..20 {
            fs::write(format!("{dir}/f{i}"), b"x").map_err(|e| format!("write f{i}: {e}"))?;
        }
        let names: BTreeSet<String> = fs::read_dir(&dir)
            .map_err(|e| format!("readdir: {e}"))?
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        if names.len() != 21 || !names.contains("b") || !names.contains("f19") {
            return Err(format!("readdir saw {} entries", names.len()));
        }
        for n in &names {
            fs::remove_file(format!("{dir}/{n}")).map_err(|e| format!("unlink {n}: {e}"))?;
        }
        Ok("write, rename, append, stat, readdir(21), unlink".into())
    })();
    let _ = fs::remove_dir(&dir);
    r
}

fn tcp_bind() -> Result<String, String> {
    // Just the listening half, so a hang in tcp_loopback can be placed: bind, getsockname, nonblocking accept.
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
    let addr = l.local_addr().map_err(|e| format!("getsockname: {e}"))?;
    l.set_nonblocking(true).map_err(|e| format!("nonblock: {e}"))?;
    match l.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(format!("listening on {addr}, accept would block")),
        other => Err(format!("accept on an idle listener returned {other:?}")),
    }
}

/// A scratch directory: the root on a kernel image (pid 1 is root), the temp dir as a user on Linux.
fn scratch() -> Result<String, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let mut dir = format!("/gate-{}-{n}", std::process::id());
    if fs::create_dir(&dir).is_err() {
        dir = format!("{}/gate-{}-{n}", std::env::temp_dir().display(), std::process::id());
        fs::create_dir(&dir).map_err(|e| format!("mkdir {dir}: {e}"))?;
    }
    Ok(dir)
}

fn files_meta() -> Result<String, String> {
    use std::os::unix::fs::{symlink, FileExt, PermissionsExt};
    let dir = scratch()?;
    let r = (|| -> Result<String, String> {
        let f = format!("{dir}/f");
        fs::write(&f, b"0123456789").map_err(|e| format!("write: {e}"))?;
        symlink("f", format!("{dir}/sym")).map_err(|e| format!("symlink: {e}"))?;
        if fs::read_link(format!("{dir}/sym")).map_err(|e| format!("readlink: {e}"))?.to_str() != Some("f") {
            return Err("readlink target".into());
        }
        if fs::read(format!("{dir}/sym")).map_err(|e| format!("read through symlink: {e}"))? != b"0123456789" {
            return Err("read through symlink differs".into());
        }
        if !fs::symlink_metadata(format!("{dir}/sym")).map_err(|e| format!("lstat: {e}"))?.file_type().is_symlink() {
            return Err("lstat of a symlink is not a symlink".into());
        }
        fs::hard_link(&f, format!("{dir}/hard")).map_err(|e| format!("link: {e}"))?;
        fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).map_err(|e| format!("chmod: {e}"))?;
        let mode = fs::metadata(format!("{dir}/hard")).map_err(|e| format!("stat: {e}"))?.permissions().mode() & 0o7777;
        if mode != 0o640 {
            return Err(format!("mode through a hard link is {mode:o}, want 640"));
        }
        let fh = fs::OpenOptions::new().read(true).write(true).open(&f).map_err(|e| format!("open: {e}"))?;
        fh.set_len(4).map_err(|e| format!("truncate: {e}"))?;
        let mut b = [0u8; 8];
        let n = fh.read_at(&mut b, 1).map_err(|e| format!("pread: {e}"))?;
        if &b[..n] != b"123" {
            return Err(format!("pread after truncate gave {:?}", &b[..n]));
        }
        fh.write_all_at(b"ab", 6).map_err(|e| format!("pwrite: {e}"))?;
        if fh.metadata().map_err(|e| format!("fstat: {e}"))?.len() != 8 {
            return Err("size after a write past the end".into());
        }
        let mut all = Vec::new();
        fs::File::open(&f).and_then(|mut x| x.read_to_end(&mut all)).map_err(|e| format!("reread: {e}"))?;
        if all != b"0123\0\0ab" {
            return Err(format!("hole did not read as zeros: {all:?}"));
        }
        fs::remove_file(format!("{dir}/hard")).map_err(|e| format!("unlink: {e}"))?;
        if fs::metadata(&f).is_err() {
            return Err("removing one hard link removed the file".into());
        }
        Ok("symlink, readlink, link, chmod, truncate, pread/pwrite, hole reads zero".into())
    })();
    let _ = fs::remove_dir_all(&dir);
    r
}

fn big_file() -> Result<String, String> {
    let dir = scratch()?;
    let r = (|| -> Result<String, String> {
        let f = format!("{dir}/big");
        let chunk: Vec<u8> = (0..65_536u32).map(|i| (i.wrapping_mul(7) % 251) as u8).collect();
        let mut fh = fs::File::create(&f).map_err(|e| format!("create: {e}"))?;
        for _ in 0..128 {
            fh.write_all(&chunk).map_err(|e| format!("write: {e}"))?; // 8 MiB
        }
        drop(fh);
        let mut fh = fs::File::open(&f).map_err(|e| format!("open: {e}"))?;
        let mut buf = vec![0u8; 65_536];
        let mut n = 0;
        loop {
            let k = fh.read(&mut buf).map_err(|e| format!("read: {e}"))?;
            if k == 0 {
                break;
            }
            if buf[..k] != chunk[..k] {
                return Err(format!("content differs at {}", n));
            }
            n += k;
        }
        if n != 8 << 20 {
            return Err(format!("read back {n} bytes"));
        }
        Ok("8 MiB written and read back".into())
    })();
    let _ = fs::remove_dir_all(&dir);
    r
}

fn pipe_throughput() -> Result<String, String> {
    // A child writes 4 MiB down a pipe (more than any pipe buffer), the parent sums it as it drains.
    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut c = Command::new(&me)
        .args(["--child", "flood"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    let mut out = c.stdout.take().unwrap();
    let mut buf = vec![0u8; 65_536];
    let (mut n, mut sum) = (0usize, 0u64);
    loop {
        let k = out.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if k == 0 {
            break;
        }
        n += k;
        sum += buf[..k].iter().map(|b| u64::from(*b)).sum::<u64>();
    }
    let st = c.wait().map_err(|e| format!("wait: {e}"))?;
    if !st.success() || n != 4 << 20 || sum != (4u64 << 20) * 3 {
        return Err(format!("{st:?}, {n} bytes, sum {sum}"));
    }
    Ok("4 MiB through a pipe".into())
}

fn signal_kill() -> Result<String, String> {
    use std::os::unix::process::ExitStatusExt;
    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut c = Command::new(&me).args(["--child", "sleep"]).stdin(Stdio::null()).spawn().map_err(|e| format!("spawn: {e}"))?;
    thread::sleep(Duration::from_millis(200));
    if c.try_wait().map_err(|e| format!("try_wait: {e}"))?.is_some() {
        return Err("the sleeping child exited by itself".into());
    }
    c.kill().map_err(|e| format!("kill: {e}"))?;
    let st = c.wait().map_err(|e| format!("wait: {e}"))?;
    if st.signal() != Some(9) {
        return Err(format!("status after SIGKILL: {st:?}"));
    }
    Ok("SIGKILL delivered, wait reports signal 9".into())
}

fn env_args_cwd() -> Result<String, String> {
    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = scratch()?;
    let r = (|| -> Result<String, String> {
        let out = Command::new(&me)
            .args(["--child", "env", "a b", "c\"d"])
            .env_clear()
            .env("GATE_X", "1=2")
            .current_dir(&dir)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("spawn: {e}"))?;
        let s = String::from_utf8_lossy(&out.stdout).into_owned();
        let want = format!("args=a b|c\"d env=1=2 cwd={dir}\n");
        if s != want {
            return Err(format!("child saw {s:?}, want {want:?}"));
        }
        Ok("argv, env and cwd reach the child intact".into())
    })();
    let _ = fs::remove_dir_all(&dir);
    r
}

fn stack_growth() -> Result<String, String> {
    // Recursion deep enough to grow the main stack by ~4 MiB, and a thread with a 1 MiB requested stack.
    fn depth(n: u32) -> u64 {
        let pad = [n as u8; 256];
        if n == 0 { u64::from(pad[0]) } else { depth(n - 1) + u64::from(pad[(n % 256) as usize]) % 2 + 1 }
    }
    let t = thread::Builder::new().stack_size(8 << 20).spawn(|| depth(12_000)).map_err(|e| format!("spawn: {e}"))?;
    let v = t.join().map_err(|_| "deep recursion crashed".to_string())?;
    Ok(format!("recursed 12000 frames on an 8 MiB thread stack ({v})"))
}

fn spawn_churn() -> Result<String, String> {
    // Far more sequential children than any table has slots: each is waited for, so every slot must come back.
    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    for i in 0..120 {
        let st = Command::new(&me).args(["--child", "exit7"]).status().map_err(|e| format!("spawn #{i}: {e}"))?;
        if st.code() != Some(7) {
            return Err(format!("child #{i} status {:?}", st.code()));
        }
    }
    Ok("120 sequential children, each waited for".into())
}

fn thread_churn() -> Result<String, String> {
    // Far more sequential threads than any table has slots: each is joined, so every slot must come back.
    for i in 0..200u32 {
        let v = thread::Builder::new().spawn(move || i * 2).map_err(|e| format!("spawn #{i}: {e}"))?
            .join().map_err(|_| format!("thread #{i} panicked"))?;
        if v != i * 2 {
            return Err(format!("thread #{i} returned {v}"));
        }
    }
    Ok("200 sequential threads, each joined".into())
}

fn alive_at_once(n: usize) -> Result<String, String> {
    // `n` threads that are all alive together (a barrier holds them until the last one has started).
    let barrier = Arc::new(std::sync::Barrier::new(n + 1));
    let mut hs = Vec::new();
    for i in 0..n {
        let b = barrier.clone();
        hs.push(thread::Builder::new().spawn(move || { b.wait(); i }).map_err(|e| format!("spawn #{i} of {n}: {e}"))?);
    }
    barrier.wait();
    let mut sum = 0;
    for h in hs {
        sum += h.join().map_err(|_| "a thread panicked".to_string())?;
    }
    if sum != n * (n - 1) / 2 {
        return Err(format!("sum {sum}"));
    }
    Ok(format!("{n} threads alive at once"))
}

/// `getcpu(2)` through the raw syscall (musl's `sched_getcpu` and HotSpot ask for it at startup; on ENOSYS
/// HotSpot falls back to the legacy vsyscall page and the JVM dies with SIGSEGV before `main`).
/// The main amd64 kernel answered ENOSYS until stage 1 of `docs/archive/AKUMA_ECOSYSTEM_DEMO.md`.
fn getcpu() -> Result<String, String> {
    let mut cpu: u32 = u32::MAX;
    let mut node: u32 = u32::MAX;
    let ret: i64;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!("syscall", inlateout("rax") 309i64 => ret,
            in("rdi") &mut cpu as *mut u32, in("rsi") &mut node as *mut u32, in("rdx") 0usize,
            lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("svc 0", in("x8") 168i64, inlateout("x0") &mut cpu as *mut u32 as i64 => ret,
            in("x1") &mut node as *mut u32, in("x2") 0usize, options(nostack));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        ret = 0;
    }
    if ret != 0 {
        return Err(format!("getcpu returned {ret}"));
    }
    if cpu >= 4096 || node >= 4096 {
        return Err(format!("implausible cpu={cpu} node={node}"));
    }
    Ok(format!("cpu={cpu} node={node}"))
}

fn many_threads() -> Result<String, String> {
    alive_at_once(32)
}

/// Past the tokio kernels' 48-slot spawn ring (`NSLOTS`): a capacity limit, not a leak (`spawn_churn` and
/// `thread_churn` show every slot comes back). tokio's blocking pool may ask for 512 threads.
fn threads_128() -> Result<String, String> {
    alive_at_once(128)
}

fn udp_loopback() -> Result<String, String> {
    let a = std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("bind a: {e}"))?;
    let b = std::net::UdpSocket::bind("127.0.0.1:0").map_err(|e| format!("bind b: {e}"))?;
    a.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| format!("timeout: {e}"))?;
    b.send_to(b"ping-1234", a.local_addr().map_err(|e| format!("addr: {e}"))?).map_err(|e| format!("send_to: {e}"))?;
    let mut buf = [0u8; 64];
    let (n, from) = a.recv_from(&mut buf).map_err(|e| format!("recv_from: {e}"))?;
    if &buf[..n] != b"ping-1234" || from != b.local_addr().map_err(|e| format!("addr: {e}"))? {
        return Err(format!("got {:?} from {from}", &buf[..n]));
    }
    Ok("datagram delivered with the right source".into())
}

fn tcp_loopback() -> Result<String, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
    let addr = l.local_addr().map_err(|e| format!("addr: {e}"))?;
    let srv = thread::spawn(move || -> std::io::Result<usize> {
        let (mut s, _) = l.accept()?;
        let mut total = 0;
        let mut buf = [0u8; 4096];
        loop {
            let n = s.read(&mut buf)?;
            if n == 0 {
                return Ok(total);
            }
            total += n;
            s.write_all(&buf[..n])?;
        }
    });
    let mut c = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
    let data: Vec<u8> = (0..65_536u32).map(|i| (i % 253) as u8).collect();
    let mut echoed = Vec::new();
    let mut rd = c.try_clone().map_err(|e| format!("clone: {e}"))?;
    let reader = thread::spawn(move || {
        let mut v = Vec::new();
        let _ = rd.read_to_end(&mut v);
        v
    });
    c.write_all(&data).map_err(|e| format!("send: {e}"))?;
    c.shutdown(std::net::Shutdown::Write).map_err(|e| format!("shutdown: {e}"))?;
    echoed.extend(reader.join().map_err(|_| "reader panicked".to_string())?);
    let served = srv.join().map_err(|_| "server panicked".to_string())?.map_err(|e| format!("server: {e}"))?;
    if echoed != data || served != data.len() {
        return Err(format!("echoed {} of {} (server saw {served})", echoed.len(), data.len()));
    }
    Ok("accept/connect, 64 KiB echoed, half-close".into())
}

fn time() -> Result<String, String> {
    let t0 = Instant::now();
    thread::sleep(Duration::from_millis(100));
    let dt = t0.elapsed();
    if dt < Duration::from_millis(95) || dt > Duration::from_millis(2000) {
        return Err(format!("sleep(100ms) took {dt:?}"));
    }
    let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| format!("clock: {e}"))?;
    if wall.as_secs() < 1_600_000_000 {
        return Err(format!("wall clock {}s is before 2020 (no epoch= on the command line?)", wall.as_secs()));
    }
    Ok(format!("sleep(100ms)={dt:?}, wall clock sane"))
}

fn memory() -> Result<String, String> {
    // 64 MiB touched page by page, then freed and re-grown: anonymous mmap, demand faults, munmap, brk/mremap.
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(64 << 20).map_err(|e| format!("reserve: {e}"))?;
    v.resize(64 << 20, 0);
    for i in (0..v.len()).step_by(4096) {
        v[i] = (i >> 12) as u8;
    }
    let sum: u64 = v.iter().step_by(4096).map(|b| u64::from(*b)).sum();
    drop(v);
    let mut w: Vec<u64> = Vec::new();
    for i in 0..1_000_000u64 {
        w.push(i); // realloc/mremap growth
    }
    if w[999_999] != 999_999 {
        return Err("regrown vec corrupted".into());
    }
    Ok(format!("64 MiB touched (checksum {sum}), 8 MB regrown"))
}

fn threads_and_children() -> Result<String, String> {
    // A multithreaded process forking: children spawned from several threads at once.
    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let (tx, rx) = mpsc::channel();
    for t in 0..4 {
        let tx = tx.clone();
        let me = me.clone();
        thread::spawn(move || {
            let r = Command::new(&me).args(["--child", "echo"]).stdin(Stdio::null()).output();
            let _ = tx.send((t, r.map(|o| o.status.success()).unwrap_or(false)));
        });
    }
    drop(tx);
    let ok = rx.iter().filter(|(_, ok)| *ok).count();
    if ok != 4 {
        return Err(format!("{ok}/4 children from threads succeeded"));
    }
    Ok("4 threads each spawned a child".into())
}

const CHECKS: &[(&str, Check)] = &[
    ("threads", threads),
    ("time", time),
    ("getcpu", getcpu),
    ("memory", memory),
    ("files", files),
    ("tcp_bind", tcp_bind),
    ("spawn_wait", spawn_wait),
    ("spawn_many", spawn_many),
    ("threads_and_children", threads_and_children),
    ("files_meta", files_meta),
    ("big_file", big_file),
    ("pipe_throughput", pipe_throughput),
    ("signal_kill", signal_kill),
    ("env_args_cwd", env_args_cwd),
    ("stack_growth", stack_growth),
    ("udp_loopback", udp_loopback),
    ("spawn_churn", spawn_churn),
    ("thread_churn", thread_churn),
    ("many_threads", many_threads),
    // Last: on the tokio kernels the blocked connect wedges the guest, and must not hide the checks before it.
    ("tcp_loopback", tcp_loopback),
    // After everything: past the capacity it leaves its blocked threads behind and nothing else can spawn.
    ("threads_128", threads_128),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--child") {
        match args.get(2).map(String::as_str) {
            Some("echo") => {
                println!("child-says-hi");
                eprintln!("child-err");
            }
            Some("exit7") => std::process::exit(7),
            Some("flood") => {
                let chunk = [3u8; 65_536];
                let mut o = std::io::stdout().lock();
                for _ in 0..64 {
                    if o.write_all(&chunk).is_err() {
                        std::process::exit(1);
                    }
                }
            }
            Some("sleep") => thread::sleep(Duration::from_secs(60)),
            Some("env") => {
                let rest: Vec<String> = args.iter().skip(3).cloned().collect();
                println!(
                    "args={} env={} cwd={}",
                    rest.join("|"),
                    std::env::var("GATE_X").unwrap_or_default(),
                    std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default()
                );
            }
            _ => std::process::exit(2),
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--list") {
        for (name, _) in CHECKS {
            println!("{name}");
        }
        return;
    }
    let only: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let mut failed = 0;
    for (name, f) in CHECKS {
        if !only.is_empty() && !only.contains(name) {
            continue;
        }
        // Each check on its own thread with a deadline: a hang is a FAIL line, not a wedged probe.
        let (tx, rx) = mpsc::channel();
        let t0 = Instant::now();
        thread::spawn(move || {
            let _ = tx.send(f());
        });
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(d)) => println!("GATE {name} PASS {d} ({:.2}s)", t0.elapsed().as_secs_f64()),
            Ok(Err(e)) => {
                failed += 1;
                println!("GATE {name} FAIL {e}");
            }
            Err(_) => {
                failed += 1;
                println!("GATE {name} FAIL timed out after 20s");
            }
        }
        let _ = std::io::stdout().flush();
    }
    if failed == 0 {
        println!("GATE RESULT PASS");
    } else {
        println!("GATE RESULT FAIL {failed}");
    }
    let _ = std::io::stdout().flush();
    // As pid 1 there is nothing to return to; the harness reads the console. On Linux the status is the verdict.
    std::process::exit(i32::from(failed != 0));
}
