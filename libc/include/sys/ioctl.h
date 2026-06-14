#ifndef _SYS_IOCTL_H
#define _SYS_IOCTL_H
#define FIONBIO 0x5421
#define FIONREAD 0x541B
int ioctl(int, unsigned long, ...);
#endif
