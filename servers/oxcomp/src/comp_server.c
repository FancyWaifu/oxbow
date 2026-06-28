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
/* §92: mute/unmute the kbd->tty path (on != 0 = mute). Called on focus changes so
 * keystrokes go only to a focused non-terminal window, not also to the shell. */
extern void comp_tty_mute(int on);
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
/* §90 Phase 4: when set (the gpu's shared cursor-state region), the cursor is a
 * HARDWARE cursor — we publish g_cx/g_cy here and the gpu composites it, instead
 * of painting the arrow into the framebuffer. NULL = software cursor. */
static volatile unsigned int *g_hwcur = 0;
void comp_server_set_hwcursor(unsigned int *region) { g_hwcur = region; }

static unsigned int        g_serial;    /* event serial counter */
static int           g_composited;
static int g_btn_left; /* last reported left-button state (edge detection) */

/* ---- 8x8 bitmap font (§91) — the compositor draws its own chrome (panel clock,
 * launcher labels, window titles), so it needs to render text. Classic 8x8
 * glyphs, MSB = leftmost pixel; indexed [c-0x20] for 0x20..0x5F. Lowercase is
 * mapped to uppercase in draw_char (no lowercase glyphs). Unauthored chars are
 * blank. */
static const unsigned char font8x8[64][8] = {
  {0,0,0,0,0,0,0,0},                                              /* 0x20 space */
  {0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},          /* ! " # */
  {0,0,0,0,0,0,0,0},                                              /* $ */
  {0x62,0x64,0x08,0x10,0x26,0x46,0,0},                            /* % */
  {0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},          /* & ' ( */
  {0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},          /* ) * + */
  {0,0,0,0,0,0x18,0x18,0x30},                                    /* , */
  {0,0,0,0x7e,0,0,0,0},                                          /* - */
  {0,0,0,0,0,0x18,0x18,0},                                        /* . */
  {0x02,0x04,0x08,0x10,0x20,0x40,0x80,0},                         /* / */
  {0x3c,0x66,0x6e,0x76,0x66,0x66,0x3c,0},                         /* 0 */
  {0x18,0x38,0x18,0x18,0x18,0x18,0x7e,0},                         /* 1 */
  {0x3c,0x66,0x06,0x0c,0x18,0x30,0x7e,0},                         /* 2 */
  {0x3c,0x66,0x06,0x1c,0x06,0x66,0x3c,0},                         /* 3 */
  {0x0c,0x1c,0x3c,0x6c,0x7e,0x0c,0x0c,0},                         /* 4 */
  {0x7e,0x60,0x7c,0x06,0x06,0x66,0x3c,0},                         /* 5 */
  {0x3c,0x66,0x60,0x7c,0x66,0x66,0x3c,0},                         /* 6 */
  {0x7e,0x66,0x0c,0x18,0x18,0x18,0x18,0},                         /* 7 */
  {0x3c,0x66,0x66,0x3c,0x66,0x66,0x3c,0},                         /* 8 */
  {0x3c,0x66,0x66,0x3e,0x06,0x66,0x3c,0},                         /* 9 */
  {0,0x18,0x18,0,0,0x18,0x18,0},                                  /* : */
  {0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},          /* ; < = */
  {0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},          /* > ? @ */
  {0x3c,0x66,0x66,0x7e,0x66,0x66,0x66,0},                         /* A */
  {0x7c,0x66,0x66,0x7c,0x66,0x66,0x7c,0},                         /* B */
  {0x3c,0x66,0x60,0x60,0x60,0x66,0x3c,0},                         /* C */
  {0x78,0x6c,0x66,0x66,0x66,0x6c,0x78,0},                         /* D */
  {0x7e,0x60,0x60,0x7c,0x60,0x60,0x7e,0},                         /* E */
  {0x7e,0x60,0x60,0x7c,0x60,0x60,0x60,0},                         /* F */
  {0x3c,0x66,0x60,0x6e,0x66,0x66,0x3e,0},                         /* G */
  {0x66,0x66,0x66,0x7e,0x66,0x66,0x66,0},                         /* H */
  {0x3c,0x18,0x18,0x18,0x18,0x18,0x3c,0},                         /* I */
  {0x1e,0x0c,0x0c,0x0c,0x0c,0x6c,0x38,0},                         /* J */
  {0x66,0x6c,0x78,0x70,0x78,0x6c,0x66,0},                         /* K */
  {0x60,0x60,0x60,0x60,0x60,0x60,0x7e,0},                         /* L */
  {0x63,0x77,0x7f,0x6b,0x63,0x63,0x63,0},                         /* M */
  {0x66,0x76,0x7e,0x7e,0x6e,0x66,0x66,0},                         /* N */
  {0x3c,0x66,0x66,0x66,0x66,0x66,0x3c,0},                         /* O */
  {0x7c,0x66,0x66,0x7c,0x60,0x60,0x60,0},                         /* P */
  {0x3c,0x66,0x66,0x66,0x6e,0x3c,0x06,0},                         /* Q */
  {0x7c,0x66,0x66,0x7c,0x78,0x6c,0x66,0},                         /* R */
  {0x3c,0x66,0x60,0x3c,0x06,0x66,0x3c,0},                         /* S */
  {0x7e,0x18,0x18,0x18,0x18,0x18,0x18,0},                         /* T */
  {0x66,0x66,0x66,0x66,0x66,0x66,0x3c,0},                         /* U */
  {0x66,0x66,0x66,0x66,0x66,0x3c,0x18,0},                         /* V */
  {0x63,0x63,0x63,0x6b,0x7f,0x77,0x63,0},                         /* W */
  {0x66,0x66,0x3c,0x18,0x3c,0x66,0x66,0},                         /* X */
  {0x66,0x66,0x66,0x3c,0x18,0x18,0x18,0},                         /* Y */
  {0x7e,0x06,0x0c,0x18,0x30,0x60,0x7e,0},                         /* Z */
  {0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},          /* [ \ ] */
  {0,0,0,0,0,0,0,0},{0,0,0,0,0,0,0,0},                            /* ^ _ */
};

/* Current clip rectangle (the damage rect being composited). All compositor-drawn
 * chrome (panel/overview/text) clips to it so only damaged pixels are touched —
 * the same discipline as the cursor draw. Defaults to the whole screen. */
static int g_clip_x0, g_clip_y0, g_clip_x1 = 1 << 30, g_clip_y1 = 1 << 30;

/* Fill a rectangle with `color`, clipped to the clip rect + screen. */
static void fill_rect(int x0, int y0, int x1, int y1, unsigned int color)
{
  if (x0 < g_clip_x0) x0 = g_clip_x0;
  if (y0 < g_clip_y0) y0 = g_clip_y0;
  if (x1 > g_clip_x1) x1 = g_clip_x1;
  if (y1 > g_clip_y1) y1 = g_clip_y1;
  if (x0 < 0) x0 = 0;
  if (y0 < 0) y0 = 0;
  if (x1 > g_w) x1 = g_w;
  if (y1 > g_h) y1 = g_h;
  for (int y = y0; y < y1; y++)
    for (int x = x0; x < x1; x++)
      g_back[(long)y * g_pitch_words + x] = color;
}

/* Draw one glyph at (x,y) in `color`, clipped to the clip rect + screen. */
static void draw_char(int x, int y, char ch, unsigned int color)
{
  unsigned char c = (unsigned char)ch;
  if (c >= 'a' && c <= 'z') c -= 0x20; /* fold to uppercase */
  if (c < 0x20 || c > 0x5f) return;
  const unsigned char *g = font8x8[c - 0x20];
  for (int row = 0; row < 8; row++) {
    int py = y + row;
    if (py < g_clip_y0 || py >= g_clip_y1 || py < 0 || py >= g_h) continue;
    for (int col = 0; col < 8; col++) {
      if (!((g[row] >> (7 - col)) & 1)) continue;
      int px = x + col;
      if (px < g_clip_x0 || px >= g_clip_x1 || px < 0 || px >= g_w) continue;
      g_back[(long)py * g_pitch_words + px] = color;
    }
  }
}

/* Draw a NUL-terminated string; returns the x just past the last glyph. */
static int draw_text(int x, int y, const char *s, unsigned int color)
{
  for (; *s; s++) {
    draw_char(x, y, *s, color);
    x += 8;
  }
  return x;
}

/* Pixel width of a string at the 8px advance. */
static int text_width(const char *s) { int n = 0; while (s[n]) n++; return n * 8; }

/* §93: scale a bw*bh source image into the screen rect [dx0,dx1)×[dy0,dy1),
 * nearest-neighbor, clipped to g_clip + screen. Single source of truth for the
 * window content scaler AND the overview/Alt-Tab live cards. */
static void blit_scaled(const unsigned int *src, int bw, int bh, int dx0, int dy0,
                        int dx1, int dy1)
{
  if (!src || bw <= 0 || bh <= 0 || dx1 <= dx0 || dy1 <= dy0)
    return;
  int dw = dx1 - dx0, dh = dy1 - dy0;
  int cx0 = dx0 < g_clip_x0 ? g_clip_x0 : dx0;
  int cy0 = dy0 < g_clip_y0 ? g_clip_y0 : dy0;
  int cx1 = dx1 > g_clip_x1 ? g_clip_x1 : dx1;
  int cy1 = dy1 > g_clip_y1 ? g_clip_y1 : dy1;
  if (cx0 < 0) cx0 = 0;
  if (cy0 < 0) cy0 = 0;
  if (cx1 > g_w) cx1 = g_w;
  if (cy1 > g_h) cy1 = g_h;
  if (cx0 >= cx1 || cy0 >= cy1)
    return;
  /* §perf: 1:1 (no scaling — a window rendered at its display size, e.g. a
   * reflowed terminal) → straight row copies, no per-pixel work at all. */
  if (bw == dw && bh == dh) {
    for (int y = cy0; y < cy1; y++) {
      const unsigned int *srow = src + (long)(y - dy0) * bw + (cx0 - dx0);
      memcpy(&g_back[(long)y * g_pitch_words + cx0], srow, (size_t)(cx1 - cx0) * 4);
    }
    return;
  }
  /* §perf: scaling path — precompute the source column for each dest x ONCE
   * (it's identical for every row), turning a multiply+divide PER PIXEL into one
   * table build + a lookup per pixel. A full-screen scaled window was doing
   * millions of divides per composite; this is the choppiness lever for scaled
   * (maximized, fixed-content) windows like a fullscreen game. */
  static int xmap[4096];
  if (cx1 - cx0 > 4096)
    cx1 = cx0 + 4096;
  int n = cx1 - cx0;
  for (int i = 0; i < n; i++) {
    int sx = (cx0 + i - dx0) * bw / dw;
    xmap[i] = sx >= bw ? bw - 1 : sx;
  }
  for (int y = cy0; y < cy1; y++) {
    int sy = (y - dy0) * bh / dh;
    if (sy >= bh) sy = bh - 1;
    const unsigned int *srow = src + (long)sy * bw;
    unsigned int *drow = &g_back[(long)y * g_pitch_words + cx0];
    for (int i = 0; i < n; i++)
      drow[i] = srow[xmap[i]];
  }
}

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
  int                 bw, bh; /* §91: backing (client buffer) size; w/h is the
                              * DISPLAY size — the compositor scales bw*bh -> w*h,
                              * so the user can resize any window even though the
                              * client renders at a fixed size. */
  unsigned int       *backing;
  long                backing_cap;
  char                title[48]; /* §91: xdg_toplevel.set_title, drawn in the bar */
  /* §93 GNOME-feel window management. */
  int                 maximized;   /* 1 = maximized OR edge-snapped (has saved geom) */
  int                 minimized;   /* 1 = hidden from desktop, reachable via overview/alt-tab */
  int                 sx, sy, sw, sh; /* saved floating geom, valid while maximized */
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

/* The terminal (oxterm) is the only window that consumes shell input via the tty;
 * every other window wants keystrokes routed only to itself. Identify it by the
 * title oxui sets at window-create time. */
static int surf_is_terminal(struct surf *s)
{
  return s && s->title[0] && strcmp(s->title, "oxterm") == 0;
}

/* §92: mute the kbd->tty path while a non-terminal window holds focus, so its
 * keystrokes don't ALSO surface as shell commands; unmute when the terminal (or
 * nothing) is focused. Called on every focus/title change; only sends on a real
 * state change. */
static int g_tty_muted = 0;
static void update_tty_mute(void)
{
  int want = (g_focus_view && !surf_is_terminal(g_focus_view)) ? 1 : 0;
  if (want != g_tty_muted) {
    g_tty_muted = want;
    comp_tty_mute(want);
  }
}

/* §57 window management: a titlebar above each window, and a cursor-mode state
 * machine (tinywl) for interactive move. */
#define TBH 22 /* titlebar height in px */
#define RESIZE_ZONE 18 /* bottom-right corner grip for resize */
#define SNAP_ZONE 12   /* §93: drop a dragged window this close to an edge → tile */
enum { MODE_PASSTHROUGH, MODE_MOVE, MODE_RESIZE };
static int          g_cursor_mode;
static struct surf *g_grab;        /* view being dragged/resized */
static int          g_grab_dx, g_grab_dy; /* cursor offset within the window */
/* §93: Alt-Tab window switcher state. */
static int          g_alt_down;    /* left Alt held (evdev 0x38) */
static int          g_switching;   /* the switcher overlay is up */
static int          g_switch_index; /* selected slot in the switch ring */
/* §93: Super (Meta) window-management chords, GNOME-style. */
static int          g_super_down;  /* left Super held (evdev 125) */
static int          g_super_used;  /* a chord fired during this Super hold */

/* ---- §91 GNOME-style shell: a top bar + an Activities app launcher --------- */
#define PANEL_H 28                 /* top bar height */
#define PANEL_BG   0x00282828u     /* GNOME-ish dark bar */
#define PANEL_FG   0x00e8e8e8u     /* bar text */
#define PANEL_HL   0x00404552u     /* hovered/active button */
#define OVL_BG     0x00202428u     /* overview backdrop */
#define CARD_BG    0x00363b42u     /* app card */
static int   g_overview;           /* is the Activities overview open? */
static void *g_display;            /* the wl_display, for launching apps at runtime */

/* §93: GNOME-style titlebar window controls — three TBH-wide cells at the right
 * edge of the bar (right→left: close, maximize, minimize). ONE geometry source so
 * the draw code and the hit-test can never drift. */
enum { BTN_MIN, BTN_MAX, BTN_CLOSE };
static void tb_btn_xrange(struct surf *s, int which, int *x0, int *x1)
{
  int right = s->x + s->w; /* close occupies the last cell, then max, then min */
  int cell  = which == BTN_CLOSE ? 0 : which == BTN_MAX ? 1 : 2;
  *x1 = right - cell * TBH;
  *x0 = *x1 - TBH;
}
/* The button under (px,py) on s's titlebar, or -1. Titlebar rows are [y-TBH, y). */
static int tb_btn_hit(struct surf *s, int px, int py)
{
  if (py < s->y - TBH || py >= s->y)
    return -1;
  for (int b = 0; b < 3; b++) {
    int x0, x1;
    tb_btn_xrange(s, b, &x0, &x1);
    if (px >= x0 && px < x1)
      return b;
  }
  return -1;
}
/* Paint the three control glyphs into s's titlebar (called after the bar fill, so
 * it overdraws). Clipped via g_clip by fill_rect. */
static void draw_tb_buttons(struct surf *s)
{
  for (int b = 0; b < 3; b++) {
    int x0, x1;
    tb_btn_xrange(s, b, &x0, &x1);
    int gy0 = s->y - TBH + 6, gy1 = s->y - 6; /* glyph inset within the cell */
    int gx0 = x0 + 6, gx1 = x1 - 6;
    if (b == BTN_CLOSE) {
      fill_rect(x0, s->y - TBH, x1, s->y, 0x00c04040u); /* red cell */
      int span = gx1 - gx0 > 0 ? gx1 - gx0 : 1; /* a small white X */
      for (int t = 0; t < span; t++) {
        int yy = gy0 + t * (gy1 - gy0) / span;
        fill_rect(gx0 + t, yy, gx0 + t + 1, yy + 1, 0x00ffffffu);
        fill_rect(gx1 - 1 - t, yy, gx1 - t, yy + 1, 0x00ffffffu);
      }
    } else if (b == BTN_MIN) {
      fill_rect(gx0, gy1 - 2, gx1, gy1, PANEL_FG); /* low horizontal bar */
    } else { /* BTN_MAX: hollow square, or a double square when maximized (restore) */
      if (s->maximized) {
        fill_rect(gx0 + 2, gy0, gx1, gy0 + 2, PANEL_FG);
        fill_rect(gx1 - 2, gy0, gx1, gy1 - 2, PANEL_FG);
        fill_rect(gx0, gy0 + 2, gx0 + 2, gy1, PANEL_FG);
        fill_rect(gx0, gy1 - 2, gx1 - 2, gy1, PANEL_FG);
        fill_rect(gx0 + 2, gy0 + 2, gx1 - 2, gy1 - 2, 0x00444444u);
      } else {
        fill_rect(gx0, gy0, gx1, gy0 + 2, PANEL_FG);
        fill_rect(gx0, gy1 - 2, gx1, gy1, PANEL_FG);
        fill_rect(gx0, gy0, gx0 + 2, gy1, PANEL_FG);
        fill_rect(gx1 - 2, gy0, gx1, gy1, PANEL_FG);
      }
    }
  }
}

/* Launch an app by id (provided by Rust main.rs): 0=terminal, 1=monitor, 2=rings.
 * Returns a Wayland-socket fd to attach, or -1. */
extern int comp_server_launch_app(int app_id);

/* The launcher's apps (icon color + label). app id == index. */
#define NAPPS 4
static const unsigned int app_icon[NAPPS] = {0x00264f78u, 0x00367a4au, 0x00803050u, 0x00a01818u};
static const char *const  app_label[NAPPS] = {"TERMINAL", "MONITOR", "RINGS", "DOOM"};

/* Geometry of overview card `i` (centered row). */
static void app_card_rect(int i, int *cx, int *cy, int *cw, int *ch)
{
  int w = 170, h = 130, gap = 28;
  int total = NAPPS * w + (NAPPS - 1) * gap;
  *cx = (g_w - total) / 2 + i * (w + gap);
  *cy = (g_h - h) / 2;
  *cw = w;
  *ch = h;
}

/* §93: the window-switch ring — mapped views top→bottom (topmost first), INCLUDING
 * minimized ones (the overview + Alt-Tab are how you reach them). Fills `out` (cap
 * MAXVIEWS), returns the count. */
static int switch_ring(struct surf **out)
{
  int n = 0;
  for (int v = g_nviews - 1; v >= 0; v--)
    if (g_views[v]->mapped)
      out[n++] = g_views[v];
  return n;
}
/* Geometry of live window card `i` of `n` — a centered row above the app cards. */
static void win_card_rect(int i, int n, int *cx, int *cy, int *cw, int *ch)
{
  int margin = 40, gap = 16, maxw = 200, h = 120;
  int avail = g_w - 2 * margin - (n - 1) * gap;
  int w = n > 0 ? avail / n : maxw;
  if (w > maxw) w = maxw;
  if (w < 60) w = 60;
  int total = n * w + (n - 1) * gap;
  *cx = (g_w - total) / 2 + i * (w + gap);
  *cy = g_h / 2 - 170; /* a row above the app-launch cards (centered at g_h/2) */
  *cw = w;
  *ch = h;
}

/* Format uptime (no RTC) as HH:MM:SS into `buf` (>=9 bytes) — the panel clock. */
static void format_clock(char *buf)
{
  unsigned int s = ox_now_ms() / 1000u;
  unsigned int hh = (s / 3600u) % 100u, mm = (s / 60u) % 60u, ss = s % 60u;
  buf[0] = '0' + hh / 10; buf[1] = '0' + hh % 10; buf[2] = ':';
  buf[3] = '0' + mm / 10; buf[4] = '0' + mm % 10; buf[5] = ':';
  buf[6] = '0' + ss / 10; buf[7] = '0' + ss % 10; buf[8] = 0;
}

/* Draw the top bar (clipped to g_clip): Activities on the left, the clock center. */
static void draw_panel(void)
{
  fill_rect(0, 0, g_w, PANEL_H, PANEL_BG);
  if (g_overview)
    fill_rect(0, 0, 8 * 10 + 16, PANEL_H, PANEL_HL); /* Activities highlighted */
  draw_text(12, (PANEL_H - 8) / 2, "Activities", PANEL_FG);
  char clk[9];
  format_clock(clk);
  draw_text(g_w / 2 - text_width(clk) / 2, (PANEL_H - 8) / 2, clk, PANEL_FG);
}

/* Draw the Activities overview (clipped to g_clip): a backdrop + a row of app
 * cards, each an icon swatch with its label. */
static void draw_overview(void)
{
  fill_rect(0, PANEL_H, g_w, g_h, OVL_BG);
  /* §93: a row of LIVE window cards (running + minimized) — GNOME's overview.
   * Click one to focus/restore it. */
  struct surf *ring[MAXVIEWS];
  int rn = switch_ring(ring);
  for (int i = 0; i < rn; i++) {
    int x, y, w, h;
    win_card_rect(i, rn, &x, &y, &w, &h);
    fill_rect(x, y, x + w, y + h, CARD_BG);
    if (ring[i]->backing)
      blit_scaled(ring[i]->backing, ring[i]->bw, ring[i]->bh, x + 4, y + 4, x + w - 4,
                  y + h - 20);
    int saved = g_clip_x1;
    if (g_clip_x1 > x + w) g_clip_x1 = x + w; /* keep the title inside the card */
    int tw = text_width(ring[i]->title);
    draw_text(x + (w - tw) / 2, y + h - 14, ring[i]->title,
              ring[i]->minimized ? 0x00808080u : PANEL_FG);
    g_clip_x1 = saved;
  }
  for (int i = 0; i < NAPPS; i++) {
    int x, y, w, h;
    app_card_rect(i, &x, &y, &w, &h);
    fill_rect(x, y, x + w, y + h, CARD_BG);
    fill_rect(x + 30, y + 22, x + w - 30, y + h - 40, app_icon[i]); /* icon swatch */
    int tw = text_width(app_label[i]);
    draw_text(x + (w - tw) / 2, y + h - 26, app_label[i], PANEL_FG);
  }
}

/* §93: the Alt-Tab switcher — a centered strip of live window cards with the
 * selected one highlighted. Drawn over everything while Alt+Tab is held. */
static void draw_switcher(void)
{
  struct surf *ring[MAXVIEWS];
  int rn = switch_ring(ring);
  if (rn < 1)
    return;
  int cw = 140, ch = 96, gap = 14, pad = 16;
  int total = rn * cw + (rn - 1) * gap;
  int x0 = (g_w - total) / 2 - pad, y0 = g_h / 2 - ch / 2 - pad;
  int x1 = (g_w + total) / 2 + pad, y1 = g_h / 2 + ch / 2 + pad + 12;
  fill_rect(x0, y0, x1, y1, OVL_BG); /* the switcher backdrop */
  int sel = ((g_switch_index % rn) + rn) % rn;
  for (int i = 0; i < rn; i++) {
    int cx = (g_w - total) / 2 + i * (cw + gap), cy = g_h / 2 - ch / 2;
    if (i == sel)
      fill_rect(cx - 3, cy - 3, cx + cw + 3, cy + ch + 3, PANEL_HL); /* highlight */
    fill_rect(cx, cy, cx + cw, cy + ch, CARD_BG);
    if (ring[i]->backing)
      blit_scaled(ring[i]->backing, ring[i]->bw, ring[i]->bh, cx + 3, cy + 3, cx + cw - 3,
                  cy + ch - 3);
    int saved = g_clip_x1;
    if (g_clip_x1 > cx + cw) g_clip_x1 = cx + cw;
    int tw = text_width(ring[i]->title);
    draw_text(cx + (cw - tw) / 2, cy + ch + 2, ring[i]->title, PANEL_FG);
    g_clip_x1 = saved;
  }
}

/* The panel clock ticks on a 1-second event-loop timer: recomposite just the top
 * bar (cheap) and re-arm. */
static struct wl_event_source *g_clock_timer;
static void composite_rect(int x0, int y0, int x1, int y1);
static int g_greeter = 1; /* §92: 1 = login screen shown, desktop hidden+gated */
static int clock_tick(void *data)
{
  (void)data;
  /* The clock is desktop chrome — don't paint it behind the greeter, but always
   * re-arm so it resumes once the desktop is revealed. */
  if (!g_greeter)
    composite_rect(0, 0, g_w, PANEL_H);
  if (g_clock_timer)
    wl_event_source_timer_update(g_clock_timer, 1000);
  return 0;
}

/* ===========================================================================
 * §92 — the graphical login GREETER. Login moves out of the terminal: the
 * compositor draws a login screen before the desktop, captures keystrokes into a
 * username/password, and relays "username\npassword" to the SHELL over the
 * session channel (BOOT_SESSION_CHAN, wrapped as g_session_fd). The shell is the
 * sole credential authority — it verifies, mints the user's home capability, and
 * replies one byte (`1` ok / `0` fail). On `logout` the shell sends `L` and the
 * greeter re-appears. So the compositor authenticates NOTHING; it only asserts.
 * =========================================================================== */
static int  g_session_fd = -1; /* the session-channel byte stream to the shell */
static char g_user[33];
static int  g_userlen;
static char g_pass[64];
static int  g_passlen;
static int  g_field;     /* 0 = username, 1 = password */
static int  g_login_err; /* show "login incorrect" */
static int  g_shift;     /* greeter modifier state */
static int  g_awaiting_verdict; /* §A4: creds sent to the shell, awaiting its reply on the session fd */

/* set-1 scancode (low 7 bits) -> US-ASCII for the greeter's own text input. The
 * compositor normally forwards raw scancodes to clients (xkb decodes them); the
 * greeter decodes its own. Unshifted; letters uppercase under shift. */
static const char kc_ascii[128] = {
    [0x02] = '1', [0x03] = '2', [0x04] = '3', [0x05] = '4', [0x06] = '5',
    [0x07] = '6', [0x08] = '7', [0x09] = '8', [0x0a] = '9', [0x0b] = '0',
    [0x0c] = '-', [0x0d] = '=', [0x10] = 'q', [0x11] = 'w', [0x12] = 'e',
    [0x13] = 'r', [0x14] = 't', [0x15] = 'y', [0x16] = 'u', [0x17] = 'i',
    [0x18] = 'o', [0x19] = 'p', [0x1a] = '[', [0x1b] = ']', [0x1e] = 'a',
    [0x1f] = 's', [0x20] = 'd', [0x21] = 'f', [0x22] = 'g', [0x23] = 'h',
    [0x24] = 'j', [0x25] = 'k', [0x26] = 'l', [0x27] = ';', [0x28] = '\'',
    [0x29] = '`', [0x2b] = '\\', [0x2c] = 'z', [0x2d] = 'x', [0x2e] = 'c',
    [0x2f] = 'v', [0x30] = 'b', [0x31] = 'n', [0x32] = 'm', [0x33] = ',',
    [0x34] = '.', [0x35] = '/', [0x39] = ' ',
};

#define GR_BG     0x00101418u
#define GR_CARD   0x001c2230u
#define GR_FG     0x00e8e8e8u
#define GR_DIM    0x00808a94u
#define GR_FIELD  0x000c1016u
#define GR_ACCENT 0x003a6ea5u
#define GR_ERR    0x00d06058u

/* Card geometry (centred login box). */
static void greeter_card(int *cx, int *cy, int *cw, int *ch)
{
  *cw = 460;
  *ch = 320;
  *cx = (g_w - *cw) / 2;
  *cy = (g_h - *ch) / 2;
}

/* One labelled input box. `mask` renders the content as dots (password). */
static void draw_field(int lx, int y, int fx, int fw, const char *label,
                       const char *content, int len, int active, int mask)
{
  draw_text(lx, y + 6, label, GR_DIM);
  fill_rect(fx, y, fx + fw, y + 20, GR_FIELD);
  unsigned int b = active ? GR_ACCENT : GR_DIM;
  fill_rect(fx, y, fx + fw, y + 1, b);
  fill_rect(fx, y + 19, fx + fw, y + 20, b);
  fill_rect(fx, y, fx + 1, y + 20, b);
  fill_rect(fx + fw - 1, y, fx + fw, y + 20, b);
  int tx = fx + 7;
  if (mask) {
    for (int i = 0; i < len && tx + i * 10 < fx + fw - 8; i++)
      fill_rect(tx + i * 10, y + 8, tx + i * 10 + 5, y + 13, GR_FG); /* a dot */
  } else {
    draw_text(tx, y + 6, content, GR_FG);
  }
  if (active) {
    int caret = mask ? tx + len * 10 : tx + len * 8;
    if (caret < fx + fw - 2)
      fill_rect(caret, y + 4, caret + 1, y + 16, GR_FG); /* blink-less caret */
  }
}

/* Paint the whole login screen into the back buffer (clipped via fill_rect/
 * draw_text to the current damage rect). */
static void draw_greeter(void)
{
  fill_rect(0, 0, g_w, g_h, GR_BG);
  int cx, cy, cw, ch;
  greeter_card(&cx, &cy, &cw, &ch);
  fill_rect(cx, cy, cx + cw, cy + ch, GR_CARD);
  fill_rect(cx, cy, cx + cw, cy + 4, GR_ACCENT); /* accent stripe */
  draw_text(cx + (cw - text_width("oxbow")) / 2, cy + 40, "oxbow", GR_FG);
  draw_text(cx + (cw - text_width("sign in")) / 2, cy + 70, "sign in", GR_DIM);
  int fx = cx + 96, fw = cw - 96 - 40;
  draw_field(cx + 40, cy + 120, fx, fw, "user", g_user, g_userlen, g_field == 0, 0);
  draw_field(cx + 40, cy + 160, fx, fw, "pass", g_pass, g_passlen, g_field == 1, 1);
  if (g_login_err)
    draw_text(cx + 40, cy + 200, "login incorrect", GR_ERR);
  draw_text(cx + (cw - text_width("tab switch   enter login")) / 2, cy + ch - 34,
            "tab switch   enter login", GR_DIM);
}

/* Re-show the greeter (called on logout): reset fields, gate input, repaint. */
static void show_greeter(void)
{
  g_greeter = 1;
  g_login_err = 0;
  g_userlen = 0;
  g_passlen = 0;
  g_field = 0;
  composite_rect(0, 0, g_w, g_h);
}

/* Relay the typed credentials to the shell and act on its one-byte verdict. */
static void greeter_submit(void)
{
  if (g_userlen == 0 || g_session_fd < 0)
    return;
  char msg[100];
  int k = 0;
  for (int i = 0; i < g_userlen && k < 98; i++)
    msg[k++] = g_user[i];
  msg[k++] = '\n';
  for (int i = 0; i < g_passlen && k < 99; i++)
    msg[k++] = g_pass[i];
  (void)!write(g_session_fd, msg, k);
  __builtin_memset(g_pass, 0, sizeof g_pass);
  g_passlen = 0;
  /* §A4: do NOT block for the verdict — the event loop must keep running (Xwayland and
   * keyboard input depend on it; a slow shell reply under load would otherwise freeze the
   * whole compositor). The shell's one-byte reply arrives asynchronously on the session fd
   * and is handled by on_session(). */
  g_awaiting_verdict = 1;
}

/* Feed one raw scancode to the greeter (press/release with the 0x80 break bit). */
static void greeter_key(unsigned char raw)
{
  int           release = raw & 0x80;
  unsigned char kc = raw & 0x7f;
  if (kc == 0x2a || kc == 0x36) { /* shift */
    g_shift = !release;
    return;
  }
  if (release)
    return;
  if (kc == 0x1c) { /* enter */
    greeter_submit();
    return;
  }
  if (kc == 0x0f) { /* tab: switch field */
    g_field = !g_field;
    composite_rect(0, 0, g_w, g_h);
    return;
  }
  if (kc == 0x0e) { /* backspace */
    if (g_field == 0 && g_userlen > 0)
      g_userlen--;
    else if (g_field == 1 && g_passlen > 0)
      g_passlen--;
    composite_rect(0, 0, g_w, g_h);
    return;
  }
  char c = kc_ascii[kc];
  if (!c)
    return;
  if (g_shift && c >= 'a' && c <= 'z')
    c -= 32;
  if (g_field == 0) {
    if (g_userlen < 32)
      g_user[g_userlen++] = c;
  } else {
    if (g_passlen < 63)
      g_pass[g_passlen++] = c;
  }
  composite_rect(0, 0, g_w, g_h);
}

/* Event-loop callback: a byte on the session channel while the desktop is up means
 * the shell logged out (`L`) — re-show the greeter. */
static int on_session(int fd, uint32_t mask, void *data)
{
  (void)mask;
  (void)data;
  char b[16];
  long n = read(fd, b, sizeof b);
  for (long i = 0; i < n; i++) {
    if (g_awaiting_verdict) {
      /* §A4: the shell's reply to a login attempt: '1' = ok (reveal desktop), else error. */
      g_awaiting_verdict = 0;
      g_login_err = (b[i] != '1');
      if (b[i] == '1')
        g_greeter = 0;
      composite_rect(0, 0, g_w, g_h);
    } else if (b[i] == 'L') {
      show_greeter(); /* logged in already: a byte means `logout` */
    }
  }
  return 0;
}

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
  /* Restrict compositor-drawn chrome (panel/overview/text) to this damage rect. */
  g_clip_x0 = x0; g_clip_y0 = y0; g_clip_x1 = x1; g_clip_y1 = y1;
  if (g_greeter) {
    draw_greeter(); /* §92: the login screen replaces the desktop entirely */
  } else {
  for (int y = y0; y < y1; y++)
    for (int x = x0; x < x1; x++)
      g_back[(long)y * g_pitch_words + x] = 0x000d3b45u; /* desktop bg */
  for (int v = 0; v < g_nviews; v++) {
    struct surf *s = g_views[v];
    if (!s->mapped || !s->backing || s->minimized)
      continue;
    unsigned int bar = (s == g_focus_view) ? 0x003a6ea5u : 0x00444444u;
    /* titlebar rows [s->y-TBH, s->y) clipped to the damage rect */
    int b0 = (s->y - TBH > y0) ? s->y - TBH : y0;
    int b1 = (s->y < y1) ? s->y : y1;
    int a0 = (s->x > x0) ? s->x : x0;
    int a1 = (s->x + s->w < x1) ? s->x + s->w : x1;
    for (int y = b0; y < b1; y++)
      for (int x = a0; x < a1; x++)
        g_back[(long)y * g_pitch_words + x] = bar;
    /* §93: the three window-control buttons (min/max/close) over the bar fill. */
    draw_tb_buttons(s);
    /* §91: the window title in the bar, clipped short of the control buttons. */
    if (s->title[0]) {
      int saved = g_clip_x1;
      if (g_clip_x1 > s->x + s->w - 3 * TBH - 2) g_clip_x1 = s->x + s->w - 3 * TBH - 2;
      draw_text(s->x + 8, s->y - TBH + (TBH - 8) / 2, s->title, 0x00ffffffu);
      g_clip_x1 = saved;
    }
    /* content rows [s->y, s->y+s->h) — scale the bw*bh backing to w*h (clipped). */
    if (s->w > 0 && s->h > 0)
      blit_scaled(s->backing, s->bw, s->bh, s->x, s->y, s->x + s->w, s->y + s->h);
    /* §91: a resize grip — 3 diagonal ticks in the bottom-right corner. */
    for (int t = 1; t <= 3; t++)
      for (int k = 0; k < 3; k++) {
        int px = s->x + s->w - 2 - k, py = s->y + s->h - t * 4 + k;
        if (px >= g_clip_x0 && px < g_clip_x1 && py >= g_clip_y0 && py < g_clip_y1 &&
            px >= 0 && px < g_w && py >= 0 && py < g_h)
          g_back[(long)py * g_pitch_words + px] = 0x00b0b6c0u;
      }
  }
  /* §91: the Activities overview (modal, over the windows) then the top bar — both
   * always on top of client windows, clipped to the damage rect. */
  if (g_overview)
    draw_overview();
  draw_panel();
  if (g_switching) /* §93: Alt-Tab overlay, on top of the panel */
    draw_switcher();
  } /* §92: end of the !g_greeter desktop block */
  if (g_hwcur) {
    /* Hardware cursor: the gpu composites it on the device side; just publish the
     * pointer position for the gpu to read (no painting into the framebuffer). */
    g_hwcur[0] = (unsigned int)g_cx;
    g_hwcur[1] = (unsigned int)g_cy;
    g_hwcur[2] = 1;
  } else {
    /* Software cursor on top, clipped. */
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
    if (s->mapped && !s->minimized && s->frame_cb && !content_fully_occluded(s)) {
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
  s->bw = bw; /* §91: backing size; display w/h is set once (below) then resizable */
  s->bh = bh;
  wl_shm_buffer_begin_access(shm);
  unsigned char *data = wl_shm_buffer_get_data(shm);
  if (s->backing)
    for (int y = 0; y < bh; y++)
      memcpy(s->backing + (long)y * bw, data + (long)y * stride, (size_t)bw * 4);
  wl_shm_buffer_end_access(shm);
  if (!s->mapped) {
    s->w = bw; /* first frame: display 1:1 with the buffer */
    s->h = bh;
    /* §67: place each window near a different screen anchor (top-left, top-right,
     * bottom-left, bottom-right, then center) instead of a tight cascade — so a new
     * window isn't buried under a previous large one. g_nviews is the count of
     * already-mapped windows, so it is this window's anchor slot. A per-cycle jitter
     * keeps a 6th+ window off an exact repeat. Everything is clamped on-screen. */
    const int M = 40; /* screen-edge margin */
    int slot = g_nviews;
    int j = (slot / 5) * 32; /* jitter once we wrap past the 5 anchors */
    int top = PANEL_H + TBH + M; /* §91: anchor below the top bar + titlebar */
    switch (slot % 5) {
      case 0: s->x = M + j;                  s->y = top + j; break;                 /* TL */
      case 1: s->x = g_w - s->w - M - j;     s->y = top + j; break;                 /* TR */
      case 2: s->x = M + j;                  s->y = g_h - s->h - M - j; break;      /* BL */
      case 3: s->x = g_w - s->w - M - j;     s->y = g_h - s->h - M - j; break;      /* BR */
      default: s->x = (g_w - s->w) / 2;      s->y = (g_h - s->h) / 2; break;        /* center */
    }
    /* keep the whole window (titlebar included) below the panel + on-screen */
    if (s->x < 0) s->x = 0;
    if (s->x + s->w > g_w) s->x = g_w - s->w;
    if (s->y - TBH < PANEL_H) s->y = PANEL_H + TBH;
    if (s->y + s->h > g_h) s->y = g_h - s->h;
    s->mapped = 1;
    views_raise(s);
    g_focus_view = s;
    update_tty_mute(); /* a non-terminal window grabbing focus mutes shell input */
    struct seatc *sc = seat_for(c);
    if (sc && sc->kbd) {
      struct wl_array keys;
      wl_array_init(&keys);
      wl_keyboard_send_enter(sc->kbd, ++g_serial, res, &keys);
      wl_array_release(&keys);
    }
    composite_scene(); /* new window: place + focus → full recomposite */
  } else if (!s->minimized) {
    /* §59: an animation frame only changed THIS window — damage just its area.
     * §61: and skip the parts hidden behind windows above, so a window animating
     * behind an opaque one costs only its VISIBLE area, not its full size.
     * §93: a minimized window draws nothing (but still releases its buffer below
     * and has its frame callback withheld, so the client pauses). */
    composite_occluded(s, s->x, s->y - TBH, s->x + s->w, s->y + s->h);
  }
  g_composited = 1;
  wl_buffer_send_release(s->buffer); /* client may reuse the buffer */
  /* Tell the client this frame is on screen and it may draw the next one. Its
   * frame-callback handler redraws + commits again → the surface animates.
   * §62: but if this window is now FULLY hidden behind opaque windows, withhold
   * the callback — the client blocks, the hidden animation pauses (no wasted CPU),
   * and wake_unoccluded() re-delivers it when the window becomes visible again. */
  if (s->frame_cb && !s->minimized && !content_fully_occluded(s)) {
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
    if (g_focus_view == s) {
      g_focus_view = NULL;
      update_tty_mute(); /* focused window gone → restore shell input */
    }
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
{
  (void)c;
  struct surf *s = wl_resource_get_user_data(r);
  if (!s || !t)
    return;
  int i = 0;
  for (; t[i] && i < (int)sizeof(s->title) - 1; i++)
    s->title[i] = t[i];
  s->title[i] = 0;
  if (s == g_focus_view)
    update_tty_mute(); /* title can arrive after focus → re-evaluate the mute */
  composite_scene(); /* §91: redraw the bar with the new title */
}
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
    if (s->mapped && !s->minimized && px >= s->x && px < s->x + s->w && py >= s->y &&
        py < s->y + s->h)
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
/* §93 window-management actions (defined just after focus_view). */
static void toggle_maximize(struct surf *s);
static void minimize_view(struct surf *s);
static void unminimize_focus(struct surf *s);
static void snap_view(struct surf *s, int zone);
static void send_configure(struct surf *s);
/* §91: launch app `id`, attach its Wayland socket to the display, close the
 * overview. */
static void launch_and_attach(int id)
{
  int fd = comp_server_launch_app(id);
  if (fd >= 0 && g_display)
    wl_client_create((struct wl_display *)g_display, fd);
  g_overview = 0;
  composite_scene();
}

static void pointer_button(int left)
{
  if (g_greeter)
    return; /* §92: the login screen is keyboard-driven — swallow all pointer input */
  if (left) {
    /* §91: the GNOME-style shell intercepts clicks on the top bar + overview
     * BEFORE windows. */
    if (g_overview) {
      /* §93: a live window card focuses/restores that window. */
      struct surf *ring[MAXVIEWS];
      int rn = switch_ring(ring);
      for (int i = 0; i < rn; i++) {
        int x, y, w, h;
        win_card_rect(i, rn, &x, &y, &w, &h);
        if (g_cx >= x && g_cx < x + w && g_cy >= y && g_cy < y + h) {
          g_overview = 0;
          unminimize_focus(ring[i]);
          composite_scene();
          return;
        }
      }
      for (int i = 0; i < NAPPS; i++) {
        int x, y, w, h;
        app_card_rect(i, &x, &y, &w, &h);
        if (g_cx >= x && g_cx < x + w && g_cy >= y && g_cy < y + h) {
          launch_and_attach(i);
          return;
        }
      }
      /* click outside any card closes the overview */
      g_overview = 0;
      composite_scene();
      return;
    }
    if (g_cy < PANEL_H) {
      if (g_cx < 8 * 10 + 16) { /* the "Activities" button toggles the overview */
        g_overview = 1;
        composite_scene();
      }
      return; /* the panel swallows clicks; windows never see them */
    }
    /* §57: a press on a titlebar focuses + either closes (close box) or starts a
     * move drag (tinywl begin_interactive). Topmost titlebar wins. */
    for (int v = g_nviews - 1; v >= 0; v--) {
      struct surf *s = g_views[v];
      if (!s->mapped || s->minimized)
        continue;
      if (g_cx >= s->x && g_cx < s->x + s->w && g_cy >= s->y - TBH && g_cy < s->y) {
        focus_view(s);
        int b = tb_btn_hit(s, g_cx, g_cy); /* §93: min/max/close cells */
        if (b == BTN_CLOSE) {
          if (s->xdg_toplevel)
            xdg_toplevel_send_close(s->xdg_toplevel); /* ask the client to quit */
        } else if (b == BTN_MAX) {
          toggle_maximize(s);
        } else if (b == BTN_MIN) {
          minimize_view(s);
        } else { /* drag the bar → move (tinywl begin_interactive) */
          g_cursor_mode = MODE_MOVE;
          g_grab = s;
          g_grab_dx = g_cx - s->x;
          g_grab_dy = g_cy - s->y;
        }
        composite_scene();
        return;
      }
      /* §91: a press in the bottom-right corner grip starts a resize drag. */
      if (g_cx >= s->x + s->w - RESIZE_ZONE && g_cx < s->x + s->w &&
          g_cy >= s->y + s->h - RESIZE_ZONE && g_cy < s->y + s->h) {
        focus_view(s);
        g_cursor_mode = MODE_RESIZE;
        g_grab = s;
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
    if (g_cursor_mode == MODE_MOVE || g_cursor_mode == MODE_RESIZE) { /* end drag/resize */
      /* §93: dropping a moved window into a screen-edge zone tiles it. */
      if (g_cursor_mode == MODE_MOVE && g_grab) {
        if (g_cy <= PANEL_H + SNAP_ZONE)        snap_view(g_grab, 0); /* top → max */
        else if (g_cx <= SNAP_ZONE)             snap_view(g_grab, 1); /* left half */
        else if (g_cx >= g_w - SNAP_ZONE)       snap_view(g_grab, 2); /* right half */
      }
      /* §93b: interactive resize scales live during the drag; on release, tell the
       * client its final size so it re-renders sharp at that resolution. */
      if (g_cursor_mode == MODE_RESIZE && g_grab)
        send_configure(g_grab);
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
  update_tty_mute(); /* clicking to a non-terminal window mutes shell input */
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

/* §93b: tell the client its new on-screen size via xdg configure, so it re-renders
 * its buffer at that resolution (sharp) instead of the compositor up-scaling a
 * fixed buffer (blocky). oxui honors this — tl_configure resizes + repaints; until
 * the new buffer arrives the old one keeps scaling, so there's no flash. */
static void send_configure(struct surf *s)
{
  if (!s->xdg_toplevel || !s->xdg_surface)
    return;
  struct wl_array states;
  wl_array_init(&states);
  if (s->maximized) {
    uint32_t *st = wl_array_add(&states, sizeof(uint32_t));
    if (st) *st = XDG_TOPLEVEL_STATE_MAXIMIZED;
  }
  xdg_toplevel_send_configure(s->xdg_toplevel, s->w, s->h, &states);
  wl_array_release(&states);
  xdg_surface_send_configure(s->xdg_surface, ++g_serial);
}
/* §93: maximize to the work area (below the panel) or restore the saved geometry.
 * The titlebar lives in rows [y-TBH, y), so y=PANEL_H+TBH puts it flush under the
 * panel. §93b: a configure is sent so the client re-renders sharp at the new size. */
static void toggle_maximize(struct surf *s)
{
  if (!s->maximized) {
    s->sx = s->x; s->sy = s->y; s->sw = s->w; s->sh = s->h;
    s->maximized = 1;
    s->x = 0;
    s->y = PANEL_H + TBH;
    s->w = g_w;
    s->h = g_h - (PANEL_H + TBH);
  } else {
    s->x = s->sx; s->y = s->sy; s->w = s->sw; s->h = s->sh;
    s->maximized = 0;
  }
  send_configure(s);
  composite_scene();
}
/* §93: tile a dragged window — zone 0=top(maximize), 1=left half, 2=right half.
 * Saves the floating geom (only if not already tiled, so the restore target
 * survives a max→snap transition); the max button (a restore glyph now) un-tiles. */
static void snap_view(struct surf *s, int zone)
{
  if (!s->maximized) {
    s->sx = s->x; s->sy = s->y; s->sw = s->w; s->sh = s->h;
  }
  s->maximized = 1;
  int top = PANEL_H + TBH, hh = g_h - (PANEL_H + TBH);
  s->y = top;
  s->h = hh;
  if (zone == 1) { s->x = 0; s->w = g_w / 2; }
  else if (zone == 2) { s->x = g_w / 2; s->w = g_w - g_w / 2; }
  else { s->x = 0; s->w = g_w; } /* top → maximize */
  send_configure(s); /* §93b: re-render sharp at the tiled size */
  composite_scene();
}
/* The topmost mapped, non-minimized view other than `except` (for focus handoff). */
static struct surf *topmost_normal(struct surf *except)
{
  for (int v = g_nviews - 1; v >= 0; v--) {
    struct surf *s = g_views[v];
    if (s != except && s->mapped && !s->minimized)
      return s;
  }
  return NULL;
}
/* §93: hide a window from the desktop; it stays in g_views (gated by ->minimized)
 * and is reachable via the overview / Alt-Tab. Hands focus to the next window. */
static void minimize_view(struct surf *s)
{
  if (!s || s->minimized)
    return;
  s->minimized = 1;
  if (g_grab == s) { g_cursor_mode = MODE_PASSTHROUGH; g_grab = NULL; }
  if (g_ptr_view == s)
    g_ptr_view = NULL;
  if (g_focus_view == s) {
    if (s->surface) {
      struct seatc *osc = seat_for(wl_resource_get_client(s->surface));
      if (osc && osc->kbd)
        wl_keyboard_send_leave(osc->kbd, ++g_serial, s->surface);
    }
    g_focus_view = NULL; /* null first so focus_view() below isn't a no-op */
    struct surf *n = topmost_normal(s);
    if (n)
      focus_view(n);
    else
      update_tty_mute(); /* nothing left focused → restore shell input */
  }
  composite_scene();
  wake_unoccluded();
}
/* §93: restore a minimized window and give it focus (overview/Alt-Tab path). */
static void unminimize_focus(struct surf *s)
{
  if (!s)
    return;
  s->minimized = 0;
  focus_view(s);
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
    if (g_greeter) {
      /* §92: the greeter owns the keyboard — decode keystrokes itself and never
       * forward them to clients while the login screen is up. */
      greeter_key(buf[i]);
      continue;
    }
    /* §93: Alt-Tab window switcher — intercept Alt (0x38) + Tab (0x0f) HERE, before
     * the focus guard, so it works even with no client focused. These keys are
     * consumed (never forwarded), so clients can't see Alt chords. */
    unsigned int code = buf[i] & 0x7f;
    int          rel  = buf[i] & 0x80;
    if (code == 0x38) { /* left Alt */
      if (!rel) {
        g_alt_down = 1;
      } else {
        if (g_switching) {
          struct surf *ring[MAXVIEWS];
          int rn = switch_ring(ring);
          g_switching = 0;
          if (rn > 0)
            unminimize_focus(ring[((g_switch_index % rn) + rn) % rn]);
          composite_scene();
          /* focus may have changed → recompute the kbd target for later keys. */
          kbd = NULL;
          if (g_focus_view && g_focus_view->surface) {
            struct seatc *sc2 = seat_for(wl_resource_get_client(g_focus_view->surface));
            kbd = sc2 ? sc2->kbd : NULL;
          }
        }
        g_alt_down = 0;
      }
      continue;
    }
    if (g_alt_down && code == 0x0f) { /* Tab while Alt held → cycle the selection */
      if (!rel) {
        if (!g_switching) { g_switching = 1; g_switch_index = 1; } /* start on "previous" */
        else g_switch_index++;
        composite_scene();
      }
      continue;
    }
    /* §93: Super (Meta, evdev 125) — GNOME window management. A bare tap toggles the
     * Activities overview; Super+arrow tiles/maximizes/minimizes the focused window.
     * All consumed (clients never see Super chords). */
    if (code == 125) {
      if (!rel) {
        g_super_down = 1; g_super_used = 0;
      } else {
        if (g_super_down && !g_super_used) { /* bare tap → toggle Activities */
          g_overview = !g_overview;
          composite_scene();
        }
        g_super_down = 0;
      }
      continue;
    }
    if (g_super_down && (code == 103 || code == 108 || code == 105 || code == 106)) {
      if (!rel && g_focus_view) {
        if (code == 103) toggle_maximize(g_focus_view);      /* Super+Up    = maximize */
        else if (code == 108) minimize_view(g_focus_view);   /* Super+Down  = minimize */
        else if (code == 105) snap_view(g_focus_view, 1);    /* Super+Left  = tile left */
        else if (code == 106) snap_view(g_focus_view, 2);    /* Super+Right = tile right */
        g_super_used = 1;
      }
      continue; /* consume press AND release */
    }
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
  } else if (g_cursor_mode == MODE_RESIZE && g_grab) {
    /* §91: resizing changes the window extent — full redraw is simplest+correct. */
    composite_scene();
    wake_unoccluded();
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
      } else if (g_cursor_mode == MODE_RESIZE && g_grab) {
        int nw = g_cx - g_grab->x, nh = g_cy - g_grab->y; /* drag the SE corner */
        g_grab->w = nw < 140 ? 140 : nw;                  /* min window size */
        g_grab->h = nh < 70 ? 70 : nh;
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
void *comp_server_setup(int fd, int input_fd, int mouse_fd, int session_fd, unsigned int *fb,
                        int w, int h, int pitch_words)
{
  g_fb = fb;
  g_w = w;
  g_h = h;
  g_pitch_words = pitch_words;
  g_composited = 0;
  g_session_fd = session_fd; /* §92: the byte stream to the shell credential gate */
  g_cx = w / 2;
  g_cy = h / 2;

  /* §58: allocate the offscreen back buffer (same layout as the framebuffer) and
   * render the initial frame (empty desktop + cursor) through the flip path. */
  g_back = malloc((size_t)h * pitch_words * 4);
  composite_scene();

  struct wl_display *d = wl_display_create();
  if (!d)
    return NULL;
  g_display = d; /* §91: kept so a launcher click can attach a new client */
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
  /* §92: the session channel — a byte from the shell after the desktop is up means
   * `logout` (`L`); wake and re-show the greeter. */
  if (session_fd >= 0)
    wl_event_loop_add_fd(wl_display_get_event_loop(d), session_fd, WL_EVENT_READABLE,
                         on_session, d);
  /* §91: a 1-second timer to tick the panel clock (recomposite just the top bar). */
  g_clock_timer = wl_event_loop_add_timer(wl_display_get_event_loop(d), clock_tick, NULL);
  if (g_clock_timer)
    wl_event_source_timer_update(g_clock_timer, 1000);
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

/* §A4: 1 once the user has logged in (greeter dismissed). The Rust side spawns the X
 * session apps (twm + xeyes) only AFTER this flips, so the wayland greeter login isn't
 * starved by X-client traffic while the user is still typing credentials. */
int comp_server_logged_in(void)
{
  return !g_greeter;
}
