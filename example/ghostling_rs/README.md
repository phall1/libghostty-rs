# ghostling_rs

Rust port of [ghostling](https://github.com/ghostty-org/ghostling) — a minimal terminal emulator built on libghostty-vt and raylib.

Demonstrates: PTY management, keyboard/mouse input encoding, render state traversal, styled text rendering, scrollbar, and focus reporting.

The renderer embeds `JetBrainsMono-Regular.ttf` from `fonts/` and loads it at
HiDPI-aware pixel size to match upstream ghostling's font metrics and visual
output across retina and non-retina displays.

Raylib's default font-loader path only includes the standard printable ASCII
set when no explicit codepoints are supplied. This example therefore builds a
terminal-focused Unicode codepoint list (including box drawing, dingbats, and
Powerline private-use symbols) before creating the font atlas.
