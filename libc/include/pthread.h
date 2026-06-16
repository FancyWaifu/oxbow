#ifndef _PTHREAD_H
#define _PTHREAD_H
#include <stddef_shim.h>
#include <time.h>
/* oxbow is single-threaded: pthreads are inert no-op stubs. Mutex lock/unlock
 * are no-ops (no contention possible); pthread_once runs the init once; thread
 * creation fails (so JS Workers/threaded features degrade, but eval works). */
typedef unsigned long pthread_t;
typedef struct { int _u; } pthread_attr_t;
typedef struct { int _u; } pthread_mutex_t;
typedef struct { int _u; } pthread_mutexattr_t;
typedef struct { int _u; } pthread_cond_t;
typedef struct { int _u; } pthread_condattr_t;
typedef int pthread_once_t;

#define PTHREAD_ONCE_INIT 0
#define PTHREAD_MUTEX_INITIALIZER {0}
#define PTHREAD_COND_INITIALIZER {0}
#define PTHREAD_CREATE_JOINABLE 0
#define PTHREAD_CREATE_DETACHED 1

static inline int pthread_mutex_init(pthread_mutex_t *m, const void *a) { (void)m; (void)a; return 0; }
static inline int pthread_mutex_destroy(pthread_mutex_t *m) { (void)m; return 0; }
static inline int pthread_mutex_lock(pthread_mutex_t *m) { (void)m; return 0; }
static inline int pthread_mutex_unlock(pthread_mutex_t *m) { (void)m; return 0; }
static inline int pthread_mutex_trylock(pthread_mutex_t *m) { (void)m; return 0; }

static inline int pthread_cond_init(pthread_cond_t *c, const void *a) { (void)c; (void)a; return 0; }
static inline int pthread_cond_destroy(pthread_cond_t *c) { (void)c; return 0; }
static inline int pthread_cond_signal(pthread_cond_t *c) { (void)c; return 0; }
static inline int pthread_cond_broadcast(pthread_cond_t *c) { (void)c; return 0; }
static inline int pthread_cond_wait(pthread_cond_t *c, pthread_mutex_t *m) { (void)c; (void)m; return 0; }
static inline int pthread_cond_timedwait(pthread_cond_t *c, pthread_mutex_t *m, const void *t) { (void)c; (void)m; (void)t; return 0; }
static inline int pthread_cond_timedwait_relative_np(pthread_cond_t *c, pthread_mutex_t *m, const void *t) { (void)c; (void)m; (void)t; return 0; }

static inline int pthread_condattr_init(pthread_condattr_t *a) { (void)a; return 0; }
static inline int pthread_condattr_destroy(pthread_condattr_t *a) { (void)a; return 0; }
static inline int pthread_condattr_setclock(pthread_condattr_t *a, int c) { (void)a; (void)c; return 0; }

static inline int pthread_attr_init(pthread_attr_t *a) { (void)a; return 0; }
static inline int pthread_attr_destroy(pthread_attr_t *a) { (void)a; return 0; }
static inline int pthread_attr_setdetachstate(pthread_attr_t *a, int s) { (void)a; (void)s; return 0; }
static inline int pthread_attr_setstacksize(pthread_attr_t *a, size_t s) { (void)a; (void)s; return 0; }

static inline int pthread_create(pthread_t *t, const pthread_attr_t *a, void *(*f)(void *), void *arg) {
    (void)t; (void)a; (void)f; (void)arg; return 11; /* EAGAIN: no threads */
}
static inline int pthread_join(pthread_t t, void **r) { (void)t; (void)r; return 0; }
static inline int pthread_once(pthread_once_t *once, void (*f)(void)) {
    if (once && !*once) { *once = 1; f(); }
    return 0;
}

/* Thread-local keys: single-threaded, so one global value per key suffices. */
typedef unsigned long pthread_key_t;
int  pthread_key_create(pthread_key_t *key, void (*destructor)(void *));
int  pthread_key_delete(pthread_key_t key);
void *pthread_getspecific(pthread_key_t key);
int  pthread_setspecific(pthread_key_t key, const void *value);
#endif
