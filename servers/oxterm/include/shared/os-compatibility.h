#ifndef OS_COMPATIBILITY_H
#define OS_COMPATIBILITY_H
#include <unistd.h>
#include <sys/mman.h>
/* oxbow: a shareable anonymous file == a memfd-backed shm region. */
static inline int os_create_anonymous_file(off_t size) {
    int fd = memfd_create("wl-shm", 0);
    if (fd < 0) return -1;
    if (ftruncate(fd, size) != 0) { close(fd); return -1; }
    return fd;
}
#endif
