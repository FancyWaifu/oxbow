#ifndef _SYS_SELECT_H
#define _SYS_SELECT_H
#include <sys/types.h>
#include <sys/time.h>
#define FD_SETSIZE 64
typedef struct { unsigned long fds_bits[FD_SETSIZE/64 + 1]; } fd_set;
#define FD_ZERO(s) do{ unsigned _i; for(_i=0;_i<sizeof(fd_set)/sizeof(unsigned long);_i++) ((fd_set*)(s))->fds_bits[_i]=0; }while(0)
#define FD_SET(fd,s) ((s)->fds_bits[(fd)/64] |= (1UL<<((fd)%64)))
#define FD_CLR(fd,s) ((s)->fds_bits[(fd)/64] &= ~(1UL<<((fd)%64)))
#define FD_ISSET(fd,s) (((s)->fds_bits[(fd)/64] >> ((fd)%64)) & 1)
int select(int, fd_set *, fd_set *, fd_set *, struct timeval *);
#endif
