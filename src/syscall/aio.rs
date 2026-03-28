use super::*;

// Linux AIO ring buffer magic — glibc checks this to decide whether to read
// events directly from the ring or fall back to the io_getevents syscall.
const AIO_RING_MAGIC: u32 = 0xa10a10a1;
const AIO_RING_HEADER_SIZE: u32 = 32; // sizeof(struct aio_ring)
const AIO_RING_EVENT_SIZE: usize = 32; // sizeof(struct io_event)
const PAGE_SIZE: usize = 4096;
const AIO_MAX_NR_EVENTS: u32 =
    ((PAGE_SIZE - AIO_RING_HEADER_SIZE as usize) / AIO_RING_EVENT_SIZE) as u32; // 126

// Layout of Linux's struct aio_ring (first 32 bytes of the mapped page).
// The ctx_idp written by io_setup IS the VA of this ring — userspace reads it
// directly via the shared-memory path in glibc's io_getevents wrapper.
#[repr(C)]
struct AioRingHeader {
    id: u32,
    nr: u32,
    head: u32,
    tail: u32,
    magic: u32,
    compat_features: u32,
    incompat_features: u32,
    header_length: u32,
}

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
    if unsafe {
        copy_from_user_safe(
            &mut existing as *mut u64 as *mut u8,
            ctx_idp as *const u8,
            8,
        )
        .is_err()
    } {
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
    let proc = match akuma_exec::process::lookup_process(owner_pid) {
        Some(p) => p,
        None => return EFAULT,
    };

    let ring_va = match proc.memory.alloc_mmap(PAGE_SIZE) {
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
    let ring_kva = akuma_exec::mmu::phys_to_virt(ring_phys) as *mut AioRingHeader;
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
    if unsafe {
        copy_to_user_safe(
            ctx_idp as *mut u8,
            &ring_va_u64 as *const u64 as *const u8,
            8,
        )
        .is_err()
    } {
        crate::irq::with_irqs_disabled(|| {
            AIO_CONTEXTS.lock().remove(&ring_va_u64);
        });
        // Physical page is already tracked in address_space; it will be freed
        // on process exit.  We can't easily unmap here, but this path is rare.
        return EFAULT;
    }

    crate::tprint!(64, "[io_setup] nr_events={} ring_va=0x{:x}\n", capped_nr, ring_va);
    0
}

/// io_submit(ctx: aio_context_t, nr: long, iocbpp: **iocb) -> long
///
/// Stub: always returns 0 (no events submitted).  We never actually submit I/O.
/// CRITICAL: Must never return a negative value.  Go treats negative returns as
/// pointers (e.g. EINVAL=-22, then Go accesses *(x0+16) = *(-6) → WILD-DA).
pub(super) fn sys_io_submit(ctx: u64, _nr: i64, _iocbpp: u64) -> u64 {
    let exists = crate::irq::with_irqs_disabled(|| AIO_CONTEXTS.lock().contains_key(&ctx));
    if !exists {
        crate::tprint!(96, "[io_submit] ctx=0x{:x} not found → 0\n", ctx);
    } else {
        crate::tprint!(128, "[io_submit] ctx=0x{:x} nr={} → stub 0\n", ctx, _nr);
    }
    0
}

/// io_cancel(ctx: aio_context_t, iocb: *iocb, result: *io_event) -> long
///
/// Stub: always returns 0.  We never submit I/O so there is nothing to cancel.
/// CRITICAL: Must never return a negative value — same WILD-DA risk as io_submit.
pub(super) fn sys_io_cancel(ctx: u64, _iocb: u64, _result: u64) -> u64 {
    let exists = crate::irq::with_irqs_disabled(|| AIO_CONTEXTS.lock().contains_key(&ctx));
    if !exists {
        crate::tprint!(128, "[io_cancel] ctx=0x{:x} not found → 0\n", ctx);
    } else {
        crate::tprint!(128, "[io_cancel] ctx=0x{:x} → 0\n", ctx);
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
    let exists = crate::irq::with_irqs_disabled(|| AIO_CONTEXTS.lock().contains_key(&ctx));
    if !exists {
        crate::tprint!(128, "[io_getevents] ctx=0x{:x} not found → 0\n", ctx);
    }
    // Ring is always empty (head == tail), so 0 events are ready.
    0
}

/// io_destroy(ctx: aio_context_t) -> i64
pub(super) fn sys_io_destroy(ctx: u64) -> u64 {
    let removed =
        crate::irq::with_irqs_disabled(|| AIO_CONTEXTS.lock().remove(&ctx));
    match removed {
        Some(_aio_ctx) => {
            // The physical page is tracked in proc.address_space and will be
            // freed when the process exits (or we could unmap it here, but
            // leaving it mapped until exit is safe since bun never reuses the
            // address and the page is read-only after io_destroy).
            crate::tprint!(64, "[io_destroy] ctx=0x{:x} destroyed\n", ctx);
            0
        }
        None => EINVAL,
    }
}
