# What was changed in this copy of iced_tiny_skia 0.14.1

This is the unmodified `iced_tiny_skia` 0.14.1 (MIT licence, see `LICENSE`) with the changes below.
It is used through `[patch.crates-io]` in the top-level `Cargo.toml`. Remove the patch (and this
folder) to go back to the published crate; the program still works, only scrolling gets slower.

1. `src/engine.rs`, `draw_quad`: a box that sticks out of its scroll area (for example a tall card
   while scrolling) was always filled through a clip mask, which is about ten times slower than a
   plain fill. The new function `visible_part` builds the outline of just the visible part and
   fills that without a mask. When the clip cuts through a rounded corner the old way is used.
   The clip mask is filled without smoothing, so the clip edges are rounded to whole pixels to
   give the same picture.
2. `src/lib.rs`: nothing else is changed.

Measured with the program's own screens, optimized build, window 1366 x 700 drawn at 200 % scale
(so four times the pixels of a normal screen), whole frame including the test's screenshot copy,
same machine, same program, only this change switched on or off:

| Screen | Before | After |
| --- | --- | --- |
| Add-account form, manual settings (tall) | 38.5 ms | 15.5 ms |
| Same form in a small window | 28.0 ms | 8.2 ms |
| Message reader | 27.9 ms | 17.6 ms |
| Settings | 18.1 ms | 16.4 ms |

Checked for identical output: all 36 screen pictures of the program drawn with and without the
change differ by at most 1 out of 255 on a few edge pixels (and by the text that shows a
temporary folder name on the Settings screen).
