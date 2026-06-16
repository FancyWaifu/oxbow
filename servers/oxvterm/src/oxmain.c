/* vterm-test — feed terminal output (text + a colour escape) into libvterm and
 * read back the resulting screen grid, the way the terminal window will render
 * the shell's output (§50). */
#include <stdio.h>
#include <string.h>
#include <vterm.h>

int main(void)
{
  printf("[vterm-test] libvterm on oxbow\n");

  VTerm *vt = vterm_new(24, 80);
  if (!vt) {
    printf("[vterm-test] vterm_new failed\n");
    return 1;
  }
  vterm_set_utf8(vt, 1);

  VTermScreen *screen = vterm_obtain_screen(vt);
  vterm_screen_reset(screen, 1);

  /* Plain text, a SGR colour escape, more text, then a newline. */
  const char *in = "Hello, oxbow!\x1b[31mRED\x1b[0m\r\n";
  vterm_input_write(vt, in, strlen(in));

  /* Read row 0 back out of the grid. */
  char    row[81];
  int     j = 0;
  for (int col = 0; col < 80 && j < 80; col++) {
    VTermPos        pos = { .row = 0, .col = col };
    VTermScreenCell cell;
    vterm_screen_get_cell(screen, pos, &cell);
    if (cell.chars[0] == 0)
      break;
    row[j++] = (char)cell.chars[0];
  }
  row[j] = 0;
  printf("[vterm-test] row 0 = \"%s\"\n", row);

  /* Check the colour of the 'R' in RED (col 13). */
  VTermPos        rpos = { .row = 0, .col = 13 };
  VTermScreenCell rcell;
  vterm_screen_get_cell(screen, rpos, &rcell);
  printf("[vterm-test] cell 13 '%c' fg rgb = %d,%d,%d\n",
         (char)rcell.chars[0], rcell.fg.rgb.red, rcell.fg.rgb.green,
         rcell.fg.rgb.blue);

  vterm_free(vt);
  printf("[vterm-test] done\n");
  return 0;
}
