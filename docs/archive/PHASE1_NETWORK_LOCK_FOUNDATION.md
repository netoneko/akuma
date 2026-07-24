# Network Lock Foundation - Phase 1 Implementation Notes

**Status**: ✅ COMPLETE  
**Date**: 2026-07-24  
**Part of**: BKL Fine-Grained Locking Plan - Phase 1

## Summary

Successfully implemented the lock infrastructure foundation for replacing the BKL with fine-grained networking locks. This provides the scaffolding for Phase 2 (BKL-free network operations).

## Implementation Details

### Files Created

1. **`crates/akuma-net/src/locks.rs`** (350+ lines)
   - Global `NETWORK_LOCK` spinlock
   - Global `SOCKET_TABLE_LOCK` spinlock  
   - Lock ordering constants (`LOCK_LEVEL_NETWORK`, `LOCK_LEVEL_SOCKET_TABLE`, `LOCK_LEVEL_SOCKET`)
   - Lock holder tracking for watchdog monitoring
   - Profiling infrastructure (`NetworkLockStats`)
   - Lock ordering enforcement with runtime checks
   - Comprehensive unit tests

2. **`crates/akuma-net/src/lock_tests.rs`** (120+ lines)
   - Additional integration tests
   - Host-only validation tests
   - Statistics tracking tests

### Files Modified

1. **`crates/akuma-net/src/lib.rs`**
   - Added `pub mod locks;` to expose lock infrastructure

## Key Features

### Lock Hierarchy

```
NETWORK_LOCK (level 10) → SOCKET_TABLE_LOCK (level 20) → Per-socket locks (level 30)
```

Locks must be acquired in strict order to prevent deadlocks. Violations panic at runtime.

### Lock Ordering Enforcement

- `acquire_network_lock()` - Always succeeds (highest priority)
- `acquire_socket_table_lock()` - Requires `NETWORK_LOCK` held first
- Panic on ordering violations
- Automatic violation tracking in statistics

### Profiling Infrastructure

```rust
pub struct NetworkLockStats {
    pub network_contention_count: u64,
    pub network_contention_spins: u64,
    pub socket_table_contention_count: u64,
    pub socket_table_contention_spins: u64,
    pub ordering_violations: u64,
}
```

Functions:
- `get_lock_stats()` - Read current statistics
- `reset_lock_stats()` - Clear statistics (for A/B testing)

### Lock Holder Tracking

- `network_lock_holder()` - Returns current holder or `LOCK_HOLDER_NONE`
- `socket_table_lock_holder()` - Returns current holder or `LOCK_HOLDER_NONE`
- Used by SSH stall watchdog in `src/main.rs::memory_monitor`

## Testing

### Unit Tests (in `locks.rs`)

All tests pass on host:
- ✅ `test_lock_constants` - Verify lock level constants
- ✅ `test_lock_ordering_enforcement` - Correct hierarchy works
- ✅ `test_socket_table_without_network` - Violations panic
- ✅ `test_multiple_acquisitions_same_level` - Idempotent acquisitions
- ✅ `test_stats_tracking` - Statistics recorded correctly
- ✅ `test_holder_tracking` - Holder tracking works
- ✅ `test_reset_stats` - Statistics reset works
- ✅ `test_nested_lock_hierarchy` - Complex hierarchies work
- ✅ `test_invalid_lock_level` - Invalid levels panic

### Build Status

- ✅ Compiles for `aarch64-unknown-none` target
- ✅ All unit tests pass on host
- ✅ No changes to existing networking functionality
- ✅ Zero-overhead when not in use (compile-time feature gates)

## Design Decisions

1. **Spinlocks vs Mutexes**: Used spinlocks for simplicity and existing pattern in networking code
2. **Manual Release Functions**: Manual `release_*()` functions instead of RAII guards to match existing code patterns
3. **Panic on Violations**: Chosen to catch deadlocks early in development rather than silent failures
4. **Holder Tracking**: Added for watchdog compatibility with existing `network_holder_snapshot()`
5. **Const Initialization**: Used `const fn new()` for `NetworkLockStats` to work in static context

## Integration Points

### Existing Code

The new locks integrate with existing patterns:

- **`SOCKET_TABLE`**: Currently uses `Spinlock<Option<Vec<Option<KernelSocket>>>>`
- **`NETWORK`**: Currently uses `Spinlock<Option<NetworkState>>`
- **`PreemptGuard`**: Still required for lock holds under BKL (until Phase 2)

### Future Integration (Phase 2)

These locks will replace BKL for:
- Syscall entry points (`sys_socket`, `sys_bind`, etc.)
- IRQ handler (`poll()`)
- Blocking operations (`wait_until()`, `dns_query()`, etc.)

## Performance Considerations

- **Zero cost when unused**: Locks are static, no allocation
- **Ordering checks**: Minimal overhead (atomic bit operations)
- **Profiling**: Optional (can be disabled in production)
- **Holder tracking**: Best-effort (no acquire ordering vs spinlock)

## Known Limitations

1. **No actual locking yet**: Phase 1 is infrastructure only
2. **Per-socket locks**: Not yet implemented (reserved for future optimization)
3. **Contention tracking**: Counters not yet incremented (will be in Phase 2)
4. **BKL still held**: Network operations still acquire BKL at syscall boundaries

## Next Steps (Phase 2)

1. Update syscall handlers to drop BKL before network operations
2. Update IRQ handler to drop BKL before packet processing
3. Add contention tracking to lock acquisition
4. Create BKL-free entry points for all network syscalls
5. Test under SMP load to measure contention reduction

## Validation

Run the following to validate Phase 1:

```bash
# Build for target
cargo build --package akuma-net --target aarch64-unknown-none

# Run unit tests on host
cargo test --package akuma-net --lib locks::tests

# Verify no regressions
cargo build --release
```

## References

- **Main Plan**: `docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md`
- **SMP Design**: `docs/reference/subsystems/smp-shared.md`
- **Lock Implementation**: `crates/akuma-net/src/locks.rs`
- **Network Audit**: Phase 0 audit results in main plan

---

**Author**: Auto-generated during Phase 1 implementation  
**Status**: Ready for Phase 2 (BKL-free network operations)