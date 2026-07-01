/* oxbow-shell.c — P4/P5: a minimal Weston shell so real clients can map windows.
 *
 * Uses libweston-desktop (weston's xdg-shell / wl-shell implementation) and implements
 * the tiny weston_desktop_api a shell must provide: when a client creates a toplevel we
 * make a view for it, and on its first commit we center it on screen + insert it into a
 * layer so weston composites it. This is the DE-lite that replaces oxcomp's hand-written
 * xdg-shell. See docs/weston-port.md. */
#include "config.h"

#include <stdlib.h>

#include <libweston/libweston.h>
#include <libweston/zalloc.h>
#include <libweston-desktop/libweston-desktop.h>
#include "shared/helpers.h"

extern int oxbow_fb_w, oxbow_fb_h;

/* libweston-desktop.c calls this to set up X11-window integration; we don't build
 * xwayland.c (no X clients under Weston here), so stub the hook to satisfy the link. */
struct weston_desktop;
void weston_desktop_xwayland_init(struct weston_desktop *desktop) { (void)desktop; }

struct oxbow_shell {
    struct weston_compositor *compositor;
    struct weston_desktop *desktop;
    struct weston_layer layer; /* toplevel views live here */
};

static void surface_added(struct weston_desktop_surface *ds, void *data)
{
    struct oxbow_shell *shell = data;
    struct weston_view *view = weston_desktop_surface_create_view(ds);
    if (!view)
        return;
    weston_desktop_surface_set_user_data(ds, view);
    weston_desktop_surface_set_activated(ds, true);
    struct weston_seat *seat;
    wl_list_for_each(seat, &shell->compositor->seat_list, link)
        weston_view_activate(view, seat, 0);
    weston_log("oxbow-shell: toplevel added\n");
}

static void surface_removed(struct weston_desktop_surface *ds, void *data)
{
    struct weston_view *view = weston_desktop_surface_get_user_data(ds);
    (void)data;
    if (view) {
        weston_desktop_surface_unlink_view(view);
        weston_view_destroy(view);
    }
    weston_desktop_surface_set_user_data(ds, NULL);
}

static void committed(struct weston_desktop_surface *ds, int32_t sx, int32_t sy, void *data)
{
    struct oxbow_shell *shell = data;
    struct weston_view *view = weston_desktop_surface_get_user_data(ds);
    struct weston_surface *surface = weston_desktop_surface_get_surface(ds);
    (void)sx; (void)sy;
    if (!view || surface->width == 0)
        return;

    if (!weston_surface_is_mapped(surface)) {
        /* Tile: stagger each new toplevel so multiple windows don't stack on center. */
        static int n_mapped;
        int x = 80 + (n_mapped % 3) * 360;
        int y = 80 + (n_mapped % 2) * 320;
        n_mapped++;
        if (x + surface->width > oxbow_fb_w) x = oxbow_fb_w - surface->width;
        if (y + surface->height > oxbow_fb_h) y = oxbow_fb_h - surface->height;
        if (x < 0) x = 0;
        if (y < 0) y = 0;
        weston_view_set_position(view, x, y);
        weston_view_update_transform(view);
        weston_layer_entry_insert(&shell->layer.view_list, &view->layer_link);
        view->is_mapped = true;
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
    weston_layer_set_position(&shell->layer, WESTON_LAYER_POSITION_NORMAL);
    shell->desktop = weston_desktop_create(compositor, &shell_api, shell);
    if (!shell->desktop) {
        weston_log("oxbow-shell: weston_desktop_create failed\n");
        return;
    }
    weston_log("oxbow-shell: up (xdg-shell via libweston-desktop)\n");
}
