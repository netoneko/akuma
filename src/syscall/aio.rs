use super::*;

struct AioContext {
    _nr_events: u32,
}

static AIO_CONTEXTS: Spinlock<BTreeMap<u64, AioContext>> = Spinlock::new(BTreeMap::new());
static NEXT_AIO_ID: AtomicU64 = AtomicU64::new(1);

/// io_setup(nr_events: u32, ctx_idp: *mut aio_context_t) -> i64
pub(super) fn sys_io_setup(nr_events: u64, ctx_idp: u64) -> u64 {
    if nr_events == 0 {
        return EINVAL;
    }
    // ctx_idp must be a valid writable user pointer to u64
    if !validate_user_ptr(ctx_idp, 8) {
        return EFAULT;
    }
    // *ctx_idp must be 0 (no existing context)
    let mut existing: u64 = 0;
    if unsafe { copy_from_user_safe(&mut existing as *mut u64 as *mut u8, ctx_idp as *const u8, 8).is_err() } {
        return EFAULT;
    }
    if existing != 0 {
        return EEXIST;
    }

    let id = NEXT_AIO_ID.fetch_add(1, Ordering::Relaxed);
    crate::irq::with_irqs_disabled(|| {
        AIO_CONTEXTS.lock().insert(id, AioContext { _nr_events: nr_events as u32 });
    });

    if unsafe { copy_to_user_safe(ctx_idp as *mut u8, &id as *const u64 as *const u8, 8).is_err() } {
        crate::irq::with_irqs_disabled(|| { AIO_CONTEXTS.lock().remove(&id); });
        return EFAULT;
    }

    crate::tprint!(64, "[io_setup] nr_events={} ctx={}\n", nr_events, id);
    0
}

/// io_destroy(ctx: aio_context_t) -> i64
pub(super) fn sys_io_destroy(ctx: u64) -> u64 {
    let removed = crate::irq::with_irqs_disabled(|| {
        AIO_CONTEXTS.lock().remove(&ctx)
    });
    match removed {
        Some(_) => {
            crate::tprint!(64, "[io_destroy] ctx={} destroyed\n", ctx);
            0
        }
        None => EINVAL,
    }
}
