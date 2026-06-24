/* doomgeneric platform layer for oxbow — renders DOOM into an oxui window.
 *
 * doomgeneric factors DOOM so a host implements ~6 DG_* hooks + a main loop. oxbow's
 * oxui toolkit hands us an XRGB8888 canvas + draw/key callbacks and hides all Wayland/
 * shm/xkb/frame-pacing. So the integration is: drive doomgeneric_Tick() once per oxui
 * frame (animate mode); DG_DrawFrame (called inside Tick) blits DG_ScreenBuffer to the
 * current canvas; the oxui key callback feeds DOOM's key queue. The WAD is read with
 * stdio from /doom1.wad. */
#include "doomgeneric.h"
#include "doomkeys.h"
#include "oxui.h"
#include <string.h>
#include <stdint.h>
#include <stdarg.h>

extern unsigned long ox_uptime_ms(void); /* oxbow-libc: monotonic ms since boot */
extern void ox_dbg(const char *p, unsigned long len); /* direct console write (Rust) */
static void dbg(const char *s)
{
	unsigned long n = 0;
	while (s[n]) n++;
	ox_dbg(s, n);
}

/* --- libc gap-fills (oxbow-libc lacks these; DOOM uses them only on non-critical
 *     config/error paths). --- */

/* mkdir: DOOM makes a savegame dir; running (not saving) doesn't need it. */
int mkdir(const char *path, unsigned mode)
{
	(void)path;
	(void)mode;
	return 0;
}

/* system: only reached by i_system's zenity error-box probe; report "unavailable". */
int system(const char *cmd)
{
	(void)cmd;
	return -1;
}

/* A minimal sscanf covering DOOM's uses: a single integer conversion (%d/%i/%x/%X/%o)
 * with optional leading whitespace + literal prefix chars (e.g. " 0x%x"). Returns the
 * number of items matched. */
static int dg_isspace(int c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r'; }
static int dg_digit(int c, int base)
{
	int v;
	if (c >= '0' && c <= '9') v = c - '0';
	else if (c >= 'a' && c <= 'f') v = c - 'a' + 10;
	else if (c >= 'A' && c <= 'F') v = c - 'A' + 10;
	else return -1;
	return v < base ? v : -1;
}
int sscanf(const char *s, const char *fmt, ...)
{
	va_list ap;
	va_start(ap, fmt);
	int count = 0;
	const char *p = s;
	while (*fmt) {
		if (dg_isspace((unsigned char)*fmt)) {
			while (dg_isspace((unsigned char)*p)) p++;
			fmt++;
			continue;
		}
		if (*fmt != '%') {
			if (*p != *fmt) break; /* literal must match */
			p++;
			fmt++;
			continue;
		}
		fmt++;                 /* past '%' */
		char conv = *fmt++;    /* d/i/x/X/o */
		while (dg_isspace((unsigned char)*p)) p++;
		int base, neg = 0;
		if (conv == 'x' || conv == 'X') base = 16;
		else if (conv == 'o') base = 8;
		else if (conv == 'i') base = 0; /* auto-detect */
		else if (conv == 'd') base = 10;
		else break;            /* unsupported conversion */
		if (*p == '+' || *p == '-') { neg = (*p == '-'); p++; }
		if (base == 0) {
			if (p[0] == '0' && (p[1] == 'x' || p[1] == 'X')) { base = 16; p += 2; }
			else if (p[0] == '0') { base = 8; p++; }
			else base = 10;
		} else if (base == 16 && p[0] == '0' && (p[1] == 'x' || p[1] == 'X')) {
			p += 2;            /* optional 0x prefix */
		}
		long val = 0;
		int any = 0, d;
		while ((d = dg_digit((unsigned char)*p, base)) >= 0) { val = val * base + d; p++; any = 1; }
		if (!any) break;
		int *out = va_arg(ap, int *);
		*out = (int)(neg ? -val : val);
		count++;
	}
	va_end(ap);
	return count;
}

/* The oxui canvas for the frame currently being painted (set before each Tick). */
static oxui_canvas g_canvas;
static int g_have_canvas = 0;
static oxui_window *g_win = 0;

/* DOOM key queue (pressed<<8 | doomkey), drained by DG_GetKey. */
#define KQ 64
static unsigned short kq[KQ];
static int kq_r = 0, kq_w = 0;

void DG_Init(void) {}

/* Blit DOOM's 32-bit framebuffer to the oxui canvas. With the window created at
 * DOOMGENERIC_RESX x DOOMGENERIC_RESY the canvas matches 1:1 (a straight copy); if the
 * compositor handed us a different size we copy the overlapping region row by row. */
void DG_DrawFrame(void)
{
	if (!g_have_canvas)
		return;
	int cw = g_canvas.width, ch = g_canvas.height;
	if (cw == DOOMGENERIC_RESX && ch == DOOMGENERIC_RESY) {
		memcpy(g_canvas.pixels, DG_ScreenBuffer,
		       (size_t)DOOMGENERIC_RESX * DOOMGENERIC_RESY * 4);
		return;
	}
	int rows = ch < DOOMGENERIC_RESY ? ch : DOOMGENERIC_RESY;
	int cols = cw < DOOMGENERIC_RESX ? cw : DOOMGENERIC_RESX;
	for (int y = 0; y < rows; y++)
		memcpy(g_canvas.pixels + (size_t)y * cw,
		       DG_ScreenBuffer + (size_t)y * DOOMGENERIC_RESX,
		       (size_t)cols * 4);
}

void DG_SleepMs(uint32_t ms) { (void)ms; /* oxui's frame callback paces us */ }

uint32_t DG_GetTicksMs(void) { return (uint32_t)ox_uptime_ms(); }

int DG_GetKey(int *pressed, unsigned char *doomKey)
{
	if (kq_r == kq_w)
		return 0;
	unsigned short d = kq[kq_r];
	kq_r = (kq_r + 1) % KQ;
	*pressed = d >> 8;
	*doomKey = d & 0xff;
	return 1;
}

void DG_SetWindowTitle(const char *title) { (void)title; }

/* Map an xkb keysym (what oxui delivers) to a DOOM key code. */
static unsigned char keysym_to_doom(uint32_t ks)
{
	switch (ks) {
	case 0xff52: return KEY_UPARROW;   /* XKB_KEY_Up        */
	case 0xff54: return KEY_DOWNARROW; /* XKB_KEY_Down      */
	case 0xff51: return KEY_LEFTARROW; /* XKB_KEY_Left      */
	case 0xff53: return KEY_RIGHTARROW;/* XKB_KEY_Right     */
	case 0xff0d: return KEY_ENTER;     /* XKB_KEY_Return    */
	case 0xff1b: return KEY_ESCAPE;    /* XKB_KEY_Escape    */
	case 0xff08: return KEY_BACKSPACE; /* XKB_KEY_BackSpace */
	case 0xff09: return KEY_TAB;       /* XKB_KEY_Tab       */
	case 0x0020: return KEY_USE;       /* space = use/open  */
	case 0xffe3: case 0xffe4: return KEY_FIRE;   /* Ctrl  = fire   */
	case 0xffe9: case 0xffea: return KEY_RALT;   /* Alt   = strafe */
	case 0xffe1: case 0xffe2: return KEY_RSHIFT; /* Shift = run    */
	default:
		if (ks >= 'A' && ks <= 'Z') return (unsigned char)(ks + 32); /* lower */
		if (ks >= 0x20 && ks < 0x7f) return (unsigned char)ks;       /* ascii */
		return 0;
	}
}

static void on_key(oxui_window *w, uint32_t keysym, int pressed, void *u)
{
	(void)w; (void)u;
	unsigned char dk = keysym_to_doom(keysym);
	if (!dk)
		return;
	int n = (kq_w + 1) % KQ;
	if (n != kq_r) { /* drop on overflow */
		kq[kq_w] = (unsigned short)((pressed ? 1 : 0) << 8 | dk);
		kq_w = n;
	}
}

/* oxui animate-mode paint: run one DOOM frame; DG_DrawFrame blits into this canvas. */
static void draw(oxui_window *w, oxui_canvas c, void *u)
{
	(void)u;
	g_win = w;
	g_canvas = c;
	g_have_canvas = 1;
	doomgeneric_Tick();
}

int main(int argc, char **argv)
{
	dbg("[doom] starting on oxbow\n");
	/* Default to the shareware IWAD at /doom1.wad when no args are given. */
	static char *def[] = {"doom", "-iwad", "/doom1.wad", 0};
	if (argc < 2) {
		argc = 3;
		argv = def;
	}
	/* The compositor hands DOOM its filesystem cap on slot 1 (BOOT_EP, so doomgeneric
	 * opens /doom1.wad via stdio), the console on slot 2 (BOOT_CONSOLE — oxbow-libc's
	 * stdout), and the Wayland socket on slot 4; point oxui at slot 4. */
	extern int oxui_wl_slot;
	oxui_wl_slot = 4;
	doomgeneric_Create(argc, argv); /* load the WAD + init the engine */
	g_win = oxui_window_create("DOOM", DOOMGENERIC_RESX, DOOMGENERIC_RESY);
	if (!g_win)
		return 1;
	oxui_handlers h = {0};
	h.draw = draw;     /* each frame: doomgeneric_Tick() -> DG_DrawFrame blits the canvas */
	h.key = on_key;    /* keyboard -> DOOM key queue */
	h.animate = 1;     /* run the game loop continuously */
	oxui_run(g_win, &h, 0);
	return 0;
}
