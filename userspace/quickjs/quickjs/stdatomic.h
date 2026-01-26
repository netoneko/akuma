/* Minimal stdatomic.h stub for QuickJS */
#ifndef _STDATOMIC_H
#define _STDATOMIC_H

/* We run single-threaded, so atomics can be regular operations */

typedef int atomic_int;
typedef unsigned int atomic_uint;
typedef _Bool atomic_bool;
typedef unsigned long atomic_uintptr_t;

#define ATOMIC_VAR_INIT(value) (value)

#define atomic_init(obj, value) (*(obj) = (value))
#define atomic_load(obj) (*(obj))
#define atomic_store(obj, value) (*(obj) = (value))
#define atomic_exchange(obj, value) ({ \
    __typeof__(*(obj)) __old = *(obj); \
    *(obj) = (value); \
    __old; \
})

#define atomic_fetch_add(obj, arg) ({ \
    __typeof__(*(obj)) __old = *(obj); \
    *(obj) += (arg); \
    __old; \
})

#define atomic_fetch_sub(obj, arg) ({ \
    __typeof__(*(obj)) __old = *(obj); \
    *(obj) -= (arg); \
    __old; \
})

#define atomic_fetch_or(obj, arg) ({ \
    __typeof__(*(obj)) __old = *(obj); \
    *(obj) |= (arg); \
    __old; \
})

#define atomic_fetch_and(obj, arg) ({ \
    __typeof__(*(obj)) __old = *(obj); \
    *(obj) &= (arg); \
    __old; \
})

#define atomic_compare_exchange_strong(obj, expected, desired) ({ \
    _Bool __success = (*(obj) == *(expected)); \
    if (__success) *(obj) = (desired); \
    else *(expected) = *(obj); \
    __success; \
})

#define atomic_compare_exchange_weak atomic_compare_exchange_strong

/* Memory orders - ignored in single-threaded mode */
#define memory_order_relaxed 0
#define memory_order_consume 1
#define memory_order_acquire 2
#define memory_order_release 3
#define memory_order_acq_rel 4
#define memory_order_seq_cst 5

#define atomic_load_explicit(obj, order) atomic_load(obj)
#define atomic_store_explicit(obj, value, order) atomic_store(obj, value)
#define atomic_fetch_add_explicit(obj, arg, order) atomic_fetch_add(obj, arg)
#define atomic_fetch_sub_explicit(obj, arg, order) atomic_fetch_sub(obj, arg)

#endif /* _STDATOMIC_H */
