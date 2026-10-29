# Glyph fallback

`DejaVuSans.ttf` is the unmodified DejaVu Sans 2.37 font from the upstream
[TTF release](https://github.com/dejavu-fonts/dejavu-fonts/releases/tag/version_2_37).
Its redistribution licence and copyright notices are in `DejaVu-LICENSE.txt`.

The theme embeds it after its primary and egui fonts in every family, including
named weight families and monospace. It supplies HUD arrows and Hebrew/Arabic
language names without depending on fonts installed on the host. It is not a
universal Unicode font; other scripts, including CJK, still use platform fallbacks.

SHA-256: `7da195a74c55bef988d0d48f9508bd5d849425c1770dba5d7bfc6ce9ed848954`
