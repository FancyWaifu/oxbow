/* oxbow: trimmed FreeType module list (§51) — only what a TrueType glyph
 * rasterizer needs: the TrueType driver + sfnt + psnames, the smooth (AA) and
 * mono renderers, and the autofitter for hinting. */
FT_USE_MODULE( FT_Module_Class, autofit_module_class )
FT_USE_MODULE( FT_Driver_ClassRec, tt_driver_class )
FT_USE_MODULE( FT_Module_Class, psnames_module_class )
FT_USE_MODULE( FT_Module_Class, sfnt_module_class )
FT_USE_MODULE( FT_Renderer_Class, ft_smooth_renderer_class )
FT_USE_MODULE( FT_Renderer_Class, ft_raster1_renderer_class )
