#include <stdio.h>
#include "ares.h"

extern int oxbow_cares_resolve(const char *host, unsigned char out_ip[4]);

int main(int argc, char **argv)
{
  const char *host = (argc > 1) ? argv[1] : "example.com";
  printf("[cares-test] c-ares %s — resolving %s\n", ares_version(NULL), host);

  unsigned char ip[4] = { 0, 0, 0, 0 };
  if (oxbow_cares_resolve(host, ip)) {
    printf("[cares-test] %s -> %u.%u.%u.%u\n", host, ip[0], ip[1], ip[2], ip[3]);
    return 0;
  }
  printf("[cares-test] %s -> resolution failed\n", host);
  return 1;
}
