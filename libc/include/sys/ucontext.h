#ifndef _SYS_UCONTEXT_H
#define _SYS_UCONTEXT_H
/* Stub: oxbow has no POSIX signals, so tcc's signal-handler backtrace (the only
 * user of this) never runs — these types exist only so tccrun.c compiles. */
#define REG_RIP 16
#define REG_RBP 10
#define REG_RSP 15
typedef struct { long gregs[32]; } mcontext_t;
typedef struct ucontext_t {
    unsigned long uc_flags;
    struct ucontext_t *uc_link;
    mcontext_t uc_mcontext;
} ucontext_t;
#endif
