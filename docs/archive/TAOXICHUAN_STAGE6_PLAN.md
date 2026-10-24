# Taoxichuan stage 6 — `apk add curl` in the guest (PLAN, written 2026-10-24)

Branch `taoxichuan-s4` @ `29d4b562`. Planning only — no kernel code in this commit. Numbers marked
**(guess)** are estimates; everything else is read off the tree today.

**Stage 5 check (one line):** nothing left to verify — the stage-4 image already boots `init`
kernel-spawned from `vm_cmdline` (`init=/bin/sshd`), with no herd/container machinery in the guest
path; stage 4's gate is the stage-5 constraint satisfied.

## 1. The syscall gap, measured not guessed

Diff of the measured apk set (`docs/archive/APK_MISSING_SYSCALLS.md`, aarch64 numbers mapped to
x86_64) against the arms `taoxichuan/src/main.rs` dispatches today (it handles: read, write, writev,
open/openat, close, stat, lstat, getdents64, fcntl, ioctl→ENOTTY, brk, mmap, munmap+mprotect,
nanosleep×2, clock_gettime, gettimeofday, getrandom, exit×2, socket/bind/listen/accept/connect/
sendto/recvfrom/shutdown, uid/gid/pid family, rt_sigaction/procmask, set_tid_address, arch_prctl,
plus private SPAWN/WAITPID/CLOSE_CHILD_STDIN). **Missing set, ordered by when apk first needs it:**

| Phase | Missing arm (x86_64 nr) | Note |
|---|---|---|
| A: apk startup | `fstat` (5), `newfstatat` (262), `lseek` (8), `uname` (63), `fstatfs` (138)/`statfs` (137), `faccessat` (269), `readlinkat` (267) | nr 5 already seen live on sshd. fstatfs must return `0xEF53` + sane counts — aarch64 measured that ENOSYS here made apk treat later ops as forbidden. taoxichuan has legacy `stat`(4)/`lstat`(6) but musl uses 262 everywhere. Symlink *read* infra exists (`/bin/sh` resolves); only the arm is missing. |
| B: index fetch | `pselect6` (270) or `ppoll` (271), `getsockopt` (55, SO_ERROR), `getsockname` (51), `setsockopt` (54, stub ok), `mremap` (25), `madvise` (28, stub ok), `readv` (19), maybe `sendmsg`/`recvmsg` (46/47) | aarch64 measured ~80 mremaps during APKINDEX processing. musl's resolver may use sendto/recvfrom (present) — add 46/47 only if the console names them. `connect` is present; blocking-with-timeout behavior must match pselect6 semantics. |
| C: unpack + commit | `mkdirat` (258), `unlinkat` (263), `renameat` (264), `symlinkat` (266), `fchmod` (91)/`fchmodat` (268, stub ok), `fchownat` (260, stub ok), `umask` (95, stub ok), `getcwd` (79), `dup` (32), `flock` (73, stub ok), `linkat` (265, low) | renameat needs dirfd-relative resolution (AT_FDCWD at minimum; aarch64 measured apk opens the cache dir and passes its fd). |
| D: scripts (contingency, §5) | `clone` (56), `execve` (59), `wait4` (61), `pipe2` (293), `dup2`/`dup3` (33/292), `chdir` (80), `fchdir` (81) | only if the curl dep tree actually runs a script — likely it does not (§5). |
| E: run `/usr/bin/curl` | `set_robust_list` (273), `prlimit64` (302), `sigaltstack` (131), `futex` (202), mmap `MAP_FIXED` | dynamic-loader stubs; ENOSYS tolerable for some (sshd already tolerates 258). |

≈32 thin arms for the core (A–C), +7 contingency, +4–5 stubs. All go in **both** dispatch sites in
`taoxichuan/src/main.rs` (~line 976 parent, ~1442 child path) or a shared match. QoL, do first:
print a name (reverse-map nr→string) instead of the bare number in the `unhandled nr` arm — each
boot then reads as a checklist.

**nr 4415 identified (was open in stage 4):** the private range is `AKUMA_PRIVATE_BASE = 0x1000`
(`crates/akuma-ring3-amd64/src/lib.rs:857`); `0x1000 + 319 = UPTIME`
(`crates/akuma-syscalls-linux/src/nr.rs:119`). libakuma's sshd/sh asks `uptime()` and taoxichuan
doesn't serve it, hence the repeating `[sc] unhandled nr 4415`. Method that finds it in one look:
diff the private constants (301 SPAWN ✓, 303 WAITPID ✓, 315 SPAWN_EXT ✗, 319 UPTIME ✗, 326
CLOSE_CHILD_STDIN ✓) against taoxichuan's private arms. Fix is one arm returning a boot-relative
counter (or accept it; it is harmless). `SPAWN_EXT` (4411) may surface the same way.

## 2. Filesystem needs

`akuma-ext2` already has the write internals (`write_inode`, `write_range`, `write_dir_range`,
`remove_dir_entry`, `create_symlink_internal`, `write_superblock`/`write_bgd` — written for the
full kernel). What's missing is the **wiring in taoxichuan**: its open/stat path today only calls
`read_dir`/`resolve_inode`; the create/write/rename/unlink/symlink `Filesystem` trait methods must
be routed into the new C-phase arms. `fsync` is not in the measured set (apk tolerates ENOSYS —
verify, don't assume). No tmpfs needed.

**The image.** Today `/boot/akuma-dev-root-ssh.img` is a 16 MiB ext2 *copy* of
`/boot/akuma-serve-root.img` (never written; stage-4 rule). For stage 6, grow to **64 MiB (guess)**
— curl's dep tree (musl, ca-certificates-bundle, libcurl, brotli-libs, nghttp2-libs, zlib,
openssl/libsssl3, curl) is roughly 8–12 MiB installed on top of an already-mostly-full 16 MiB.
Settle the exact number in phase 0 on the host by summing `size` in APKINDEX before building.
Contents added: `/bin/apk` (apk-tools static, **x86_64**), `/etc/apk/{arch=x86_64, repositories,
world(seed, empty)}`, `/etc/apk/keys/*.rsa.pub` (Alpine keys), `/lib/apk/db/{installed,triggers}`
(empty), `/etc/resolv.conf` (`nameserver 10.0.2.2`). `/etc/ssl/certs/ca-certificates.crt` **only** if
HTTPS is ever tried (§3: the gate does not).

**Reproducible build:** a small host script (`scripts/kvm/goose/taoxichuan-06-image.sh`, new) that:
copies the pristine serve image, grows it offline (`truncate` + `resize2fs`, or build fresh with
`mke2fs -q -b 4096` and copy the tree with `debugfs -w`/loop mount), populates the files above from
a frozen mirror snapshot, and prints a sha256. Every gate boots a **fresh copy**, so `apk add` runs
against a known disk each time and the stage-1–4 regression image stays untouched.

## 3. Network path outbound

The relay already implements outbound: `Gateway::out_accept`/`out_refuse`, host-side sockets, an
`outbound: bool` gate (`amd64/src/kvm/relay.rs`, enabled per spec at
`amd64/src/kvm/linux/machine.rs:1143`), plus a DNS forwarder (`dns_sock`, `dns_fwd`/`dns_answered`
counters). It has never been exercised guest-ward from taoxichuan. Config: add **`vm_outbound =
true`** to `/etc/herd/available/vm-dev.conf` — that is the literal key `vm-serve-mc.conf` uses; same
file, same shape as `vm_forward`. Gate: destination `10.0.2.2` (the gateway), which
`outbound_allowed()` permits (rejects only 0/127/224+). **Unverified assumption (M1 settles it):**
guest→`10.0.2.2:8080` maps to a host-loopback socket on the same port.

**DNS:** apk.static is pure musl — it reads `/etc/resolv.conf` and speaks UDP:53 itself; `akuma-net`'s
kernel-side resolver (`dns.rs`, configured + fallback resolvers) is *not* on this path and needs no
change. The relay's DNS forwarder answers queries sent to `10.0.2.2`. If busybox in the image has
`nslookup`, M2 gates it; otherwise the relay's `dns_answered` counter + a later `apk update` success
is the evidence. DNS is **not** on the critical path: the gate repository is an IP literal (below).

**HTTP vs HTTPS:** the gate uses **plain HTTP against a host-local mirror** — the host serves a
frozen Alpine snapshot on `:8123` and `/etc/apk/repositories` says
`http://10.0.2.2:8123/alpine/latest-stable/main`. Rationale: `dl-cdn.alpinelinux.org` measurably 403s
plain HTTP (aarch64 doc), pinning some third-party HTTP mirror is fragile for a gate, and a local
mirror is deterministic, offline, and makes the host reference run (§7) use the *same bytes*.
HTTPS would add: a CA bundle in the image, a correct wall clock (certificate notBefore/notAfter),
entropy (fine — RDRAND), and TLS inside apk.static (bundled). Keep HTTPS as a stretch goal after
M6; nothing in the gate needs it.

## 4. Clock

apk on plain HTTP needs no trustworthy realtime; TLS needs it (cert validity windows). Cheapest
correct source: **the host, via the kernel command line** — the herd wrapper sets
`vm_cmdline` to include `epoch=<date +%s>`; the kernel anchors `CLOCK_REALTIME` as that value plus
measured uptime, and `clock_gettime`/`gettimeofday` (both dispatched, both currently zero) return
it. Zero network, zero new syscalls, deterministic; drift over a minutes-long gate is irrelevant.
The SNTP path exists and is host-tested (`akuma-kernel-core/src/ntp_boot.rs` wiring +
`akuma_syscalls_time::sntp`), **but** the relay's outbound UDP is DNS-specific (port 53) — SNTP's
UDP:123 would need new relay work. Do the cmdline anchor in M3 (one tiny arm), leave SNTP as a
documented alternative. `utc_seconds` is currently `None`/boot-relative everywhere.

## 5. Process model

Stage-4 SPAWN runs a spawned child **to completion inside the parent's syscall** (nested-task knobs,
`akuma-ring3-amd64/src/lib.rs:542`): one task at a time, no fork, no scheduler. That is exactly how
the stage-4 ssh exec gate works and it is enough for `apk add curl` **if apk never forks**. apk
forks only to run `.post-install`/trigger scripts. The Alpine `curl` dep tree (musl,
ca-certificates-bundle, libcurl, brotli, nghttp2, zlib, openssl, curl) ships no install scripts —
**verify on the host in phase 0** (`tar -tzf` each fetched .apk for `.post-install`/trigger entries;
one command, settles it). If none: nothing in D-phase is built; `apk add curl` runs as a single
SPAWN-nested process and stdout goes to the ssh session as it does today. **Smallest change that
works if a script does appear:** map musl's `posix_spawn` shape (`clone(CLONE_VM|CLONE_VFORK)` →
child `execve` → parent `wait4`) onto the existing SPAWN machinery — snapshot argv/env, load the
image, run to completion nested, reap by pid — plus `pipe2` for the script's captured stdout, plus
`execve` with shebang support (aarch64 needed CLOEXEC + shebang; here CLOEXEC is moot with run-to-
completion nesting). No scheduler. Absolute fallback: gate command becomes
`apk add --no-scripts curl` — decide only with evidence.

## 6. Milestones with mini-gates

Boot dance and rules per stage 4 (`herd stop vm-serve-mc` → boot vm-dev → restore; one kvm0 VM;
never write `/boot/akuma-serve-root.img`; console must stay free of `[Fault]`/`PANIC`).

| # | Build | Files | Gate (over `/bin/ssh -p 2224 root@localhost`) | Boots (guess) | Risk |
|---|---|---|---|---|---|
| M0 (host only, no boot) | freeze mirror snapshot; sum dep-tree sizes → image size; check dep tree for scripts; host reference run (§7) | `/srv/alpine-mirror/`, `scripts/kvm/goose/taoxichuan-06-image.sh` | reference outputs saved + sha256 | 0 | wrong arch (must be x86_64 apk) |
| M1 | `vm_outbound = true` in vm-dev.conf; host listener on :8123 | vm-dev.conf | guest `busybox nc 10.0.2.2 8123` sees host banner (or wget fetches `/`); relay `out_opened`>0 in log | 1–3 | relay 10.0.2.2 mapping unproven — **plan-killer if dead** (§9) |
| M2 (optional) | nothing (DNS forwarder is in the relay) | — | `nslookup` name via 10.0.2.2, or `dns_answered`>0 | 0–1 | busybox may lack nslookup |
| M3 | ET_DYN static-PIE in the loader (accept `ET_DYN`, fixed PIE base, AT_PHDR/AT_ENTRY offsets, skip relocs — musl self-relocates); `epoch=` cmdline anchor for CLOCK_REALTIME; UPTIME arm | `taoxichuan/src/main.rs`, `crates/akuma-ring3-amd64/src/lib.rs`, vm-dev.conf | `apk --version` → `apk-tools 2.14.x` for x86_64; `date` sane | 3–6 | loader is ET_EXEC-only today (lib.rs:75,93) — biggest single unknown (§9) |
| M4 | Phase A+B arms; `/etc/resolv.conf`, repositories (IP mirror) | `taoxichuan/src/main.rs`, image script | `apk update` exit 0; `sha256sum /var/cache/apk/APKINDEX.tar.gz` equals host copy | 4–8 | pselect6/mremap semantics; mmap/brk slab pressure (§9) |
| M5 | Phase C arms + ext2 write wiring (create/write/rename/unlink/symlink into dispatch) | `taoxichuan/src/main.rs` (+ vfs trait routing) | `apk add curl` exit 0; `cat /etc/apk/world` lists curl tree; file list matches reference | 4–10 | ext2 write path never exercised on THIS kernel (§9); image space |
| M6 | Phase E stubs; PT_INTERP loader path if missing | `taoxichuan/src/main.rs` | `curl --version` first line equals host reference | 2–4 | dynamic-linker stubs; MAP_FIXED |
| M7 (contingency) | §5 script path (clone/execve/wait4/pipe2/dup2, shebang) | `taoxichuan/src/main.rs` | `apk add <pkg-with-script>` runs its `.post-install`, exit 0 | 4–8 | only if M0 finds scripts or M5 hits one |

**Final gate** (fresh image copy, one boot): `apk update` → `apk add curl` → `cat /etc/apk/world` →
`apk info -L curl` → `/usr/bin/curl --version`, plus `sha256sum` of `APKINDEX.tar.gz` and
`/usr/bin/curl`, all byte-compared to the host reference (§7); zero `[Fault]`/`PANIC`/`[sig!]`.

## 7. Verification against the host

The box is x86_64, so the **same apk.static binary** produces the reference on the host:

```sh
# host, before any boot (M0):
mkdir -p /srv/alpine-mirror && wget -r -np -nH --cut-dirs=0 \
  http://dl-cdn.alpinelinux.org/alpine/latest-stable/main/x86_64/ -A '*.apk,APKINDEX*' -P /srv/alpine-mirror
# (or whatever subset covers curl + deps + keys); then serve it:
python3 -m http.server 8123 --directory /srv/alpine-mirror &
/tmp/apk.static --root /tmp/refroot --initdb --repositories-file <(echo http://127.0.0.1:8123/alpine/latest-stable/main) \
  --keys-dir /srv/alpine-mirror/keys add curl
cat /tmp/refroot/etc/apk/world                       # reference world
sha256sum /tmp/refroot/var/cache/apk/APKINDEX.tar.gz # reference index
```

```sh
# guest side, one command per connection, no pty:
/bin/ssh -p 2224 root@localhost 'apk update'
/bin/ssh -p 2224 root@localhost 'apk add curl'
/bin/ssh -p 2224 root@localhost 'cat /etc/apk/world'
/bin/ssh -p 2224 root@localhost 'apk info -L curl | sha256sum'
/bin/ssh -p 2224 root@localhost 'sha256sum /usr/bin/curl /usr/lib/libcurl.so.* /var/cache/apk/APKINDEX.tar.gz'
/bin/ssh -p 2224 root@localhost '/usr/bin/curl --version'
```

Supervisor compares: world line-set, `apk info -L curl` file list, and the sha256s — guest vs
`/tmp/refroot`, byte-equal (paths normalized). Also record per-package sizes from `apk info -s`.

## 8. Token/time budget and session boundaries (all guesses, calibrated on stage 4)

Window ≈ 41M input tokens; burn scales hard with context (220k ctx ≈ 60%/h; fresh 59k ≈ 1/10 of
that). Estimated cost per milestone: M0 ≈ 1M; M1+M2 ≈ 2–4M; M3 ≈ 8–15M (loader work, rebuild+boot
cycles); M4 ≈ 10–20M; M5 ≈ 10–20M; M6 ≈ 5–10M; M7 (if needed) ≈ 8–15M. Total ≈ 36–85M → **2–3
sessions**. Boot cycles total ≈ 15–30 at ~1–2 min each.

- **Session A (cheapest first, run this when quota refills): M0 + M1 + M2 + the clock/UPTIME arms**
  (≈ 4–6M). Ends on hard gates (`nc` through the relay; `date` sane). **Stop and restart goose
  here** — this session also settles §9.1, the plan-killer, for ~5M tokens.
- **Session B: M3 alone** (loader, 8–15M, highest variance). Gate `apk --version`. Stop.
- **Session C: M4 → M5 → M6**, stopping at whichever gate completes if the window closes; each of
  M4/M5 is independently resumable because every boot ends in an ssh-visible checkpoint and a
  committed RESULT note.
- Session discipline: fresh context per session, keep RESULT.md the only long-lived prose, commit
  after every gate, never carry console dumps across sessions.

## 9. Unknowns that could kill the plan (each with its settling experiment)

1. **Relay outbound mapping guest→10.0.2.2.** M1: one `nc`/wget through the relay. If dead: add the
   mapping in `relay.rs` (code exists, documented for service guests — low) — but if that proves
   structurally wrong, there is no stage 6.
2. **ET_DYN static-PIE on the minimal loader** (it is ET_EXEC-only, no relocation). M3. Escape
   hatch: rebuild apk-tools from source as non-PIE static (`ET_EXEC`) — the aarch64 work shows the
   binary is the only PIE component; small build, host toolchain.
3. **ext2 write path on THIS kernel.** The crate's write internals exist but taoxichuan's vfs
   wiring is read-only today and untested under rename/unlink/symlink sequences. Settle early in M5
   with a static non-PIE busybox call sequence (`touch`, `mv`, `ln -s` against the image) before
   letting apk at it; `cargo test -p akuma-ext2` on the host first.
4. **16 MiB image space for the curl tree.** Settle in M0 on the host (sum `size` fields in
   APKINDEX); if >~6 MiB free after growth fails, build the 64 MiB fresh image in M0.
5. **1 MiB mmap/brk slabs under apk's mremap/realloc pressure** (aarch64 saw 330 MB pathological
   growth pre-mremap). Settle in M4: slab watermark prints on the console during `apk update`; if
   exhausted, grow the slab constants — one-line change, but budget a boot cycle for it.

## Portability note (rules)

taoxichuan and `akuma-ring3-amd64` are already amd64-only crates — new loader/syscall/clock code
there needs no gating. If any shared crate is touched (`akuma-ext2`, `akuma-vfs`, `akuma-net`),
new code must be `#[cfg(target_arch = "x86_64")]`-gated, and nothing amd64-only may be added to the
workspace root `default-members`. Host-side artifacts (`scripts/kvm/goose/taoxichuan-06-image.sh`,
mirror) are shell and need nothing. This commit contains no code, so there is nothing to gate here.
