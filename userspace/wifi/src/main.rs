//! `wifi` — Akuma's wifi manager (`proposals/AKUMA_WIFI_CONTROL.md`).
//!
//! ```text
//! wifi status                     the link, from /dev/wifi0
//! wifi scan                       networks in range (* = known)
//! wifi list                       known networks in /etc/wifi (never the key)
//! wifi add <name> [ssid]          passphrase on stdin -> /etc/wifi/<name> (psk, 0600)
//! wifi add -i                     asks for the network name and password
//! wifi add-open <name> [ssid]     an open network
//! wifi connect [name]             join <name>, or the best known network in range
//! wifi disconnect
//! wifi forget <name>
//! wifi auto                       the herd service: keep the best known network joined
//! ```
//!
//! No supplicant: the kernel does WPA2. This program picks a network and hands
//! the driver one line — `connect wlan0 <ssid-hex> <psk-hex>` — then watches the
//! state the device reports. The pure parts are `lib.rs`, host-tested.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;

use akuma_wifi::cmd::{self, Command};
use akuma_wifi::status::{Link, Status};
use akuma_wifi::{IfName, Ssid};
use libakuma::{arg, argc, chmod, close, exit, mkdir_p, open, open_flags, print, read, read_dir, rename, sleep_ms, syscall, unlink, write};
use wifi::{
    AutoStep, CONFIG_DIR, Network, auto_backoff, auto_step, choose, derive_psk, name_for_ssid, parse_config, printable_ssid, replacing,
    scan_mark, valid_name, valid_passphrase,
};

const DEVICE: &str = "/dev/wifi0";
const IFACE: &[u8] = b"wlan0";

fn out(s: &str) {
    print(s);
}

fn fail(s: &str) -> ! {
    out("wifi: ");
    out(s);
    out("\n");
    exit(1)
}

fn usage() -> ! {
    out("usage: wifi status | scan | list | add [-i | <name> [ssid]] | add-open <name> [ssid] |\n");
    out("            connect [name] | disconnect | forget <name> | auto\n");
    exit(2)
}

fn iface() -> IfName {
    IfName::new(IFACE).unwrap_or_else(|| fail("bad interface name"))
}

// ---------------------------------------------------------------- device

fn read_status() -> Result<Status, String> {
    let fd = open(DEVICE, open_flags::O_RDONLY);
    if fd < 0 {
        return Err(format!("{DEVICE}: open failed ({fd}) — no wifi backend in this kernel?"));
    }
    let mut text = Vec::with_capacity(4096);
    let mut buf = [0u8; 1024];
    loop {
        let n = read(fd, &mut buf);
        if n < 0 {
            close(fd);
            return Err(format!("{DEVICE}: read failed ({n})"));
        }
        if n == 0 {
            break;
        }
        text.extend_from_slice(&buf[..n as usize]);
    }
    close(fd);
    Status::parse(&text).ok_or_else(|| String::from("unreadable status from the device"))
}

fn send(c: &Command) -> Result<(), String> {
    let mut line = [0u8; 256];
    let n = cmd::write(c, &mut line).ok_or_else(|| String::from("command too long"))?;
    let fd = open(DEVICE, open_flags::O_WRONLY);
    if fd < 0 {
        return Err(format!("{DEVICE}: open failed ({fd})"));
    }
    let r = write(fd, &line[..n]);
    close(fd);
    if r < 0 { Err(format!("{DEVICE}: command refused ({r})")) } else { Ok(()) }
}

/// Scan and wait (up to ~10 s) for the result.
fn scan() -> Result<Status, String> {
    let before = read_status()?.scans;
    send(&Command::Scan { iface: iface() })?;
    for _ in 0..100 {
        let s = read_status()?;
        if s.scans != before {
            return Ok(s);
        }
        sleep_ms(100);
    }
    Err(String::from("scan did not complete within 10 s"))
}

/// Join and wait (up to ~90 s) until connected or failed. A real radio retries
/// a join that times out (the rtw89 station: three whole attempts, each up to
/// ~25 s on a bad channel), so the wait covers all of them.
fn join(n: &Network) -> Result<Status, String> {
    send(&Command::Connect { iface: iface(), ssid: n.ssid, psk: n.psk, bssid: n.bssid })?;
    for _ in 0..900 {
        let s = read_status()?;
        if s.ssid == n.ssid && matches!(s.link, Link::Connected | Link::Failed) {
            return Ok(s);
        }
        sleep_ms(100);
    }
    Err(String::from("no answer from the driver within 90 s"))
}

// ---------------------------------------------------------------- /etc/wifi

fn read_file(path: &str) -> Option<String> {
    let fd = open(path, open_flags::O_RDONLY);
    if fd < 0 {
        return None;
    }
    let mut bytes = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let n = read(fd, &mut buf);
        if n <= 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n as usize]);
    }
    close(fd);
    String::from_utf8(bytes).ok()
}

/// Every parseable known network; a broken file is reported and skipped.
fn known() -> Vec<Network> {
    let mut v = Vec::new();
    let Some(dir) = read_dir(CONFIG_DIR) else { return v };
    for e in dir {
        if e.is_dir || !valid_name(&e.name) {
            continue;
        }
        let path = format!("{CONFIG_DIR}/{}", e.name);
        match read_file(&path).map(|t| parse_config(&e.name, &t)) {
            Some(Ok(n)) => v.push(n),
            Some(Err(err)) => out(&format!("wifi: {path}: {err:?} — skipped\n")),
            None => out(&format!("wifi: {path}: unreadable — skipped\n")),
        }
    }
    v.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.name.cmp(&b.name)));
    v
}

fn load(name: &str) -> Network {
    if !valid_name(name) {
        fail("network names are 1-64 of [A-Za-z0-9._-], not starting with a dot");
    }
    let path = format!("{CONFIG_DIR}/{name}");
    let text = read_file(&path).unwrap_or_else(|| fail(&format!("no such network: {path}")));
    parse_config(name, &text).unwrap_or_else(|e| fail(&format!("{path}: {e:?}")))
}

/// Write `/etc/wifi/<name>`, root-only before any byte of the key is in it.
///
/// Written to `.<name>.tmp` (a dot-name, which [`known`] never reads) and
/// renamed over the real file, so a crash or a full disk mid-write leaves the
/// old file intact rather than a truncated one that `wifi auto` then skips as
/// unparseable - which on a box reached over wifi is the box gone.
fn save(n: &Network) {
    if !mkdir_p(CONFIG_DIR) {
        fail(&format!("cannot create {CONFIG_DIR}"));
    }
    if chmod(CONFIG_DIR, 0o700) < 0 {
        fail(&format!("{CONFIG_DIR}: cannot make it root-only; not writing a key into it"));
    }
    let path = format!("{CONFIG_DIR}/{}", n.name);
    let tmp = format!("{CONFIG_DIR}/.{}.tmp", n.name);
    unlink(&tmp);
    let fd = open(&tmp, open_flags::O_WRONLY | open_flags::O_CREAT | open_flags::O_TRUNC);
    if fd < 0 {
        fail(&format!("{tmp}: cannot create ({fd})"));
    }
    if chmod(&tmp, 0o600) < 0 {
        close(fd);
        unlink(&tmp);
        fail(&format!("{tmp}: cannot make it root-only; not writing the key"));
    }
    let text = n.to_config();
    let w = write(fd, text.as_bytes());
    close(fd);
    if w < 0 || w as usize != text.len() {
        unlink(&tmp);
        fail(&format!("{tmp}: write failed ({w})"));
    }
    let r = rename(&tmp, &path);
    if r < 0 {
        unlink(&tmp);
        fail(&format!("{path}: cannot replace it ({r})"));
    }
}

/// Linux `TCGETS`/`TCSETS` (the same numbers on x86_64 and aarch64) and the
/// 36-byte kernel `struct termios` they move: four `u32` flag words, then
/// `c_line` and `c_cc`. `c_lflag` is the fourth word.
const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const LFLAG_OFFSET: usize = 12;
const ECHO: u32 = 0o10;

/// Turn terminal echo off on stdin, returning the settings to put back, or
/// `None` when stdin is not a terminal (a redirected file needs nothing).
fn echo_off() -> Option<[u8; 36]> {
    let mut saved = [0u8; 36];
    if (syscall(syscall::IOCTL, 0, TCGETS, saved.as_mut_ptr() as u64, 0, 0, 0) as i64) < 0 {
        return None;
    }
    let mut quiet = saved;
    let mut lflag = [0u8; 4];
    lflag.copy_from_slice(&quiet[LFLAG_OFFSET..LFLAG_OFFSET + 4]);
    let lflag = u32::from_ne_bytes(lflag) & !ECHO;
    quiet[LFLAG_OFFSET..LFLAG_OFFSET + 4].copy_from_slice(&lflag.to_ne_bytes());
    if (syscall(syscall::IOCTL, 0, TCSETS, quiet.as_ptr() as u64, 0, 0, 0) as i64) < 0 {
        return None;
    }
    Some(saved)
}

fn echo_restore(saved: Option<[u8; 36]>) {
    if let Some(t) = saved {
        syscall(syscall::IOCTL, 0, TCSETS, t.as_ptr() as u64, 0, 0, 0);
    }
}

// ---------------------------------------------------------------- commands

fn print_status(s: &Status) {
    let name = printable_ssid(&s.ssid);
    let line = match s.link {
        Link::NoRadio => format!("{}: no radio\n", s.iface.as_str()),
        Link::Down => format!("{}: down (radio {})\n", s.iface.as_str(), s.radio),
        Link::Scanning => format!("{}: scanning\n", s.iface.as_str()),
        Link::Associating => format!("{}: joining {name}\n", s.iface.as_str()),
        Link::Connected => format!(
            "{}: connected to {name} ({}), chan {}, {} dBm, {} (radio {})\n",
            s.iface.as_str(),
            mac(&s.bssid),
            s.chan,
            s.signal,
            s.security.name(),
            s.radio
        ),
        Link::Failed => format!("{}: joining {name} failed: {}\n", s.iface.as_str(), s.error.name()),
    };
    out(&line);
}

fn mac(m: &[u8; 6]) -> String {
    let mut b = [0u8; 17];
    akuma_wifi::hex::encode_mac(m, &mut b);
    String::from(core::str::from_utf8(&b).unwrap_or("?"))
}

fn cmd_scan() {
    let s = scan().unwrap_or_else(|e| fail(&e));
    let known = known();
    out("   signal chan security  bssid              ssid\n");
    let mut rows: Vec<_> = s.results().to_vec();
    rows.sort_by_key(|b| core::cmp::Reverse(b.signal));
    for b in rows {
        let mark = scan_mark(&known, &b);
        out(&format!(" {mark} {:>4}  {:>4} {:<9} {} {}\n", b.signal, b.chan, b.security.name(), mac(&b.bssid), printable_ssid(&b.ssid)));
    }
}

fn cmd_list() {
    let known = known();
    if known.is_empty() {
        out(&format!("no known networks ({CONFIG_DIR} is empty)\n"));
        return;
    }
    out("name                 priority auto security ssid\n");
    for n in &known {
        out(&format!(
            "{:<20} {:>8} {:<4} {:<8} {}\n",
            n.name,
            n.priority,
            if n.autoconnect { "yes" } else { "no" },
            if n.psk.is_some() { "psk" } else { "open" },
            printable_ssid(&n.ssid)
        ));
    }
}

/// One line from stdin into `buf`, without the `\n` (or `\r\n`). Stops at EOF
/// or a full buffer. The returned slice borrows `buf`, which the caller wipes.
fn read_line(buf: &mut [u8]) -> &[u8] {
    let mut len = 0;
    while len < buf.len() {
        let n = read(0, &mut buf[len..]);
        if n <= 0 {
            break;
        }
        len += n as usize;
        if buf[..len].contains(&b'\n') {
            break;
        }
    }
    let line = buf[..len].split(|&c| c == b'\n').next().unwrap_or(&[]);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Read a passphrase line. On a terminal, echo is off while it is typed, so the
/// passphrase is not left on the screen (or in a console log); `prompt` is
/// shown only then.
fn read_secret(buf: &mut [u8], prompt: &str) -> usize {
    let saved = echo_off();
    if saved.is_some() {
        out(prompt);
    }
    let len = read_line(buf).len();
    echo_restore(saved);
    if saved.is_some() {
        out("\n");
    }
    len
}

/// Write `/etc/wifi/<name>` (keeping what a re-add must keep) and say so.
fn store(name: &str, ssid: Ssid, psk: Option<[u8; 32]>) {
    // Re-adding a network keeps what was set on it (see `wifi::replacing`).
    let path = format!("{CONFIG_DIR}/{name}");
    let old = read_file(&path).and_then(|t| parse_config(name, &t).ok());
    let verb = if old.is_some() { "updated" } else { "added" };
    let kind = if psk.is_some() { "psk" } else { "open" };
    let fresh = Network { name: String::from(name), ssid, psk, priority: 0, autoconnect: true, bssid: None };
    save(&replacing(fresh, old.as_ref()));
    out(&format!("{verb} {name} ({path}, {kind})\n"));
}

fn cmd_add(open_net: bool) {
    let name = arg(2).unwrap_or_else(|| usage());
    if !valid_name(name) {
        fail("network names are 1-64 of [A-Za-z0-9._-], not starting with a dot");
    }
    let ssid_text = arg(3).unwrap_or(name);
    let ssid = Ssid::new(ssid_text.as_bytes()).filter(|s| !s.is_empty()).unwrap_or_else(|| fail("an SSID is 1-32 bytes"));
    let psk = if open_net {
        None
    } else {
        // One line on stdin (see `read_secret`).
        let mut buf = [0u8; 128];
        let len = read_secret(&mut buf, "passphrase: ");
        let line = &buf[..len];
        if !valid_passphrase(line) {
            buf.fill(0);
            fail("a WPA2 passphrase is 8-63 printable ASCII characters (one line on stdin)");
        }
        let k = derive_psk(line, ssid.as_bytes());
        buf.fill(0);
        Some(k)
    };
    store(name, ssid, psk);
}

/// `wifi add -i`: ask for the network name and the password, then write the
/// same file `wifi add` / `wifi add-open` would. The name is the SSID — any
/// bytes, UTF-8 included — and the file is named from it by `name_for_ssid`
/// (the SSID itself when that is a safe file name, else a slug plus a hash), and
/// the name is printed so `wifi connect <name>` / `wifi forget <name>` can be
/// typed. An empty password means an open network.
fn cmd_add_interactive() {
    out("network name (SSID): ");
    let mut nbuf = [0u8; 128];
    let ssid_bytes = read_line(&mut nbuf);
    let ssid = Ssid::new(ssid_bytes).filter(|s| !s.is_empty()).unwrap_or_else(|| fail("an SSID is 1-32 bytes"));
    let name = name_for_ssid(ssid.as_bytes());

    let mut buf = [0u8; 128];
    let len = read_secret(&mut buf, "password (empty for an open network): ");
    let psk = if len == 0 {
        None
    } else if valid_passphrase(&buf[..len]) {
        Some(derive_psk(&buf[..len], ssid.as_bytes()))
    } else {
        buf.fill(0);
        fail("a WPA2 passphrase is 8-63 printable ASCII characters");
    };
    buf.fill(0);
    store(&name, ssid, psk);
}

fn cmd_connect() {
    let n = match arg(2) {
        Some(name) => load(name),
        None => {
            let s = scan().unwrap_or_else(|e| fail(&e));
            let known = known();
            let Some((n, _)) = choose(&known, s.results()) else { fail("no known network in range") };
            n.clone()
        }
    };
    out(&format!("joining {} ({})\n", n.name, printable_ssid(&n.ssid)));
    let s = join(&n).unwrap_or_else(|e| fail(&e));
    print_status(&s);
    if s.link != Link::Connected {
        exit(1);
    }
}

fn cmd_forget() {
    let name = arg(2).unwrap_or_else(|| usage());
    if !valid_name(name) {
        fail("network names are 1-64 of [A-Za-z0-9._-], not starting with a dot");
    }
    let path = format!("{CONFIG_DIR}/{name}");
    // A file that no longer parses must still be removable: `forget` used to
    // `load` it first and stop at the parse error, so the only way to drop a
    // broken file was `rm`. Disconnect only when it parses and is the link.
    let text = read_file(&path).unwrap_or_else(|| fail(&format!("no such network: {path}")));
    if let Ok(n) = parse_config(name, &text)
        && let Ok(s) = read_status()
        && s.link == Link::Connected
        && s.ssid == n.ssid
    {
        let _ = send(&Command::Disconnect { iface: iface() });
    }
    if unlink(&format!("{CONFIG_DIR}/{name}")) < 0 {
        fail(&format!("cannot remove {CONFIG_DIR}/{name}"));
    }
    out(&format!("forgot {name}\n"));
}

/// The herd service: join the best known network in range and keep it joined.
/// Polls every 5 s; after a failed join, waits longer (up to a minute). The
/// policy is `wifi::auto_step` (host-tested against the lossy simulated
/// radio); this is the loop around it.
fn cmd_auto() -> ! {
    out("wifi: auto — keeping the best known network joined\n");
    let mut backoff_s = 5u64;
    loop {
        let joined = match read_status() {
            Err(e) => {
                out(&format!("wifi: {e}\n"));
                false
            }
            Ok(s) => match auto_step(s.link) {
                AutoStep::Keep => true,
                AutoStep::NoRadio => {
                    out("wifi: no radio\n");
                    false
                }
                // The kernel is joining (or rejoining a dropped link) on its
                // own; a scan or connect now would only restart it.
                AutoStep::Wait => true,
                AutoStep::Join => match scan() {
                    Err(e) => {
                        out(&format!("wifi: {e}\n"));
                        false
                    }
                    Ok(s) => {
                        let known = known();
                        match choose(&known, s.results()) {
                            None => {
                                out("wifi: no known network in range\n");
                                false
                            }
                            Some((n, _)) => {
                                out(&format!("wifi: joining {} ({})\n", n.name, printable_ssid(&n.ssid)));
                                match join(n) {
                                    Ok(st) => {
                                        print_status(&st);
                                        st.link == Link::Connected
                                    }
                                    Err(e) => {
                                        out(&format!("wifi: {e}\n"));
                                        false
                                    }
                                }
                            }
                        }
                    }
                },
            },
        };
        backoff_s = auto_backoff(backoff_s, joined);
        sleep_ms(backoff_s * 1000);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() {
    if argc() < 2 {
        usage();
    }
    match arg(1).unwrap_or("") {
        "status" => print_status(&read_status().unwrap_or_else(|e| fail(&e))),
        "scan" => cmd_scan(),
        "list" => cmd_list(),
        "add" if arg(2) == Some("-i") => cmd_add_interactive(),
        "add" => cmd_add(false),
        "add-open" => cmd_add(true),
        "connect" => cmd_connect(),
        "disconnect" => {
            send(&Command::Disconnect { iface: iface() }).unwrap_or_else(|e| fail(&e));
            out("disconnected\n");
        }
        "forget" => cmd_forget(),
        "auto" => cmd_auto(),
        _ => usage(),
    }
    exit(0)
}
