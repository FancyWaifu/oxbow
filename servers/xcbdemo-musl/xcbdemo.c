// Minimal X client built on REAL libxcb (not raw protocol) — the milestone that proves
// the actual client library every upstream X app uses works on oxbow. Connects to
// Xwayland over loopback TCP (xcb parses "127.0.0.1:0" -> TCP port 6000), creates a
// window with a green background, maps it, and waits for events.
#include <xcb/xcb.h>
#include <unistd.h>
#include <string.h>
#include <stdlib.h>

static void logs(const char *m) { write(2, m, strlen(m)); }

int main(void) {
    logs("[xcbdemo] start\n");

    // Retry the connect — Xwayland may still be coming up.
    xcb_connection_t *c = NULL;
    for (int try = 0; try < 40; try++) {
        c = xcb_connect("127.0.0.1:0", NULL);
        if (c && !xcb_connection_has_error(c))
            break;
        if (c) xcb_disconnect(c);
        c = NULL;
        for (volatile long i = 0; i < 30000000; i++) {} // crude backoff
    }
    if (!c || xcb_connection_has_error(c)) { logs("[xcbdemo] xcb_connect failed\n"); return 1; }
    logs("[xcbdemo] connected via libxcb\n");

    const xcb_setup_t *setup = xcb_get_setup(c);
    xcb_screen_t *screen = xcb_setup_roots_iterator(setup).data;
    if (!screen) { logs("[xcbdemo] no screen\n"); return 1; }

    xcb_window_t win = xcb_generate_id(c);
    uint32_t mask = XCB_CW_BACK_PIXEL | XCB_CW_EVENT_MASK;
    uint32_t values[2] = { 0x0000cc44u /* green-ish */, XCB_EVENT_MASK_EXPOSURE };
    xcb_create_window(c, XCB_COPY_FROM_PARENT, win, screen->root,
                      120, 120, 380, 280, 3,
                      XCB_WINDOW_CLASS_INPUT_OUTPUT, screen->root_visual,
                      mask, values);
    xcb_map_window(c, win);
    xcb_flush(c);
    logs("[xcbdemo] window created + mapped via libxcb\n");

    // Stay alive so the window persists; drain events.
    for (;;) {
        xcb_generic_event_t *e = xcb_wait_for_event(c);
        if (!e) { logs("[xcbdemo] connection closed\n"); break; }
        free(e);
    }
    return 0;
}
