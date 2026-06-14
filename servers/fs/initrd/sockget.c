/* HTTP GET via BSD sockets — exercises oxbow-libc's socket shim.
 * Build + run on oxbow:  cc /sockget.c -o /sg   then:  exec /sg 10.0.2.2 8080 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

int main(int argc, char **argv) {
    const char *ip = (argc > 1) ? argv[1] : "10.0.2.2";
    int port = (argc > 2) ? atoi(argv[2]) : 8080;

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) { printf("socket() failed\n"); return 1; }

    struct sockaddr_in sa;
    memset(&sa, 0, sizeof sa);
    sa.sin_family = AF_INET;
    sa.sin_port = htons(port);
    inet_pton(AF_INET, ip, &sa.sin_addr);

    printf("connecting to %s:%d ...\n", ip, port);
    if (connect(fd, (struct sockaddr *)&sa, sizeof sa) < 0) {
        printf("connect() failed\n");
        return 1;
    }
    printf("connected. sending request.\n");
    const char *req = "GET / HTTP/1.0\r\nHost: oxbow\r\n\r\n";
    send(fd, req, strlen(req), 0);

    char buf[256];
    long n, total = 0;
    while ((n = recv(fd, buf, sizeof buf - 1, 0)) > 0) {
        buf[n] = 0;
        fwrite(buf, 1, (size_t)n, stdout);
        total += n;
    }
    printf("\n[received %ld bytes total]\n", total);
    close(fd);
    return 0;
}
