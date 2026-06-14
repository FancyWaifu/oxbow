#ifndef _SYS_UN_H
#define _SYS_UN_H
#include <sys/socket.h>
struct sockaddr_un { sa_family_t sun_family; char sun_path[108]; };
#define AF_UNIX 1
#define AF_LOCAL 1
#endif
