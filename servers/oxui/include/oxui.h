/* oxui — oxbow's tiny UI toolkit (§64).
 *
 * A program that wants a window does NOT touch Wayland, shm, memfd, xkb,
 * double-buffering, frame pacing, or the close box. It does:
 *
 *   static void draw(oxui_window *w, oxui_canvas c, void *user) {
 *       // paint c.pixels (XRGB8888, c.width*c.height), using c.time_ms to animate
 *   }
 *   int main(void) {
 *       oxui_window *w = oxui_window_create("hello", 640, 480);
 *       oxui_handlers h = { .draw = draw, .animate = 1 };
 *       return oxui_run(w, &h, NULL);
 *   }
 *
 * All the boilerplate (connect, bind globals, surface+xdg_toplevel, the two-buffer
 * shm pool with release tracking + deferred redraw, the event loop, xkb keysym
 * decode, the close box) lives in liboxui — written once, with the §63 fixes baked
 * in as library invariants.
 */
#ifndef OXUI_H
#define OXUI_H

#include <stdint.h>

typedef struct oxui_window oxui_window;

/* A frame to paint into. pixels is width*height XRGB8888, row-major. time_ms is
 * milliseconds since start — use it to animate. */
typedef struct {
    int       width, height;
    uint32_t *pixels;
    uint32_t  time_ms;
} oxui_canvas;

typedef struct {
    /* Paint the next frame. Called when the window is dirty (oxui_request_redraw),
     * resized, or — if .animate is set — every frame. Required. */
    void (*draw)(oxui_window *w, oxui_canvas c, void *user);
    /* A key was pressed/released (xkb keysym; pressed=1 on press). Optional. */
    void (*key)(oxui_window *w, uint32_t keysym, int pressed, void *user);
    /* The window's close box was clicked. Optional; default quits the loop. */
    void (*closed)(oxui_window *w, void *user);
    /* An extra app fd to also wait on (e.g. a terminal's tty). -1 = none. When it
     * is readable, fd_ready is called. Optional. */
    int  extra_fd;
    void (*fd_ready)(oxui_window *w, void *user);
    /* Keep repainting every frame (continuous animation). 0 = event-driven: only
     * repaint on oxui_request_redraw / resize. */
    int  animate;
} oxui_handlers;

/* Create a window (title shown to the WM; w×h content area). NULL on failure. */
oxui_window *oxui_window_create(const char *title, int w, int h);

/* Mark the window dirty so the next loop iteration repaints it (for event-driven
 * apps that changed their content). */
void oxui_request_redraw(oxui_window *w);

/* Ask the run loop to exit at the next iteration. */
void oxui_quit(oxui_window *w);

/* Current window content size (updates across resizes). */
int oxui_width(oxui_window *w);
int oxui_height(oxui_window *w);

/* Run the event loop until the window closes / oxui_quit. Returns 0. Blocks the
 * caller (no busy-poll): sleeps in the kernel until a Wayland event, the extra fd,
 * or — in animate mode — the next frame callback. */
int oxui_run(oxui_window *w, const oxui_handlers *h, void *user);

void oxui_window_destroy(oxui_window *w);

#endif /* OXUI_H */
