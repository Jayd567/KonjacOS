# Kernel assets

Raw pixel and glyph data baked into the kernel with `include_bytes!`. The
kernel has no image or font decoders, so everything here is decoded
offline. `tools/gen_desktop_assets.py` regenerates all of it except the
cursor; the script's docstring describes each file's layout.

| File | What | Source |
| --- | --- | --- |
| `cursor_arrow.rgba` | Mouse pointer | "Minimalistic Modern Cursor Set" by Dante Berlin (CC BY) |
| `wallpaper.rgb` | Desktop wallpaper, 1280x800 | Photo by [MagicPattern](https://unsplash.com/@magicpattern) on Unsplash ([Unsplash License](https://unsplash.com/license)) |
| `logo_k*.a8` | The "K" boot logo, three sizes | KonjacOS's own logo |
| `icons.kico` | Taskbar, window and file icons | [Fluent UI System Icons](https://github.com/microsoft/fluentui-system-icons), MIT, (c) Microsoft |
| `font_ui*.kfnt`, `font_small.kfnt`, `font_display.kfnt` | Interface text | [Inter](https://github.com/rsms/inter), SIL OFL 1.1 |
| `font_mono.kfnt` | Terminal text | [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono), SIL OFL 1.1 |

The full licence texts for the icons and fonts are in `licenses/`.
