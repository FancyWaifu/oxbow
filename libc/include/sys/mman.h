#ifndef _SYS_MMAN_H
#define _SYS_MMAN_H
#include <stddef_shim.h>
#define PROT_NONE  0
#define PROT_READ  1
#define PROT_WRITE 2
#define PROT_EXEC  4
#define MAP_PRIVATE   2
#define MAP_SHARED    1
#define MAP_ANONYMOUS 0x20
#define MAP_ANON      0x20
#define MAP_FIXED     0x10
#define MAP_FAILED ((void*)-1)
#define MS_ASYNC      1
#define MS_INVALIDATE 2
#define MS_SYNC       4
void *mmap(void *, size_t, int, int, int, off_t);
int munmap(void *, size_t);
int mprotect(void *, size_t, int);
int msync(void *, size_t, int);
#define MFD_CLOEXEC 1
#define MFD_ALLOW_SEALING 2
int memfd_create(const char *, unsigned int);
#endif
