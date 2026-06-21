/* Linux x86_64 syscall numbers the personality recognizes. (Full canonical list
 * lives in musl's arch/x86_64/bits/syscall.h; we only enumerate what we dispatch.)
 * Numbers are the real Linux x86_64 NRs, since that is the ABI musl is built for. */
#ifndef OXBOW_LINUX_NR_H
#define OXBOW_LINUX_NR_H

#define NR_read              0
#define NR_write             1
#define NR_open              2
#define NR_close             3
#define NR_stat              4
#define NR_fstat             5
#define NR_lseek             8
#define NR_mmap              9
#define NR_mprotect          10
#define NR_munmap            11
#define NR_brk               12
#define NR_rt_sigaction      13
#define NR_rt_sigprocmask    14
#define NR_ioctl             16
#define NR_readv             19
#define NR_writev            20
#define NR_access            21
#define NR_sched_yield       24
#define NR_madvise           28
#define NR_dup               32
#define NR_dup2              33
#define NR_nanosleep         35
#define NR_getpid            39
#define NR_exit              60
#define NR_uname             63
#define NR_fcntl             72
#define NR_getcwd            79
#define NR_readlink          89
#define NR_getuid            102
#define NR_getgid            104
#define NR_geteuid           107
#define NR_getegid           108
#define NR_arch_prctl        158
#define NR_gettid            186
#define NR_futex             202
#define NR_set_tid_address   218
#define NR_clock_gettime     228
#define NR_exit_group        231
#define NR_openat            257
#define NR_newfstatat        262
#define NR_getrandom         318

/* arch_prctl codes */
#define ARCH_SET_FS 0x1002
#define ARCH_GET_FS 0x1003

/* clock ids */
#define CLOCK_REALTIME  0
#define CLOCK_MONOTONIC 1

/* a few errnos we return as negatives */
#define E_NOSYS  38
#define E_BADF   9
#define E_INVAL  22
#define E_FAULT  14

#endif
