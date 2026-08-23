# Vec audit: finite resources stored in a heap `Vec` instead of a fixed buffer

**Date:** 2026-08-24. **Scope:** `src/` and `crates/*/src` (excludes
`tests.rs`, `#[test]`-gated modules, and anything under a `tests/` directory).
**Status:** survey only — nothing remediated yet.

**One line:** of 301 non-test `Vec<...>` usages, the overwhelming majority are
legitimately variable-length (path components, `/proc` snapshots, ELF program
headers, VFS `read_dir`/`read_file` results, per-call mmap-region lists); three
are a `Vec` standing in for a resource with a real, known, compile-time-ish
cardinality cap, and one of the three (`akuma-net::socket::SOCKET_TABLE`) has
a sibling module in the same crate (`smoltcp_net.rs`) that already solved the
identical problem with a fixed array — so the fix pattern already exists
in-tree.

## Method

```
grep -rn --include='*.rs' -E '\bVec<' src crates \
  | grep -v -E '/tests?/|test_|_test|#\[test\]|mod tests|tests\.rs'
```

368 raw hits → 301 after dropping test files. Then manual read of every
non-trivial cluster (grouped by file; 53 files hit) to classify each as (a) a
snapshot/collection whose size is inherently caller- or state-dependent, or
(b) a stand-in for a resource that has a real upper bound somewhere in the
system (hardware IRQ count, a `MAX_*` constant, a wire-format field width).

Files with >10 hits were read in full rather than just at the grep line,
since a table declaration and its access pattern are rarely on the same line.

## Findings — should probably be a fixed buffer

### 1. `crates/akuma-exec/src/mmu/mod.rs:1649-1835` — page-table frame list per page map

`map_user_page` / `map_user_page_no_flush` / their shared `map_user_page_inner`
return `(Vec<PhysFrame>, bool)`, where the `Vec` collects any *new* page-table
frames allocated while walking to install one PTE. AArch64 here is a 4-level
walk (L0/L1/L2/L3); the L0 root always pre-exists for a live address space, so
**at most 3** new frames can ever come out of a single call. The common case
(all levels already populated) is 0.

This runs on the page-fault / mmap-fault path — i.e. it's hot — and today it
does `let mut allocated_tables = Vec::new();` and pushes into it every single
call, meaning a heap allocation (or at least an allocator round-trip for an
empty `Vec`, plus real allocation when non-empty) on a path that has a known
3-element ceiling. A `[Option<PhysFrame>; 3]` (or equivalent small fixed
array with a count) would remove the allocation entirely.

### 2. `crates/akuma-net/src/socket.rs:364-400` — legacy socket table

```rust
static SOCKET_TABLE: Spinlock<Option<Vec<Option<KernelSocket>>>> = Spinlock::new(None);
...
if table.len() < MAX_SOCKETS { table.push(Some(socket)); ... }
```

`MAX_SOCKETS = 128` (`socket.rs:34`) is an explicit, permanent ceiling — the
table is never allowed to grow past it. That's a fixed-size resource growing
one `push` at a time up to a known cap, which is exactly what a
`[Option<KernelSocket>; MAX_SOCKETS]` is for.

Notably, `crates/akuma-net/src/smoltcp_net.rs` (same crate, a different/newer
socket layer, also with its own `MAX_SOCKETS` constant) already made this
call correctly:

```rust
static mut SOCKET_STORAGE: [SocketStorage<'static>; MAX_SOCKETS] = [SocketStorage::EMPTY; MAX_SOCKETS];
```

`socket.rs`'s table is the outlier relative to its own crate's established
pattern, not an unprecedented design decision.

### 3. `src/irq.rs:36-55` — IRQ handler table

```rust
struct IrqHandlers { handlers: Vec<Option<IrqHandler>> }
...
while handlers.handlers.len() <= irq as usize { handlers.handlers.push(None); }
handlers.handlers[irq as usize] = Some(handler);
```

The IRQ line count is a fixed property of the GIC, known at boot (and in
practice bounded well below any register-registration hot loop). Growing the
vector one `None` at a time up to whatever the highest-registered IRQ number
happens to be is both an unnecessary heap allocation and a slightly odd
"vector as sparse array" pattern. A fixed `[Option<IrqHandler>; MAX_IRQ]`
would be simpler and alloc-free. Lower priority than #1/#2: registration
happens at boot only, and the per-IRQ-dispatch lookup (`handlers.get(irq)`)
is O(1) either way — the cost here is design cleanliness, not runtime cost.

## Checked and ruled out — bounded, but `Vec` is still correct

- **`src/syscall/msgqueue.rs`** — `KernelMsg.data: Vec<u8>` is capped by
  `MSGMAX = 8192`, but real messages vary widely below that ceiling and queue
  up in a `VecDeque<KernelMsg>`. A fixed `[u8; 8192]` per queued message would
  make every queued message cost 8 KB regardless of actual size — worse than
  today's exact-sized allocation.
- **`crates/akuma-net/src/unix.rs`** — `UnixName::Abstract(Vec<u8>)` /
  `Path(Vec<u8>)` are capped by `SUN_PATH_LEN = 108`, but the file's own doc
  comment (around line 200) explains that the *wire* struct (`SockAddrUn`)
  deliberately keeps `[u8; 108]` + a separate `len` rather than trusting a NUL
  scan, because `addrlen` — not string length — is what Linux uses to
  delimit an abstract name. The parsed-name `Vec`s downstream of that are
  small, bind/connect-only, and the design tradeoff was already made
  consciously; not worth reopening for a marginal win.
- **`crates/akuma-pmm/src/lib.rs`** — `bitmap: Vec<u64>` is sized once at
  boot from detected RAM and never resized afterward. It already behaves
  like a fixed buffer for its whole lifetime; it's just spelled `Vec`
  instead of a raw allocated slice. No behavioral issue, not worth touching.
- **`crates/akuma-terminal/src/lib.rs`** — `canon_buffer` / `echo` grow with
  no cap anywhere in this implementation. That means they are *not* actually
  a bounded resource today, so this isn't a Vec-vs-buffer question — it's a
  separate, unrelated gap (Linux's N_TTY line discipline caps a canonical
  line at 4095 bytes; this implementation enforces nothing). Out of scope
  for this audit; flagged here so it isn't lost.
- **`crates/akuma-exec/src/process/table.rs`** and friends — the process
  table itself is already fixed-size arrays (`MAX_PROCESSES = 256`,
  `PROCESS_SLOTS: [AtomicPtr<Process>; MAX_PROCESSES]`, etc). The `Vec`s
  showing up in `list_processes()` / `collect_pids()` / `list_sockets()` /
  `list_kernel_threads()` are introspection snapshots of however many
  entries are currently live — correctly variable-length output, not a
  resource in themselves.

## Not investigated further — not real kernel resources

- `crates/akuma-exec/src/bkl_model.rs`, `crates/akuma-scheduler/src/*` — the
  host-only BKL/scheduler modeling tools (`akuma-scheduler` is explicitly
  **not** in `default-members`, see root `CLAUDE.md`). Their `Vec` usage is
  over abstract graph states / simulated threads, not a real bounded kernel
  table.
- `crates/akuma-ext2/src/ext2.rs` block cache (`chunks: Vec<Vec<u8>>`) — the
  file's own doc comment already documents why a single contiguous
  `Vec<u8>` (the prior design) didn't scale and was replaced by chunking;
  this is a considered design, not an oversight.
- The remaining bulk of the 301 hits: `&str` path-component splits,
  `Filesystem::read_dir`/`read_file` results, ELF program-header lists,
  mmap-region lists (grow/shrink with arbitrary `mmap`/`munmap` calls),
  futex waiter queues (bounded only by live thread count, which is itself
  unbounded-in-practice), copyin/copyout scratch buffers sized to a
  caller-supplied length. None of these have a fixed, known-in-advance
  cardinality that a buffer could exploit.

## Suggested next step

If picked up, do them in priority order: `map_user_page`'s page-table-frame
`Vec` (#1) is the only one on a genuinely hot path and has the clearest fix
shape (a 3-element inline array). `socket.rs`'s `SOCKET_TABLE` (#2) can
likely copy `smoltcp_net.rs`'s existing `SOCKET_STORAGE` pattern almost
verbatim. `irq.rs` (#3) is boot-time-only and lowest priority.
