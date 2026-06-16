/* ft-test — initialise FreeType (compile+link smoke test; glyph rasterization
 * once a font is embedded). */
#include <stdio.h>
#include <ft2build.h>
#include FT_FREETYPE_H

int main(void)
{
  printf("[ft-test] FreeType on oxbow\n");
  FT_Library lib;
  FT_Error err = FT_Init_FreeType(&lib);
  if (err) {
    printf("[ft-test] FT_Init_FreeType failed: %d\n", err);
    return 1;
  }
  FT_Int major, minor, patch;
  FT_Library_Version(lib, &major, &minor, &patch);
  printf("[ft-test] FreeType %d.%d.%d initialised\n", major, minor, patch);
  FT_Done_FreeType(lib);
  printf("[ft-test] done\n");
  return 0;
}
