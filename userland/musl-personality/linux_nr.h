/* Linux x86_64 syscall numbers the personality recognizes. (Full canonical list
 * lives in musl's arch/x86_64/bits/syscall.h; we only enumerate what we dispatch.)
 * Numbers are the real Linux x86_64 NRs, since that is the ABI musl is built for. */
#ifndef OXBOW_LINUX_NR_H
#define OXBOW_LINUX_NR_H

#define NR_read              0
#define NR_write             1
#define NR_open              2
#define NR_close             3
/* BSD sockets (Phase 1 TCP client; Phase 2 adds UDP + DNS) */
#define NR_socket            41
#define NR_connect           42
#define NR_accept            43
#define NR_sendto            44
#define NR_recvfrom          45
#define NR_sendmsg           46
#define NR_recvmsg           47   /* musl's DNS resolver reads UDP replies via recvmsg */
#define NR_shutdown          48
#define NR_bind              49
#define NR_listen            50
#define NR_getsockname       51
#define NR_getpeername       52
#define NR_setsockopt        54
#define NR_getsockopt        55
#define NR_accept4           288
#define NR_stat              4
#define NR_fstat             5
#define NR_lstat             6
#define NR_poll              7
#define NR_lseek             8
#define NR_select            23
#define NR_ppoll             271
#define NR_pselect6          270
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
#define NR_pipe              22
#define NR_pipe2             293
#define NR_dup3              292
#define NR_nanosleep         35
#define NR_alarm             37
#define NR_ftruncate         77
#define NR_setuid            105
#define NR_setgid            106
#define NR_setreuid          113
#define NR_setregid          114
#define NR_setgroups         116
#define NR_setresuid         117
#define NR_setresgid         119
#define NR_setfsuid          122
#define NR_setfsgid          123
#define NR_memfd_create      319  /* §wayland: wl_shm pools are memfd-backed */
#define NR_getdents64        217
#define NR_getpid            39
#define NR_getppid           110
#define NR_clone             56
#define NR_fork              57
#define NR_execve            59
#define NR_exit              60
#define NR_wait4             61
#define NR_kill              62
#define NR_tkill             200
#define NR_tgkill            234
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
#define NR_statx             332

/* epoll / timerfd / signalfd — libwayland's server event loop (weston) needs these */
#define NR_epoll_create     213
#define NR_epoll_wait       232
#define NR_epoll_ctl        233
#define NR_epoll_pwait      281
#define NR_epoll_create1    291
#define NR_timerfd_create   283
#define NR_timerfd_settime  286
#define NR_timerfd_gettime  287
#define NR_signalfd         282
#define NR_signalfd4        289
#define NR_eventfd          284
#define NR_eventfd2         290
#define NR_fallocate        285

/* epoll_ctl ops + event bits (subset) */
#define LEPOLL_CTL_ADD 1
#define LEPOLL_CTL_DEL 2
#define LEPOLL_CTL_MOD 3
#define LEPOLLIN  0x001
#define LEPOLLOUT 0x004

/* arch_prctl codes */
#define ARCH_SET_FS 0x1002
#define ARCH_GET_FS 0x1003

/* clock ids */
#define CLOCK_REALTIME  0
#define CLOCK_MONOTONIC 1

/* termios ioctls (x86_64) — struct termios is 44 bytes (NCCS=19); winsize is 8. */
#define TCGETS      0x5401
#define TCSETS      0x5402
#define TCSETSW     0x5403
#define TCSETSF     0x5404
#define TIOCGWINSZ  0x5413
#define TIOCSWINSZ  0x5414
#define TIOCGPTN    0x80045430UL /* §pty: get the pts number */
#define TIOCSPTLCK  0x40045431UL /* §pty: (un)lock the pts (noop here) */
/* termios c_lflag bits / c_cc indices used in the cooked default */
#define T_ISIG   0x0001
#define T_ICANON 0x0002
#define T_ECHO   0x0008
#define T_ECHOE  0x0010
#define T_ECHOK  0x0020
#define T_IEXTEN 0x8000

/* a few errnos we return as negatives */
#define E_NOSYS  38
#define E_INTR   4
/* __oxbow_read returns this (= -EINTR) when a tty read is interrupted by Ctrl-C. */
#define OX_READ_EINTR (-4L)
#define E_BADF   9
#define E_INVAL  22
#define E_FAULT  14
#define E_NOENT  2
#define E_EXIST  17
#define E_MFILE  24
#define E_NOTTY  25
#define E_AFNOSUPPORT 97
#define E_NETUNREACH  101
#define E_CONNREFUSED 111

/* socket() domain/type/protocol */
#define LAF_INET      2
#define LSOCK_STREAM  1
#define LSOCK_DGRAM   2
#define LSOCK_TYPE_MASK 0xff   /* SOCK_CLOEXEC/SOCK_NONBLOCK are ORed above this */

/* fcntl commands */
#define F_DUPFD         0
#define F_GETFD         1
#define F_SETFD         2
#define F_GETFL         3
#define F_SETFL         4
#define F_DUPFD_CLOEXEC 1030

/* openat dirfd / flags */
#define AT_FDCWD       (-100)
#define AT_EMPTY_PATH  0x1000
#define LO_WRONLY  01
#define LO_RDWR    02
#define LO_CREAT   0100
#define LO_EXCL    0200
#define LO_TRUNC   01000

#endif
