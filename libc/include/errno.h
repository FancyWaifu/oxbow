#ifndef _ERRNO_H
#define _ERRNO_H
/* §96 Phase 3: errno via a function, not a bare data symbol. A non-PIE oxbow exe can't
 * export DATA to a .so, so dynamically-linked code (oxui/wayland in liboxui.so) reaches
 * errno through __errno_location() (a JUMP_SLOT import). Static code resolves it locally.
 * Transparent: `errno = x`, `if (errno)`, `&errno` all still work through the macro. */
int *__errno_location(void);
#define errno (*__errno_location())
#define ENOENT 2
#define EINTR 4
#define EIO 5
#define ENOMEM 12
#define EACCES 13
#define EEXIST 17
#define EINVAL 22
#define EPERM 1
#define EBADF 9
#define EAGAIN 11
#define EWOULDBLOCK 11
#define ENODEV 19
#define ENOTDIR 20
#define EISDIR 21
#define ENFILE 23
#define EMFILE 24
#define EFBIG 27
#define ENOSPC 28
#define ESPIPE 29
#define EROFS 30
#define EPIPE 32
#define ERANGE 34
#define ENOSYS 38
#define ENOTSUP 95
#define EOPNOTSUPP 95
#define EAFNOSUPPORT 97
#define EADDRINUSE 98
#define ETIMEDOUT 110
#define ECONNREFUSED 111
#define EALREADY 114
#define EINPROGRESS 115
#define ENOTCONN 107
#define ENOBUFS 105
#define ECONNRESET 104
#define ECONNABORTED 103
#define EHOSTUNREACH 113
#define ENETUNREACH 101
#define EADDRNOTAVAIL 99
#define EDESTADDRREQ 89
#define EMSGSIZE 90
#define EISCONN 106
#define ENOTSOCK 88
#define EDQUOT 122
#define ELOOP 40
#define ENAMETOOLONG 36
#define ENETDOWN 100
#define EHOSTDOWN 112
#define ENXIO 6
#define EFAULT 14
#define EMLINK 31
#define ENOTEMPTY 39
#define ENODATA 61
#define ENOATTR 61
#define ESRCH 3
#define ENOPROTOOPT 92
#define EPROTONOSUPPORT 93
#define ENOTSUPP 524
#define E2BIG 7
#define EOVERFLOW 75
#define ENOMSG 42
#define EPROTO 71
#endif
