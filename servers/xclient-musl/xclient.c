// Minimal raw-X11 client for oxbow — no libxcb / libX11. Connects to Xwayland's TCP
// listener over loopback (127.0.0.1:6000 = display :0), does the X11 connection-setup
// handshake, then CreateWindow + MapWindow. The server paints the window's background
// pixel, so a solid rectangle appears inside the "XWAYLAND ON :0" root — proving a real
// X client renders on oxbow. Stays connected (else the server frees the window).
//
// The X11 wire protocol is little-endian here ('l'); we run on x86_64.
#include <sys/socket.h>
#include <netinet/in.h>
#include <unistd.h>
#include <string.h>
#include <stdint.h>
#include <fcntl.h>
#include <errno.h>
#include <sched.h>

static void logs(const char *m) { write(2, m, strlen(m)); }

// The socket is O_NONBLOCK so read()/write() never pin the single-threaded net server
// (which must stay free to serve Xwayland, the peer producing our data over loopback).
// On EAGAIN we yield so Xwayland gets scheduled, then retry.
static int rd_all(int fd, void *buf, int n) {
    char *p = buf; int got = 0;
    while (got < n) {
        int r = read(fd, p + got, n - got);
        if (r < 0) { if (errno == EAGAIN) { sched_yield(); continue; } return -1; }
        if (r == 0) return -1; // peer closed
        got += r;
    }
    return 0;
}
static int wr_all(int fd, const void *buf, int n) {
    const char *p = buf; int put = 0;
    while (put < n) {
        int r = write(fd, p + put, n - put);
        if (r < 0) { if (errno == EAGAIN) { sched_yield(); continue; } return -1; }
        if (r == 0) return -1;
        put += r;
    }
    return 0;
}
static uint32_t rd32(const unsigned char *p) {
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}
static uint16_t rd16(const unsigned char *p) { return (uint16_t)p[0] | ((uint16_t)p[1] << 8); }

int main(void) {
    logs("[xclient] start\n");

    // 1. Connect to 127.0.0.1:6000 with retry — Xwayland may still be coming up.
    int fd = -1;
    for (int try = 0; try < 40; try++) {
        fd = socket(AF_INET, SOCK_STREAM, 0);
        if (fd < 0) { logs("[xclient] socket() failed\n"); return 1; }
        struct sockaddr_in sa;
        memset(&sa, 0, sizeof sa);
        sa.sin_family = AF_INET;
        sa.sin_port = htons(6000);
        sa.sin_addr.s_addr = htonl(0x7f000001); // 127.0.0.1
        if (connect(fd, (struct sockaddr *)&sa, sizeof sa) == 0) break;
        close(fd); fd = -1;
        for (volatile long i = 0; i < 30000000; i++) {} // crude backoff (~0.5s)
    }
    if (fd < 0) { logs("[xclient] connect failed after retries\n"); return 1; }
    logs("[xclient] connected to 127.0.0.1:6000\n");
    fcntl(fd, F_SETFL, O_NONBLOCK); // non-blocking I/O — see rd_all/wr_all above

    // 2. Connection setup request: byte-order 'l', protocol 11.0, no auth.
    unsigned char req[12] = {0};
    req[0] = 'l';
    req[2] = 11; req[3] = 0; // major 11, minor 0 (LE)
    // auth name len (2) + auth data len (2) = 0; 2 pad
    if (wr_all(fd, req, sizeof req) < 0) { logs("[xclient] setup write failed\n"); return 1; }

    // 3. Setup reply header (8 bytes): success, pad, major(2), minor(2), addlen-words(2).
    unsigned char hdr[8];
    if (rd_all(fd, hdr, 8) < 0) { logs("[xclient] setup read failed\n"); return 1; }
    if (hdr[0] != 1) { logs("[xclient] X setup NOT success\n"); return 1; }
    int addlen = rd16(hdr + 6) * 4;
    logs("[xclient] X setup success\n");

    // 4. Read the additional setup data and parse the bits we need.
    static unsigned char rest[8192];
    if (addlen > (int)sizeof rest) addlen = sizeof rest;
    if (rd_all(fd, rest, addlen) < 0) { logs("[xclient] setup body read failed\n"); return 1; }

    uint32_t id_base = rd32(rest + 4);
    uint16_t vendor_len = rd16(rest + 16);
    uint8_t  num_formats = rest[21];
    int vendor_pad = (vendor_len + 3) & ~3;
    int screen_off = 32 + vendor_pad + num_formats * 8;
    uint32_t root = rd32(rest + screen_off + 0);
    uint32_t white = rd32(rest + screen_off + 8);
    (void) white;

    uint32_t wid = id_base | 1; // first client-allocated resource id

    // 5. CreateWindow (opcode 1): 400x300 InputOutput child of root, depth/visual
    //    CopyFromParent, CWBackPixel = a bright blue so it stands out on the X stipple.
    unsigned char cw[36];
    memset(cw, 0, sizeof cw);
    cw[0] = 1;          // opcode CreateWindow
    cw[1] = 0;          // depth = CopyFromParent
    cw[2] = 9; cw[3] = 0; // request length = 9 words (8 fixed + 1 value)
    memcpy(cw + 4, &wid, 4);
    memcpy(cw + 8, &root, 4);
    // x=80, y=80 (INT16 LE)
    cw[12] = 80; cw[13] = 0; cw[14] = 80; cw[15] = 0;
    // width=400, height=300
    cw[16] = (400 & 0xff); cw[17] = (400 >> 8);
    cw[18] = (300 & 0xff); cw[19] = (300 >> 8);
    cw[20] = 0; cw[21] = 0; // border-width
    cw[22] = 1; cw[23] = 0; // class = InputOutput
    // visual = 0 (CopyFromParent) at cw[24..28] already zero
    uint32_t mask = 0x00000002; // CWBackPixel
    memcpy(cw + 28, &mask, 4);
    uint32_t bg = 0x00003cff; // blue-ish (TrueColor RGB); harmless if mapped otherwise
    memcpy(cw + 32, &bg, 4);
    if (wr_all(fd, cw, sizeof cw) < 0) { logs("[xclient] CreateWindow failed\n"); return 1; }

    // 6. MapWindow (opcode 8).
    unsigned char mw[8];
    memset(mw, 0, sizeof mw);
    mw[0] = 8; mw[2] = 2; mw[3] = 0; // opcode 8, length 2 words
    memcpy(mw + 4, &wid, 4);
    if (wr_all(fd, mw, sizeof mw) < 0) { logs("[xclient] MapWindow failed\n"); return 1; }
    logs("[xclient] window created + mapped\n");

    // 7. Stay connected (closing would free the window). Drain whatever the server sends.
    for (;;) {
        unsigned char ev[32];
        if (rd_all(fd, ev, 32) < 0) { logs("[xclient] server closed\n"); return 0; }
    }
}
