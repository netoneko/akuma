/* Minimal pthread.h stub for QuickJS */
#ifndef _PTHREAD_H
#define _PTHREAD_H

/* We run single-threaded, so pthread is mostly no-op */

typedef unsigned long pthread_t;
typedef struct { int dummy; } pthread_mutex_t;
typedef struct { int dummy; } pthread_mutexattr_t;
typedef struct { int dummy; } pthread_cond_t;
typedef struct { int dummy; } pthread_condattr_t;

#define PTHREAD_MUTEX_INITIALIZER { 0 }
#define PTHREAD_COND_INITIALIZER { 0 }

/* Stub declarations */
int pthread_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr);
int pthread_mutex_destroy(pthread_mutex_t *mutex);
int pthread_mutex_lock(pthread_mutex_t *mutex);
int pthread_mutex_unlock(pthread_mutex_t *mutex);

int pthread_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr);
int pthread_cond_destroy(pthread_cond_t *cond);
int pthread_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex);
int pthread_cond_signal(pthread_cond_t *cond);
int pthread_cond_broadcast(pthread_cond_t *cond);

pthread_t pthread_self(void);

#endif /* _PTHREAD_H */
