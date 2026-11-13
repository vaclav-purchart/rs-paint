# Changelog

All notable changes to rs-paint are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and this project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Open images from the command line (`rs-paint file.png`) and, on macOS, from
  Finder / **Open With** / `open -a rs-paint file.png` (PNG, JPEG, BMP, GIF).
- If rs-paint is already running, the file opens in that window as a new tab and
  the window comes to the front instead of starting a second instance.
- Opening a file that's already open switches to its tab and reloads it from
  disk (previous content stays on the undo stack).

### Fixed
- The active tab is scrolled into view in the tab bar when it changes (e.g.
  after opening a file while many tabs are open).

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
