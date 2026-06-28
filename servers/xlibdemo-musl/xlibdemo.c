// Minimal X client on REAL libX11 (Xlib) — roadmap A2. Proves Xlib (the classic X
// client API that toolkits and most upstream X apps are built on) works on oxbow, on
// top of the libxcb transport ported in A1. XOpenDisplay -> loopback TCP -> X handshake,
// then XCreateSimpleWindow + XMapWindow render a window in the Xwayland root.
#include <X11/Xlib.h>
#include <unistd.h>
#include <string.h>

static void logs(const char *m) { write(2, m, strlen(m)); }

int main(void) {
    logs("[xlibdemo] start\n");

    Display *d = NULL;
    for (int t = 0; t < 40; t++) {
        d = XOpenDisplay("127.0.0.1:0"); // host "127.0.0.1" -> TCP 6000 (loopback)
        if (d) break;
        for (volatile long i = 0; i < 30000000; i++) {} // crude backoff while Xwayland comes up
    }
    if (!d) { logs("[xlibdemo] XOpenDisplay failed\n"); return 1; }
    logs("[xlibdemo] XOpenDisplay ok (libX11)\n");

    int s = DefaultScreen(d);
    Window win = XCreateSimpleWindow(d, RootWindow(d, s),
                                     160, 160, 360, 260, 4,
                                     BlackPixel(d, s), 0x0000ccccu /* cyan */);
    XStoreName(d, win, "xlibdemo");
    XSelectInput(d, win, ExposureMask | KeyPressMask);
    XMapWindow(d, win);
    XFlush(d);
    logs("[xlibdemo] window mapped (libX11)\n");

    XEvent e;
    for (;;) {
        XNextEvent(d, &e); // blocks; keeps the connection (and window) alive
    }
    return 0;
}
