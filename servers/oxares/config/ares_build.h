#ifndef __CARES_BUILD_H
#define __CARES_BUILD_H
#define CARES_TYPEOF_ARES_SOCKLEN_T socklen_t
#define CARES_TYPEOF_ARES_SSIZE_T ssize_t
#define CARES_HAVE_SYS_TYPES_H 1
#define CARES_HAVE_SYS_SOCKET_H 1
#define CARES_HAVE_SYS_SELECT_H 1
#ifdef CARES_HAVE_SYS_TYPES_H
#  include <sys/types.h>
#endif
#ifdef CARES_HAVE_SYS_SOCKET_H
#  include <sys/socket.h>
#endif
#ifdef CARES_HAVE_SYS_SELECT_H
#  include <sys/select.h>
#endif
#endif /* __CARES_BUILD_H */
