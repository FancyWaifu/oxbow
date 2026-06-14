#ifndef _SYS_UIO_H
#define _SYS_UIO_H
#include <sys/types.h>
struct iovec { void *iov_base; size_t iov_len; };
long readv(int, const struct iovec *, int);
long writev(int, const struct iovec *, int);
#endif
