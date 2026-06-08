# rs-paint

A lightweight **MS Paint clone** for the desktop, built with **Rust + [egui](https://github.com/emilk/egui)**.

It compiles to a single self-contained binary (~3.7 MB) with no runtime
dependencies — no Electron, no system GUI framework to install. The whole
drawing surface is a single RGBA pixel buffer that tools mutate directly and
that is re-uploaded to a GPU texture only when it changes.

Primarily developed for macOS, but the code is cross-platform (Windows / Linux)
because egui is.

## Features

**Tools:** Pencil · Brush · Eraser · Fill (flood) · Eyedropper · Line ·
Rectangle · Rounded Rectangle · Ellipse · Polygon · Curve (cubic Bézier) ·
Text · Rectangular Select.

- Foreground & background colors, classic 28-swatch palette + full color pickers.
- Adjustable brush/outline size, fill-shape toggle, corner radius, font size.
- Rectangular **selection** with move, and **clipboard** (cut / copy / paste / delete).
- **System clipboard** for images — copy a selection and paste it into another
  app (or paste an image/screenshot copied from elsewhere straight onto the canvas).
- **Drag-to-resize handles** (Select tool): 8 handles resize/resample a selection;
  the right/bottom/corner handles resize the canvas itself (crop or extend).
- **Swap foreground/background** colors (button, the `X` key, or click the
  color indicator).
- **Transparency** — pick the **transparent swatch** in the palette (left-click =
  foreground, right-click = background) to draw/fill with transparency, the eraser
  clears to transparent, **Delete** on a selection leaves transparency, New offers a
  transparent canvas, and transparent areas show a checkerboard. Saved as RGBA
  (PNG keeps the alpha).
- **Undo / redo** (40 steps), zoom from **25% to 800%**.
- **Image ▸ Resize** (resample to a new size, optional aspect lock) and
  **Image ▸ Crop to Selection**.
- **Brush-preview ring** that follows the cursor and matches the current size.
- **Status bar** showing cursor position, canvas size, zoom, brush size, and
  live selection dimensions.
- **Tabs** for multiple images at once — Open/New create a tab, `⌘T` opens a
  blank tab, `⌘W` closes one (prompting to save if needed), each tab shows a `•`
  when unsaved and a `×` to close, **File ▸ Save All** saves them all, and Paste
  asks whether to drop the image into the current tab or a new one.
- **Light / dark theme** that follows the system, with a manual override in
  **View ▸ Theme** (System / Light / Dark). Tool icons are drawn as crisp
  vectors (no icon-font dependency) and recolor with the theme.
- **Remembers your settings** between runs — selected tool, foreground/background
  colors, brush size, fill mode, corner radius, font size, zoom, theme, window size,
  and the **open tabs** (file-backed ones are reopened on next launch).
- Open & save **PNG, JPEG, BMP, GIF** via native file dialogs.

## Requirements

- **Rust** (stable) with `cargo` — install via [rustup](https://rustup.rs).
  Developed with Rust 1.96.
- **macOS:** the `.app` bundling step (`packaging/bundle.sh`) uses `sips` and
  `iconutil`, which ship with macOS. Building/running the binary itself needs
  nothing extra.
- **Linux:** standard build tools plus the usual windowing/GL dev packages
  (e.g. `libxcb`, `libxkbcommon`, `libgl`) that `eframe`/`winit` require.

## Compile

```bash
# Debug build (faster to compile)
cargo build

# Optimized release build (recommended) -> target/release/rs-paint
cargo build --release
```

The release profile is tuned for a small, fast binary (LTO, `strip`,
`panic = "abort"`, single codegen unit) in `Cargo.toml`.

## Run

```bash
# Build & run in one step
cargo run --release

# Or run the already-built binary directly
./target/release/rs-paint
```

## Build a macOS `.app` (with icon)

```bash
./packaging/bundle.sh        # produces ./rs-paint.app
open rs-paint.app            # or drag it into /Applications
```

The script builds the release binary, regenerates the icon
(`cargo run --release --example gen_icon`), converts it to `AppIcon.icns`
via `sips` + `iconutil`, and assembles the bundle.

`bundle.sh` **signs** the app: it uses a "Developer ID Application" certificate
if one is in your keychain (set `CODESIGN_ID` to force a specific one), otherwise
it applies an **ad-hoc** signature (fine for running on your own Mac).

### Distributing to other Macs (trusted, no Gatekeeper warning)

Requires an [Apple Developer Program](https://developer.apple.com/programs/)
membership ($99/yr) and a *Developer ID Application* certificate. Then notarize
and staple in one step:

```bash
# one-time: store notary credentials in the keychain
xcrun notarytool store-credentials rspaint-notary \
  --apple-id "you@example.com" --team-id "TEAMID" --password "app-specific-pw"

./packaging/notarize.sh        # builds, signs, notarizes, staples
```

Without a Developer ID, an ad-hoc/unsigned app can still be opened on another Mac
by right-clicking ▸ **Open**, or clearing quarantine:
`xattr -dr com.apple.quarantine rs-paint.app`

## Controls

- **Left-click = foreground color, right-click = background color** — on both the
  palette swatches and the canvas (right-drag paints with the background color).
- **Text:** click to place, type in the popup; **drag on the canvas to reposition**
  the text while editing (a blue caret marks the anchor). Place commits it.
- **Polygon / Curve:** click to add points; `Enter` or double-click to finish,
  `Esc` to cancel.
- **Select:** drag to select, drag inside the selection to move it;
  `⌘C` / `⌘X` / `⌘V` copy / cut / paste via the system clipboard, `Delete` clears.
- **Paste** drops the clipboard image onto the canvas as a movable selection.
  If it's bigger than the canvas, rs-paint asks whether to **enlarge the canvas**
  or **clip** the pasted image.
- **Tool hotkeys:** `P` pencil · `B` brush · `E` eraser · `F` fill · `I` pick ·
  `L` line · `A` arrow · `R` rectangle · `U` rounded · `O` ellipse · `Y` polygon ·
  `C` curve · `T` text · `M` select. (Shown in each tool's tooltip.)
- **Hold Shift** while dragging for straight/45° lines, perfect squares & circles.
- **Hold Alt** to temporarily pick the color under the cursor, then keep painting.
- **`[` / `]`** shrink/grow the brush. **Drag an image file** onto the window to open it.
- **Selection:** `⌘A` select all; arrow keys nudge by 1px.
- **Zoom/pan:** `⌘+` / `⌘−` / `⌘0`, **Fit to window** (View menu), `⌘`+scroll to zoom
  toward the cursor, **middle-mouse drag** or **Space-drag** to pan. Zooming in stays
  pixel-crisp; zooming out is smoothed (mipmapped) so thin lines/text don't drop out.
- **Shortcuts:** `⌘Z` undo · `⌘⇧Z`/`⌘Y` redo · `⌘S` save · `⌘O` open · `⌘N` new ·
  `⌘E` resize · `⌘G` grid · `X` swap colors.
- **Grid** (`⌘G`) overlays pixel gridlines; visible when zoomed to 300%+.
- **Anti-aliasing** (View menu) — when on, new strokes/shapes get smooth edges
  (composited correctly, including over transparent backgrounds and with the
  eraser). On by default.
- **Fill** has a **Tolerance** slider and an anti-aliased boundary: it feathers
  into a stroke's soft edge so the fill blends with the drawing (no leftover halo
  of the previous background color).
- The window title shows the file name and a `•` when there are unsaved changes;
  New/Open prompt to save first.
- **Quitting** with unsaved changes prompts to Save / Don't Save / Cancel.
- **Zoom:** 25%–800% from the View menu.

> On Windows/Linux use `Ctrl` in place of `⌘`.
>
> **Paste note:** the windowing layer (egui/winit) only forwards `⌘V`/`Ctrl+V`
> to the app when the clipboard holds *text*. rs-paint works around this on
> **macOS** and **Windows**: it puts a small text marker on the clipboard when it
> copies an image, and when the clipboard changes to an image with no text (e.g.
> a screenshot from another app) it re-publishes that image with a marker — so
> `⌘V`/`Ctrl+V` pastes images in both cases. On **Linux**, use **Edit ▸ Paste**
> for image-only clipboards.

## Windows

`cargo build --release` produces `rs-paint.exe`. A `build.rs` embeds
`assets/icon.ico` into the executable (via `winresource`), so Explorer shows the
app icon on the `.exe` itself — both when building on Windows (MSVC `rc.exe`) and
when cross-compiling (needs `llvm-rc` or `x86_64-w64-mingw32-windres` on PATH).
Regenerate the icon with `cargo run --release --example gen_icon`.

## Project layout

```
.
├── Cargo.toml            # crate + release-profile settings
├── src/main.rs           # the whole application
├── examples/gen_icon.rs  # SDF icon generator -> assets/icon_1024.png
├── packaging/
│   ├── bundle.sh         # build + assemble rs-paint.app
│   └── Info.plist        # macOS bundle metadata
├── assets/               # generated icon (icon_1024.png, AppIcon.icns)
└── README.md
```

## License

No license specified yet — add one (e.g. MIT/Apache-2.0) before distributing.
