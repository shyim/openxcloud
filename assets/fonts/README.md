# Glyph fallback

`DejaVuSans.ttf` is the unmodified DejaVu Sans 2.37 font from the upstream
[TTF release](https://github.com/dejavu-fonts/dejavu-fonts/releases/tag/version_2_37).
Its redistribution licence and copyright notices are in `DejaVu-LICENSE.txt`.

SHA-256: `7da195a74c55bef988d0d48f9508bd5d849425c1770dba5d7bfc6ce9ed848954`

`NotoSansCJKkr-Regular.otf` is the unmodified Korean regular font from
[Noto Sans CJK](https://github.com/notofonts/noto-cjk/blob/f8d157532fbfaeda587e826d4cd5b21a49186f7c/Sans/OTF/Korean/NotoSansCJKkr-Regular.otf),
pinned to commit `f8d157532fbfaeda587e826d4cd5b21a49186f7c`. It includes Hangul,
kana, and simplified/traditional Han characters. Its SIL Open Font License 1.1
is in `NotoSansCJK-LICENSE.txt`; copyright notices are embedded in the font.

SHA-256: `6bcb2a0703aa137e874fc2dffa85f6c21ba9a67fa329e81b8c801663af7e992a`

The theme embeds both after its primary and egui fonts in every family, including
named weight families and monospace. DejaVu supplies HUD arrows and Hebrew/Arabic;
Noto supplies Korean, Japanese, and Chinese. All selectable native locale names
are covered without depending on fonts installed on the host. The full CJK font
also covers localized game titles, at the cost of about 16 MB of embedded data.
