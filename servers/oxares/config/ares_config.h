/* ares_config.h — hand-written for oxbow (freestanding, oxbow-libc, no OS
 * config files / event backends). c-ares is configured to be driven manually
 * (ares_process) with servers set programmatically; the sysconfig/event source
 * files are excluded from the build. */
#ifndef ARES_CONFIG_OXBOW_H
#define ARES_CONFIG_OXBOW_H

/* Standard headers oxbow-libc provides. */
#define HAVE_ASSERT_H 1
#define HAVE_ERRNO_H 1
#define HAVE_FCNTL_H 1
#define HAVE_INTTYPES_H 1
#define HAVE_LIMITS_H 1
#define HAVE_STDBOOL_H 1
#define HAVE_STDINT_H 1
#define HAVE_STDLIB_H 1
#define HAVE_STRING_H 1
#define HAVE_STRINGS_H 1
#define HAVE_TIME_H 1
#define HAVE_SYS_TYPES_H 1
#define HAVE_SYS_TIME_H 1
#define HAVE_SYS_SOCKET_H 1
#define HAVE_NETINET_IN_H 1
#define HAVE_NETINET_TCP_H 1
#define HAVE_ARPA_INET_H 1
#define HAVE_ARPA_NAMESER_H 1
#define HAVE_NETDB_H 1
#define HAVE_SYS_IOCTL_H 1
#define HAVE_SYS_UIO_H 1
#define HAVE_BOOL_T 1
#define HAVE_SSIZE_T 1

/* Socket/IP types + functions oxbow-libc has. */
#define HAVE_STRUCT_SOCKADDR_IN6 1
#define HAVE_STRUCT_ADDRINFO 1
#define HAVE_STRUCT_TIMEVAL 1
#define HAVE_AF_INET6 1
#define HAVE_PF_INET6 1
#define HAVE_SOCKET 1
#define HAVE_CONNECT 1
#define HAVE_RECV 1
#define HAVE_RECVFROM 1
#define HAVE_SEND 1
#define HAVE_SENDTO 1
#define HAVE_INET_NTOP 1
#define HAVE_INET_PTON 1
#define HAVE_GETENV 1
#define HAVE_GETTIMEOFDAY 1
#define HAVE_FCNTL 1
#define HAVE_FCNTL_O_NONBLOCK 1
#define HAVE_IOCTL 1
#define HAVE_IOCTL_FIONBIO 1
#define HAVE_WRITEV 1
#define HAVE_ARC4RANDOM_BUF 1

/* recv/recvfrom/send/getsockopt argument types (POSIX shapes). */
#define RECV_TYPE_ARG1 int
#define RECV_TYPE_ARG2 void *
#define RECV_TYPE_ARG3 size_t
#define RECV_TYPE_ARG4 int
#define RECV_TYPE_RETV ssize_t
#define RECVFROM_TYPE_ARG1 int
#define RECVFROM_TYPE_ARG2 void *
#define RECVFROM_TYPE_ARG3 size_t
#define RECVFROM_TYPE_ARG4 int
#define RECVFROM_TYPE_ARG5 struct sockaddr *
#define RECVFROM_TYPE_ARG6 socklen_t *
#define RECVFROM_TYPE_RETV ssize_t
#define SEND_TYPE_ARG1 int
#define SEND_TYPE_ARG2 void *
#define SEND_TYPE_ARG3 size_t
#define SEND_TYPE_ARG4 int
#define SEND_TYPE_RETV ssize_t
#define GETHOSTNAME_TYPE_ARG2 size_t

/* No OS config files, event backends, threads, or platform extras. */
#define CARES_TYPEOF_ARES_SOCKLEN_T socklen_t
#define CARES_TYPEOF_ARES_SSIZE_T ssize_t

#endif /* ARES_CONFIG_OXBOW_H */
