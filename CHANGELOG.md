# Changelog

All notable changes to rs-paint are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project uses
[Semantic Versioning](https://semver.org/).

## [1.0.1] - 2026-06-09

### Fixed
- `⌘/Ctrl+V` now correctly pastes images to other apps

## [1.0.0] - 2026-06-07

First stable release.

- **Tools:** pencil, brush, eraser, fill (flood, with tolerance + anti-aliased
  edges), eyedropper, line, arrow, rectangle, rounded rectangle, ellipse,
  polygon, curve, text, and rectangular selection.
- **Selection:** move, nudge (arrow keys), select-all (`⌘/Ctrl+A`), copy/cut/
  paste/delete, drag-to-resize handles (selection resample + canvas crop/extend).
- **Transparency:** transparent color swatch, eraser-to-transparent, transparent
  canvas, checkerboard display, alpha-correct save.
- **Anti-aliasing** toggle for smooth strokes/shapes (transparency-aware).
- **System clipboard** image copy/paste — `⌘V` works for images on macOS (with a
  text-marker workaround), and **Edit ▸ Paste** works everywhere.
- **Tabs** for multiple images — open/new create tabs, `⌘/Ctrl+T` / `⌘/Ctrl+W`,
  unsaved indicators, Save All, paste-to-new-tab.
- **View:** continuous zoom (`⌘+`/`⌘-`/`⌘0`, fit-to-window, `⌘`+scroll/pinch),
  middle-mouse / space-drag pan, mipmapped zoom-out, pixel grid (`⌘G`),
  light/dark/system theme.
- **Files:** open/save PNG, JPEG, BMP, GIF; drag-and-drop to open; unsaved-changes
  prompts on New/Open/Close/Quit; settings and open tabs persisted between runs.
- **Packaging:** macOS `.app` bundle with icon, ad-hoc/Developer-ID signing and
  notarization scripts; Windows `.exe` icon embedded via `build.rs`.
