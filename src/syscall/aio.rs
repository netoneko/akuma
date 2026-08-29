use super::*;

// Linux's `struct aio_ring` and its magic moved to `akuma-syscalls-linux` on
// 2026-08-27. The two `32`s that used to be literals here — `sizeof(struct
// aio_ring)` and `sizeof(struct io_event)` — come from there now; the first is
// derived from the struct, so it cannot drift away from what this file writes
// into the ring's own `header_length` field.
//
// The ctx_idp written by io_setup IS the VA of this ring — userspace reads it
// directly via the shared-memory path in glibc's io_getevents wrapper.
use akuma_syscalls_linux::io::{
    AIO_RING_EVENT_SIZE, AIO_RING_HEADER_SIZE, AIO_RING_MAGIC, AioRingHeader,
};

// One page per ring is Akuma's sizing policy, not ABI, so it stays here.
const PAGE_SIZE: usize = 4096;
const AIO_MAX_NR_EVENTS: u32 =
    ((PAGE_SIZE - AIO_RING_HEADER_SIZE as usize) / AIO_RING_EVENT_SIZE) as u32; // 126

struct AioContext {
    // ring_va is also used as the BTreeMap key (== ctx value written to user).
    _ring_va: usize,
}

static AIO_CONTEXTS: Spinlock<BTreeMap<u64, AioContext>> = Spinlock::new(BTreeMap::new());

/// io_setup(nr_events: u32, ctx_idp: *mut aio_context_t) -> i64
pub(super) fn sys_io_setup(nr_events: u64, ctx_idp: u64) -> u64 {
    if nr_events == 0 {
        return EINVAL;
    }
    if !validate_user_ptr(ctx_idp, 8) {
        return EFAULT;
    }

    // Linux requires *ctx_idp == 0 before the call; only return EEXIST if it
    // refers to a live context.  Bun may pass uninitialized memory here.
    let mut existing: u64 = 0;
    if read_user_into(&mut existing, ctx_idp).is_err() {
        return EFAULT;
    }
    if existing != 0 {
        let live =
            crate::irq::with_irqs_disabled(|| AIO_CONTEXTS.lock().contains_key(&existing));
        if live {
            return EEXIST;
        }
    }

    // Cap to what fits in one page so we don't OOM on huge nr_events values.
    let capped_nr = (nr_events as u32).min(AIO_MAX_NR_EVENTS);

    // ── Allocate the ring-buffer page ────────────────────────────────────────
    let owner_pid = akuma_exec::process::read_current_pid().unwrap_or(0);
    let proc = match akuma_exec::process::lookup_process_shared(owner_pid) {
        Some(p) => p,
        None => return EFAULT,
    };

    let ring_va = match proc.vm_alloc_mmap(PAGE_SIZE) {
        Some(va) => va,
        None => return ENOMEM,
    };

    let frame = match crate::pmm::alloc_page_zeroed() {
        Some(f) => f,
        None => return ENOMEM,
    };
    let ring_phys = frame.addr;

    let (table_frames, _) = unsafe {
        akuma_exec::mmu::map_user_page(
            ring_va,
            ring_phys,
            akuma_exec::mmu::user_flags::RW_NO_EXEC,
        )
    };
    proc.address_space.track_user_frame(frame);
    for tf in table_frames {
        proc.address_space.track_page_table_frame(tf);
    }

    // ── Write the ring header into kernel-virtual space ──────────────────────
    let ring_kva = akuma_exec::mmu::phys_to_virt(ring_phys).cast::<AioRingHeader>();
    unsafe {
        (*ring_kva).id = 0;
        (*ring_kva).nr = capped_nr;
        (*ring_kva).head = 0;
        (*ring_kva).tail = 0;
        (*ring_kva).magic = AIO_RING_MAGIC;
        (*ring_kva).compat_features = 0;
        (*ring_kva).incompat_features = 0;
        (*ring_kva).header_length = AIO_RING_HEADER_SIZE;
    }

    // ── Register context and write VA to user ────────────────────────────────
    crate::irq::with_irqs_disabled(|| {
        AIO_CONTEXTS
            .lock()
            .insert(ring_va as u64, AioContext { _ring_va: ring_va });
    });

    let ring_va_u64 = ring_va as u64;
    if write_user_val(ctx_idp, &ring_va_u64).is_err() {
        crate::irq::with_irqs_disabled(|| {
            AIO_CONTEXTS.lock().remove(&ring_va_u64);
        });
        // Physical page is already tracked in address_space; it will be freed
        // on process exit.  We can't easily unmap here, but this path is rare.
        return EFAULT;
    }

    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        crate::tprint!(64, "[io_setup] nr_events={} ring_va=0x{:x}\n", capped_nr, ring_va);
    }
    0
}

/// io_submit(ctx: aio_context_t, nr: long, iocbpp: **iocb) -> long
///
/// Stub: always returns 0 (no events submitted).  We never actually submit I/O.
/// CRITICAL: Must never return a negative value.  Go treats negative returns as
/// pointers (e.g. EINVAL=-22, then Go accesses *(x0+16) = *(-6) → WILD-DA).
pub(super) fn sys_io_submit(ctx: u64, _nr: i64, _iocbpp: u64) -> u64 {
    // The context lookup is INSIDE the gate on purpose. It is a
    // `with_irqs_disabled` + `AIO_CONTEXTS.lock()` + map probe, and its only
    // consumer is the message below — this function returns 0 either way. With
    // the flag off (every shipping profile) that made three stub syscalls each
    // mask IRQs and take a lock to decide a string nobody prints. See
    // docs/archive/CONSOLE_LOG_COST.md §9.
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        let exists = crate::irq::with_irqs_disabled(|| AIO_CONTEXTS.lock().contains_key(&ctx));
        if exists {
            crate::tprint!(128, "[io_submit] ctx=0x{:x} nr={} → stub 0\n", ctx, _nr);
        } else {
            crate::tprint!(96, "[io_submit] ctx=0x{:x} not found → 0\n", ctx);
        }
    }
    0
}

/// io_cancel(ctx: aio_context_t, iocb: *iocb, result: *io_event) -> long
///
/// Stub: always returns 0.  We never submit I/O so there is nothing to cancel.
/// CRITICAL: Must never return a negative value — same WILD-DA risk as io_submit.
pub(super) fn sys_io_cancel(ctx: u64, _iocb: u64, _result: u64) -> u64 {
    // The context lookup is INSIDE the gate on purpose. It is a
    // `with_irqs_disabled` + `AIO_CONTEXTS.lock()` + map probe, and its only
    // consumer is the message below — this function returns 0 either way. With
    // the flag off (every shipping profile) that made three stub syscalls each
    // mask IRQs and take a lock to decide a string nobody prints. See
    // docs/archive/CONSOLE_LOG_COST.md §9.
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        let exists = crate::irq::with_irqs_disabled(|| AIO_CONTEXTS.lock().contains_key(&ctx));
        if exists {
            crate::tprint!(128, "[io_cancel] ctx=0x{:x} → 0\n", ctx);
        } else {
            crate::tprint!(128, "[io_cancel] ctx=0x{:x} not found → 0\n", ctx);
        }
    }
    0
}

/// io_getevents(ctx: aio_context_t, min_nr: long, nr: long, events: *io_event, timeout: *timespec) -> long
///
/// Stub: always returns 0 (no events ready).  Ring is always empty (head == tail).
/// CRITICAL: Must never return a negative value.  Returning ENOSYS (-38) caused Go
/// to dereference it as a pointer → WILD-DA at FAR=0xffffffffffffffda.  Returning
/// EINVAL (-22) has the same risk: Go accesses *(x0+offset) → WILD-DA.
pub(super) fn sys_io_getevents(ctx: u64, _min_nr: i64, _nr: i64, _events: u64, _timeout: u64) -> u64 {
    // The context lookup is INSIDE the gate on purpose. It is a
    // `with_irqs_disabled` + `AIO_CONTEXTS.lock()` + map probe, and its only
    // consumer is the message below — this function returns 0 either way. With
    // the flag off (every shipping profile) that made three stub syscalls each
    // mask IRQs and take a lock to decide a string nobody prints. See
    // docs/archive/CONSOLE_LOG_COST.md §9.
    if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
        let exists = crate::irq::with_irqs_disabled(|| AIO_CONTEXTS.lock().contains_key(&ctx));
        if !exists {
            crate::tprint!(128, "[io_getevents] ctx=0x{:x} not found → 0\n", ctx);
        }
    }
    // Ring is always empty (head == tail), so 0 events are ready.
    0
}

/// io_destroy(ctx: aio_context_t) -> i64
///
/// Linux returns EINVAL for unknown ctx; negative errno breaks Go (`compile`)
/// (errno-as-pointer WILD-DA — crash10.log: `[EINVAL] nr=1` then FAR=-6).
/// Same policy as `sys_io_submit`: return 0 for unknown ctx (idempotent).
pub(super) fn sys_io_destroy(ctx: u64) -> u64 {
    let removed =
        crate::irq::with_irqs_disabled(|| AIO_CONTEXTS.lock().remove(&ctx));
    if let Some(_aio_ctx) = removed {
        // The physical page is tracked in proc.address_space and will be
        // freed when the process exits (or we could unmap it here, but
        // leaving it mapped until exit is safe since bun never reuses the
        // address and the page is read-only after io_destroy).
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::tprint!(64, "[io_destroy] ctx=0x{:x} destroyed\n", ctx);
        }
        0
    } else {
        if crate::config::SYSCALL_DEBUG_INFO_ENABLED {
            crate::tprint!(96, "[io_destroy] ctx=0x{:x} not found → 0 (avoid EINVAL for Go)\n", ctx);
        }
        0
    }
}
