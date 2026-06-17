/* comp_server.c — the compositor half. Advertises wl_compositor + wl_shm, and on
 * a wl_surface.commit copies the attached shm buffer's pixels into the
 * framebuffer. Separate translation unit (server headers) from the client. */
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>   /* read(), ftruncate(), close() */
#include <sys/mman.h> /* mmap for staging the keymap into a memfd */
#include "wayland-server.h"
#include "wayland-server-protocol.h"
#include "xdg-shell-server-protocol.h"
#include "../../oxxkb/xkb/us_keymap.h" /* the US keymap we hand clients (§48) */

extern int memfd_create(const char *name, unsigned int flags);

extern void ox_log(const char *p, unsigned long len);
/* Milliseconds since boot — the frame-callback timestamp clients animate from. */
extern unsigned int ox_now_ms(void);
static void slog(const char *s)
{
  unsigned long n = 0;
  while (s[n])
    n++;
  ox_log(s, n);
}

static unsigned int *g_fb;   /* the live scanout framebuffer */
static unsigned int *g_back; /* §58: offscreen back buffer (double-buffering) */
static int           g_w, g_h, g_pitch_words;

/* ---- software cursor (§54) ---------------------------------------------- */
#define CURW 11
#define CURH 17
/* A classic top-left arrow: 'X' = black outline, '.' = white fill, ' ' = clear. */
static const char *const cursor_bits[CURH] = {
  "X          ", "XX         ", "X.X        ", "X..X       ",
  "X...X      ", "X....X     ", "X.....X    ", "X......X   ",
  "X.......X  ", "X........X ", "X.....XXXXX", "X..X..X    ",
  "X.X X..X   ", "XX  X..X   ", "X    X..X  ", "     X..X  ",
  "      XX   ",
};
static int g_cx = 200, g_cy = 200; /* logical cursor position */

static unsigned int        g_serial;    /* event serial counter */
static int           g_composited;
static int g_btn_left; /* last reported left-button state (edge detection) */

struct surf {
  struct wl_resource *buffer;       /* pending/current attached wl_buffer */
  struct wl_resource *surface;      /* the wl_surface resource itself */
  struct wl_resource *xdg_surface;  /* the xdg_surface role object, if any */
  struct wl_resource *xdg_toplevel; /* the xdg_toplevel, if any */
  struct wl_resource *frame_cb;     /* pending wl_callback from wl_surface.frame */
  int                 configured;   /* have we sent the initial configure? */
  /* §56 multi-window: on-screen geometry + a backing copy of the last frame, so
   * the whole scene can be re-composited in z-order when any window changes. */
  int                 x, y, w, h, mapped;
  unsigned int       *backing;
  long                backing_cap;
};

/* The scene: views ordered bottom→top (last = topmost/focused). */
#define MAXVIEWS 8
static struct surf *g_views[MAXVIEWS];
static int          g_nviews;

static void views_remove(struct surf *s)
{
  int j = 0;
  for (int i = 0; i < g_nviews; i++)
    if (g_views[i] != s)
      g_views[j++] = g_views[i];
  g_nviews = j;
}
/* Raise `s` to the top of the z-order (focus). */
static void views_raise(struct surf *s)
{
  views_remove(s);
  if (g_nviews < MAXVIEWS)
    g_views[g_nviews++] = s;
}

/* Per-client seat resources — several clients each bind the seat (§56). */
#define MAXSEATS 8
struct seatc {
  struct wl_client   *client;
  struct wl_resource *kbd, *ptr;
};
static struct seatc g_seats[MAXSEATS];
static int          g_nseats;
static struct seatc *seat_for(struct wl_client *c)
{
  for (int i = 0; i < g_nseats; i++)
    if (g_seats[i].client == c)
      return &g_seats[i];
  if (g_nseats < MAXSEATS) {
    g_seats[g_nseats].client = c;
    g_seats[g_nseats].kbd = NULL;
    g_seats[g_nseats].ptr = NULL;
    return &g_seats[g_nseats++];
  }
  return NULL;
}
static struct surf *g_focus_view; /* topmost view = keyboard focus */
static struct surf *g_ptr_view;   /* view currently under the pointer */

/* §57 window management: a titlebar above each window, and a cursor-mode state
 * machine (tinywl) for interactive move. */
#define TBH 22 /* titlebar height in px */
enum { MODE_PASSTHROUGH, MODE_MOVE };
static int          g_cursor_mode;
static struct surf *g_grab;        /* view being dragged */
static int          g_grab_dx, g_grab_dy; /* cursor offset within the window */

/* Draw view `s`'s titlebar (above its content): a bar — brighter when focused —
 * with a red close box at the right end. (Drawn inline + clipped in composite_rect.)
 *
 * §59 damage tracking: recomposite + flip ONLY the rectangle [x0,y0)×[x1,y1) that
 * actually changed, instead of the whole 1280×800 screen every frame. Renders the
 * background, every view's titlebar+content clipped to the rect, and the cursor,
 * into the back buffer, then flips just those rows to the framebuffer. The hot
 * paths (a client's animation frame, cursor motion) damage only a small area, so
 * the per-frame cost is bounded by the window/cursor size, not the screen. */
static void composite_rect(int x0, int y0, int x1, int y1)
{
  if (!g_back)
    return;
  if (x0 < 0) x0 = 0;
  if (y0 < 0) y0 = 0;
  if (x1 > g_w) x1 = g_w;
  if (y1 > g_h) y1 = g_h;
  if (x0 >= x1 || y0 >= y1)
    return;
  for (int y = y0; y < y1; y++)
    for (int x = x0; x < x1; x++)
      g_back[(long)y * g_pitch_words + x] = 0x000d3b45u; /* desktop bg */
  for (int v = 0; v < g_nviews; v++) {
    struct surf *s = g_views[v];
    if (!s->mapped || !s->backing)
      continue;
    unsigned int bar = (s == g_focus_view) ? 0x003a6ea5u : 0x00444444u;
    /* titlebar rows [s->y-TBH, s->y) clipped to the damage rect */
    int b0 = (s->y - TBH > y0) ? s->y - TBH : y0;
    int b1 = (s->y < y1) ? s->y : y1;
    int a0 = (s->x > x0) ? s->x : x0;
    int a1 = (s->x + s->w < x1) ? s->x + s->w : x1;
    for (int y = b0; y < b1; y++) {
      int j = y - (s->y - TBH);
      for (int x = a0; x < a1; x++) {
        int i = x - s->x;
        int in_close = i >= s->w - TBH + 4 && i < s->w - 4 && j >= 4 && j < TBH - 4;
        g_back[(long)y * g_pitch_words + x] = in_close ? 0x00c04040u : bar;
      }
    }
    /* content rows [s->y, s->y+s->h) clipped */
    int cy0 = (s->y > y0) ? s->y : y0;
    int cy1 = (s->y + s->h < y1) ? s->y + s->h : y1;
    for (int y = cy0; y < cy1; y++)
      for (int x = a0; x < a1; x++)
        g_back[(long)y * g_pitch_words + x] = s->backing[(long)(y - s->y) * s->w + (x - s->x)];
  }
  /* cursor on top, clipped */
  for (int j = 0; j < CURH; j++)
    for (int i = 0; i < CURW; i++) {
      char c = cursor_bits[j][i];
      if (c == ' ')
        continue;
      int x = g_cx + i, y = g_cy + j;
      if (x < x0 || x >= x1 || y < y0 || y >= y1)
        continue;
      g_back[(long)y * g_pitch_words + x] = (c == 'X') ? 0u : 0x00ffffffu;
    }
  /* flip only the damaged rows. */
  for (int y = y0; y < y1; y++)
    memcpy(g_fb + (long)y * g_pitch_words + x0, g_back + (long)y * g_pitch_words + x0,
           (size_t)(x1 - x0) * 4);
}
static void composite_scene(void) { composite_rect(0, 0, g_w, g_h); }

/* §61 occlusion culling — the wlroots/Weston technique. When a window repaints
 * (e.g. an animation frame), the parts of it hidden behind opaque windows stacked
 * above are wasted work: we'd paint them, then overpaint them. Real compositors
 * subtract the opaque regions of higher views (pixman region difference) and only
 * repaint the visible remainder. Our renderer copies pixels and never blends, so
 * every mapped view is opaque and fully occludes whatever it covers — culling is
 * exactly consistent with what reaches the screen.
 *
 * Rectangle subtraction: rect R minus occluder O yields up to four strips (top,
 * bottom, left, right) of R that O does not cover. We subtract each higher view in
 * turn, carrying a small list of surviving rects. */
#define MAXVISRECTS 64
static int rect_subtract(const int r[4], int ox0, int oy0, int ox1, int oy1,
                         int out[][4], int n)
{
  int cx0 = ox0 > r[0] ? ox0 : r[0], cy0 = oy0 > r[1] ? oy0 : r[1];
  int cx1 = ox1 < r[2] ? ox1 : r[2], cy1 = oy1 < r[3] ? oy1 : r[3];
  if (cx0 >= cx1 || cy0 >= cy1) { /* no overlap → R survives whole */
    if (n < MAXVISRECTS) { out[n][0]=r[0]; out[n][1]=r[1]; out[n][2]=r[2]; out[n][3]=r[3]; n++; }
    return n;
  }
  if (r[1] < cy0 && n < MAXVISRECTS) { out[n][0]=r[0]; out[n][1]=r[1]; out[n][2]=r[2]; out[n][3]=cy0; n++; } /* top */
  if (cy1 < r[3] && n < MAXVISRECTS) { out[n][0]=r[0]; out[n][1]=cy1; out[n][2]=r[2]; out[n][3]=r[3]; n++; } /* bottom */
  if (r[0] < cx0 && n < MAXVISRECTS) { out[n][0]=r[0]; out[n][1]=cy0; out[n][2]=cx0; out[n][3]=cy1; n++; } /* left */
  if (cx1 < r[2] && n < MAXVISRECTS) { out[n][0]=cx1; out[n][1]=cy0; out[n][2]=r[2]; out[n][3]=cy1; n++; } /* right */
  return n;
}

/* Composite the damage rect for view `s`, skipping areas occluded by mapped views
 * stacked above it. Falls back to a plain composite_rect on rect-list overflow. */
static void composite_occluded(struct surf *s, int x0, int y0, int x1, int y1)
{
  if (x0 < 0) x0 = 0; if (y0 < 0) y0 = 0; if (x1 > g_w) x1 = g_w; if (y1 > g_h) y1 = g_h;
  if (x0 >= x1 || y0 >= y1) return;
  int si = -1;
  for (int v = 0; v < g_nviews; v++) if (g_views[v] == s) { si = v; break; }
  int vis[MAXVISRECTS][4];
  vis[0][0]=x0; vis[0][1]=y0; vis[0][2]=x1; vis[0][3]=y1;
  int nv = 1;
  for (int v = si + 1; v < g_nviews; v++) {
    struct surf *o = g_views[v];
    if (!o->mapped || !o->backing) continue;
    int next[MAXVISRECTS][4], nn = 0;
    for (int r = 0; r < nv; r++)
      nn = rect_subtract(vis[r], o->x, o->y - TBH, o->x + o->w, o->y + o->h, next, nn);
    if (nn >= MAXVISRECTS) { composite_rect(x0, y0, x1, y1); return; } /* overflow: be correct */
    memcpy(vis, next, (size_t)nn * 4 * sizeof(int));
    nv = nn;
    if (nv == 0) return; /* fully hidden — nothing reaches the screen */
  }
  for (int r = 0; r < nv; r++) composite_rect(vis[r][0], vis[r][1], vis[r][2], vis[r][3]);
}

/* §62: is view `s`'s CONTENT area entirely hidden behind opaque views above it?
 * (The titlebar is compositor-drawn and static, so it doesn't matter here — only
 * the client's own pixels decide whether the client needs to keep drawing.) Same
 * rect-subtraction as composite_occluded, on the content rect; nothing surviving
 * ⟹ fully occluded. */
static int content_fully_occluded(struct surf *s)
{
  int si = -1;
  for (int v = 0; v < g_nviews; v++) if (g_views[v] == s) { si = v; break; }
  if (si < 0) return 0;
  int vis[MAXVISRECTS][4];
  vis[0][0]=s->x; vis[0][1]=s->y; vis[0][2]=s->x + s->w; vis[0][3]=s->y + s->h;
  int nv = 1;
  for (int v = si + 1; v < g_nviews; v++) {
    struct surf *o = g_views[v];
    if (!o->mapped || !o->backing) continue;
    int next[MAXVISRECTS][4], nn = 0;
    for (int r = 0; r < nv; r++)
      nn = rect_subtract(vis[r], o->x, o->y - TBH, o->x + o->w, o->y + o->h, next, nn);
    if (nn >= MAXVISRECTS) return 0; /* overflow → assume visible (keep animating) */
    memcpy(vis, next, (size_t)nn * 4 * sizeof(int));
    nv = nn;
    if (nv == 0) return 1; /* every piece consumed → fully hidden */
  }
  return 0;
}

/* §62: a fully-occluded window is held with its frame callback PENDING (§51 sends
 * it on commit only when visible), so its client blocks and stops animating — no
 * CPU for frames nobody sees. When the scene changes (a window above moves, closes,
 * or this one is raised) and it becomes even partially visible, deliver the held
 * callback so the client resumes. Cheap: ≤8 views, a few rect ops each. */
static void wake_unoccluded(void)
{
  for (int v = 0; v < g_nviews; v++) {
    struct surf *s = g_views[v];
    if (s->mapped && s->frame_cb && !content_fully_occluded(s)) {
      wl_callback_send_done(s->frame_cb, ox_now_ms());
      wl_resource_destroy(s->frame_cb);
      s->frame_cb = NULL;
    }
  }
}

/* ---- wl_surface ---------------------------------------------------------- */
static void surface_destroy(struct wl_client *c, struct wl_resource *res)
{
  (void)c;
  wl_resource_destroy(res);
}
static void surface_attach(struct wl_client *c, struct wl_resource *res,
                           struct wl_resource *buffer, int32_t x, int32_t y)
{
  (void)c;
  (void)x;
  (void)y;
  struct surf *s = wl_resource_get_user_data(res);
  s->buffer = buffer;
}
static void surface_damage(struct wl_client *c, struct wl_resource *res,
                           int32_t x, int32_t y, int32_t w, int32_t h)
{
  (void)c; (void)res; (void)x; (void)y; (void)w; (void)h;
}
static void surface_frame(struct wl_client *c, struct wl_resource *res, uint32_t cb)
{
  struct surf *s = wl_resource_get_user_data(res);
  /* The client asks to be told when it may draw the next frame. Create the
   * wl_callback now; we fire its `done` right after we composite this surface,
   * which drives the client's redraw loop = animation. */
  s->frame_cb = wl_resource_create(c, &wl_callback_interface, 1, cb);
}
static void surface_set_opaque_region(struct wl_client *c, struct wl_resource *res,
                                      struct wl_resource *region)
{
  (void)c; (void)res; (void)region;
}
static void surface_set_input_region(struct wl_client *c, struct wl_resource *res,
                                     struct wl_resource *region)
{
  (void)c; (void)res; (void)region;
}
static void surface_commit(struct wl_client *c, struct wl_resource *res)
{
  (void)c;
  struct surf *s = wl_resource_get_user_data(res);
  /* xdg_shell handshake: the client's initial commit (no buffer) asks us to
   * configure it. Reply with a configure; the client acks, draws, and commits
   * again with a buffer. */
  if (s->xdg_surface && s->xdg_toplevel && !s->configured) {
    struct wl_array states;
    wl_array_init(&states);
    xdg_toplevel_send_configure(s->xdg_toplevel, 0, 0, &states);
    wl_array_release(&states);
    xdg_surface_send_configure(s->xdg_surface, 1);
    s->configured = 1;
    return;
  }
  if (!s->buffer)
    return;
  struct wl_shm_buffer *shm = wl_shm_buffer_get(s->buffer);
  if (!shm) {
    slog("[oxcomp/srv] commit: buffer is not wl_shm\n");
    return;
  }
  int bw     = wl_shm_buffer_get_width(shm);
  int bh     = wl_shm_buffer_get_height(shm);
  int stride = wl_shm_buffer_get_stride(shm);
  /* §56: copy the frame into this view's backing store so the whole scene can be
   * recomposited in z-order (windows may overlap). */
  long need = (long)bw * bh * 4;
  if (s->backing_cap < need) {
    free(s->backing);
    s->backing = malloc(need);
    s->backing_cap = s->backing ? need : 0;
  }
  s->w = bw;
  s->h = bh;
  wl_shm_buffer_begin_access(shm);
  unsigned char *data = wl_shm_buffer_get_data(shm);
  if (s->backing)
    for (int y = 0; y < bh; y++)
      memcpy(s->backing + (long)y * bw, data + (long)y * stride, (size_t)bw * 4);
  wl_shm_buffer_end_access(shm);
  if (!s->mapped) {
    /* §67: place each window near a different screen anchor (top-left, top-right,
     * bottom-left, bottom-right, then center) instead of a tight cascade — so a new
     * window isn't buried under a previous large one. g_nviews is the count of
     * already-mapped windows, so it is this window's anchor slot. A per-cycle jitter
     * keeps a 6th+ window off an exact repeat. Everything is clamped on-screen. */
    const int M = 40; /* screen-edge margin */
    int slot = g_nviews;
    int j = (slot / 5) * 32; /* jitter once we wrap past the 5 anchors */
    switch (slot % 5) {
      case 0: s->x = M + j;                  s->y = TBH + M + j; break;             /* TL */
      case 1: s->x = g_w - s->w - M - j;     s->y = TBH + M + j; break;             /* TR */
      case 2: s->x = M + j;                  s->y = g_h - s->h - M - j; break;      /* BL */
      case 3: s->x = g_w - s->w - M - j;     s->y = g_h - s->h - M - j; break;      /* BR */
      default: s->x = (g_w - s->w) / 2;      s->y = (g_h - s->h) / 2; break;        /* center */
    }
    /* keep the whole window (titlebar included) on-screen */
    if (s->x < 0) s->x = 0;
    if (s->x + s->w > g_w) s->x = g_w - s->w;
    if (s->y - TBH < 0) s->y = TBH;
    if (s->y + s->h > g_h) s->y = g_h - s->h;
    s->mapped = 1;
    views_raise(s);
    g_focus_view = s;
    struct seatc *sc = seat_for(c);
    if (sc && sc->kbd) {
      struct wl_array keys;
      wl_array_init(&keys);
      wl_keyboard_send_enter(sc->kbd, ++g_serial, res, &keys);
      wl_array_release(&keys);
    }
    composite_scene(); /* new window: place + focus → full recomposite */
  } else {
    /* §59: an animation frame only changed THIS window — damage just its area.
     * §61: and skip the parts hidden behind windows above, so a window animating
     * behind an opaque one costs only its VISIBLE area, not its full size. */
    composite_occluded(s, s->x, s->y - TBH, s->x + s->w, s->y + s->h);
  }
  g_composited = 1;
  wl_buffer_send_release(s->buffer); /* client may reuse the buffer */
  /* Tell the client this frame is on screen and it may draw the next one. Its
   * frame-callback handler redraws + commits again → the surface animates.
   * §62: but if this window is now FULLY hidden behind opaque windows, withhold
   * the callback — the client blocks, the hidden animation pauses (no wasted CPU),
   * and wake_unoccluded() re-delivers it when the window becomes visible again. */
  if (s->frame_cb && !content_fully_occluded(s)) {
    wl_callback_send_done(s->frame_cb, ox_now_ms());
    wl_resource_destroy(s->frame_cb);
    s->frame_cb = NULL;
  }
}
static void surface_set_buffer_transform(struct wl_client *c, struct wl_resource *r, int32_t t)
{
  (void)c; (void)r; (void)t;
}
static void surface_set_buffer_scale(struct wl_client *c, struct wl_resource *r, int32_t s)
{
  (void)c; (void)r; (void)s;
}
static void surface_damage_buffer(struct wl_client *c, struct wl_resource *r,
                                  int32_t x, int32_t y, int32_t w, int32_t h)
{
  (void)c; (void)r; (void)x; (void)y; (void)w; (void)h;
}
static void surface_offset(struct wl_client *c, struct wl_resource *r, int32_t x, int32_t y)
{
  (void)c; (void)r; (void)x; (void)y;
}
static const struct wl_surface_interface surface_impl = {
  surface_destroy, surface_attach, surface_damage, surface_frame,
  surface_set_opaque_region, surface_set_input_region, surface_commit,
  surface_set_buffer_transform, surface_set_buffer_scale, surface_damage_buffer,
  surface_offset,
};

static void surface_resource_destroy(struct wl_resource *res)
{
  struct surf *s = wl_resource_get_user_data(res);
  if (s) {
    views_remove(s);
    if (g_focus_view == s)
      g_focus_view = NULL;
    if (g_ptr_view == s)
      g_ptr_view = NULL;
    free(s->backing);
    free(s);
  }
  composite_scene();
  wake_unoccluded(); /* §62: closing a window uncovers anything it was hiding */
}

/* ---- wl_region (no-op; we don't clip) ------------------------------------ */
static void region_destroy(struct wl_client *c, struct wl_resource *r)
{
  (void)c;
  wl_resource_destroy(r);
}
static void region_add(struct wl_client *c, struct wl_resource *r,
                       int32_t x, int32_t y, int32_t w, int32_t h)
{
  (void)c; (void)r; (void)x; (void)y; (void)w; (void)h;
}
static void region_subtract(struct wl_client *c, struct wl_resource *r,
                            int32_t x, int32_t y, int32_t w, int32_t h)
{
  (void)c; (void)r; (void)x; (void)y; (void)w; (void)h;
}
static const struct wl_region_interface region_impl = {
  region_destroy, region_add, region_subtract
};

/* ---- xdg_shell: the standard window protocol (so real apps map) ---------- */
static void noop_destroy(struct wl_client *c, struct wl_resource *r)
{
  (void)c;
  wl_resource_destroy(r);
}

/* xdg_toplevel: window properties — all no-ops for our single fixed window. */
static void tl_set_parent(struct wl_client *c, struct wl_resource *r, struct wl_resource *p)
{ (void)c; (void)r; (void)p; }
static void tl_set_title(struct wl_client *c, struct wl_resource *r, const char *t)
{ (void)c; (void)r; (void)t; }
static void tl_set_app_id(struct wl_client *c, struct wl_resource *r, const char *a)
{ (void)c; (void)r; (void)a; }
static void tl_show_window_menu(struct wl_client *c, struct wl_resource *r,
                                struct wl_resource *seat, uint32_t serial, int32_t x, int32_t y)
{ (void)c; (void)r; (void)seat; (void)serial; (void)x; (void)y; }
static void tl_move(struct wl_client *c, struct wl_resource *r, struct wl_resource *seat, uint32_t s)
{ (void)c; (void)r; (void)seat; (void)s; }
static void tl_resize(struct wl_client *c, struct wl_resource *r, struct wl_resource *seat,
                      uint32_t serial, uint32_t edges)
{ (void)c; (void)r; (void)seat; (void)serial; (void)edges; }
static void tl_set_max_size(struct wl_client *c, struct wl_resource *r, int32_t w, int32_t h)
{ (void)c; (void)r; (void)w; (void)h; }
static void tl_set_min_size(struct wl_client *c, struct wl_resource *r, int32_t w, int32_t h)
{ (void)c; (void)r; (void)w; (void)h; }
static void tl_set_maximized(struct wl_client *c, struct wl_resource *r) { (void)c; (void)r; }
static void tl_unset_maximized(struct wl_client *c, struct wl_resource *r) { (void)c; (void)r; }
static void tl_set_fullscreen(struct wl_client *c, struct wl_resource *r, struct wl_resource *o)
{ (void)c; (void)r; (void)o; }
static void tl_unset_fullscreen(struct wl_client *c, struct wl_resource *r) { (void)c; (void)r; }
static void tl_set_minimized(struct wl_client *c, struct wl_resource *r) { (void)c; (void)r; }
static const struct xdg_toplevel_interface toplevel_impl = {
  noop_destroy, tl_set_parent, tl_set_title, tl_set_app_id, tl_show_window_menu,
  tl_move, tl_resize, tl_set_max_size, tl_set_min_size, tl_set_maximized,
  tl_unset_maximized, tl_set_fullscreen, tl_unset_fullscreen, tl_set_minimized,
};

/* xdg_surface */
static void xs_get_toplevel(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct surf        *s = wl_resource_get_user_data(res);
  struct wl_resource *tl =
    wl_resource_create(c, &xdg_toplevel_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(tl, &toplevel_impl, s, NULL);
  s->xdg_toplevel = tl;
}
static void xs_get_popup(struct wl_client *c, struct wl_resource *res, uint32_t id,
                         struct wl_resource *parent, struct wl_resource *positioner)
{ (void)c; (void)res; (void)id; (void)parent; (void)positioner; }
static void xs_set_window_geometry(struct wl_client *c, struct wl_resource *r,
                                   int32_t x, int32_t y, int32_t w, int32_t h)
{ (void)c; (void)r; (void)x; (void)y; (void)w; (void)h; }
static void xs_ack_configure(struct wl_client *c, struct wl_resource *r, uint32_t serial)
{ (void)c; (void)r; (void)serial; }
static const struct xdg_surface_interface xdg_surface_impl = {
  noop_destroy, xs_get_toplevel, xs_get_popup, xs_set_window_geometry, xs_ack_configure,
};

/* xdg_positioner (popups only; minimal — never driven by a toplevel client) */
static const struct xdg_positioner_interface positioner_impl = { 0 };

/* xdg_wm_base */
static void wm_create_positioner(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct wl_resource *p =
    wl_resource_create(c, &xdg_positioner_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(p, &positioner_impl, NULL, NULL);
}
static void wm_get_xdg_surface(struct wl_client *c, struct wl_resource *res, uint32_t id,
                               struct wl_resource *surface)
{
  struct surf        *s = wl_resource_get_user_data(surface);
  struct wl_resource *xs =
    wl_resource_create(c, &xdg_surface_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(xs, &xdg_surface_impl, s, NULL);
  s->xdg_surface = xs;
}
static void wm_pong(struct wl_client *c, struct wl_resource *r, uint32_t serial)
{ (void)c; (void)r; (void)serial; }
static const struct xdg_wm_base_interface wm_base_impl = {
  noop_destroy, wm_create_positioner, wm_get_xdg_surface, wm_pong,
};
static void wm_base_bind(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
  (void)data;
  struct wl_resource *res = wl_resource_create(c, &xdg_wm_base_interface, version, id);
  wl_resource_set_implementation(res, &wm_base_impl, NULL, NULL);
}

/* ---- wl_compositor ------------------------------------------------------- */
static void compositor_create_surface(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct surf        *s = calloc(1, sizeof *s);
  struct wl_resource *sr =
    wl_resource_create(c, &wl_surface_interface, wl_resource_get_version(res), id);
  s->surface = sr;
  wl_resource_set_implementation(sr, &surface_impl, s, surface_resource_destroy);
}
static void compositor_create_region(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct wl_resource *rr =
    wl_resource_create(c, &wl_region_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(rr, &region_impl, NULL, NULL);
}
static const struct wl_compositor_interface compositor_impl = {
  compositor_create_surface, compositor_create_region
};
static void compositor_bind(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
  (void)data;
  struct wl_resource *res = wl_resource_create(c, &wl_compositor_interface, version, id);
  wl_resource_set_implementation(res, &compositor_impl, NULL, NULL);
}

/* ---- wl_seat / wl_keyboard (§47, on-screen input) ----------------------- */
static void keyboard_release(struct wl_client *c, struct wl_resource *res)
{
  (void)c;
  wl_resource_destroy(res);
}
static const struct wl_keyboard_interface keyboard_impl = { keyboard_release };
static void keyboard_resource_destroy(struct wl_resource *res)
{
  for (int i = 0; i < g_nseats; i++)
    if (g_seats[i].kbd == res)
      g_seats[i].kbd = NULL;
}
/* Hand the client our keymap (§48): stage the keymap string into a memfd and
 * send it as wl_keyboard.keymap. The client mmaps it and builds an xkb_state, so
 * it decodes keycodes → characters the standard way. */
static void send_keymap(struct wl_resource *kbd)
{
  size_t size = sizeof us_keymap; /* includes the trailing NUL */
  int    fd   = memfd_create("xkb-keymap", 0);
  if (fd < 0)
    return;
  if (ftruncate(fd, (long)size) < 0) {
    close(fd);
    return;
  }
  void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (p == MAP_FAILED) {
    close(fd);
    return;
  }
  memcpy(p, us_keymap, size);
  munmap(p, size);
  wl_keyboard_send_keymap(kbd, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, fd, (uint32_t)size);
  close(fd);
}
static void seat_get_keyboard(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct wl_resource *k =
    wl_resource_create(c, &wl_keyboard_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(k, &keyboard_impl, NULL, keyboard_resource_destroy);
  struct seatc *sc = seat_for(c);
  if (sc)
    sc->kbd = k;
  send_keymap(k);
  slog("[oxcomp/srv] wl_keyboard bound (keymap sent)\n");
}
/* ---- wl_pointer (§55) --------------------------------------------------- */
static void pointer_set_cursor(struct wl_client *c, struct wl_resource *res, uint32_t serial,
                               struct wl_resource *surface, int32_t hx, int32_t hy)
{
  (void)c; (void)res; (void)serial; (void)surface; (void)hx; (void)hy;
  /* We draw our own cursor, so ignore client cursor surfaces for now. */
}
static void pointer_release(struct wl_client *c, struct wl_resource *res)
{
  (void)c;
  wl_resource_destroy(res);
}
static const struct wl_pointer_interface pointer_impl = {
  pointer_set_cursor, pointer_release
};
static void pointer_resource_destroy(struct wl_resource *res)
{
  for (int i = 0; i < g_nseats; i++)
    if (g_seats[i].ptr == res)
      g_seats[i].ptr = NULL;
}
static void seat_get_pointer(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  struct wl_resource *p =
    wl_resource_create(c, &wl_pointer_interface, wl_resource_get_version(res), id);
  wl_resource_set_implementation(p, &pointer_impl, NULL, pointer_resource_destroy);
  struct seatc *sc = seat_for(c);
  if (sc)
    sc->ptr = p;
  slog("[oxcomp/srv] wl_pointer bound\n");
}
static void seat_get_touch(struct wl_client *c, struct wl_resource *res, uint32_t id)
{
  (void)c; (void)res; (void)id;
}
static void seat_release(struct wl_client *c, struct wl_resource *res)
{
  (void)c;
  wl_resource_destroy(res);
}
static const struct wl_seat_interface seat_impl = {
  seat_get_pointer, seat_get_keyboard, seat_get_touch, seat_release
};
static void seat_bind(struct wl_client *c, void *data, uint32_t version, uint32_t id)
{
  (void)data;
  struct wl_resource *res = wl_resource_create(c, &wl_seat_interface, version, id);
  wl_resource_set_implementation(res, &seat_impl, NULL, NULL);
  wl_seat_send_capabilities(res,
                            WL_SEAT_CAPABILITY_KEYBOARD | WL_SEAT_CAPABILITY_POINTER);
}

/* The topmost mapped view containing (px,py), or NULL. */
static struct surf *view_at(int px, int py)
{
  for (int v = g_nviews - 1; v >= 0; v--) {
    struct surf *s = g_views[v];
    if (s->mapped && px >= s->x && px < s->x + s->w && py >= s->y && py < s->y + s->h)
      return s;
  }
  return NULL;
}
static struct wl_resource *ptr_of(struct surf *s)
{
  if (!s || !s->surface)
    return NULL;
  struct seatc *sc = seat_for(wl_resource_get_client(s->surface));
  return sc ? sc->ptr : NULL;
}

/* §55/§56: route pointer motion to the topmost view under the cursor — enter on
 * transition (tinywl process_cursor_motion), leave the previous, motion inside. */
static void pointer_update(void)
{
  struct surf *target = view_at(g_cx, g_cy);
  if (target != g_ptr_view) {
    struct wl_resource *op = ptr_of(g_ptr_view);
    if (op && g_ptr_view && g_ptr_view->surface)
      wl_pointer_send_leave(op, ++g_serial, g_ptr_view->surface);
    struct wl_resource *np = ptr_of(target);
    if (np)
      wl_pointer_send_enter(np, ++g_serial, target->surface,
                            wl_fixed_from_int(g_cx - target->x),
                            wl_fixed_from_int(g_cy - target->y));
    g_ptr_view = target;
  }
  struct wl_resource *p = ptr_of(target);
  if (p)
    wl_pointer_send_motion(p, ox_now_ms(), wl_fixed_from_int(g_cx - target->x),
                           wl_fixed_from_int(g_cy - target->y));
}

/* Click-to-focus + raise (tinywl focus_view): give the clicked window keyboard
 * focus and raise it, then forward the button. */
static void focus_view(struct surf *s);
static void pointer_button(int left)
{
  if (left) {
    /* §57: a press on a titlebar focuses + either closes (close box) or starts a
     * move drag (tinywl begin_interactive). Topmost titlebar wins. */
    for (int v = g_nviews - 1; v >= 0; v--) {
      struct surf *s = g_views[v];
      if (!s->mapped)
        continue;
      if (g_cx >= s->x && g_cx < s->x + s->w && g_cy >= s->y - TBH && g_cy < s->y) {
        focus_view(s);
        if (g_cx >= s->x + s->w - TBH + 4 && g_cx < s->x + s->w - 4 && s->xdg_toplevel)
          xdg_toplevel_send_close(s->xdg_toplevel); /* close box → ask client to quit */
        else {
          g_cursor_mode = MODE_MOVE;
          g_grab = s;
          g_grab_dx = g_cx - s->x;
          g_grab_dy = g_cy - s->y;
        }
        composite_scene();
        return;
      }
    }
    /* a press in window content → focus that window, forward the button. */
    if (g_ptr_view && g_ptr_view != g_focus_view) {
      focus_view(g_ptr_view);
      composite_scene();
    }
    struct wl_resource *p = ptr_of(g_ptr_view);
    if (p)
      wl_pointer_send_button(p, ++g_serial, ox_now_ms(), 0x110,
                             WL_POINTER_BUTTON_STATE_PRESSED);
  } else {
    if (g_cursor_mode == MODE_MOVE) { /* end the drag */
      g_cursor_mode = MODE_PASSTHROUGH;
      g_grab = NULL;
      return;
    }
    struct wl_resource *p = ptr_of(g_ptr_view);
    if (p)
      wl_pointer_send_button(p, ++g_serial, ox_now_ms(), 0x110,
                             WL_POINTER_BUTTON_STATE_RELEASED);
  }
}

/* Event-loop callback: drain the keyboard channel and deliver each set-1 scancode
 * to the focused client as a wl_keyboard.key event (§48). The break bit (0x80)
 * selects press vs release; the low 7 bits ARE the evdev keycode for the main
 * block, which the client offsets by 8 for xkb. We always read() (even with no
 * focus) so the kbd driver's channel never backs up. */
/* Move keyboard focus to view `s` (tinywl focus_view): leave the old surface,
 * raise + enter the new, and route subsequent keys to its client. */
static void focus_view(struct surf *s)
{
  if (!s || s == g_focus_view)
    return;
  if (g_focus_view && g_focus_view->surface) {
    struct seatc *osc = seat_for(wl_resource_get_client(g_focus_view->surface));
    if (osc && osc->kbd)
      wl_keyboard_send_leave(osc->kbd, ++g_serial, g_focus_view->surface);
  }
  views_raise(s);
  g_focus_view = s;
  if (s->surface) {
    struct seatc *nsc = seat_for(wl_resource_get_client(s->surface));
    if (nsc && nsc->kbd) {
      struct wl_array keys;
      wl_array_init(&keys);
      wl_keyboard_send_enter(nsc->kbd, ++g_serial, s->surface, &keys);
      wl_array_release(&keys);
    }
  }
  wake_unoccluded(); /* §62: raising `s` to the top uncovers whatever it hid */
}

static int on_input(int fd, uint32_t mask, void *data)
{
  (void)mask;
  (void)data;
  unsigned char buf[64];
  long          n = read(fd, buf, sizeof buf);
  struct wl_resource *kbd = NULL;
  if (g_focus_view && g_focus_view->surface) {
    struct seatc *sc = seat_for(wl_resource_get_client(g_focus_view->surface));
    kbd = sc ? sc->kbd : NULL;
  }
  for (long i = 0; i < n; i++) {
    if (!kbd)
      continue;
    unsigned char sc      = buf[i];
    uint32_t      keycode = sc & 0x7f;
    uint32_t      state   = (sc & 0x80) ? WL_KEYBOARD_KEY_STATE_RELEASED
                                        : WL_KEYBOARD_KEY_STATE_PRESSED;
    wl_keyboard_send_key(kbd, ++g_serial, ox_now_ms(), keycode, state);
  }
  return 0;
}

/* §54: drain PS/2 mouse packets and move the cursor. Each packet is 3 bytes:
 * [flags, dx, dy] with 9-bit signed deltas (sign bits in flags). Mouse Y points
 * up, screen Y down, so dy is subtracted. */
/* §60: flush pending COALESCED motion — composite ONCE for a whole batch of
 * mouse packets. `ocx/ocy` is the cursor and `ogx/ogy` the dragged window, both
 * captured when the batch began; g_cx/g_cy (and g_grab->x/y) already hold the
 * final position. Returns with no pending motion. */
static void flush_mouse_motion(int ocx, int ocy, int ogx, int ogy)
{
  if (g_cursor_mode == MODE_MOVE && g_grab) {
    /* §57: drag the grabbed window — damage its OLD + NEW area (§59). */
    int nx0 = ogx < g_grab->x ? ogx : g_grab->x;
    int ny0 = (ogy < g_grab->y ? ogy : g_grab->y) - TBH;
    int nx1 = (ogx > g_grab->x ? ogx : g_grab->x) + g_grab->w;
    int ny1 = (ogy > g_grab->y ? ogy : g_grab->y) + g_grab->h;
    composite_rect(nx0, ny0, nx1, ny1);
    wake_unoccluded(); /* §62: dragging this window may have uncovered another */
  } else {
    pointer_update(); /* §55: deliver motion + enter/leave to the client */
    /* §59: damage only the cursor's old + new footprint. */
    int cx0 = ocx < g_cx ? ocx : g_cx, cy0 = ocy < g_cy ? ocy : g_cy;
    int cx1 = (ocx > g_cx ? ocx : g_cx) + CURW;
    int cy1 = (ocy > g_cy ? ocy : g_cy) + CURH;
    composite_rect(cx0, cy0, cx1, cy1);
  }
}

static int on_mouse(int fd, uint32_t mask, void *data)
{
  (void)mask;
  (void)data;
  static unsigned char pkt[3];
  static int           pi = 0;
  /* Drain a generous batch per pump (256 packets) so motion can be coalesced. */
  unsigned char        buf[768];
  long                 n = read(fd, buf, sizeof buf);

  /* §60: COALESCE motion. PS/2 delivers ~100 packets/sec and a single read may
   * carry dozens; compositing the (large) dragged window once per packet makes
   * it lag behind the cursor as packets back up in the channel. Instead we apply
   * every packet's delta to the live cursor/window position but composite ONCE
   * per batch, against the union of where the batch started and ended. A button
   * transition bounds a gesture, so we flush the pending motion before acting on
   * it (and at the end of the batch). */
  int have_motion = 0;
  int ocx = g_cx, ocy = g_cy;                 /* cursor at batch start */
  int ogx = 0, ogy = 0;                       /* dragged window at batch start */
  if (g_cursor_mode == MODE_MOVE && g_grab) { ogx = g_grab->x; ogy = g_grab->y; }

  for (long i = 0; i < n; i++) {
    pkt[pi++] = buf[i];
    if (pi < 3)
      continue;
    pi = 0;
    int flags = pkt[0];
    int dx = pkt[1] - ((flags & 0x10) ? 256 : 0);
    int dy = pkt[2] - ((flags & 0x20) ? 256 : 0);
    if (dx || dy) {
      if (!have_motion) {
        have_motion = 1;
        ocx = g_cx;
        ocy = g_cy;
        if (g_cursor_mode == MODE_MOVE && g_grab) { ogx = g_grab->x; ogy = g_grab->y; }
      }
      g_cx += dx;
      g_cy -= dy;
      if (g_cx < 0) g_cx = 0;
      if (g_cx >= g_w) g_cx = g_w - 1;
      if (g_cy < 0) g_cy = 0;
      if (g_cy >= g_h) g_cy = g_h - 1;
      if (g_cursor_mode == MODE_MOVE && g_grab) {
        g_grab->x = g_cx - g_grab_dx;
        g_grab->y = g_cy - g_grab_dy;
      }
    }
    int left = flags & 0x01;
    if (left != g_btn_left) {
      /* flush accumulated motion before the button changes the gesture/mode */
      if (have_motion) {
        flush_mouse_motion(ocx, ocy, ogx, ogy);
        have_motion = 0;
      }
      g_btn_left = left;
      pointer_button(left); /* focus/move/close paths recomposite as needed */
    }
  }
  if (have_motion)
    flush_mouse_motion(ocx, ocy, ogx, ogy);
  return 0;
}

/* ---- exported driver entry points --------------------------------------- */
void *comp_server_setup(int fd, int input_fd, int mouse_fd, unsigned int *fb, int w, int h,
                        int pitch_words)
{
  g_fb = fb;
  g_w = w;
  g_h = h;
  g_pitch_words = pitch_words;
  g_composited = 0;
  g_cx = w / 2;
  g_cy = h / 2;

  /* §58: allocate the offscreen back buffer (same layout as the framebuffer) and
   * render the initial frame (empty desktop + cursor) through the flip path. */
  g_back = malloc((size_t)h * pitch_words * 4);
  composite_scene();

  struct wl_display *d = wl_display_create();
  if (!d)
    return NULL;
  wl_global_create(d, &wl_compositor_interface, 4, NULL, compositor_bind);
  wl_global_create(d, &xdg_wm_base_interface, 1, NULL, wm_base_bind);
  wl_global_create(d, &wl_seat_interface, 5, NULL, seat_bind);
  if (wl_display_init_shm(d) < 0) {
    wl_display_destroy(d);
    return NULL;
  }
  /* Watch the keyboard channel fd in the same event loop as the Wayland clients,
   * so the busy-poll dispatch picks up keystrokes (§47). */
  if (input_fd >= 0)
    wl_event_loop_add_fd(wl_display_get_event_loop(d), input_fd, WL_EVENT_READABLE,
                         on_input, d);
  /* §54: the mouse channel — moves the software cursor. */
  if (mouse_fd >= 0)
    wl_event_loop_add_fd(wl_display_get_event_loop(d), mouse_fd, WL_EVENT_READABLE,
                         on_mouse, d);
  if (!wl_client_create(d, fd)) {
    wl_display_destroy(d);
    return NULL;
  }
  return d;
}

/* §56: attach an additional Wayland client (a second window) on its own fd. */
void comp_server_add_client(void *d, int fd)
{
  if (fd >= 0)
    wl_client_create((struct wl_display *)d, fd);
}

void comp_server_pump(void *d)
{
  struct wl_display *dpy = d;
  /* §63: block (timeout -1) until a client commit, keyboard, or mouse event wakes
   * the event loop — instead of timeout 0, which spun the CPU at 100%. The blocking
   * epoll_wait sleeps in the kernel (sys_chan_wait) until a watched channel is
   * readable, so an idle compositor uses no CPU and clients get the core. */
  wl_event_loop_dispatch(wl_display_get_event_loop(dpy), -1);
  wl_display_flush_clients(dpy);
}

int comp_server_composited(void)
{
  return g_composited;
}
