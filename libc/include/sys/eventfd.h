#ifndef _SYS_EVENTFD_H
#define _SYS_EVENTFD_H
#include <stdint.h>
typedef uint64_t eventfd_t;
#define EFD_CLOEXEC  02000000
#define EFD_NONBLOCK 04000
#define EFD_SEMAPHORE 1
int eventfd(unsigned int initval, int flags);
int eventfd_read(int fd, eventfd_t *value);
int eventfd_write(int fd, eventfd_t value);
#endif
