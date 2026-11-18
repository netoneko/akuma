# Tokio and JVM gates rerun on Taoxichuan, Shanghai and Denver (2026-11-18)

Asked for after the bwrap/seccomp merge, to be sure the platform kernels still hold. They did not: the rerun found a real regression in shared code.

## Symptom
`userspace/tokioprobe` on Taoxichuan: RUNTIME and UNAME pass, HTTPS fails in DNS with `Invalid flags`. `trace-err` showed `open` (nr 2) returning `-14` (EFAULT) on a `.rodata` path.

## Cause
`akuma_user_access::read_user_cstr` (used by `copy_from_user_str`) prefaults each page-aligned string start so a path whose first byte sits on a not-yet-faulted page is mapped before the copy. Commit `9cc41207` (2026-11-16) narrowed that to `cfg!(target_arch = "aarch64")`. The tokio kernels' ring-0 fault handler fills only zero pages, so a lazily mapped file page (`.rodata`) was never populated and the copy failed with EFAULT. The original fix is in `TAOXICHUAN_SHARED_MAPPINGS_AND_LAZY_IMAGE.md`.

## Fix
Run the per-page `validate_user_range(addr, 1, Prefault::Yes)` on every architecture (`crates/akuma-user-access/src/lib.rs`).

## Verified (QEMU microvm, `-accel kvm`, snapshot disks, `epoch=<unix seconds>` on the command line since there is no RTC)
| gate | result |
|---|---|
| Taoxichuan tokioprobe, pre-fix kernel (control) | FAIL, DNS `Invalid flags` |
| Taoxichuan tokioprobe, fixed | PASS (RUNTIME, UNAME, HTTPS 200) |
| Shanghai tokioprobe | PASS |
| Denver tokioprobe, default and JVM feature sets | PASS |
| Denver JVM: Clojure 1.12.0 server on OpenJDK 21 | PASS: `curl` returned `{"server":"clojure 1.12.0",...,"arch":"amd64"}`, hit counter advanced on a second request |

Notes: these ran on the host's KVM, not Akuma's hypervisor. The Denver image was built without Docker (Alpine v3.22 `apk.static`, same package set and SHA-1-checked jars as `overlays/denver/build-image.sh`). aarch64 platform kernels were built (`premerge_build.sh`) but not booted.

## Open
`copy_from_user_str` is used by every path syscall; re-run `scripts/bwrap_suite.py` against the main amd64 kernel before the next merge to main.
