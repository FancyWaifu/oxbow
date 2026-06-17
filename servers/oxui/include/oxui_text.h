/* oxui_text — optional FreeType text helper for oxui (§64).
 *
 * An app that wants to draw text calls oxui_text(canvas, x, y, "hello", color)
 * and never touches FreeType. Backed by an embedded monospace font, initialised
 * lazily on first use. Link with the FreeType archive + the font include path.
 */
#ifndef OXUI_TEXT_H
#define OXUI_TEXT_H

#include "oxui.h"

/* Draw `str` into the canvas with its top-left at (x, baseline-relative y). Color
 * is 0x00RRGGBB; pixels are alpha-blended over whatever is already in the canvas.
 * Returns the x just past the drawn text. Newlines advance to the next line. */
int oxui_text(oxui_canvas c, int x, int y, const char *str, uint32_t color);

/* Line height (px) of the embedded font, for laying out multiple lines. */
int oxui_text_line_height(void);

#endif /* OXUI_TEXT_H */
