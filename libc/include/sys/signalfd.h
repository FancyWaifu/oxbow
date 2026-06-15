#ifndef _SYS_SIGNALFD_H
#define _SYS_SIGNALFD_H
#include <stdint.h>
#define SFD_CLOEXEC 02000000
#define SFD_NONBLOCK 04000
struct signalfd_siginfo {
    uint32_t ssi_signo;
    uint8_t  __pad[128 - 4];
};
int signalfd(int fd, const void *mask, int flags);
#endif
