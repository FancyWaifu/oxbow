# oxbow UI toolkit — scope ("a program shouldn't implement rendering")

Status: **scoping only**. Goal: a program that wants a window calls something like
`win = oxui_window("Title", 720, 400)` and gets a pixel buffer + an event callback — it
never touches Wayland, shm, memfd, xkb, double-buffering, frame pacing, or the close box.
All of that lives in one backend library.

## The problem this solves
`servers/oxterm/src/term.c` and `servers/wlclient/src/simple-shm.c` are ~1000 lines each,
and the **vast majority is identical boilerplate**: connect to the compositor, bind
`wl_compositor`/`wl_shm`/`xdg_wm_base`/`wl_seat`, create the surface + xdg_toplevel, the
two-shm-buffer pool with release tracking, the `prepare_read`/`poll`/dispatch loop, xkb
keymap compilation, the deferred-redraw + dirty logic we just debugged in §63. Every new
GUI program would re-copy all of it (and re-introduce the same bugs — e.g. the
double-buffer abort, the budget exhaustion).

The library makes that boilerplate **write-once**. The §63 fixes (event-driven loop, dirty
deferral, buffer cycling) become library invariants instead of per-app footguns.

## Proposed API (C first, since the stack is C)

```c
typedef struct oxui_window oxui_window;

typedef struct {
    int        width, height;
    uint32_t  *pixels;     /* XRGB8888, width*height; draw here */
} oxui_canvas;

typedef struct {
    enum { OXUI_KEY, OXUI_POINTER, OXUI_RESIZE, OXUI_CLOSE } type;
    /* key: keysym + utf8 + pressed; pointer: x,y,buttons; resize: w,h */
    ...
} oxui_event;

oxui_window *oxui_window_create(const char *title, int w, int h);
oxui_canvas  oxui_begin_frame(oxui_window *);   /* get a free buffer to paint */
void         oxui_commit(oxui_window *);         /* present what you painted */
/* Blocking event pump: calls your handler for input/resize/close, and your
 * draw callback only when the app marked itself dirty or was resized. */
int          oxui_run(oxui_window *, oxui_handlers *, void *user);
void         oxui_request_redraw(oxui_window *); /* "I changed, repaint me" */
void         oxui_window_destroy(oxui_window *);
```

A client becomes, in full:
```c
static void on_draw(oxui_canvas c, void *u) { /* fill c.pixels */ }
static void on_key (oxui_event e, void *u)  { /* react, maybe oxui_request_redraw */ }
int main(void){ oxui_window *w = oxui_window_create("hi", 640,480);
                oxui_run(w, &(oxui_handlers){.draw=on_draw,.key=on_key}, NULL); }
```
No Wayland in sight.

## What the library owns (the extracted boilerplate)
- Connection + global binding (compositor/shm/xdg/seat), xdg surface+toplevel, the close
  handler → `OXUI_CLOSE`.
- The shm buffer pool: N buffers, release tracking, **deferred redraw when all busy**
  (the §63 fix), size-change reallocation only on real resize.
- The event loop: block on the Wayland fd (+ any extra fds the app registers, e.g. a
  terminal's tty), dispatch, and only invoke `draw` when dirty/resized — so apps are
  event-driven and idle-cheap by construction (§63).
- xkb: keymap compile + keysym/UTF-8 decode → `OXUI_KEY` (apps never see scancodes).
- The memory budget gotcha (size buffers to fit; double-buffer by default).

## What it deliberately does NOT do (yet)
- No widgets (buttons, text fields). That's a *second* layer on top
  (`oxui_label`/`oxui_button` drawing into the canvas) — scope separately.
- No GL/GPU. Pure CPU pixel buffers (matches the current fb/shm model).
- Text rendering: offer an optional `oxui_text(canvas, x, y, str)` backed by the FreeType
  glue oxterm already has, so apps get fonts without re-wiring FreeType.

## Migration / proof
1. Build `liboxui` extracting the boilerplate from `simple-shm.c`.
2. **Rewrite `wlclient` (the rings demo) on top of it** — should shrink to ~50 lines and
   render identically. That's the proof the API is sufficient.
3. Rewrite `oxterm` on it (its terminal-specific parts — vterm + the tty fd + FreeType —
   become the app; the window/buffer/loop plumbing goes to the library).
4. Then any new app (a clock, a file viewer) is trivial.

## Packaging
A C library crate under `servers/` (e.g. `servers/oxui/`) built with the same C-port
harness, plus a thin Rust wrapper later if we want Rust GUI apps. Header in
`servers/oxui/include/oxui.h`. ABI section to follow when it lands.

## Relationship to SMP
Independent. The toolkit is a userspace client library; SMP is a kernel arc. They don't
block each other — the toolkit can land first and immediately makes every future GUI app
cheap to write.
