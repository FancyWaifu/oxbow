/* oxbow-shell.c — P4/P5: a minimal Weston shell so real clients can map windows.
 *
 * Uses libweston-desktop (weston's xdg-shell / wl-shell implementation) and implements
 * the tiny weston_desktop_api a shell must provide: when a client creates a toplevel we
 * make a view for it, and on its first commit we center it on screen + insert it into a
 * layer so weston composites it. This is the DE-lite that replaces oxcomp's hand-written
 * xdg-shell. See docs/weston-port.md.
 *
 * Also paints a solid desktop background (a color surface at the BACKGROUND layer) and a
 * simple frame behind each window (a color surface a few px larger, with a taller strip on
 * top for a title-bar look) so windows are visible against the desktop instead of black-on-
 * black. Backgrounds/frames are solid-color surfaces composited by weston — they work with
 * use_shadow (unlike a one-time framebuffer clear, which the shadow copy overwrites). */
#include "config.h"

#include <stdlib.h>

#include <libweston/libweston.h>
#include <libweston/zalloc.h>
#include <libweston-desktop/libweston-desktop.h>
#include "shared/helpers.h"

extern int oxbow_fb_w, oxbow_fb_h;

#define FRAME_BORDER 3   /* px of frame peeking out on left/right/bottom */
#define FRAME_TITLE 22   /* px of frame above the window (title-bar strip) */

/* libweston-desktop.c calls this to set up X11-window integration; we don't build
 * xwayland.c (no X clients under Weston here), so stub the hook to satisfy the link. */
struct weston_desktop;
void weston_desktop_xwayland_init(struct weston_desktop *desktop) { (void)desktop; }

struct oxbow_shell {
    struct weston_compositor *compositor;
    struct weston_desktop *desktop;
    struct weston_layer layer;            /* toplevel views + their frames live here */
    struct weston_layer background_layer;  /* the desktop background surface */
};

/* Per-toplevel state: the client's view + the frame surface drawn behind it. */
struct oxwin {
    struct weston_view *view;
    struct weston_surface *frame_surf;
    struct weston_view *frame_view;
};

/* Create a solid-color surface + view of size w×h at (x,y), inserted into `layer`.
 * Decorative (no input region) so it never steals pointer/keyboard focus from windows. */
static struct weston_view *solid_view(struct weston_compositor *ec, struct weston_layer *layer,
                                      int x, int y, int w, int h,
                                      float r, float g, float b, struct weston_surface **out_surf)
{
    struct weston_surface *s = weston_surface_create(ec);
    if (!s)
        return NULL;
    struct weston_view *v = weston_view_create(s);
    if (!v) {
        weston_surface_destroy(s);
        return NULL;
    }
    weston_surface_set_color(s, r, g, b, 1.0f);
    pixman_region32_fini(&s->opaque);
    pixman_region32_init_rect(&s->opaque, 0, 0, w, h);
    pixman_region32_fini(&s->input);
    pixman_region32_init_rect(&s->input, 0, 0, 0, 0); /* no input — decorative only */
    weston_surface_set_size(s, w, h);
    weston_view_set_position(v, x, y);
    weston_layer_entry_insert(&layer->view_list, &v->layer_link);
    weston_view_update_transform(v);
    v->is_mapped = true;
    s->is_mapped = true;
    if (out_surf)
        *out_surf = s;
    return v;
}

static void surface_added(struct weston_desktop_surface *ds, void *data)
{
    struct oxbow_shell *shell = data;
    struct oxwin *w = zalloc(sizeof *w);
    if (!w)
        return;
    w->view = weston_desktop_surface_create_view(ds);
    if (!w->view) {
        free(w);
        return;
    }
    weston_desktop_surface_set_user_data(ds, w);
    weston_desktop_surface_set_activated(ds, true);
    struct weston_seat *seat;
    wl_list_for_each(seat, &shell->compositor->seat_list, link)
        weston_view_activate(w->view, seat, 0);
    weston_log("oxbow-shell: toplevel added\n");
}

static void surface_removed(struct weston_desktop_surface *ds, void *data)
{
    struct oxwin *w = weston_desktop_surface_get_user_data(ds);
    (void)data;
    if (w) {
        if (w->frame_view)
            weston_view_destroy(w->frame_view);
        if (w->frame_surf)
            weston_surface_destroy(w->frame_surf);
        if (w->view) {
            weston_desktop_surface_unlink_view(w->view);
            weston_view_destroy(w->view);
        }
        free(w);
    }
    weston_desktop_surface_set_user_data(ds, NULL);
}

static void committed(struct weston_desktop_surface *ds, int32_t sx, int32_t sy, void *data)
{
    struct oxbow_shell *shell = data;
    struct oxwin *w = weston_desktop_surface_get_user_data(ds);
    struct weston_surface *surface = weston_desktop_surface_get_surface(ds);
    (void)sx; (void)sy;
    if (!w || !w->view || surface->width == 0)
        return;

    if (!weston_surface_is_mapped(surface)) {
        /* Tile: stagger each new toplevel so multiple windows don't stack on center. */
        static int n_mapped;
        int x = 80 + (n_mapped % 3) * 360;
        int y = 80 + (n_mapped % 2) * 320;
        n_mapped++;
        if (x + surface->width > oxbow_fb_w) x = oxbow_fb_w - surface->width;
        if (y + surface->height > oxbow_fb_h) y = oxbow_fb_h - surface->height;
        if (x < FRAME_BORDER) x = FRAME_BORDER;
        if (y < FRAME_TITLE) y = FRAME_TITLE; /* leave room for the title-bar strip */

        /* Frame first (a color surface a bit larger than the window), then the window on
         * top — insert order sets z (last inserted = topmost), so the border peeks out. */
        w->frame_view = solid_view(shell->compositor, &shell->layer,
                                   x - FRAME_BORDER, y - FRAME_TITLE,
                                   surface->width + 2 * FRAME_BORDER,
                                   surface->height + FRAME_TITLE + FRAME_BORDER,
                                   0.25f, 0.35f, 0.5f, &w->frame_surf);

        weston_view_set_position(w->view, x, y);
        weston_view_update_transform(w->view);
        weston_layer_entry_insert(&shell->layer.view_list, &w->view->layer_link);
        w->view->is_mapped = true;
        surface->is_mapped = true;
        weston_compositor_schedule_repaint(shell->compositor);
        weston_log("oxbow-shell: toplevel mapped %dx%d at %d,%d\n",
                   surface->width, surface->height, x, y);
    }
}

static const struct weston_desktop_api shell_api = {
    .struct_size = sizeof(struct weston_desktop_api),
    .surface_added = surface_added,
    .surface_removed = surface_removed,
    .committed = committed,
};

void oxbow_shell_init(struct weston_compositor *compositor)
{
    struct oxbow_shell *shell = zalloc(sizeof *shell);
    if (!shell)
        return;
    shell->compositor = compositor;
    weston_layer_init(&shell->layer, compositor);
    weston_layer_init(&shell->background_layer, compositor);
    weston_layer_set_position(&shell->layer, WESTON_LAYER_POSITION_NORMAL);
    weston_layer_set_position(&shell->background_layer, WESTON_LAYER_POSITION_BACKGROUND);

    /* Solid desktop background (dark navy) covering the whole output. */
    solid_view(compositor, &shell->background_layer, 0, 0, oxbow_fb_w, oxbow_fb_h,
               0.106f, 0.165f, 0.29f, NULL);

    shell->desktop = weston_desktop_create(compositor, &shell_api, shell);
    if (!shell->desktop) {
        weston_log("oxbow-shell: weston_desktop_create failed\n");
        return;
    }
    weston_log("oxbow-shell: up (xdg-shell via libweston-desktop)\n");
}
