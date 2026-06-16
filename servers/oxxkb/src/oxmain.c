/* xkb-test — compile a real US keymap from a string (the full xkbcomp pipeline:
 * scanner, bison parser, keycodes/types/compat/symbols passes) and decode
 * keycodes → characters, exactly the path the desktop's keyboard will use (§48). */
#include <stdio.h>
#include <string.h>
#include <xkbcommon/xkbcommon.h>
#include "us_keymap.h"

int main(void)
{
  printf("[xkb-test] libxkbcommon on oxbow\n");

  /* No default includes: we compile a complete keymap from a string, so the
   * library never needs the on-disk xkb config tree. */
  struct xkb_context *ctx = xkb_context_new(XKB_CONTEXT_NO_DEFAULT_INCLUDES);
  if (!ctx) {
    printf("[xkb-test] xkb_context_new failed\n");
    return 1;
  }

  struct xkb_keymap *km = xkb_keymap_new_from_string(
      ctx, us_keymap, XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS);
  if (!km) {
    printf("[xkb-test] keymap compile FAILED\n");
    xkb_context_unref(ctx);
    return 1;
  }
  printf("[xkb-test] keymap compiled (%u-byte source)\n", (unsigned)sizeof us_keymap);

  struct xkb_state *st = xkb_state_new(km);

  /* evdev KEY_A=30, Wayland/xkb keycode = evdev + 8 = 38. Decode plain + shifted. */
  char buf[8];
  xkb_state_key_get_utf8(st, 38, buf, sizeof buf);
  printf("[xkb-test] key 38        -> '%s'\n", buf);

  /* press shift (KEY_LEFTSHIFT=42 -> 50), then A again -> 'A' */
  xkb_state_update_key(st, 50, XKB_KEY_DOWN);
  xkb_state_key_get_utf8(st, 38, buf, sizeof buf);
  printf("[xkb-test] shift+key 38  -> '%s'\n", buf);
  xkb_state_update_key(st, 50, XKB_KEY_UP);

  /* keycode 36 = evdev KEY_ENTER(28)+8 -> the Return keysym */
  xkb_keysym_t sym = xkb_state_key_get_one_sym(st, 36);
  printf("[xkb-test] key 36 sym    = %#x (Return=0xff0d)\n", sym);

  xkb_state_unref(st);
  xkb_keymap_unref(km);
  xkb_context_unref(ctx);
  printf("[xkb-test] done\n");
  return 0;
}
