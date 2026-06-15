/* cares-test — exercise libc's getaddrinfo, which is now backed by c-ares
 * system-wide. Resolving here proves the whole stack: getaddrinfo -> c-ares ->
 * custom socket functions -> net server -> DHCP-leased resolver. */
#include <stdio.h>
#include <string.h>
#include <netdb.h>
#include <netinet/in.h>
#include <sys/socket.h>

int main(int argc, char **argv)
{
  const char *host = (argc > 1) ? argv[1] : "example.com";
  printf("[cares-test] resolving %s via getaddrinfo (c-ares)\n", host);

  struct addrinfo  hints;
  struct addrinfo *res = NULL;
  memset(&hints, 0, sizeof(hints));
  hints.ai_family   = AF_INET;
  hints.ai_socktype = SOCK_STREAM;

  int rc = getaddrinfo(host, NULL, &hints, &res);
  if (rc != 0 || res == NULL) {
    printf("[cares-test] %s -> resolution failed (rc=%d)\n", host, rc);
    return 1;
  }

  struct sockaddr_in *si = (struct sockaddr_in *)res->ai_addr;
  unsigned char      *ip = (unsigned char *)&si->sin_addr;
  printf("[cares-test] %s -> %u.%u.%u.%u\n", host, ip[0], ip[1], ip[2], ip[3]);
  freeaddrinfo(res);
  return 0;
}
