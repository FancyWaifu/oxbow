/* oxbow-input.c — P3: feed oxbow's keyboard + mouse into a weston_seat.
 *
 * The oxbow input drivers push RAW byte streams over two boot channels (no MsgBuf tag):
 *   - BOOT_INPUT_CHAN: 1 byte/key event; keycode = b & 0x7f (already an evdev keycode),
 *     release = b & 0x80.
 *   - BOOT_MOUSE_CHAN: 3-byte PS/2 packets [flags, dx, dy]; left = flags&1, sign bits in
 *     flags (0x10 for dx, 0x20 for dy), screen Y inverted.
 * We wrap each channel cap as an fd (ox_chan_fd), watch it in weston's event loop, and
 * inject via notify_key / notify_motion_absolute / notify_button. No libinput/udev — this
 * replaces that whole layer. Keymap = oxbow's compiled-in US xkb keymap. See
 * docs/weston-port.md. */
#include "config.h"

#include <stdint.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

#include <libweston/libweston.h>
#include "backend.h"            /* notify_key / notify_motion_absolute / notify_button */
#include "libweston-internal.h" /* weston_seat_init / _init_keyboard / _init_pointer */
#include <xkbcommon/xkbcommon.h>
#include <wayland-server-core.h>
#include "../oxxkb/xkb/us_keymap.h"

#define BOOT_INPUT_CHAN 36
#define BOOT_MOUSE_CHAN 43
#define BTN_LEFT 0x110 /* linux/input-event-codes.h */

extern int ox_chan_fd(int slot);           /* personality: wrap a boot channel cap as an fd */
extern int oxbow_fb_w, oxbow_fb_h;
extern void oxbow_cursor_move(int px, int py); /* direct-blit cursor (backend) */

static struct weston_seat oxbow_seat;
static int g_have_kbd;         /* keyboard successfully initialized (has a keymap) */
/* absolute cursor position — exported so the backend can paint a software cursor. */
double oxbow_ptr_x = 640, oxbow_ptr_y = 400;
#define g_cx oxbow_ptr_x
#define g_cy oxbow_ptr_y
static int g_btn_left;         /* last reported left-button state (edge detect) */
static unsigned char g_mpkt[3]; /* partial PS/2 packet across reads */
static int g_mpi;

static void now_ts(struct timespec *ts) { clock_gettime(CLOCK_MONOTONIC, ts); }

static int kbd_handler(int fd, uint32_t mask, void *data)
{
    (void)mask; (void)data;
    unsigned char buf[64];
    long n = read(fd, buf, sizeof buf);
    if (n <= 0)
        return 0;
    if (!g_have_kbd) /* no keymap → no keyboard on the seat; notify_key would deref NULL */
        return 0;
    struct timespec ts;
    now_ts(&ts);
    for (long i = 0; i < n; i++) {
        uint32_t key = buf[i] & 0x7f;
        enum wl_keyboard_key_state st = (buf[i] & 0x80)
            ? WL_KEYBOARD_KEY_STATE_RELEASED : WL_KEYBOARD_KEY_STATE_PRESSED;
        notify_key(&oxbow_seat, &ts, key, st, STATE_UPDATE_AUTOMATIC);
    }
    return 0;
}

static int mouse_handler(int fd, uint32_t mask, void *data)
{
    (void)mask; (void)data;
    unsigned char buf[768];
    long n = read(fd, buf, sizeof buf);
    if (n <= 0)
        return 0;
    struct timespec ts;
    now_ts(&ts);
    int moved = 0;
    for (long i = 0; i < n; i++) {
        unsigned char b = buf[i];
        /* PS/2 resync: byte 0 of every packet has bit 3 set. If the stream has
         * desynced (a dropped/partial byte, or the first read starting mid-packet),
         * drop bytes until a valid packet start — otherwise every 3-byte group is
         * misframed and the cursor teleports ("jumpy"), permanently. */
        if (g_mpi == 0 && !(b & 0x08))
            continue;
        g_mpkt[g_mpi++] = b;
        if (g_mpi < 3)
            continue;
        g_mpi = 0;
        int flags = g_mpkt[0];
        /* Overflow packets (X/Y overflow bits): the delta is saturated garbage — drop. */
        if (flags & 0xC0)
            continue;
        int dx = g_mpkt[1] - ((flags & 0x10) ? 256 : 0);
        int dy = g_mpkt[2] - ((flags & 0x20) ? 256 : 0);
        if (dx || dy) {
            g_cx += dx;
            g_cy -= dy; /* PS/2 Y is up, screen Y is down */
            if (g_cx < 0) g_cx = 0;
            if (g_cx > oxbow_fb_w) g_cx = oxbow_fb_w;
            if (g_cy < 0) g_cy = 0;
            if (g_cy > oxbow_fb_h) g_cy = oxbow_fb_h;
            moved = 1;
        }
        int left = flags & 0x01;
        if (left != g_btn_left) {
            g_btn_left = left;
            notify_button(&oxbow_seat, &ts, BTN_LEFT,
                          left ? WL_POINTER_BUTTON_STATE_PRESSED
                               : WL_POINTER_BUTTON_STATE_RELEASED);
        }
    }
    if (moved) {
        notify_motion_absolute(&oxbow_seat, &ts, g_cx, g_cy);
        /* Move the cursor with a direct fb blit instead of a full compositor repaint —
         * moving an 11x17 sprite must not re-composite every surface (that was the lag). */
        oxbow_cursor_move((int)g_cx, (int)g_cy);
    }
    notify_pointer_frame(&oxbow_seat);
    return 0;
}

/* Create the seat (keyboard + pointer) and start reading the oxbow input channels. */
void oxbow_input_init(struct weston_compositor *compositor)
{
    struct wl_event_loop *loop = wl_display_get_event_loop(compositor->wl_display);

    weston_seat_init(&oxbow_seat, compositor, "oxbow-seat");

    /* Keyboard: compile oxbow's self-contained US xkb keymap. Use NO_DEFAULT_INCLUDES so
     * xkb_context_new doesn't try (and fail) to add /usr/share/X11/xkb — that failure left
     * the context unable to compile even a self-contained keymap. */
    if (!compositor->xkb_context)
        compositor->xkb_context = xkb_context_new(XKB_CONTEXT_NO_DEFAULT_INCLUDES);
    struct xkb_keymap *keymap = compositor->xkb_context
        ? xkb_keymap_new_from_string(compositor->xkb_context, us_keymap,
                                     XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS)
        : NULL;
    if (keymap) {
        if (weston_seat_init_keyboard(&oxbow_seat, keymap) == 0)
            g_have_kbd = 1;
        else
            weston_log("oxbow: keyboard init failed\n");
        xkb_keymap_unref(keymap); /* the seat took its own ref */
    } else {
        weston_log("oxbow: US keymap compile failed; keyboard disabled\n");
    }

    weston_seat_init_pointer(&oxbow_seat);
    g_cx = oxbow_fb_w / 2.0;
    g_cy = oxbow_fb_h / 2.0;

    int kfd = ox_chan_fd(BOOT_INPUT_CHAN);
    int mfd = ox_chan_fd(BOOT_MOUSE_CHAN);
    if (kfd >= 0)
        wl_event_loop_add_fd(loop, kfd, WL_EVENT_READABLE, kbd_handler, NULL);
    if (mfd >= 0)
        wl_event_loop_add_fd(loop, mfd, WL_EVENT_READABLE, mouse_handler, NULL);
    weston_log("oxbow: input wired — keyboard %s, pointer on\n",
               g_have_kbd ? "on" : "DISABLED (keymap)");
}
