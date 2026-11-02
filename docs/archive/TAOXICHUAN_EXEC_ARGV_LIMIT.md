# Taoxichuan's execve capped argv at 448 entries

**Status: FIXED 2026-11-03** in `crates/akuma-ring3-amd64` and `crates/akuma-tokio-kernel-amd64`. Found
during [`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md).

## Symptom

cargo's `rustc` line for `infinity-agent-cli` had 570 arguments (23 KB): one `-L dependency=...` per
dependency, because cargo's build directory gives each crate its own `out/`. Taoxichuan's `execve` returned
`E2BIG` at 449 entries or more, whatever their size. GLM measured this from Python: 447 ten-byte arguments
passed, 448 failed, 300 hundred-byte arguments passed.

Cargo would have retried with an `@argfile`, but the `E2BIG` never reached it
([`UNIX_SOCKET_READ_WRITE_BYPASSED_RECORDS.md`](UNIX_SOCKET_READ_WRITE_BYPASSED_RECORDS.md)). GLM then wrote
a `RUSTC_WRAPPER` that moves the `-L` flags into a response file. The Python version failed for a different
reason ([`TAOXICHUAN_EXEC_SHEBANG.md`](TAOXICHUAN_EXEC_SHEBANG.md)). The C version could not help either:
cargo's exec of the wrapper carries the same 571 arguments.

## Cause

There were three fixed sizes, each derived from the one beneath it:

- `ring3::build_stack` recorded every string's address in `[u64; 512]` (a 4 KiB array on the 64 KiB syscall
  stack) and asserted `argv + envp <= 512`.
- `load_task` wrote the block into a fixed 12-page window (`ARG_BLOCK_PAGES`).
- `child.rs` set the caps from those two: `ARGV_MAX = 448`, `ENV_MAX = 256`, `MAX_STRS = 512`, and
  `ARG_BYTES_MAX` = 12 pages less 2 KiB.

## Fix

- **`build_stack` keeps no table.** String `i` (env first, then argv) sits at `stack_top` less the lengths of
  strings `0..=i`, so the pointer vectors are recomputed from the lengths. A new `stack_block_len(argv, env,
  n_aux)` returns the bytes the builder writes, as an upper bound, and `build_stack` asserts that its buffer
  holds that many.
- **The block is sized by its contents.** `load_task` builds the block first, into a fallibly allocated
  buffer of `stack_block_len` rounded up to pages. It then maps at least `USER_STACK_PAGES` (16) stack
  pages, or the block plus four pages if that is larger, copying the block into the top pages as they are
  allocated. `ARG_BLOCK_PAGES` and the fixed `stack_frames` array are gone.
- **The caps bound work, not a table.** `ARGV_MAX` is 32768, `ENV_MAX` 8192, `STR_MAX` 128 KiB (Linux's
  `MAX_ARG_STRLEN`), and `ARG_BYTES_MAX` 1 MiB.
- **Alignment, found on the way.** `build_stack` promised a 16-byte-aligned `rsp`, but never padded. The
  words under the strings come to an odd count whenever argc + envc is even, and then `rsp` was 8 mod 16.
  musl's `_start` realigns, so it never mattered in practice, and the old tests happened to use odd counts.
  It now pads one word, as Linux does.

## Verification

- Host tests: `akuma-ring3-amd64`, 7 passed. The new `a_cargo_sized_argv_lays_out_in_a_block_of_stack_block_len`
  builds 570 arguments and 84 variables and checks every pointer against its string. Before the alignment
  pad was added, it failed on `rsp % 16 == 8`.
- TCG, SMP=4: Rust std spawning `/bin/true` with 500 arguments succeeds on both the fork path and the
  `posix_spawn` path (`E2BIG` before), and `env /bin/true $(seq 1 5000)` succeeds.
- Ryzen guest (the Infinity run's kernel): Python `subprocess.run(["/bin/true"] + ["x"] * n)` returns 0 for
  n = 600 and n = 5000.

## Still open

- The block is copied twice: built in a heap buffer, then copied into the stack frames. Building it in place
  would need the frames mapped first.

## Background

- [`TAOXICHUAN_BUGS_FOUND_2026_10_30.md`](TAOXICHUAN_BUGS_FOUND_2026_10_30.md): the previous raise, from 128
  entries.
- [`INFINITY_ON_TAOXICHUAN.md`](INFINITY_ON_TAOXICHUAN.md).
