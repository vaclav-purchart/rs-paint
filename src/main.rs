#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! rs-paint — a lightweight MS Paint clone built with Rust + egui.
//!
//! The drawing surface is a single `ColorImage` (an RGBA pixel buffer). Tools
//! mutate that buffer directly; we re-upload it to a GPU texture whenever it
//! changes. Layout mirrors classic MS Paint: menu bar on top, toolbox on the
//! left, color palette on the bottom, canvas in the center.

use eframe::egui;
use egui::{
    Color32, ColorImage, FontId, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};
use std::path::PathBuf;

const DEFAULT_W: usize = 800;
const DEFAULT_H: usize = 600;
const MAX_UNDO: usize = 40;
const CLIP_MARKER: &str = "rs-paint-image";

/// Texture sampling for the canvas: crisp (nearest) when zoomed in, but smooth
/// (linear + mipmaps) when zoomed out so thin lines/text don't drop out.
fn canvas_tex_options() -> TextureOptions {
    TextureOptions {
        magnification: egui::TextureFilter::Nearest,
        minification: egui::TextureFilter::Linear,
        wrap_mode: egui::TextureWrapMode::ClampToEdge,
        mipmap_mode: Some(egui::TextureFilter::Linear),
    }
}

type Pt = (i32, i32);

/// Append a text marker to the system pasteboard *without* clearing the image
/// already on it. egui/winit only forwards Cmd+V to the app when the clipboard
/// holds text, so this makes image paste reachable from the keyboard too.
#[cfg(target_os = "macos")]
fn mac_add_text_marker(marker: &str) {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::{NSArray, NSString};
    // SAFETY: plain AppKit pasteboard calls, run on the UI (main) thread.
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        let ty = NSPasteboardTypeString;
        let types = NSArray::from_slice(&[ty]);
        pb.addTypes_owner(&types, None);
        pb.setString_forType(&NSString::from_str(marker), ty);
    }
}

/// Monotonic pasteboard change counter — cheap way to detect that the clipboard
/// changed (including changes made by other apps) without decoding its contents.
#[cfg(target_os = "macos")]
fn mac_change_count() -> isize {
    use objc2_app_kit::NSPasteboard;
    NSPasteboard::generalPasteboard().changeCount() as isize
}

/// A cheap monotonic clipboard "version" used to detect changes without
/// decoding the contents.
#[cfg(target_os = "macos")]
fn clipboard_seq() -> isize {
    mac_change_count()
}
#[cfg(target_os = "windows")]
fn clipboard_seq() -> isize {
    clipboard_win::seq_num().map(|n| n.get() as isize).unwrap_or(0)
}

/// Encode a ColorImage as BMP bytes (for the Windows clipboard).
#[cfg(target_os = "windows")]
fn encode_bmp(img: &ColorImage) -> Option<Vec<u8>> {
    let [w, h] = img.size;
    let mut bytes = Vec::with_capacity(w * h * 4);
    for px in &img.pixels {
        bytes.extend_from_slice(&px.to_srgba_unmultiplied());
    }
    let rgba = image::RgbaImage::from_raw(w as u32, h as u32, bytes)?;
    let mut out = std::io::Cursor::new(Vec::new());
    rgba.write_to(&mut out, image::ImageFormat::Bmp).ok()?;
    Some(out.into_inner())
}

/// Windows: put an image AND a text marker on the clipboard in one session, so
/// Ctrl+V is delivered to the app (egui/winit only forwards it with text).
#[cfg(target_os = "windows")]
mod win_clip {
    use clipboard_win::{formats, Clipboard, Setter};

    pub fn set_image_and_marker(bmp: &[u8], marker: &str) {
        if let Ok(_clip) = Clipboard::new_attempts(10) {
            let _ = clipboard_win::empty();
            // Pass &&[u8] / &&str so the generic `T` is the *sized* &[u8]/&str
            // (write_clipboard takes &T where T: Sized).
            let _ = formats::Bitmap.write_clipboard(&bmp);
            let _ = formats::Unicode.write_clipboard(&marker);
        }
    }

    /// Image only, no text marker (so other apps get a clean image).
    pub fn set_image(bmp: &[u8]) {
        if let Ok(_clip) = Clipboard::new_attempts(10) {
            let _ = clipboard_win::empty();
            let _ = formats::Bitmap.write_clipboard(&bmp);
        }
    }
}

/// macOS Quit (Cmd+Q / app menu) goes through the app delegate's
/// `applicationShouldTerminate:`, which winit doesn't implement — so the app
/// would terminate without our unsaved-changes prompt. We install that method
/// on winit's delegate, cancel the termination, and let the app handle quitting.
#[cfg(target_os = "macos")]
mod mac_quit {
    use objc2::runtime::{AnyObject, Imp, Sel};
    use objc2::{sel, MainThreadMarker};
    use objc2_app_kit::NSApplication;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    pub static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static CTX: Mutex<Option<egui::Context>> = Mutex::new(None);

    unsafe extern "C-unwind" fn should_terminate(
        _this: *mut AnyObject,
        _cmd: Sel,
        _sender: *mut AnyObject,
    ) -> isize {
        QUIT_REQUESTED.store(true, Ordering::SeqCst);
        if let Ok(g) = CTX.lock() {
            if let Some(ctx) = g.as_ref() {
                ctx.request_repaint();
            }
        }
        0 // NSTerminateCancel — we drive quitting ourselves
    }

    /// Install the hook once (retries until winit's delegate exists).
    pub fn try_install(ctx: &egui::Context) {
        if INSTALLED.load(Ordering::Relaxed) {
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        let Some(delegate) = app.delegate() else {
            return;
        };
        *CTX.lock().unwrap() = Some(ctx.clone());
        let obj: &AnyObject = (*delegate).as_ref();
        let cls = obj.class();
        let imp: Imp = unsafe {
            std::mem::transmute(
                should_terminate
                    as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> isize,
            )
        };
        unsafe {
            objc2::ffi::class_addMethod(
                cls as *const _ as *mut _,
                sel!(applicationShouldTerminate:),
                imp,
                c"q@:@".as_ptr(),
            );
        }
        INSTALLED.store(true, Ordering::Relaxed);
    }
}

#[derive(PartialEq, Eq, Clone, Copy, serde::Serialize, serde::Deserialize)]
enum Tool {
    Pencil,
    Brush,
    Eraser,
    Fill,
    Eyedropper,
    Line,
    Arrow,
    Rectangle,
    RoundedRect,
    Ellipse,
    Polygon,
    Curve,
    Text,
    Select,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Pencil => "Pencil",
            Tool::Brush => "Brush",
            Tool::Eraser => "Eraser",
            Tool::Fill => "Fill",
            Tool::Eyedropper => "Pick",
            Tool::Line => "Line",
            Tool::Arrow => "Arrow",
            Tool::Rectangle => "Rectangle",
            Tool::RoundedRect => "Rounded",
            Tool::Ellipse => "Ellipse",
            Tool::Polygon => "Polygon",
            Tool::Curve => "Curve",
            Tool::Text => "Text",
            Tool::Select => "Select",
        }
    }

    const ALL: [Tool; 14] = [
        Tool::Select,
        Tool::Pencil,
        Tool::Brush,
        Tool::Eraser,
        Tool::Fill,
        Tool::Eyedropper,
        Tool::Line,
        Tool::Arrow,
        Tool::Rectangle,
        Tool::RoundedRect,
        Tool::Ellipse,
        Tool::Polygon,
        Tool::Curve,
        Tool::Text,
    ];

    /// Single-key shortcut (no modifier) and its display label.
    fn hotkey(self) -> (egui::Key, &'static str) {
        use egui::Key;
        match self {
            Tool::Pencil => (Key::P, "P"),
            Tool::Brush => (Key::B, "B"),
            Tool::Eraser => (Key::E, "E"),
            Tool::Fill => (Key::F, "F"),
            Tool::Eyedropper => (Key::I, "I"),
            Tool::Line => (Key::L, "L"),
            Tool::Arrow => (Key::A, "A"),
            Tool::Rectangle => (Key::R, "R"),
            Tool::RoundedRect => (Key::U, "U"),
            Tool::Ellipse => (Key::O, "O"),
            Tool::Polygon => (Key::Y, "Y"),
            Tool::Curve => (Key::C, "C"),
            Tool::Text => (Key::T, "T"),
            Tool::Select => (Key::M, "M"),
        }
    }
}

/// Light/dark theme preference, persisted between runs.
#[derive(PartialEq, Eq, Clone, Copy, serde::Serialize, serde::Deserialize)]
enum ThemeChoice {
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    fn to_pref(self) -> egui::ThemePreference {
        match self {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
        }
    }
    fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "System",
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
        }
    }
}

/// One of the 8 resize handles around a box (corners + edge midpoints).
#[derive(Clone, Copy, PartialEq)]
enum Grip {
    Nw,
    N,
    Ne,
    E,
    Se,
    S,
    Sw,
    W,
}

impl Grip {
    const ALL: [Grip; 8] = [
        Grip::Nw,
        Grip::N,
        Grip::Ne,
        Grip::E,
        Grip::Se,
        Grip::S,
        Grip::Sw,
        Grip::W,
    ];
    /// Position of the grip within the box, as fractions in 0..=1.
    fn frac(self) -> (f32, f32) {
        match self {
            Grip::Nw => (0.0, 0.0),
            Grip::N => (0.5, 0.0),
            Grip::Ne => (1.0, 0.0),
            Grip::E => (1.0, 0.5),
            Grip::Se => (1.0, 1.0),
            Grip::S => (0.5, 1.0),
            Grip::Sw => (0.0, 1.0),
            Grip::W => (0.0, 0.5),
        }
    }
    fn cursor(self) -> egui::CursorIcon {
        match self {
            Grip::Nw | Grip::Se => egui::CursorIcon::ResizeNwSe,
            Grip::Ne | Grip::Sw => egui::CursorIcon::ResizeNeSw,
            Grip::N | Grip::S => egui::CursorIcon::ResizeVertical,
            Grip::E | Grip::W => egui::CursorIcon::ResizeHorizontal,
        }
    }
}

const GRIP_HIT: f32 = 11.0; // half-size of a grip's clickable area, in screen px
const MIN_DIM: i32 = 2; // minimum width/height when resizing

/// An action deferred until the user resolves an unsaved-changes prompt.
enum Pending {
    CloseTab,
}

/// Per-document (per-tab) state. The *active* document's state lives in the
/// `PaintApp` fields directly (so drawing code is untouched); inactive tabs are
/// stashed here and swapped in on tab change.
#[derive(Default)]
struct DocState {
    image: ColorImage,
    undo_stack: Vec<ColorImage>,
    redo_stack: Vec<ColorImage>,
    file_path: Option<PathBuf>,
    modified: bool,
    zoom: f32,
    scroll_offset: Vec2,
}

/// Write a ColorImage to `path` as RGBA (straight alpha). Returns success.
fn write_image(image: &ColorImage, path: &PathBuf) -> bool {
    let [w, h] = image.size;
    let mut buf = Vec::with_capacity(w * h * 4);
    for px in &image.pixels {
        buf.extend_from_slice(&px.to_srgba_unmultiplied());
    }
    match image::RgbaImage::from_raw(w as u32, h as u32, buf) {
        Some(img) => match img.save(path) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("Failed to save image: {e}");
                false
            }
        },
        None => {
            eprintln!("Failed to build image buffer");
            false
        }
    }
}

/// A lifted / pasted region that floats above the canvas until committed.
struct Floating {
    img: ColorImage,
    pos: Pt,
}

/// In-progress cubic Bézier curve (classic MS Paint: a line, then two bends).
#[derive(Clone, Copy)]
struct CurveState {
    p0: Pt,
    p1: Pt,
    c1: Pt,
    c2: Pt,
    stage: u8, // 0 = dragging endpoints, 1 = bend #1, 2 = bend #2
}

/// A persisted open tab: its file path and zoom level.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct TabPersist {
    path: String,
    #[serde(default)]
    zoom: f32,
}

/// User preferences persisted between runs (via eframe storage).
#[derive(serde::Serialize, serde::Deserialize)]
struct Settings {
    tool: Tool,
    fg: [u8; 4],
    bg: [u8; 4],
    brush_size: i32,
    fill_shapes: bool,
    corner_radius: i32,
    text_size: f32,
    theme: ThemeChoice,
    show_grid: bool,
    antialias: bool,
    fill_tolerance: i32,
    /// Open tabs (untitled tabs are omitted), with per-tab zoom, restored next run.
    #[serde(default)]
    open_tabs: Vec<TabPersist>,
    #[serde(default)]
    active_tab: usize,
}

struct PaintApp {
    image: ColorImage,
    texture: Option<TextureHandle>,
    checker_tex: Option<TextureHandle>,
    dirty: bool,

    tool: Tool,
    last_tool: Tool,
    fg: Color32,
    bg: Color32,
    brush_size: i32,
    fill_shapes: bool,
    corner_radius: i32,
    text_size: f32,
    zoom: f32,
    theme: ThemeChoice,
    show_grid: bool,
    antialias: bool,
    fill_tolerance: i32,

    // freehand / drag-shape state
    active_secondary: bool,
    constrain: bool,
    cursor_pos: Option<Pt>,
    last_pos: Option<Pt>,
    shape_start: Option<Pt>,
    snapshot: Option<ColorImage>,

    // multi-click tools
    poly_points: Vec<Pt>,
    curve: Option<CurveState>,

    // text tool
    text_pos: Option<Pt>,
    text_buf: String,
    text_grab: Option<Pt>,

    // selection / clipboard
    define_start: Option<Pt>,
    selection_rect: Option<(Pt, Pt)>,
    floating: Option<Floating>,
    floating_tex: Option<TextureHandle>,
    floating_dirty: bool,
    floating_grab: Option<Pt>,
    lifted: bool,
    sel_undo_pushed: bool,
    clipboard: Option<ColorImage>,
    sys_clipboard: Option<arboard::Clipboard>,
    #[allow(dead_code)] // only read on macOS/Windows
    last_change_count: isize,
    #[allow(dead_code)] // only read on macOS/Windows
    was_focused: bool,

    undo_stack: Vec<ColorImage>,
    redo_stack: Vec<ColorImage>,
    file_path: Option<PathBuf>,

    // new-canvas dialog
    show_new_dialog: bool,
    new_w: usize,
    new_h: usize,
    new_transparent: bool,

    // resize dialog
    show_resize_dialog: bool,
    focus_resize: bool,
    resize_w: usize,
    resize_h: usize,
    resize_keep_aspect: bool,
    resize_aspect: f32,

    // oversized-paste dialog
    show_paste_dialog: bool,
    pending_paste: Option<ColorImage>,

    // unsaved-changes tracking
    modified: bool,
    show_quit_dialog: bool,
    force_quit: bool,

    // dialog key actions for the current frame (Esc cancels, Enter confirms)
    dlg_cancel: bool,
    dlg_confirm: bool,

    // unsaved-changes prompt for New/Open/close-tab
    show_discard_dialog: bool,
    pending: Option<Pending>,

    // tabs (inactive documents; the active one lives in the fields above)
    docs: Vec<DocState>,
    active: usize,

    // paste-destination prompt (new tab vs. current image)
    show_paste_dest_dialog: bool,
    pending_paste_img: Option<ColorImage>,

    // window title cache, and recently-used colors
    last_title: String,
    recent_colors: Vec<Color32>,

    // zoom / pan navigation
    scroll_offset: Vec2,
    pending_scroll: Option<Vec2>,
    last_img_min: Pos2,
    zoom_req: Option<f32>,
    zoom_anchor: Option<Pos2>,
    fit_requested: bool,
    canvas_id: Option<egui::Id>,
    mid_panning: bool,

    // interactive resize via drag handles
    resize_grip: Option<Grip>,
    resize_orig: Option<ColorImage>,
    resize_rect0: (i32, i32, i32, i32), // x0,y0,x1,y1 at grab time
    canvas_grip: Option<Grip>,
    canvas_orig: Option<ColorImage>,
    resize_grab: Pt,
}

impl Default for PaintApp {
    fn default() -> Self {
        Self {
            image: ColorImage::new([DEFAULT_W, DEFAULT_H], Color32::WHITE),
            texture: None,
            checker_tex: None,
            dirty: true,
            tool: Tool::Pencil,
            last_tool: Tool::Pencil,
            fg: Color32::BLACK,
            bg: Color32::WHITE,
            brush_size: 6,
            fill_shapes: false,
            corner_radius: 24,
            text_size: 32.0,
            zoom: 1.0,
            theme: ThemeChoice::System,
            show_grid: false,
            antialias: true,
            fill_tolerance: 16,
            active_secondary: false,
            constrain: false,
            cursor_pos: None,
            last_pos: None,
            shape_start: None,
            snapshot: None,
            poly_points: Vec::new(),
            curve: None,
            text_pos: None,
            text_buf: String::new(),
            text_grab: None,
            define_start: None,
            selection_rect: None,
            floating: None,
            floating_tex: None,
            floating_dirty: false,
            floating_grab: None,
            lifted: false,
            sel_undo_pushed: false,
            clipboard: None,
            sys_clipboard: None,
            last_change_count: -1,
            was_focused: true,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            file_path: None,
            show_new_dialog: false,
            new_w: DEFAULT_W,
            new_h: DEFAULT_H,
            new_transparent: false,
            show_resize_dialog: false,
            focus_resize: false,
            resize_w: DEFAULT_W,
            resize_h: DEFAULT_H,
            resize_keep_aspect: true,
            resize_aspect: DEFAULT_W as f32 / DEFAULT_H as f32,
            show_paste_dialog: false,
            pending_paste: None,
            modified: false,
            show_quit_dialog: false,
            force_quit: false,
            dlg_cancel: false,
            dlg_confirm: false,
            show_discard_dialog: false,
            pending: None,
            docs: vec![DocState::default()],
            active: 0,
            show_paste_dest_dialog: false,
            pending_paste_img: None,
            last_title: String::new(),
            recent_colors: Vec::new(),
            scroll_offset: Vec2::ZERO,
            pending_scroll: None,
            last_img_min: Pos2::ZERO,
            zoom_req: None,
            zoom_anchor: None,
            fit_requested: false,
            canvas_id: None,
            mid_panning: false,
            resize_grip: None,
            resize_orig: None,
            resize_rect0: (0, 0, 0, 0),
            canvas_grip: None,
            canvas_orig: None,
            resize_grab: (0, 0),
        }
    }
}

// ---------------------------------------------------------------------------
// Pixel-buffer primitives
// ---------------------------------------------------------------------------

/// Distance from point (px,py) to the segment a–b.
fn dist_seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let l2 = dx * dx + dy * dy;
    if l2 <= 1e-6 {
        return (px - ax).hypot(py - ay);
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / l2).clamp(0.0, 1.0);
    (px - (ax + t * dx)).hypot(py - (ay + t * dy))
}

/// Constrain a drag endpoint: lines/arrows snap to 0/45/90°, boxes to a square.
fn constrain_pos(start: Pt, pos: Pt, line: bool) -> Pt {
    let dx = (pos.0 - start.0) as f32;
    let dy = (pos.1 - start.1) as f32;
    if line {
        let len = (dx * dx + dy * dy).sqrt();
        if len < 0.5 {
            return pos;
        }
        let step = std::f32::consts::FRAC_PI_4;
        let ang = (dy.atan2(dx) / step).round() * step;
        (
            start.0 + (ang.cos() * len).round() as i32,
            start.1 + (ang.sin() * len).round() as i32,
        )
    } else {
        let s = dx.abs().max(dy.abs());
        let sx = if dx < 0.0 { -1.0 } else { 1.0 };
        let sy = if dy < 0.0 { -1.0 } else { 1.0 };
        (start.0 + (s * sx) as i32, start.1 + (s * sy) as i32)
    }
}

#[inline]
fn over(src: Color32, dst: Color32) -> Color32 {
    let a = src.a() as f32 / 255.0;
    let blend = |s: u8, d: u8| (s as f32 * a + d as f32 * (1.0 - a)).round() as u8;
    Color32::from_rgb(
        blend(src.r(), dst.r()),
        blend(src.g(), dst.g()),
        blend(src.b(), dst.b()),
    )
}

impl PaintApp {
    #[inline]
    fn w(&self) -> i32 {
        self.image.size[0] as i32
    }
    #[inline]
    fn h(&self) -> i32 {
        self.image.size[1] as i32
    }

    #[inline]
    fn set_pixel(&mut self, x: i32, y: i32, c: Color32) {
        if x >= 0 && y >= 0 && x < self.w() && y < self.h() {
            let idx = y as usize * self.image.size[0] + x as usize;
            self.image.pixels[idx] = c;
        }
    }

    #[inline]
    fn get_pixel(&self, x: i32, y: i32) -> Option<Color32> {
        if x >= 0 && y >= 0 && x < self.w() && y < self.h() {
            Some(self.image.pixels[y as usize * self.image.size[0] + x as usize])
        } else {
            None
        }
    }

    /// Composite `color` onto pixel (x,y) with edge `cov` in 0..1, using
    /// straight-alpha src-over. A fully transparent `color` (the eraser, or a
    /// transparent foreground) instead *erases* by `cov` (destination-out), so
    /// soft edges work correctly over a transparent background.
    fn cover_blend(&mut self, x: i32, y: i32, color: Color32, cov: f32) {
        if x < 0 || y < 0 || x >= self.w() || y >= self.h() {
            return;
        }
        let cov = cov.clamp(0.0, 1.0);
        if cov <= 0.0 {
            return;
        }
        let idx = y as usize * self.image.size[0] + x as usize;
        let d = self.image.pixels[idx].to_srgba_unmultiplied();
        let (dr, dg, db) = (d[0] as f32, d[1] as f32, d[2] as f32);
        let da = d[3] as f32 / 255.0;
        let s = color.to_srgba_unmultiplied();
        let sa_full = s[3] as f32 / 255.0;
        let out = if sa_full <= 0.0 {
            let na = da * (1.0 - cov);
            Color32::from_rgba_unmultiplied(d[0], d[1], d[2], (na * 255.0).round() as u8)
        } else {
            let sa = sa_full * cov;
            let oa = sa + da * (1.0 - sa);
            if oa <= 0.0 {
                Color32::TRANSPARENT
            } else {
                let mix = |sc: f32, dc: f32| (sc * sa + dc * da * (1.0 - sa)) / oa;
                Color32::from_rgba_unmultiplied(
                    mix(s[0] as f32, dr).round() as u8,
                    mix(s[1] as f32, dg).round() as u8,
                    mix(s[2] as f32, db).round() as u8,
                    (oa * 255.0).round() as u8,
                )
            }
        };
        self.image.pixels[idx] = out;
    }

    /// Coverage of pixel (x,y) for a shape, via a cheap inside/outside test:
    /// 1.0 if all corners are inside, 0.0 if the pixel is fully outside,
    /// otherwise a 4×4 supersample.
    fn pixel_coverage(x: i32, y: i32, inside: &impl Fn(f32, f32) -> bool) -> f32 {
        let (fx, fy) = (x as f32, y as f32);
        let corners = [
            inside(fx + 0.125, fy + 0.125),
            inside(fx + 0.875, fy + 0.125),
            inside(fx + 0.125, fy + 0.875),
            inside(fx + 0.875, fy + 0.875),
        ];
        let n = corners.iter().filter(|b| **b).count();
        if n == 4 {
            return 1.0;
        }
        if n == 0 && !inside(fx + 0.5, fy + 0.5) {
            return 0.0;
        }
        let mut hits = 0;
        for j in 0..4 {
            for i in 0..4 {
                if inside(fx + (i as f32 + 0.5) / 4.0, fy + (j as f32 + 0.5) / 4.0) {
                    hits += 1;
                }
            }
        }
        hits as f32 / 16.0
    }

    /// Anti-aliased fill: blend `color` over the bounding box using per-pixel
    /// coverage from `inside`.
    fn aa_fill(
        &mut self,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        color: Color32,
        inside: impl Fn(f32, f32) -> bool,
    ) {
        let (xa, xb) = (x0.min(x1).max(0), x1.max(x0).min(self.w() - 1));
        let (ya, yb) = (y0.min(y1).max(0), y1.max(y0).min(self.h() - 1));
        for y in ya..=yb {
            for x in xa..=xb {
                let cov = Self::pixel_coverage(x, y, &inside);
                if cov > 0.0 {
                    self.cover_blend(x, y, color, cov);
                }
            }
        }
    }

    /// Stamp a filled disc of diameter `size` centered at (cx, cy).
    fn stamp(&mut self, cx: i32, cy: i32, color: Color32, size: i32) {
        if self.antialias && size > 1 {
            let r = size as f32 / 2.0;
            let (fcx, fcy) = (cx as f32 + 0.5, cy as f32 + 0.5);
            let pad = r.ceil() as i32 + 1;
            self.aa_fill(cx - pad, cy - pad, cx + pad, cy + pad, color, move |x, y| {
                let (dx, dy) = (x - fcx, y - fcy);
                dx * dx + dy * dy <= r * r
            });
            return;
        }
        if size <= 1 {
            self.set_pixel(cx, cy, color);
            return;
        }
        let r = size / 2;
        let r2 = r * r;
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r2 {
                    self.set_pixel(cx + dx, cy + dy, color);
                }
            }
        }
    }

    /// Bresenham line, stamping a brush of `size` at each step.
    fn draw_line(&mut self, p0: Pt, p1: Pt, color: Color32, size: i32) {
        if self.antialias {
            let hw = (size as f32 / 2.0).max(0.5);
            let (ax, ay) = (p0.0 as f32 + 0.5, p0.1 as f32 + 0.5);
            let (bx, by) = (p1.0 as f32 + 0.5, p1.1 as f32 + 0.5);
            let pad = hw.ceil() as i32 + 1;
            self.aa_fill(
                p0.0.min(p1.0) - pad,
                p0.1.min(p1.1) - pad,
                p0.0.max(p1.0) + pad,
                p0.1.max(p1.1) + pad,
                color,
                move |x, y| dist_seg(x, y, ax, ay, bx, by) <= hw,
            );
            return;
        }
        let (mut x0, mut y0) = p0;
        let (x1, y1) = p1;
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.stamp(x0, y0, color, size);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    /// Line from p0 to p1 with an arrowhead at p1.
    fn draw_arrow(&mut self, p0: Pt, p1: Pt, color: Color32, size: i32) {
        self.draw_line(p0, p1, color, size);
        let (dx, dy) = ((p1.0 - p0.0) as f32, (p1.1 - p0.1) as f32);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1.0 {
            return;
        }
        // Arrowhead length scales with the stroke size, clamped to the line length.
        let head = (size as f32 * 3.5).max(14.0).min(len);
        let angle = dy.atan2(dx);
        let spread = 0.5; // ~28° per barb
        for s in [spread, -spread] {
            let a = angle + std::f32::consts::PI - s;
            let hx = p1.0 + (head * a.cos()).round() as i32;
            let hy = p1.1 + (head * a.sin()).round() as i32;
            self.draw_line(p1, (hx, hy), color, size);
        }
    }

    fn arc(&mut self, cx: i32, cy: i32, r: i32, a0: f32, a1: f32, color: Color32, size: i32) {
        let steps = (((a1 - a0).abs() * r as f32) as i32).max(8);
        for i in 0..=steps {
            let t = a0 + (a1 - a0) * i as f32 / steps as f32;
            let x = cx + (r as f32 * t.cos()).round() as i32;
            let y = cy + (r as f32 * t.sin()).round() as i32;
            self.stamp(x, y, color, size);
        }
    }

    fn draw_rect(&mut self, p0: Pt, p1: Pt, color: Color32, filled: bool, size: i32) {
        let (x0, x1) = (p0.0.min(p1.0), p0.0.max(p1.0));
        let (y0, y1) = (p0.1.min(p1.1), p0.1.max(p1.1));
        if filled {
            for y in y0..=y1 {
                for x in x0..=x1 {
                    self.set_pixel(x, y, color);
                }
            }
        } else {
            self.draw_line((x0, y0), (x1, y0), color, size);
            self.draw_line((x0, y1), (x1, y1), color, size);
            self.draw_line((x0, y0), (x0, y1), color, size);
            self.draw_line((x1, y0), (x1, y1), color, size);
        }
    }

    fn draw_rounded_rect(
        &mut self,
        p0: Pt,
        p1: Pt,
        radius: i32,
        color: Color32,
        filled: bool,
        size: i32,
    ) {
        let (x0, x1) = (p0.0.min(p1.0), p0.0.max(p1.0));
        let (y0, y1) = (p0.1.min(p1.1), p0.1.max(p1.1));
        let r = radius.min((x1 - x0) / 2).min((y1 - y0) / 2).max(0);
        if r == 0 {
            return self.draw_rect(p0, p1, color, filled, size);
        }
        if self.antialias {
            // A rounded rect = all points within `rad` of an inset core rect.
            let round = |fx: f32, fy: f32, xa: f32, xb: f32, ya: f32, yb: f32, rad: f32| {
                if xb - xa < 2.0 * rad || yb - ya < 2.0 * rad {
                    return fx >= xa && fx <= xb && fy >= ya && fy <= yb;
                }
                let cx = fx.clamp(xa + rad, xb - rad);
                let cy = fy.clamp(ya + rad, yb - rad);
                (fx - cx).hypot(fy - cy) <= rad
            };
            let (xmin, xmax) = (x0 as f32, (x1 + 1) as f32);
            let (ymin, ymax) = (y0 as f32, (y1 + 1) as f32);
            let rr = r as f32;
            if filled {
                self.aa_fill(x0 - 1, y0 - 1, x1 + 1, y1 + 1, color, move |fx, fy| {
                    round(fx, fy, xmin, xmax, ymin, ymax, rr)
                });
            } else {
                let t = size as f32;
                self.aa_fill(x0 - 1, y0 - 1, x1 + 1, y1 + 1, color, move |fx, fy| {
                    if !round(fx, fy, xmin, xmax, ymin, ymax, rr) {
                        return false;
                    }
                    let (ixa, ixb, iya, iyb) = (xmin + t, xmax - t, ymin + t, ymax - t);
                    if ixb <= ixa || iyb <= iya {
                        return true;
                    }
                    !round(fx, fy, ixa, ixb, iya, iyb, (rr - t).max(0.0))
                });
            }
            return;
        }
        use std::f32::consts::PI;
        if filled {
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let inside = if x >= x0 + r && x <= x1 - r {
                        true
                    } else if y >= y0 + r && y <= y1 - r {
                        true
                    } else {
                        // corner test
                        let cx = if x < x0 + r { x0 + r } else { x1 - r };
                        let cy = if y < y0 + r { y0 + r } else { y1 - r };
                        let (dx, dy) = (x - cx, y - cy);
                        dx * dx + dy * dy <= r * r
                    };
                    if inside {
                        self.set_pixel(x, y, color);
                    }
                }
            }
        } else {
            self.draw_line((x0 + r, y0), (x1 - r, y0), color, size);
            self.draw_line((x0 + r, y1), (x1 - r, y1), color, size);
            self.draw_line((x0, y0 + r), (x0, y1 - r), color, size);
            self.draw_line((x1, y0 + r), (x1, y1 - r), color, size);
            self.arc(x0 + r, y0 + r, r, PI, 1.5 * PI, color, size); // top-left
            self.arc(x1 - r, y0 + r, r, 1.5 * PI, 2.0 * PI, color, size); // top-right
            self.arc(x1 - r, y1 - r, r, 0.0, 0.5 * PI, color, size); // bottom-right
            self.arc(x0 + r, y1 - r, r, 0.5 * PI, PI, color, size); // bottom-left
        }
    }

    fn draw_ellipse(&mut self, p0: Pt, p1: Pt, color: Color32, filled: bool, size: i32) {
        let cx = (p0.0 + p1.0) as f32 / 2.0;
        let cy = (p0.1 + p1.1) as f32 / 2.0;
        let rx = ((p1.0 - p0.0).abs() as f32) / 2.0;
        let ry = ((p1.1 - p0.1).abs() as f32) / 2.0;
        if rx < 0.5 || ry < 0.5 {
            return;
        }
        if self.antialias {
            let (bx0, by0) = (p0.0.min(p1.0) - 1, p0.1.min(p1.1) - 1);
            let (bx1, by1) = (p0.0.max(p1.0) + 1, p0.1.max(p1.1) + 1);
            let norm = move |x: f32, y: f32, rx: f32, ry: f32| {
                let nx = (x - cx) / rx;
                let ny = (y - cy) / ry;
                nx * nx + ny * ny <= 1.0
            };
            if filled {
                self.aa_fill(bx0, by0, bx1, by1, color, move |x, y| norm(x, y, rx, ry));
            } else {
                let t = size as f32;
                let (irx, iry) = ((rx - t).max(0.0), (ry - t).max(0.0));
                self.aa_fill(bx0, by0, bx1, by1, color, move |x, y| {
                    norm(x, y, rx, ry)
                        && (irx <= 0.0 || iry <= 0.0 || !norm(x, y, irx, iry))
                });
            }
            return;
        }
        if filled {
            let (x0, x1) = (p0.0.min(p1.0), p0.0.max(p1.0));
            let (y0, y1) = (p0.1.min(p1.1), p0.1.max(p1.1));
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let nx = (x as f32 - cx) / rx;
                    let ny = (y as f32 - cy) / ry;
                    if nx * nx + ny * ny <= 1.0 {
                        self.set_pixel(x, y, color);
                    }
                }
            }
        } else {
            let steps = (((rx + ry) * 6.0) as i32).max(64);
            for i in 0..steps {
                let t = (i as f32) * std::f32::consts::TAU / steps as f32;
                let x = (cx + rx * t.cos()).round() as i32;
                let y = (cy + ry * t.sin()).round() as i32;
                self.stamp(x, y, color, size);
            }
        }
    }

    fn draw_polyline(&mut self, pts: &[Pt], closed: bool, color: Color32, size: i32) {
        if pts.len() < 2 {
            return;
        }
        for w in pts.windows(2) {
            self.draw_line(w[0], w[1], color, size);
        }
        if closed {
            self.draw_line(*pts.last().unwrap(), pts[0], color, size);
        }
    }

    /// Even-odd scanline polygon fill.
    fn fill_polygon(&mut self, pts: &[Pt], color: Color32) {
        if pts.len() < 3 {
            return;
        }
        if self.antialias {
            let minx = pts.iter().map(|p| p.0).min().unwrap();
            let maxx = pts.iter().map(|p| p.0).max().unwrap();
            let miny = pts.iter().map(|p| p.1).min().unwrap();
            let maxy = pts.iter().map(|p| p.1).max().unwrap();
            let poly: Vec<(f32, f32)> = pts.iter().map(|p| (p.0 as f32, p.1 as f32)).collect();
            self.aa_fill(minx, miny, maxx, maxy, color, move |fx, fy| {
                // even-odd crossing test
                let mut inside = false;
                let n = poly.len();
                let mut j = n - 1;
                for i in 0..n {
                    let (ax, ay) = poly[i];
                    let (bx, by) = poly[j];
                    if (ay > fy) != (by > fy)
                        && fx < (bx - ax) * (fy - ay) / (by - ay) + ax
                    {
                        inside = !inside;
                    }
                    j = i;
                }
                inside
            });
            return;
        }
        let min_y = pts.iter().map(|p| p.1).min().unwrap();
        let max_y = pts.iter().map(|p| p.1).max().unwrap();
        for y in min_y..=max_y {
            let yf = y as f32 + 0.5;
            let mut xs: Vec<f32> = Vec::new();
            for i in 0..pts.len() {
                let a = pts[i];
                let b = pts[(i + 1) % pts.len()];
                let (ax, ay) = (a.0 as f32, a.1 as f32);
                let (bx, by) = (b.0 as f32, b.1 as f32);
                if (ay <= yf && by > yf) || (by <= yf && ay > yf) {
                    let t = (yf - ay) / (by - ay);
                    xs.push(ax + t * (bx - ax));
                }
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mut i = 0;
            while i + 1 < xs.len() {
                let x0 = xs[i].round() as i32;
                let x1 = xs[i + 1].round() as i32;
                for x in x0..=x1 {
                    self.set_pixel(x, y, color);
                }
                i += 2;
            }
        }
    }

    fn draw_bezier(&mut self, p0: Pt, c1: Pt, c2: Pt, p1: Pt, color: Color32, size: i32) {
        let pf = |p: Pt| (p.0 as f32, p.1 as f32);
        let (p0, c1, c2, p1) = (pf(p0), pf(c1), pf(c2), pf(p1));
        let steps = if self.antialias { 48 } else { 200 };
        let mut pts: Vec<(f32, f32)> = Vec::with_capacity(steps + 1);
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let u = 1.0 - t;
            let x = u * u * u * p0.0 + 3.0 * u * u * t * c1.0 + 3.0 * u * t * t * c2.0 + t * t * t * p1.0;
            let y = u * u * u * p0.1 + 3.0 * u * u * t * c1.1 + 3.0 * u * t * t * c2.1 + t * t * t * p1.1;
            pts.push((x, y));
        }
        if self.antialias {
            let hw = (size as f32 / 2.0).max(0.5);
            let minx = pts.iter().map(|p| p.0).fold(f32::MAX, f32::min);
            let maxx = pts.iter().map(|p| p.0).fold(f32::MIN, f32::max);
            let miny = pts.iter().map(|p| p.1).fold(f32::MAX, f32::min);
            let maxy = pts.iter().map(|p| p.1).fold(f32::MIN, f32::max);
            let pad = hw.ceil() as i32 + 1;
            self.aa_fill(
                minx as i32 - pad,
                miny as i32 - pad,
                maxx as i32 + pad,
                maxy as i32 + pad,
                color,
                move |x, y| {
                    pts.windows(2).any(|w| {
                        dist_seg(x, y, w[0].0 + 0.5, w[0].1 + 0.5, w[1].0 + 0.5, w[1].1 + 0.5) <= hw
                    })
                },
            );
            return;
        }
        let mut prev = pts[0];
        for &(x, y) in &pts[1..] {
            self.draw_line(
                (prev.0.round() as i32, prev.1.round() as i32),
                (x.round() as i32, y.round() as i32),
                color,
                size,
            );
            prev = (x, y);
        }
    }

    /// 4-connected flood fill with a tolerance, plus a 1px anti-aliased feather
    /// at the boundary so the fill blends into a stroke's AA fringe (instead of
    /// leaving a halo of the old background color).
    fn flood_fill(&mut self, start: Pt, color: Color32) {
        let (w, h) = (self.w(), self.h());
        let Some(seed_c) = self.get_pixel(start.0, start.1) else {
            return;
        };
        let seed = seed_c.to_srgba_unmultiplied();
        let fill = color.to_srgba_unmultiplied();
        if seed == fill {
            return;
        }
        let idx = |x: i32, y: i32| (y * w + x) as usize;
        let straight = |c: Color32| c.to_srgba_unmultiplied();
        let dist = |a: [u8; 4], b: [u8; 4]| -> i32 {
            (0..4).map(|k| (a[k] as i32 - b[k] as i32).abs()).max().unwrap()
        };
        let tol = self.fill_tolerance;

        // Flood the connected region within tolerance of the seed.
        let mut visited = vec![false; (w * h) as usize];
        let mut region: Vec<(i32, i32)> = Vec::new();
        let mut stack = vec![start];
        visited[idx(start.0, start.1)] = true;
        while let Some((x, y)) = stack.pop() {
            region.push((x, y));
            for (nx, ny) in [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)] {
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let i = idx(nx, ny);
                if visited[i] {
                    continue;
                }
                if dist(straight(self.image.pixels[i]), seed) <= tol {
                    visited[i] = true;
                    stack.push((nx, ny));
                }
            }
        }

        // Solid-fill the region.
        for &(x, y) in &region {
            self.set_pixel(x, y, color);
        }

        // Feather the 1px boundary: each fringe pixel P (a blend of some stroke
        // color and the old background ~seed) becomes that same blend against
        // the fill color, i.e. Q = P + s·(fill − seed) with s = the seed fraction.
        let mut feathered = vec![false; (w * h) as usize];
        let edges: Vec<(i32, i32)> = region
            .iter()
            .flat_map(|&(x, y)| [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)])
            .collect();
        for (px, py) in edges {
            if px < 0 || py < 0 || px >= w || py >= h {
                continue;
            }
            let i = idx(px, py);
            if visited[i] || feathered[i] {
                continue;
            }
            feathered[i] = true;
            let p = straight(self.image.pixels[i]);
            // Estimate the stroke color from the neighbor furthest from the seed.
            let mut ld = dist(p, seed);
            for (mx, my) in [(px + 1, py), (px - 1, py), (px, py + 1), (px, py - 1)] {
                if mx < 0 || my < 0 || mx >= w || my >= h {
                    continue;
                }
                ld = ld.max(dist(straight(self.image.pixels[idx(mx, my)]), seed));
            }
            if ld == 0 {
                continue;
            }
            let s = (1.0 - dist(p, seed) as f32 / ld as f32).clamp(0.0, 1.0);
            if s <= 0.0 {
                continue;
            }
            let ch = |k: usize| {
                (p[k] as f32 + s * (fill[k] as f32 - seed[k] as f32))
                    .clamp(0.0, 255.0)
                    .round() as u8
            };
            self.image.pixels[i] = Color32::from_rgba_unmultiplied(ch(0), ch(1), ch(2), ch(3));
        }
    }

    // --- region helpers (selection / clipboard) ---

    fn capture(&self, x0: i32, y0: i32, w: usize, h: usize) -> ColorImage {
        let mut img = ColorImage::new([w, h], Color32::TRANSPARENT);
        for j in 0..h {
            for i in 0..w {
                if let Some(c) = self.get_pixel(x0 + i as i32, y0 + j as i32) {
                    img.pixels[j * w + i] = c;
                }
            }
        }
        img
    }

    fn blit(&mut self, src: &ColorImage, pos: Pt) {
        let [w, h] = src.size;
        for j in 0..h {
            for i in 0..w {
                let c = src.pixels[j * w + i];
                if c.a() == 0 {
                    continue;
                }
                let (x, y) = (pos.0 + i as i32, pos.1 + j as i32);
                if c.a() == 255 {
                    self.set_pixel(x, y, c);
                } else if let Some(d) = self.get_pixel(x, y) {
                    self.set_pixel(x, y, over(c, d));
                }
            }
        }
    }

    fn fill_region(&mut self, x0: i32, y0: i32, w: usize, h: usize, color: Color32) {
        for j in 0..h {
            for i in 0..w {
                self.set_pixel(x0 + i as i32, y0 + j as i32, color);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Undo / redo and file IO
// ---------------------------------------------------------------------------

impl PaintApp {
    fn push_undo(&mut self) {
        self.undo_stack.push(self.image.clone());
        if self.undo_stack.len() > MAX_UNDO {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
        self.modified = true;
    }

    fn undo(&mut self) {
        self.commit_floating();
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(self.image.clone());
            self.image = prev;
            self.dirty = true;
            self.modified = true;
        }
    }

    fn redo(&mut self) {
        self.commit_floating();
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(self.image.clone());
            self.image = next;
            self.dirty = true;
            self.modified = true;
        }
    }

    // --- tabs / documents -------------------------------------------------

    /// Commit or discard in-progress work so per-document state is "clean"
    /// before stashing or switching tabs.
    fn finalize_transient(&mut self) {
        self.commit_floating();
        self.poly_points.clear();
        self.curve = None;
        self.text_pos = None;
        self.text_buf.clear();
        self.text_grab = None;
        self.last_pos = None;
        self.shape_start = None;
        self.snapshot = None;
        self.resize_grip = None;
        self.resize_orig = None;
        self.canvas_grip = None;
        self.canvas_orig = None;
        self.floating_grab = None;
        self.mid_panning = false;
    }

    fn stash_active(&mut self) {
        let d = DocState {
            image: std::mem::take(&mut self.image),
            undo_stack: std::mem::take(&mut self.undo_stack),
            redo_stack: std::mem::take(&mut self.redo_stack),
            file_path: self.file_path.take(),
            modified: self.modified,
            zoom: self.zoom,
            scroll_offset: self.scroll_offset,
        };
        self.docs[self.active] = d;
    }

    fn load_active(&mut self, d: DocState) {
        self.image = d.image;
        self.undo_stack = d.undo_stack;
        self.redo_stack = d.redo_stack;
        self.file_path = d.file_path;
        self.modified = d.modified;
        self.zoom = if d.zoom > 0.0 { d.zoom } else { 1.0 };
        self.scroll_offset = d.scroll_offset;
        self.pending_scroll = Some(d.scroll_offset);
        self.dirty = true;
        self.floating_dirty = true;
        self.last_img_min = Pos2::ZERO;
    }

    fn switch_to(&mut self, idx: usize) {
        if idx >= self.docs.len() || idx == self.active {
            return;
        }
        self.finalize_transient();
        self.stash_active();
        let d = std::mem::take(&mut self.docs[idx]);
        self.load_active(d);
        self.active = idx;
    }

    /// Open `image` in a new tab and make it active.
    fn new_tab_with(&mut self, image: ColorImage, path: Option<PathBuf>, modified: bool) {
        self.finalize_transient();
        self.stash_active();
        self.docs.push(DocState::default()); // placeholder slot for the new active tab
        self.active = self.docs.len() - 1;
        self.load_active(DocState {
            image,
            file_path: path,
            modified,
            zoom: 1.0,
            ..Default::default()
        });
    }

    fn new_tab(&mut self) {
        let img = ColorImage::new([DEFAULT_W, DEFAULT_H], Color32::WHITE);
        self.new_tab_with(img, None, false);
    }

    fn close_active(&mut self) {
        self.finalize_transient();
        if self.docs.len() <= 1 {
            // Last tab: reset to a fresh blank rather than leaving none.
            self.load_active(DocState {
                image: ColorImage::new([DEFAULT_W, DEFAULT_H], Color32::WHITE),
                zoom: 1.0,
                ..Default::default()
            });
            self.docs[self.active] = DocState::default();
            return;
        }
        self.docs.remove(self.active);
        if self.active >= self.docs.len() {
            self.active = self.docs.len() - 1;
        }
        let d = std::mem::take(&mut self.docs[self.active]);
        self.load_active(d);
    }

    /// Close tab `idx`, prompting to save first if it has unsaved changes.
    fn request_close_tab(&mut self, idx: usize) {
        self.switch_to(idx);
        if self.modified {
            self.pending = Some(Pending::CloseTab);
            self.show_discard_dialog = true;
        } else {
            self.close_active();
        }
    }

    fn tab_label(&self, idx: usize) -> String {
        let (path, modified) = if idx == self.active {
            (&self.file_path, self.modified)
        } else {
            (&self.docs[idx].file_path, self.docs[idx].modified)
        };
        let name = path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".to_owned());
        format!("{}{}", if modified { "• " } else { "" }, name)
    }

    fn any_unsaved(&self) -> bool {
        self.modified
            || self
                .docs
                .iter()
                .enumerate()
                .any(|(i, d)| i != self.active && d.modified)
    }

    fn save_all(&mut self) {
        // Walk every modified tab; switching to it and calling save() writes
        // file-backed tabs directly and prompts Save As for untitled ones,
        // one by one (cancelling a prompt leaves that tab unsaved).
        let original = self.active;
        for i in 0..self.docs.len() {
            let modified = if i == self.active {
                self.modified
            } else {
                self.docs[i].modified
            };
            if modified {
                self.switch_to(i);
                self.save();
            }
        }
        self.switch_to(original.min(self.docs.len().saturating_sub(1)));
    }

    fn create_canvas(&mut self, w: usize, h: usize, transparent: bool) {
        let fill = if transparent {
            Color32::TRANSPARENT
        } else {
            Color32::WHITE
        };
        let img = ColorImage::new([w.max(1), h.max(1)], fill);
        self.new_tab_with(img, None, false);
    }

    /// Initialize and open the resize dialog with the current dimensions.
    fn open_resize_dialog(&mut self) {
        self.resize_w = self.w() as usize;
        self.resize_h = self.h() as usize;
        self.resize_aspect = self.w() as f32 / self.h() as f32;
        self.show_resize_dialog = true;
        self.focus_resize = true;
    }

    /// Resample the whole image to a new size.
    fn resize_image(&mut self, nw: usize, nh: usize) {
        let (nw, nh) = (nw.max(1), nh.max(1));
        if [nw, nh] == self.image.size {
            return;
        }
        self.commit_floating();
        self.push_undo();
        let [w, h] = self.image.size;
        let mut buf = Vec::with_capacity(w * h * 4);
        for px in &self.image.pixels {
            let [r, g, b, a] = px.to_array();
            buf.extend_from_slice(&[r, g, b, a]);
        }
        let src = image::RgbaImage::from_raw(w as u32, h as u32, buf).unwrap();
        let dst = image::imageops::resize(
            &src,
            nw as u32,
            nh as u32,
            image::imageops::FilterType::Lanczos3,
        );
        self.image = ColorImage::from_rgba_unmultiplied([nw, nh], dst.as_raw());
        self.dirty = true;
    }

    /// The current selection bounds clamped to the canvas, as (x, y, w, h).
    fn crop_rect(&self) -> Option<(i32, i32, usize, usize)> {
        let (a, b) = if let Some(f) = &self.floating {
            let [w, h] = f.img.size;
            (f.pos, (f.pos.0 + w as i32, f.pos.1 + h as i32))
        } else {
            self.selection_rect?
        };
        let x0 = a.0.min(b.0).max(0);
        let y0 = a.1.min(b.1).max(0);
        let x1 = a.0.max(b.0).min(self.w());
        let y1 = a.1.max(b.1).min(self.h());
        let (w, h) = ((x1 - x0).max(0) as usize, (y1 - y0).max(0) as usize);
        (w > 0 && h > 0).then_some((x0, y0, w, h))
    }

    /// Crop the image down to the current selection rectangle.
    fn crop_to_selection(&mut self) {
        let Some((x0, y0, w, h)) = self.crop_rect() else {
            return;
        };
        self.commit_floating(); // bake any moved/pasted selection first
        self.push_undo();
        self.image = self.capture(x0, y0, w, h);
        self.dirty = true;
    }

    fn open_file(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Images", &["png", "jpg", "jpeg", "bmp", "gif"])
            .pick_file()
        else {
            return;
        };
        self.load_path(path);
    }

    fn load_path(&mut self, path: PathBuf) {
        match image::open(&path) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width() as usize, rgba.height() as usize);
                let ci = ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
                self.new_tab_with(ci, Some(path), false);
            }
            Err(e) => eprintln!("Failed to open image: {e}"),
        }
    }

    /// New / Open create a new tab, so they never discard the current image.
    fn request_new(&mut self) {
        self.start_new_dialog();
    }

    fn start_new_dialog(&mut self) {
        self.new_w = self.w() as usize;
        self.new_h = self.h() as usize;
        self.show_new_dialog = true;
    }

    fn request_open(&mut self, path: Option<PathBuf>) {
        match path {
            Some(p) => self.load_path(p),
            None => self.open_file(),
        }
    }

    fn perform_pending(&mut self, action: Pending) {
        match action {
            Pending::CloseTab => self.close_active(),
        }
    }

    fn save_as(&mut self) {
        self.commit_floating();
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PNG", &["png"])
            .add_filter("JPEG", &["jpg", "jpeg"])
            .add_filter("BMP", &["bmp"])
            .set_file_name("untitled.png")
            .save_file()
        else {
            return;
        };
        if self.save_to(&path) {
            self.file_path = Some(path);
            self.modified = false;
        }
    }

    fn save(&mut self) {
        self.commit_floating();
        if let Some(path) = self.file_path.clone() {
            if self.save_to(&path) {
                self.modified = false;
            }
        } else {
            self.save_as();
        }
    }

    fn save_to(&self, path: &PathBuf) -> bool {
        write_image(&self.image, path)
    }
}

// ---------------------------------------------------------------------------
// Selection / clipboard logic
// ---------------------------------------------------------------------------

impl PaintApp {
    /// Dimensions of the active selection (floating, or one being dragged out).
    fn selection_size(&self) -> Option<(usize, usize)> {
        if let Some(f) = &self.floating {
            let [w, h] = f.img.size;
            Some((w, h))
        } else if let Some((a, b)) = self.selection_rect {
            Some((
                (a.0 - b.0).unsigned_abs() as usize,
                (a.1 - b.1).unsigned_abs() as usize,
            ))
        } else {
            None
        }
    }

    fn point_in_floating(&self, p: Pt) -> bool {
        if let Some(f) = &self.floating {
            let [w, h] = f.img.size;
            p.0 >= f.pos.0
                && p.1 >= f.pos.1
                && p.0 < f.pos.0 + w as i32
                && p.1 < f.pos.1 + h as i32
        } else {
            false
        }
    }

    /// Stamp a moved/pasted floating selection back into the canvas.
    fn commit_floating(&mut self) {
        if let Some(f) = self.floating.take() {
            if self.lifted {
                if !self.sel_undo_pushed {
                    self.push_undo();
                }
                self.blit(&f.img, f.pos);
                self.dirty = true;
            }
        }
        self.lifted = false;
        self.sel_undo_pushed = false;
        self.floating_grab = None;
        self.selection_rect = None;
        self.define_start = None;
        self.floating_tex = None;
        self.floating_dirty = true;
        self.resize_grip = None;
        self.resize_orig = None;
    }

    /// Lazily-initialized handle to the OS clipboard (None if unavailable).
    fn system_clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.sys_clipboard.is_none() {
            self.sys_clipboard = arboard::Clipboard::new().ok();
        }
        self.sys_clipboard.as_mut()
    }

    /// Write an image to the OS clipboard as RGBA8 (for pasting into other apps).
    /// Put the image on the OS clipboard. With `marker`, also attach a small
    /// text marker so egui/winit forwards Cmd/Ctrl+V to us — used only while the
    /// app is focused, then stripped on focus loss so other apps get a clean
    /// image-only clipboard (otherwise some apps paste the marker text).
    fn put_system_image(&mut self, img: &ColorImage, marker: bool) {
        #[cfg(target_os = "windows")]
        {
            if let Some(bmp) = encode_bmp(img) {
                if marker {
                    win_clip::set_image_and_marker(&bmp, CLIP_MARKER);
                } else {
                    win_clip::set_image(&bmp);
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let [w, h] = img.size;
            let mut bytes = Vec::with_capacity(w * h * 4);
            for px in &img.pixels {
                bytes.extend_from_slice(&px.to_srgba_unmultiplied());
            }
            if let Some(cb) = self.system_clipboard() {
                let _ = cb.set_image(arboard::ImageData {
                    width: w,
                    height: h,
                    bytes: bytes.into(),
                });
            }
            #[cfg(target_os = "macos")]
            if marker {
                mac_add_text_marker(CLIP_MARKER);
            }
            let _ = marker; // (Linux: no marker support)
        }
    }

    fn set_system_image(&mut self, img: &ColorImage) {
        self.put_system_image(img, true);
    }

    /// Remove the text marker we added (rewrite as image-only) when leaving the
    /// app, so other apps paste the image rather than the marker text.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn strip_clipboard_marker(&mut self) {
        let is_ours = if let Some(cb) = self.system_clipboard() {
            cb.get_text().ok().as_deref() == Some(CLIP_MARKER)
        } else {
            false
        };
        if !is_ours {
            return;
        }
        if let Some(img) = self.get_system_image() {
            self.put_system_image(&img, false);
            self.clipboard = Some(img);
        }
    }

    /// Read an image from the OS clipboard, if one is present.
    fn get_system_image(&mut self) -> Option<ColorImage> {
        let data = self.system_clipboard()?.get_image().ok()?;
        let (w, h) = (data.width, data.height);
        if w == 0 || h == 0 || data.bytes.len() < w * h * 4 {
            return None;
        }
        Some(ColorImage::from_rgba_unmultiplied([w, h], &data.bytes))
    }

    fn copy_selection(&mut self) {
        if let Some(f) = &self.floating {
            let img = f.img.clone();
            self.clipboard = Some(img.clone());
            self.set_system_image(&img);
        }
    }

    fn cut_selection(&mut self) {
        self.copy_selection();
        self.delete_selection();
    }

    fn delete_selection(&mut self) {
        if let Some(f) = self.floating.take() {
            if !self.lifted {
                // content still in base -> erase it now
                self.push_undo();
                let [w, h] = f.img.size;
                self.fill_region(f.pos.0, f.pos.1, w, h, Color32::TRANSPARENT);
                self.dirty = true;
            }
            // if lifted, base was already erased at lift time
        }
        self.lifted = false;
        self.sel_undo_pushed = false;
        self.floating_grab = None;
        self.selection_rect = None;
        self.floating_tex = None;
        self.floating_dirty = true;
    }

    fn paste_clipboard(&mut self) {
        // Prefer an image on the OS clipboard (possibly from another app);
        // fall back to our internal exact-pixel copy.
        let Some(img) = self.get_system_image().or_else(|| self.clipboard.clone()) else {
            return;
        };
        // Ask whether to paste into a new tab or the current image.
        self.pending_paste_img = Some(img);
        self.show_paste_dest_dialog = true;
    }

    /// Paste into the current image (oversized images prompt enlarge/clip).
    fn paste_into_current(&mut self, img: ColorImage) {
        let [pw, ph] = img.size;
        if pw as i32 > self.w() || ph as i32 > self.h() {
            self.pending_paste = Some(img);
            self.show_paste_dialog = true;
        } else {
            self.place_paste(img);
        }
    }

    /// Select the entire image (Cmd/Ctrl+A).
    fn select_all(&mut self) {
        self.commit_floating();
        let (w, h) = (self.w() as usize, self.h() as usize);
        if w == 0 || h == 0 {
            return;
        }
        let img = self.capture(0, 0, w, h);
        self.floating = Some(Floating { img, pos: (0, 0) });
        self.lifted = false;
        self.sel_undo_pushed = false;
        self.floating_dirty = true;
        self.tool = Tool::Select;
    }

    /// Nudge the active selection by a whole-pixel delta (arrow keys).
    fn nudge_selection(&mut self, d: Pt) {
        // Lift it off the canvas on the first move, like a drag.
        if !self.lifted {
            if let Some(f) = &self.floating {
                let (pos, [w, h]) = (f.pos, f.img.size);
                self.push_undo();
                self.sel_undo_pushed = true;
                self.fill_region(pos.0, pos.1, w, h, Color32::TRANSPARENT);
                self.lifted = true;
                self.floating_dirty = true;
            }
        }
        if let Some(f) = &mut self.floating {
            f.pos = (f.pos.0 + d.0, f.pos.1 + d.1);
        }
    }

    /// Drop a pasted image onto the canvas as a movable floating selection.
    fn place_paste(&mut self, img: ColorImage) {
        self.commit_floating();
        self.floating = Some(Floating { img, pos: (0, 0) });
        self.lifted = true;
        self.sel_undo_pushed = false;
        self.tool = Tool::Select;
        self.floating_dirty = true;
    }

    /// Grow the canvas to at least nw × nh, keeping existing pixels top-left.
    fn enlarge_canvas(&mut self, nw: usize, nh: usize) {
        let [w, h] = self.image.size;
        let (nw, nh) = (nw.max(w), nh.max(h));
        if [nw, nh] == self.image.size {
            return;
        }
        self.push_undo();
        let mut grown = ColorImage::new([nw, nh], Color32::WHITE);
        for y in 0..h {
            for x in 0..w {
                grown.pixels[y * nw + x] = self.image.pixels[y * w + x];
            }
        }
        self.image = grown;
        self.dirty = true;
    }

    /// When the clipboard holds an image but no text (e.g. a screenshot or an
    /// image copied from another app), re-publish it ourselves with a text
    /// marker attached. egui/winit only forwards Cmd/Ctrl+V when the clipboard
    /// carries text, so this is what makes keyboard paste work for those.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn ensure_clipboard_marker(&mut self) {
        let has_text = if let Some(cb) = self.system_clipboard() {
            cb.get_text().is_ok()
        } else {
            false
        };
        if has_text {
            // Already has text -> Cmd+V is delivered; leave the clipboard alone.
            return;
        }
        if let Some(img) = self.get_system_image() {
            // Re-write the image (so we own the pasteboard) plus a text marker.
            self.set_system_image(&img);
            self.clipboard = Some(img);
        }
    }
}

// ---------------------------------------------------------------------------
// Freehand & drag-shape strokes
// ---------------------------------------------------------------------------

impl PaintApp {
    fn paint_color(&self) -> Color32 {
        match self.tool {
            // The eraser clears to transparency.
            Tool::Eraser => Color32::TRANSPARENT,
            _ => {
                if self.active_secondary {
                    self.bg
                } else {
                    self.fg
                }
            }
        }
    }

    fn begin_stroke(&mut self, pos: Pt, secondary: bool) {
        self.active_secondary = secondary;

        if self.tool == Tool::Eyedropper {
            if let Some(c) = self.get_pixel(pos.0, pos.1) {
                if secondary {
                    self.bg = c;
                } else {
                    self.fg = c;
                }
            }
            return;
        }

        self.push_undo();
        let color = self.paint_color();
        match self.tool {
            Tool::Pencil => {
                self.stamp(pos.0, pos.1, color, 1);
                self.last_pos = Some(pos);
            }
            Tool::Brush | Tool::Eraser => {
                self.stamp(pos.0, pos.1, color, self.brush_size);
                self.last_pos = Some(pos);
            }
            Tool::Fill => self.flood_fill(pos, color),
            Tool::Line | Tool::Arrow | Tool::Rectangle | Tool::RoundedRect | Tool::Ellipse => {
                self.snapshot = Some(self.image.clone());
                self.shape_start = Some(pos);
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn continue_stroke(&mut self, pos: Pt) {
        let color = self.paint_color();
        match self.tool {
            Tool::Pencil => {
                if let Some(last) = self.last_pos {
                    self.draw_line(last, pos, color, 1);
                }
                self.last_pos = Some(pos);
            }
            Tool::Brush | Tool::Eraser => {
                if let Some(last) = self.last_pos {
                    self.draw_line(last, pos, color, self.brush_size);
                }
                self.last_pos = Some(pos);
            }
            Tool::Line | Tool::Arrow | Tool::Rectangle | Tool::RoundedRect | Tool::Ellipse => {
                if let (Some(base), Some(start)) = (self.snapshot.clone(), self.shape_start) {
                    self.image = base;
                    let size = self.brush_size.max(1);
                    let pos = if self.constrain {
                        constrain_pos(start, pos, matches!(self.tool, Tool::Line | Tool::Arrow))
                    } else {
                        pos
                    };
                    match self.tool {
                        Tool::Line => self.draw_line(start, pos, color, size),
                        Tool::Arrow => self.draw_arrow(start, pos, color, size),
                        Tool::Rectangle => self.draw_rect(start, pos, color, self.fill_shapes, size),
                        Tool::RoundedRect => self.draw_rounded_rect(
                            start,
                            pos,
                            self.corner_radius,
                            color,
                            self.fill_shapes,
                            size,
                        ),
                        Tool::Ellipse => {
                            self.draw_ellipse(start, pos, color, self.fill_shapes, size)
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn end_stroke(&mut self) {
        self.last_pos = None;
        self.shape_start = None;
        self.snapshot = None;
    }

    fn commit_polygon(&mut self) {
        if self.poly_points.len() >= 2 {
            self.push_undo();
            let color = self.fg;
            let size = self.brush_size.max(1);
            if self.fill_shapes && self.poly_points.len() >= 3 {
                let pts = self.poly_points.clone();
                self.fill_polygon(&pts, color);
            }
            let pts = self.poly_points.clone();
            self.draw_polyline(&pts, true, color, size);
            self.dirty = true;
        }
        self.poly_points.clear();
    }

    fn rasterize_text(&mut self, ctx: &egui::Context, origin: Pt, text: &str, px: f32, color: Color32) {
        if text.is_empty() {
            return;
        }
        let ppp = ctx.pixels_per_point();
        let galley = ctx.fonts(|f| {
            f.layout(
                text.to_owned(),
                FontId::proportional(px / ppp),
                color,
                f32::INFINITY,
            )
        });
        let font_img = ctx.fonts(|f| f.image());
        let aw = font_img.size[0];
        for row in &galley.rows {
            for g in &row.glyphs {
                let (mn, mx) = (g.uv_rect.min, g.uv_rect.max);
                if mx[0] <= mn[0] || mx[1] <= mn[1] {
                    continue;
                }
                let dx = origin.0 + ((g.pos.x + g.uv_rect.offset.x) * ppp).round() as i32;
                let dy = origin.1 + ((g.pos.y + g.uv_rect.offset.y) * ppp).round() as i32;
                for sy in mn[1]..mx[1] {
                    for sx in mn[0]..mx[0] {
                        let cov = font_img.pixels[sy as usize * aw + sx as usize];
                        if cov <= 0.0 {
                            continue;
                        }
                        let a = cov.powf(0.55).min(1.0);
                        let px_x = dx + (sx - mn[0]) as i32;
                        let px_y = dy + (sy - mn[1]) as i32;
                        if let Some(d) = self.get_pixel(px_x, px_y) {
                            let blended = over(
                                Color32::from_rgba_unmultiplied(
                                    color.r(),
                                    color.g(),
                                    color.b(),
                                    (a * 255.0) as u8,
                                ),
                                d,
                            );
                            self.set_pixel(px_x, px_y, blended);
                        }
                    }
                }
            }
        }
        self.dirty = true;
    }

    fn commit_text(&mut self, ctx: &egui::Context) {
        if let Some(pos) = self.text_pos.take() {
            if !self.text_buf.is_empty() {
                self.push_undo();
                let (text, size, color) = (self.text_buf.clone(), self.text_size, self.fg);
                self.rasterize_text(ctx, pos, &text, size, color);
            }
        }
        self.text_buf.clear();
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

const PALETTE: [(u8, u8, u8); 28] = [
    (0, 0, 0),
    (128, 128, 128),
    (128, 0, 0),
    (128, 128, 0),
    (0, 128, 0),
    (0, 128, 128),
    (0, 0, 128),
    (128, 0, 128),
    (128, 128, 64),
    (0, 64, 64),
    (0, 128, 255),
    (0, 64, 128),
    (64, 0, 255),
    (128, 64, 0),
    (255, 255, 255),
    (192, 192, 192),
    (255, 0, 0),
    (255, 255, 0),
    (0, 255, 0),
    (0, 255, 255),
    (0, 0, 255),
    (255, 0, 255),
    (255, 255, 128),
    (0, 255, 128),
    (128, 255, 255),
    (128, 128, 255),
    (255, 0, 128),
    (255, 128, 64),
];

/// Draw a crisp vector icon for a tool inside `r` (no font glyphs involved).
fn paint_tool_icon(p: &egui::Painter, r: Rect, tool: Tool, color: Color32) {
    use egui::Shape;
    use std::f32::consts::{FRAC_PI_2, TAU};
    let pad = r.width() * 0.16;
    let b = Rect::from_min_max(
        Pos2::new(r.min.x + pad, r.min.y + pad),
        Pos2::new(r.max.x - pad, r.max.y - pad),
    );
    let sw = (b.width() * 0.11).max(1.4);
    let st = Stroke::new(sw, color);
    let none = Stroke::NONE;
    let pt = |fx: f32, fy: f32| Pos2::new(b.min.x + fx * b.width(), b.min.y + fy * b.height());
    let seg = |a: Pos2, c: Pos2| {
        p.line_segment([a, c], st);
    };
    let poly = |pts: Vec<Pos2>, fill: Color32, stroke: Stroke| {
        p.add(Shape::convex_polygon(pts, fill, stroke));
    };
    match tool {
        Tool::Pencil => {
            seg(pt(0.22, 0.80), pt(0.80, 0.22));
            seg(pt(0.80, 0.22), pt(0.94, 0.36));
            poly(vec![pt(0.06, 0.94), pt(0.12, 0.64), pt(0.36, 0.88)], color, none);
        }
        Tool::Brush => {
            seg(pt(0.92, 0.12), pt(0.50, 0.54));
            seg(pt(0.44, 0.48), pt(0.58, 0.62));
            poly(vec![pt(0.14, 0.94), pt(0.30, 0.50), pt(0.54, 0.74)], color, none);
        }
        Tool::Eraser => {
            poly(
                vec![pt(0.10, 0.56), pt(0.50, 0.18), pt(0.90, 0.56), pt(0.50, 0.92)],
                Color32::TRANSPARENT,
                st,
            );
            seg(pt(0.30, 0.74), pt(0.70, 0.38));
        }
        Tool::Fill => {
            poly(
                vec![pt(0.18, 0.40), pt(0.56, 0.08), pt(0.86, 0.46), pt(0.48, 0.78)],
                Color32::TRANSPARENT,
                st,
            );
            p.add(Shape::circle_filled(pt(0.86, 0.80), sw * 1.5, color));
        }
        Tool::Eyedropper => {
            seg(pt(0.22, 0.82), pt(0.70, 0.34));
            p.circle_stroke(pt(0.80, 0.24), b.width() * 0.11, st);
            poly(vec![pt(0.10, 0.92), pt(0.16, 0.72), pt(0.32, 0.84)], color, none);
        }
        Tool::Line => seg(pt(0.12, 0.88), pt(0.88, 0.12)),
        Tool::Arrow => {
            seg(pt(0.12, 0.88), pt(0.86, 0.16));
            seg(pt(0.86, 0.16), pt(0.58, 0.20));
            seg(pt(0.86, 0.16), pt(0.82, 0.46));
        }
        Tool::Rectangle => {
            p.rect_stroke(Rect::from_min_max(pt(0.12, 0.22), pt(0.88, 0.78)), 0.0, st);
        }
        Tool::RoundedRect => {
            p.rect_stroke(
                Rect::from_min_max(pt(0.12, 0.22), pt(0.88, 0.78)),
                b.width() * 0.18,
                st,
            );
        }
        Tool::Ellipse => {
            p.circle_stroke(b.center(), b.width() * 0.40, st);
        }
        Tool::Polygon => {
            let pts = (0..5)
                .map(|k| {
                    let a = -FRAC_PI_2 + k as f32 * TAU / 5.0;
                    Pos2::new(
                        b.center().x + a.cos() * b.width() * 0.46,
                        b.center().y + a.sin() * b.height() * 0.46,
                    )
                })
                .collect();
            poly(pts, Color32::TRANSPARENT, st);
        }
        Tool::Curve => {
            let (p0, c1, c2, p1) = (pt(0.10, 0.86), pt(0.28, 0.0), pt(0.72, 1.0), pt(0.90, 0.14));
            let pts = (0..=24)
                .map(|k| {
                    let t = k as f32 / 24.0;
                    let u = 1.0 - t;
                    Pos2::new(
                        u * u * u * p0.x + 3.0 * u * u * t * c1.x + 3.0 * u * t * t * c2.x + t * t * t * p1.x,
                        u * u * u * p0.y + 3.0 * u * u * t * c1.y + 3.0 * u * t * t * c2.y + t * t * t * p1.y,
                    )
                })
                .collect();
            p.add(Shape::line(pts, st));
        }
        Tool::Text => {
            p.text(
                b.center(),
                egui::Align2::CENTER_CENTER,
                "A",
                egui::FontId::proportional(b.height() * 1.05),
                color,
            );
        }
        Tool::Select => {
            let rect = Rect::from_min_max(pt(0.12, 0.18), pt(0.88, 0.82));
            let corners = [
                rect.left_top(),
                rect.right_top(),
                rect.right_bottom(),
                rect.left_bottom(),
                rect.left_top(),
            ];
            let mut shapes = Vec::new();
            for w in corners.windows(2) {
                shapes.extend(Shape::dashed_line(w, st, b.width() * 0.13, b.width() * 0.10));
            }
            p.extend(shapes);
        }
    }
}

fn marching_ants(painter: &egui::Painter, rect: Rect) {
    painter.rect_stroke(rect, 0.0, Stroke::new(1.0, Color32::WHITE));
    let pts = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    let mut shapes = Vec::new();
    for w in pts.windows(2) {
        shapes.extend(egui::Shape::dashed_line(
            w,
            Stroke::new(1.0, Color32::BLACK),
            4.0,
            4.0,
        ));
    }
    painter.extend(shapes);
}

/// Resample an image to a new size (bilinear; fast enough for live dragging).
fn resample_image(src: &ColorImage, nw: usize, nh: usize) -> ColorImage {
    let (nw, nh) = (nw.max(1), nh.max(1));
    let [w, h] = src.size;
    let mut buf = Vec::with_capacity(w * h * 4);
    for px in &src.pixels {
        buf.extend_from_slice(&px.to_array());
    }
    match image::RgbaImage::from_raw(w as u32, h as u32, buf) {
        Some(rgba) => {
            let dst = image::imageops::resize(
                &rgba,
                nw as u32,
                nh as u32,
                image::imageops::FilterType::Triangle,
            );
            ColorImage::from_rgba_unmultiplied([nw, nh], dst.as_raw())
        }
        None => src.clone(),
    }
}

/// Resize the canvas (crop/extend, not resample), keeping pixels at top-left.
fn canvas_resized_from(orig: &ColorImage, nw: usize, nh: usize) -> ColorImage {
    let (nw, nh) = (nw.max(1), nh.max(1));
    let mut img = ColorImage::new([nw, nh], Color32::WHITE);
    let [ow, oh] = orig.size;
    for y in 0..oh.min(nh) {
        for x in 0..ow.min(nw) {
            img.pixels[y * nw + x] = orig.pixels[y * ow + x];
        }
    }
    img
}

/// Screen position of a canvas resize handle, placed just outside the image.
fn canvas_grip_pos(img: Rect, g: Grip) -> Pos2 {
    const OUT: f32 = 11.0;
    match g {
        Grip::E => Pos2::new(img.max.x + OUT, img.center().y),
        Grip::S => Pos2::new(img.center().x, img.max.y + OUT),
        Grip::Se => Pos2::new(img.max.x + OUT, img.max.y + OUT),
        _ => img.center(),
    }
}

/// Draw small square resize handles at the given grips of a screen rect.
fn draw_grips(painter: &egui::Painter, r: Rect, grips: &[Grip]) {
    for g in grips {
        let (fx, fy) = g.frac();
        let c = Pos2::new(r.min.x + r.width() * fx, r.min.y + r.height() * fy);
        let hr = Rect::from_center_size(c, Vec2::splat(10.0));
        painter.rect(hr, 2.0, Color32::WHITE, Stroke::new(1.0, Color32::from_gray(70)));
    }
}

impl PaintApp {
    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::horizontal()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    ui.spacing_mut().button_padding = egui::vec2(10.0, 7.0);
                    let mut switch = None;
                    let mut close = None;
                    for i in 0..self.docs.len() {
                        let selected = i == self.active;
                        let label = egui::RichText::new(self.tab_label(i)).size(14.5);
                        let resp = ui.selectable_label(selected, label);
                        if resp.clicked() {
                            switch = Some(i);
                        }
                        if resp.middle_clicked() {
                            close = Some(i);
                        }
                        if ui
                            .button(egui::RichText::new("×").size(15.0))
                            .on_hover_text("Close tab")
                            .clicked()
                        {
                            close = Some(i);
                        }
                        ui.separator();
                    }
                    if ui
                        .button(egui::RichText::new("+").size(15.0))
                        .on_hover_text("New tab (⌘T)")
                        .clicked()
                    {
                        self.new_tab();
                    }
                    if let Some(i) = switch {
                        self.switch_to(i);
                    }
                    if let Some(i) = close {
                        self.request_close_tab(i);
                    }
                });
            });
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::menu::bar(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("New…    ⌘N").clicked() {
                    self.request_new();
                    ui.close_menu();
                }
                if ui.button("New Tab    ⌘T").clicked() {
                    self.new_tab();
                    ui.close_menu();
                }
                if ui.button("Open…    ⌘O").clicked() {
                    self.request_open(None);
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Save    ⌘S").clicked() {
                    self.save();
                    ui.close_menu();
                }
                if ui.button("Save As…").clicked() {
                    self.save_as();
                    ui.close_menu();
                }
                if ui.button("Save All").clicked() {
                    self.save_all();
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Close Tab    ⌘W").clicked() {
                    self.request_close_tab(self.active);
                    ui.close_menu();
                }
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui
                    .add_enabled(!self.undo_stack.is_empty(), egui::Button::new("Undo"))
                    .clicked()
                {
                    self.undo();
                    ui.close_menu();
                }
                if ui
                    .add_enabled(!self.redo_stack.is_empty(), egui::Button::new("Redo"))
                    .clicked()
                {
                    self.redo();
                    ui.close_menu();
                }
                ui.separator();
                let has_sel = self.floating.is_some();
                if ui.add_enabled(has_sel, egui::Button::new("Cut")).clicked() {
                    self.cut_selection();
                    ui.close_menu();
                }
                if ui.add_enabled(has_sel, egui::Button::new("Copy")).clicked() {
                    self.copy_selection();
                    ui.close_menu();
                }
                // Paste may pull an image from another app, so keep it enabled.
                if ui.button("Paste").clicked() {
                    self.paste_clipboard();
                    ui.close_menu();
                }
                if ui
                    .add_enabled(has_sel, egui::Button::new("Delete"))
                    .clicked()
                {
                    self.delete_selection();
                    ui.close_menu();
                }
            });
            ui.menu_button("Image", |ui| {
                if ui.button("Resize…    ⌘E").clicked() {
                    self.open_resize_dialog();
                    ui.close_menu();
                }
                if ui
                    .add_enabled(
                        self.selection_size().is_some(),
                        egui::Button::new("Crop to Selection"),
                    )
                    .clicked()
                {
                    self.crop_to_selection();
                    ui.close_menu();
                }
            });
            ui.menu_button("View", |ui| {
                if ui
                    .checkbox(&mut self.show_grid, "Show grid    ⌘G")
                    .clicked()
                {
                    ui.close_menu();
                }
                ui.checkbox(&mut self.antialias, "Anti-aliasing")
                    .on_hover_text("Smooth edges on new strokes and shapes");
                ui.separator();
                if ui.button("Zoom in    ⌘+").clicked() {
                    self.zoom_req = Some(self.zoom * 1.25);
                    self.zoom_anchor = None;
                    ui.close_menu();
                }
                if ui.button("Zoom out    ⌘−").clicked() {
                    self.zoom_req = Some(self.zoom / 1.25);
                    self.zoom_anchor = None;
                    ui.close_menu();
                }
                if ui.button("Fit to window").clicked() {
                    self.fit_requested = true;
                    ui.close_menu();
                }
                ui.label("Zoom");
                for z in [0.25f32, 0.5, 1.0, 2.0, 4.0, 8.0] {
                    let label = if z < 1.0 {
                        format!("{}%", (z * 100.0) as i32)
                    } else {
                        format!("{}×", z as i32)
                    };
                    if ui
                        .selectable_label((self.zoom - z).abs() < 1e-3, label)
                        .clicked()
                    {
                        self.zoom = z;
                        ui.close_menu();
                    }
                }
                ui.separator();
                ui.label("Theme");
                for choice in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
                    if ui
                        .selectable_label(self.theme == choice, choice.label())
                        .clicked()
                    {
                        self.theme = choice;
                        ui.close_menu();
                    }
                }
            });
            ui.separator();
            ui.label(format!("{} × {}", self.w(), self.h()));
        });
    }

    /// A toolbox entry: a themed icon + label button that highlights when active.
    fn tool_button(&mut self, ui: &mut egui::Ui, t: Tool) {
        let selected = self.tool == t;
        let w = ui.available_width().max(60.0);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 30.0), Sense::click());
        let vis = ui.style().interact_selectable(&resp, selected);
        let p = ui.painter();
        p.rect(rect, egui::Rounding::same(6.0), vis.weak_bg_fill, vis.bg_stroke);
        let icon_rect = Rect::from_min_size(
            Pos2::new(rect.min.x + 7.0, rect.center().y - 11.0),
            Vec2::splat(22.0),
        );
        paint_tool_icon(p, icon_rect, t, vis.fg_stroke.color);
        p.text(
            Pos2::new(icon_rect.max.x + 7.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            t.label(),
            egui::FontId::proportional(12.5),
            vis.fg_stroke.color,
        );
        if resp.clicked() {
            self.tool = t;
        }
        resp.on_hover_text(format!("{} ({})", t.label(), t.hotkey().1));
    }

    fn toolbox(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.heading("Tools");
        ui.add_space(6.0);
        for t in Tool::ALL {
            self.tool_button(ui, t);
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        ui.label("Size");
        ui.add(egui::Slider::new(&mut self.brush_size, 1..=64).suffix(" px"));

        if matches!(
            self.tool,
            Tool::Rectangle | Tool::RoundedRect | Tool::Ellipse | Tool::Polygon
        ) {
            ui.add_space(2.0);
            ui.checkbox(&mut self.fill_shapes, "Fill shape");
        }
        if self.tool == Tool::RoundedRect {
            ui.label("Corner radius");
            ui.add(egui::Slider::new(&mut self.corner_radius, 0..=200));
        }
        if self.tool == Tool::Fill {
            ui.label("Tolerance");
            ui.add(egui::Slider::new(&mut self.fill_tolerance, 0..=128))
                .on_hover_text("How close a color must be to the clicked pixel to be filled");
        }
        if self.tool == Tool::Text {
            ui.label("Font size");
            ui.add(egui::Slider::new(&mut self.text_size, 8.0..=200.0));
        }
        if matches!(self.tool, Tool::Polygon | Tool::Curve) {
            ui.add_space(2.0);
            ui.small("Click to add points.\nEnter / double-click = finish\nEsc = cancel");
        }
        if self.tool == Tool::Select {
            ui.add_space(2.0);
            ui.small("Drag to select, drag inside to move.\n⌘C/⌘X/⌘V, Del");
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("FG");
            ui.color_edit_button_srgba(&mut self.fg);
            ui.label("BG");
            ui.color_edit_button_srgba(&mut self.bg);
        });
        if ui
            .button("Swap colors (X)")
            .on_hover_text("Swap foreground and background")
            .clicked()
        {
            std::mem::swap(&mut self.fg, &mut self.bg);
        }

        if !self.recent_colors.is_empty() {
            ui.add_space(6.0);
            ui.label("Recent");
            let recents = self.recent_colors.clone();
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(2.0);
                for color in recents {
                    let (r, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                    ui.painter().rect_filled(r, 2.0, color);
                    ui.painter()
                        .rect_stroke(r, 2.0, Stroke::new(1.0, Color32::from_gray(90)));
                    if resp.clicked() {
                        self.fg = color;
                    }
                    if resp.secondary_clicked() {
                        self.bg = color;
                    }
                }
            });
        }
    }

    fn palette(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (rect, ind) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::click());
            let p = ui.painter();
            let bg_rect = Rect::from_min_size(rect.min + Vec2::splat(10.0), Vec2::splat(26.0));
            let fg_rect = Rect::from_min_size(rect.min, Vec2::splat(26.0));
            // checker behind so a transparent color reads as checkerboard
            let checker = |r: Rect| {
                let s = r.width() / 2.0;
                for j in 0..2 {
                    for i in 0..2 {
                        let g = if (i + j) % 2 == 0 { 205 } else { 165 };
                        let c = Rect::from_min_size(
                            Pos2::new(r.min.x + i as f32 * s, r.min.y + j as f32 * s),
                            Vec2::splat(s),
                        );
                        p.rect_filled(c, 0.0, Color32::from_gray(g));
                    }
                }
            };
            checker(bg_rect);
            p.rect_filled(bg_rect, 2.0, self.bg);
            p.rect_stroke(bg_rect, 2.0, Stroke::new(1.0, Color32::GRAY));
            checker(fg_rect);
            p.rect_filled(fg_rect, 2.0, self.fg);
            p.rect_stroke(fg_rect, 2.0, Stroke::new(1.0, Color32::DARK_GRAY));
            if ind.on_hover_text("Click to swap (X)").clicked() {
                std::mem::swap(&mut self.fg, &mut self.bg);
            }

            ui.add_space(8.0);

            // Transparent swatch (checkerboard); L = foreground, R = background.
            let (tr, tresp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
            let tp = ui.painter();
            let cs = 6.5;
            for j in 0..4 {
                for i in 0..4 {
                    let gray = if (i + j) % 2 == 0 { 205 } else { 165 };
                    let cell = Rect::from_min_size(
                        Pos2::new(tr.min.x + i as f32 * cs, tr.min.y + j as f32 * cs),
                        Vec2::splat(cs),
                    );
                    tp.rect_filled(cell.intersect(tr), 0.0, Color32::from_gray(gray));
                }
            }
            tp.rect_stroke(tr, 2.0, Stroke::new(1.0, Color32::DARK_GRAY));
            if tresp.clicked() {
                self.fg = Color32::TRANSPARENT;
            }
            if tresp.secondary_clicked() {
                self.bg = Color32::TRANSPARENT;
            }
            tresp.on_hover_text("Transparent — L: foreground, R: background");

            ui.add_space(8.0);

            egui::Grid::new("palette")
                .num_columns(14)
                .spacing([2.0, 2.0])
                .show(ui, |ui| {
                    for (i, (r, g, b)) in PALETTE.iter().enumerate() {
                        let color = Color32::from_rgb(*r, *g, *b);
                        let (rect, resp) =
                            ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                        ui.painter().rect_filled(rect, 1.0, color);
                        ui.painter()
                            .rect_stroke(rect, 1.0, Stroke::new(1.0, Color32::from_gray(90)));
                        if resp.clicked() {
                            self.fg = color;
                        }
                        if resp.secondary_clicked() {
                            self.bg = color;
                        }
                        if i == 13 {
                            ui.end_row();
                        }
                    }
                });

            ui.add_space(8.0);
            ui.label("L-click: foreground · R-click: background");
        });
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        // Pad the interactive area so resize handles can sit just *outside* the
        // image and still receive clicks.
        let margin = 22.0;
        let avail = ui.available_rect_before_wrap();

        // ⌘/Ctrl+scroll (and trackpad pinch) zoom toward the cursor. egui folds
        // modifier+wheel into a zoom factor, exposed via `zoom_delta()`.
        let (zoom_d, hover) = ui.input(|i| (i.zoom_delta(), i.pointer.hover_pos()));
        if (zoom_d - 1.0).abs() > 1e-3 {
            if let Some(cur) = hover {
                if avail.contains(cur) {
                    self.zoom_req = Some(self.zoom * zoom_d);
                    self.zoom_anchor = Some(cur);
                }
            }
        }

        // Apply a pending zoom/fit request, anchored so a point stays put.
        if self.fit_requested {
            self.fit_requested = false;
            let (iw, ih) = (self.w() as f32, self.h() as f32);
            let fz = ((avail.width() - 2.0 * margin) / iw)
                .min((avail.height() - 2.0 * margin) / ih);
            self.zoom = fz.clamp(0.05, 16.0);
            self.pending_scroll = Some(Vec2::ZERO);
        } else if let Some(target) = self.zoom_req.take() {
            let old = self.zoom;
            let nz = target.clamp(0.1, 16.0);
            let anchor = self.zoom_anchor.take().unwrap_or(avail.center());
            let pixel = (anchor - self.last_img_min) / old;
            self.pending_scroll = Some(self.scroll_offset + pixel * (nz - old));
            self.zoom = nz;
        }

        let scale = self.zoom;
        let img_size = Vec2::new(self.w() as f32 * scale, self.h() as f32 * scale);
        let alloc_size = img_size + Vec2::splat(2.0 * margin);

        let typing = {
            let f = ui.ctx().memory(|m| m.focused());
            f.is_some() && f != self.canvas_id
        };
        let space_pan = !typing && ui.input(|i| i.key_down(egui::Key::Space));
        let mid_down = ui.input(|i| i.pointer.middle_down());
        let mut area = egui::ScrollArea::both();
        if let Some(off) = self.pending_scroll.take() {
            area = area.scroll_offset(off);
        }
        let out = area.show(ui, |ui| {
            let (rect, response) = ui.allocate_exact_size(alloc_size, Sense::click_and_drag());
            let img_rect = Rect::from_min_size(rect.min + Vec2::splat(margin), img_size);
            self.last_img_min = img_rect.min;
            self.canvas_id = Some(response.id);

            // Checkerboard behind the canvas so transparent pixels are visible.
            if let Some(ck) = &self.checker_tex {
                let cell = 8.0;
                let uv = Rect::from_min_max(
                    Pos2::ZERO,
                    Pos2::new(
                        img_rect.width() / (2.0 * cell),
                        img_rect.height() / (2.0 * cell),
                    ),
                );
                ui.painter().image(ck.id(), img_rect, uv, Color32::WHITE);
            }

            if let Some(tex) = &self.texture {
                ui.painter().image(
                    tex.id(),
                    img_rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            ui.painter()
                .rect_stroke(img_rect, 0.0, Stroke::new(1.0, Color32::from_gray(128)));

            let to_img = |p: Pos2| -> Pt {
                (
                    ((p.x - img_rect.min.x) / scale).floor() as i32,
                    ((p.y - img_rect.min.y) / scale).floor() as i32,
                )
            };
            let to_screen = |pt: Pt| -> Pos2 {
                Pos2::new(
                    img_rect.min.x + (pt.0 as f32 + 0.5) * scale,
                    img_rect.min.y + (pt.1 as f32 + 0.5) * scale,
                )
            };

            let ptr = response.interact_pointer_pos().map(to_img);
            let secondary = ui.input(|i| i.pointer.secondary_down());
            self.constrain = ui.input(|i| i.modifiers.shift);
            let alt_pick = ui.input(|i| i.modifiers.alt);

            // Middle-button drag pans (latched while held); so does Space-drag.
            if mid_down {
                if response.hovered() || self.mid_panning {
                    self.mid_panning = true;
                }
            } else {
                self.mid_panning = false;
            }
            let panning = space_pan || self.mid_panning;
            if panning {
                let delta = if self.mid_panning {
                    ui.input(|i| i.pointer.delta())
                } else if response.dragged() {
                    response.drag_delta()
                } else {
                    Vec2::ZERO
                };
                if delta != Vec2::ZERO {
                    self.pending_scroll = Some(self.scroll_offset - delta);
                }
            } else if alt_pick {
                // Alt-hold temporarily picks the color under the cursor.
                if let Some(p) = ptr {
                    if let Some(c) = self.get_pixel(p.0, p.1) {
                        self.fg = c;
                    }
                }
            } else {
            // Resize handles take priority over the active tool (any tool).
            let resizing = self.handle_resize(&response, img_rect, scale);
            if !resizing {
              match self.tool {
                Tool::Pencil
                | Tool::Brush
                | Tool::Eraser
                | Tool::Line
                | Tool::Arrow
                | Tool::Rectangle
                | Tool::RoundedRect
                | Tool::Ellipse => {
                    if response.drag_started() {
                        if let Some(p) = ptr {
                            self.begin_stroke(p, secondary);
                        }
                    } else if response.dragged() {
                        if let Some(p) = ptr {
                            self.continue_stroke(p);
                        }
                    } else if response.drag_stopped() {
                        self.end_stroke();
                    } else if response.clicked() {
                        if let Some(p) = ptr {
                            self.begin_stroke(p, false);
                            self.end_stroke();
                        }
                    }
                }
                Tool::Fill | Tool::Eyedropper => {
                    if response.clicked() {
                        if let Some(p) = ptr {
                            self.begin_stroke(p, false);
                        }
                    } else if response.secondary_clicked() {
                        if let Some(p) = ptr {
                            self.begin_stroke(p, true);
                        }
                    }
                }
                Tool::Polygon => {
                    if response.double_clicked() {
                        self.commit_polygon();
                    } else if response.clicked() {
                        if let Some(p) = ptr {
                            self.poly_points.push(p);
                        }
                    } else if response.secondary_clicked() {
                        self.commit_polygon();
                    }
                }
                Tool::Curve => self.canvas_curve(&response, ptr),
                Tool::Text => {
                    if response.drag_started() {
                        if let Some(p) = ptr {
                            // Drag to reposition: grab relative to current anchor
                            // (or start a fresh anchor under the cursor).
                            let anchor = self.text_pos.unwrap_or(p);
                            self.text_grab = Some((p.0 - anchor.0, p.1 - anchor.1));
                            self.text_pos = Some(anchor);
                        }
                    } else if response.dragged() {
                        if let (Some(p), Some(g)) = (ptr, self.text_grab) {
                            self.text_pos = Some((p.0 - g.0, p.1 - g.1));
                        }
                    } else if response.drag_stopped() {
                        self.text_grab = None;
                    } else if response.clicked() {
                        if let Some(p) = ptr {
                            self.text_pos = Some(p);
                        }
                    }
                }
                Tool::Select => self.canvas_select(&response, ptr),
              }
            }
            }

            // ---- overlays ----
            let painter = ui.painter_at(rect);

            // pixel grid (only legible when zoomed in); limited to the viewport
            if self.show_grid && scale >= 3.0 {
                let vis = ui.clip_rect();
                let grid_col = if ui.visuals().dark_mode {
                    Color32::from_white_alpha(36)
                } else {
                    Color32::from_black_alpha(36)
                };
                let gs = Stroke::new(1.0, grid_col);
                let y_top = img_rect.min.y.max(vis.min.y);
                let y_bot = img_rect.max.y.min(vis.max.y);
                let x_left = img_rect.min.x.max(vis.min.x);
                let x_right = img_rect.max.x.min(vis.max.x);
                let i0 = (((vis.min.x - img_rect.min.x) / scale).floor() as i32).clamp(0, self.w());
                let i1 = (((vis.max.x - img_rect.min.x) / scale).ceil() as i32).clamp(0, self.w());
                for i in i0..=i1 {
                    let x = img_rect.min.x + i as f32 * scale;
                    painter.line_segment([Pos2::new(x, y_top), Pos2::new(x, y_bot)], gs);
                }
                let j0 = (((vis.min.y - img_rect.min.y) / scale).floor() as i32).clamp(0, self.h());
                let j1 = (((vis.max.y - img_rect.min.y) / scale).ceil() as i32).clamp(0, self.h());
                for j in j0..=j1 {
                    let y = img_rect.min.y + j as f32 * scale;
                    painter.line_segment([Pos2::new(x_left, y), Pos2::new(x_right, y)], gs);
                }
            }

            // selection
            if let Some(f) = &self.floating {
                let [w, h] = f.img.size;
                let r = Rect::from_min_size(
                    Pos2::new(
                        img_rect.min.x + f.pos.0 as f32 * scale,
                        img_rect.min.y + f.pos.1 as f32 * scale,
                    ),
                    Vec2::new(w as f32 * scale, h as f32 * scale),
                );
                if self.lifted {
                    if let Some(tex) = &self.floating_tex {
                        painter.image(
                            tex.id(),
                            r,
                            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                }
                marching_ants(&painter, r);
            } else if let Some((a, b)) = self.selection_rect {
                let r = Rect::from_two_pos(to_screen(a), to_screen(b));
                marching_ants(&painter, r);
            }

            // resize handles
            if let Some(fr) = self.floating_screen_rect(img_rect, scale) {
                if self.tool == Tool::Select {
                    draw_grips(&painter, fr, &Grip::ALL);
                }
            } else if self.selection_rect.is_none() {
                // canvas resize handles (any tool), drawn just outside the image
                for g in [Grip::E, Grip::S, Grip::Se] {
                    let c = canvas_grip_pos(img_rect, g);
                    let hr = Rect::from_center_size(c, Vec2::splat(10.0));
                    painter.rect(
                        hr,
                        2.0,
                        Color32::WHITE,
                        Stroke::new(1.0, Color32::from_gray(70)),
                    );
                }
            }

            // polygon preview
            if !self.poly_points.is_empty() {
                let mut pts: Vec<Pos2> = self.poly_points.iter().map(|p| to_screen(*p)).collect();
                if let Some(hp) = response.hover_pos() {
                    pts.push(hp);
                }
                painter.add(egui::Shape::line(
                    pts.clone(),
                    Stroke::new(1.0, self.fg),
                ));
                for p in &pts {
                    painter.circle_filled(*p, 3.0, Color32::from_rgb(0, 120, 255));
                }
            }

            // curve preview
            if let Some(c) = self.curve {
                let pf = |p: Pt| egui::pos2(p.0 as f32, p.1 as f32);
                let (p0, c1, c2, p1) = (pf(c.p0), pf(c.c1), pf(c.c2), pf(c.p1));
                let mut pts = Vec::new();
                for i in 0..=60 {
                    let t = i as f32 / 60.0;
                    let u = 1.0 - t;
                    let x = u * u * u * p0.x
                        + 3.0 * u * u * t * c1.x
                        + 3.0 * u * t * t * c2.x
                        + t * t * t * p1.x;
                    let y = u * u * u * p0.y
                        + 3.0 * u * u * t * c1.y
                        + 3.0 * u * t * t * c2.y
                        + t * t * t * p1.y;
                    pts.push(to_screen((x.round() as i32, y.round() as i32)));
                }
                painter.add(egui::Shape::line(pts, Stroke::new(1.0, self.fg)));
            }

            // text preview
            if let (Tool::Text, Some(pos)) = (self.tool, self.text_pos) {
                let top_left = to_screen(pos) - Vec2::new(0.5 * scale, 0.5 * scale);
                if !self.text_buf.is_empty() {
                    painter.text(
                        top_left,
                        egui::Align2::LEFT_TOP,
                        &self.text_buf,
                        FontId::proportional(self.text_size * scale),
                        self.fg,
                    );
                }
                // Anchor caret, so the (possibly empty) insertion point is visible
                // and obviously draggable.
                let h = self.text_size * scale;
                painter.line_segment(
                    [top_left, top_left + Vec2::new(0.0, h)],
                    Stroke::new(1.0, Color32::from_rgb(0, 120, 255)),
                );
            }

            // cursor tracking, brush-preview ring, and per-tool cursor
            let active_ptr = response.hover_pos().or_else(|| response.interact_pointer_pos());
            if let Some(p) = active_ptr {
                let pt = to_img(p);
                let inside = pt.0 >= 0 && pt.1 >= 0 && pt.0 < self.w() && pt.1 < self.h();
                self.cursor_pos = inside.then_some(pt);
                let mut icon = match self.tool {
                    Tool::Pencil | Tool::Brush | Tool::Eraser if inside => {
                        let d = if self.tool == Tool::Pencil {
                            1
                        } else {
                            self.brush_size
                        };
                        let r = (d as f32 / 2.0 * scale).max(2.0);
                        let center = to_screen(pt);
                        painter.circle_stroke(center, r, Stroke::new(1.0, Color32::WHITE));
                        painter.circle_stroke(
                            center,
                            r + 1.0,
                            Stroke::new(1.0, Color32::from_black_alpha(160)),
                        );
                        egui::CursorIcon::None
                    }
                    Tool::Select => {
                        if let Some(g) = self.resize_grip {
                            g.cursor()
                        } else if self.floating.is_some() {
                            if let Some(g) = self.hit_selection_grip(p, img_rect, scale) {
                                g.cursor()
                            } else if self.floating_grab.is_some() {
                                egui::CursorIcon::Grabbing
                            } else if self.point_in_floating(pt) {
                                egui::CursorIcon::Move
                            } else {
                                egui::CursorIcon::Crosshair
                            }
                        } else {
                            egui::CursorIcon::Crosshair
                        }
                    }
                    Tool::Text => {
                        if self.text_pos.is_some() {
                            egui::CursorIcon::Move
                        } else {
                            egui::CursorIcon::Text
                        }
                    }
                    _ => egui::CursorIcon::Crosshair,
                };
                // Canvas resize handles apply to any tool when nothing is floating.
                if let Some(g) = self.canvas_grip {
                    icon = g.cursor();
                } else if self.floating.is_none() {
                    if let Some(g) = self.hit_canvas_grip(p, img_rect) {
                        icon = g.cursor();
                    }
                }
                if self.mid_panning {
                    icon = egui::CursorIcon::Grabbing;
                } else if space_pan {
                    icon = if response.dragged() {
                        egui::CursorIcon::Grabbing
                    } else {
                        egui::CursorIcon::Grab
                    };
                } else if alt_pick {
                    icon = egui::CursorIcon::Crosshair;
                }
                ui.ctx().set_cursor_icon(icon);
            } else {
                self.cursor_pos = None;
            }
        });
        self.scroll_offset = out.state.offset;
    }

    fn status_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            match self.cursor_pos {
                Some((x, y)) => ui.monospace(format!("📍 {x}, {y}")),
                None => ui.monospace("📍 —, —"),
            };
            ui.separator();
            ui.label(format!("Canvas {} × {}", self.w(), self.h()));
            ui.separator();
            ui.label(format!("Zoom {}%", (self.zoom * 100.0).round() as i32));
            if matches!(self.tool, Tool::Pencil | Tool::Brush | Tool::Eraser) {
                ui.separator();
                let d = if self.tool == Tool::Pencil {
                    1
                } else {
                    self.brush_size
                };
                ui.label(format!("Brush {d} px"));
            }
            if let Some((w, h)) = self.selection_size() {
                ui.separator();
                ui.colored_label(
                    Color32::from_rgb(0, 110, 220),
                    format!("Selection {w} × {h}"),
                );
            }
        });
    }

    fn canvas_curve(&mut self, response: &egui::Response, ptr: Option<Pt>) {
        match self.curve {
            None => {
                if response.drag_started() {
                    if let Some(p) = ptr {
                        self.curve = Some(CurveState {
                            p0: p,
                            p1: p,
                            c1: p,
                            c2: p,
                            stage: 0,
                        });
                    }
                }
            }
            Some(mut c) => {
                if c.stage == 0 {
                    if response.dragged() {
                        if let Some(p) = ptr {
                            c.p1 = p;
                            c.c1 = c.p0;
                            c.c2 = c.p1;
                        }
                    } else if response.drag_stopped() {
                        c.stage = 1;
                    }
                } else {
                    // stage 1 or 2: place control points
                    if response.dragged() || response.drag_started() {
                        if let Some(p) = ptr {
                            if c.stage == 1 {
                                c.c1 = p;
                            } else {
                                c.c2 = p;
                            }
                        }
                    } else if response.drag_stopped() || response.clicked() {
                        if let Some(p) = ptr {
                            if c.stage == 1 {
                                c.c1 = p;
                            } else {
                                c.c2 = p;
                            }
                        }
                        if c.stage == 1 {
                            c.stage = 2;
                        } else {
                            // commit
                            self.push_undo();
                            let color = self.fg;
                            let size = self.brush_size.max(1);
                            self.draw_bezier(c.p0, c.c1, c.c2, c.p1, color, size);
                            self.curve = None;
                            self.dirty = true;
                            return;
                        }
                    }
                }
                self.curve = Some(c);
            }
        }
    }

    /// Resize via drag handles (Select tool). Returns true if it took the drag.
    fn handle_resize(&mut self, response: &egui::Response, rect: Rect, scale: f32) -> bool {
        let to_img = |p: Pos2| -> Pt {
            (
                ((p.x - rect.min.x) / scale).floor() as i32,
                ((p.y - rect.min.y) / scale).floor() as i32,
            )
        };
        if self.resize_grip.is_some() {
            if response.dragged() {
                if let Some(p) = response.interact_pointer_pos() {
                    self.update_selection_resize(to_img(p));
                }
            } else if response.drag_stopped() {
                self.resize_grip = None;
                self.resize_orig = None;
            }
            return true;
        }
        if self.canvas_grip.is_some() {
            if response.dragged() {
                if let Some(p) = response.interact_pointer_pos() {
                    self.update_canvas_resize(to_img(p));
                }
            } else if response.drag_stopped() {
                self.canvas_grip = None;
                self.canvas_orig = None;
            }
            return true;
        }
        if response.drag_started() {
            if let Some(p) = response.interact_pointer_pos() {
                if self.floating.is_some() {
                    if let Some(g) = self.hit_selection_grip(p, rect, scale) {
                        self.begin_selection_resize(g);
                        return true;
                    }
                } else if let Some(g) = self.hit_canvas_grip(p, rect) {
                    self.begin_canvas_resize(g, to_img(p));
                    return true;
                }
            }
        }
        false
    }

    fn floating_screen_rect(&self, rect: Rect, scale: f32) -> Option<Rect> {
        let f = self.floating.as_ref()?;
        let [w, h] = f.img.size;
        Some(Rect::from_min_size(
            Pos2::new(
                rect.min.x + f.pos.0 as f32 * scale,
                rect.min.y + f.pos.1 as f32 * scale,
            ),
            Vec2::new(w as f32 * scale, h as f32 * scale),
        ))
    }

    fn hit_selection_grip(&self, p: Pos2, rect: Rect, scale: f32) -> Option<Grip> {
        let r = self.floating_screen_rect(rect, scale)?;
        Grip::ALL.into_iter().find(|g| {
            let (fx, fy) = g.frac();
            let c = Pos2::new(r.min.x + r.width() * fx, r.min.y + r.height() * fy);
            (p.x - c.x).abs() <= GRIP_HIT && (p.y - c.y).abs() <= GRIP_HIT
        })
    }

    fn hit_canvas_grip(&self, p: Pos2, img_rect: Rect) -> Option<Grip> {
        [Grip::E, Grip::S, Grip::Se].into_iter().find(|g| {
            let c = canvas_grip_pos(img_rect, *g);
            (p.x - c.x).abs() <= GRIP_HIT && (p.y - c.y).abs() <= GRIP_HIT
        })
    }

    fn begin_selection_resize(&mut self, g: Grip) {
        // Lift the selection off the canvas first (same as starting a move).
        if !self.lifted {
            if let Some(f) = &self.floating {
                let (pos, [w, h]) = (f.pos, f.img.size);
                self.push_undo();
                self.sel_undo_pushed = true;
                self.fill_region(pos.0, pos.1, w, h, Color32::TRANSPARENT);
                self.lifted = true;
            }
        }
        if let Some(f) = &self.floating {
            let [w, h] = f.img.size;
            self.resize_rect0 = (f.pos.0, f.pos.1, f.pos.0 + w as i32, f.pos.1 + h as i32);
            self.resize_orig = Some(f.img.clone());
            self.resize_grip = Some(g);
            self.floating_dirty = true;
        }
    }

    fn update_selection_resize(&mut self, p: Pt) {
        let Some(g) = self.resize_grip else { return };
        let (mut x0, mut y0, mut x1, mut y1) = self.resize_rect0;
        let (fx, fy) = g.frac();
        if fx == 0.0 {
            x0 = p.0.min(x1 - MIN_DIM);
        } else if fx == 1.0 {
            x1 = p.0.max(x0 + MIN_DIM);
        }
        if fy == 0.0 {
            y0 = p.1.min(y1 - MIN_DIM);
        } else if fy == 1.0 {
            y1 = p.1.max(y0 + MIN_DIM);
        }
        let nw = (x1 - x0).max(MIN_DIM) as usize;
        let nh = (y1 - y0).max(MIN_DIM) as usize;
        if let Some(orig) = self.resize_orig.take() {
            let resampled = resample_image(&orig, nw, nh);
            if let Some(f) = &mut self.floating {
                f.img = resampled;
                f.pos = (x0, y0);
            }
            self.resize_orig = Some(orig);
            self.floating_dirty = true;
        }
    }

    fn begin_canvas_resize(&mut self, g: Grip, start: Pt) {
        self.push_undo();
        self.canvas_orig = Some(self.image.clone());
        self.canvas_grip = Some(g);
        self.resize_grab = start;
    }

    fn update_canvas_resize(&mut self, p: Pt) {
        let Some(g) = self.canvas_grip else { return };
        let (ow, oh) = {
            let o = self.canvas_orig.as_ref().unwrap();
            (o.size[0] as i32, o.size[1] as i32)
        };
        // Drag is relative to the grab point, so the edge tracks the cursor.
        let dx = p.0 - self.resize_grab.0;
        let dy = p.1 - self.resize_grab.1;
        let nw = if matches!(g, Grip::E | Grip::Se) {
            (ow + dx).max(MIN_DIM) as usize
        } else {
            ow as usize
        };
        let nh = if matches!(g, Grip::S | Grip::Se) {
            (oh + dy).max(MIN_DIM) as usize
        } else {
            oh as usize
        };
        let new = canvas_resized_from(self.canvas_orig.as_ref().unwrap(), nw, nh);
        self.image = new;
        self.dirty = true;
    }

    fn canvas_select(&mut self, response: &egui::Response, ptr: Option<Pt>) {
        if response.drag_started() {
            if let Some(p) = ptr {
                if self.point_in_floating(p) {
                    // begin move
                    let (pos, [w, h]) = {
                        let f = self.floating.as_ref().unwrap();
                        (f.pos, f.img.size)
                    };
                    self.floating_grab = Some((p.0 - pos.0, p.1 - pos.1));
                    if !self.lifted {
                        // lift: erase original area, content keeps floating
                        self.push_undo();
                        self.sel_undo_pushed = true;
                        self.fill_region(pos.0, pos.1, w, h, Color32::TRANSPARENT);
                        self.lifted = true;
                        self.floating_dirty = true;
                        self.dirty = true;
                    }
                } else {
                    self.commit_floating();
                    self.define_start = Some(p);
                    self.selection_rect = Some((p, p));
                }
            }
        } else if response.dragged() {
            if let Some(p) = ptr {
                if let Some(grab) = self.floating_grab {
                    if let Some(f) = &mut self.floating {
                        f.pos = (p.0 - grab.0, p.1 - grab.1);
                    }
                } else if let Some(start) = self.define_start {
                    self.selection_rect = Some((start, p));
                }
            }
        } else if response.drag_stopped() {
            if self.floating_grab.take().is_some() {
                // keep floating selection where it landed
            } else if let (Some(start), Some((_, end))) =
                (self.define_start.take(), self.selection_rect)
            {
                let x0 = start.0.min(end.0).max(0);
                let y0 = start.1.min(end.1).max(0);
                let x1 = start.0.max(end.0).min(self.w());
                let y1 = start.1.max(end.1).min(self.h());
                let (w, h) = ((x1 - x0).max(0) as usize, (y1 - y0).max(0) as usize);
                if w > 0 && h > 0 {
                    let img = self.capture(x0, y0, w, h);
                    self.floating = Some(Floating {
                        img,
                        pos: (x0, y0),
                    });
                    self.lifted = false;
                    self.sel_undo_pushed = false;
                    self.floating_dirty = true;
                }
                self.selection_rect = None;
            }
        } else if response.clicked() {
            if let Some(p) = ptr {
                if !self.point_in_floating(p) {
                    self.commit_floating();
                }
            }
        }
    }

    fn new_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.show_new_dialog;
        let mut create = false;
        egui::Window::new("New Image")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("new_dims").num_columns(2).show(ui, |ui| {
                    ui.label("Width");
                    ui.add(egui::DragValue::new(&mut self.new_w).range(1..=8000));
                    ui.end_row();
                    ui.label("Height");
                    ui.add(egui::DragValue::new(&mut self.new_h).range(1..=8000));
                    ui.end_row();
                });
                ui.checkbox(&mut self.new_transparent, "Transparent background");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Create").clicked() {
                        create = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_new_dialog = false;
                    }
                });
            });
        if self.dlg_confirm {
            create = true;
        }
        if create {
            self.create_canvas(self.new_w, self.new_h, self.new_transparent);
            self.show_new_dialog = false;
        } else {
            self.show_new_dialog = open && self.show_new_dialog && !self.dlg_cancel;
        }
    }

    fn resize_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_resize_dialog {
            return;
        }
        let mut open = true;
        let mut apply = false;
        egui::Window::new("Resize Image")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("resize_dims").num_columns(2).show(ui, |ui| {
                    ui.label("Width");
                    let wr =
                        ui.add(egui::DragValue::new(&mut self.resize_w).range(1..=10000).suffix(" px"));
                    if self.focus_resize {
                        wr.request_focus();
                        self.focus_resize = false;
                    }
                    ui.end_row();
                    ui.label("Height");
                    let hr =
                        ui.add(egui::DragValue::new(&mut self.resize_h).range(1..=10000).suffix(" px"));
                    ui.end_row();
                    // live aspect-ratio linking
                    if self.resize_keep_aspect {
                        if wr.changed() {
                            self.resize_h =
                                ((self.resize_w as f32 / self.resize_aspect).round() as usize).max(1);
                        } else if hr.changed() {
                            self.resize_w =
                                ((self.resize_h as f32 * self.resize_aspect).round() as usize).max(1);
                        }
                    }
                });
                ui.checkbox(&mut self.resize_keep_aspect, "Maintain aspect ratio");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_resize_dialog = false;
                    }
                });
            });
        if apply || self.dlg_confirm {
            self.resize_image(self.resize_w, self.resize_h);
            self.show_resize_dialog = false;
        } else if !open || self.dlg_cancel {
            self.show_resize_dialog = false;
        }
    }

    fn paste_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_paste_dialog {
            return;
        }
        let (pw, ph) = match &self.pending_paste {
            Some(img) => (img.size[0], img.size[1]),
            None => {
                self.show_paste_dialog = false;
                return;
            }
        };
        let (cw, ch) = (self.w(), self.h());
        let mut choice = 0u8; // 1 = enlarge, 2 = clip, 3 = cancel
        let mut open = true;
        egui::Window::new("Paste")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!(
                    "The pasted image ({pw} × {ph}) is larger than the canvas ({cw} × {ch})."
                ));
                ui.label("Enlarge the canvas to fit, or clip the pasted image?");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Enlarge canvas").clicked() {
                        choice = 1;
                    }
                    if ui.button("Clip to canvas").clicked() {
                        choice = 2;
                    }
                    if ui.button("Cancel").clicked() {
                        choice = 3;
                    }
                });
            });
        if !open || self.dlg_cancel {
            choice = 3;
        } else if self.dlg_confirm {
            choice = 1; // Enter confirms the default (enlarge to keep all pixels)
        }
        match choice {
            1 => {
                let img = self.pending_paste.take().unwrap();
                let [pw, ph] = img.size;
                self.enlarge_canvas(pw, ph);
                self.place_paste(img);
                self.show_paste_dialog = false;
            }
            2 => {
                let img = self.pending_paste.take().unwrap();
                self.place_paste(img);
                self.show_paste_dialog = false;
            }
            3 => {
                self.pending_paste = None;
                self.show_paste_dialog = false;
            }
            _ => {}
        }
    }

    fn paste_dest_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_paste_dest_dialog {
            return;
        }
        let (pw, ph) = match &self.pending_paste_img {
            Some(i) => (i.size[0], i.size[1]),
            None => {
                self.show_paste_dest_dialog = false;
                return;
            }
        };
        let mut choice = 0u8; // 1 = current, 2 = new tab, 3 = cancel
        let mut open = true;
        egui::Window::new("Paste")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!("Paste the {pw} × {ph} image where?"));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Into current").clicked() {
                        choice = 1;
                    }
                    if ui.button("New tab").clicked() {
                        choice = 2;
                    }
                    if ui.button("Cancel").clicked() {
                        choice = 3;
                    }
                });
            });
        if !open || self.dlg_cancel {
            choice = 3;
        } else if self.dlg_confirm {
            choice = 1; // Enter pastes into the current image (classic behavior)
        }
        match choice {
            1 => {
                let img = self.pending_paste_img.take().unwrap();
                self.show_paste_dest_dialog = false;
                self.paste_into_current(img);
            }
            2 => {
                let img = self.pending_paste_img.take().unwrap();
                self.show_paste_dest_dialog = false;
                self.new_tab_with(img, None, true);
            }
            3 => {
                self.pending_paste_img = None;
                self.show_paste_dest_dialog = false;
            }
            _ => {}
        }
    }

    fn quit_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_quit_dialog {
            return;
        }
        let (mut do_save, mut dont_save, mut cancel) = (false, false, false);
        egui::Window::new("Unsaved changes")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label("You have unsaved changes. Save all before quitting?");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save All").clicked() {
                        do_save = true;
                    }
                    if ui.button("Don't Save").clicked() {
                        dont_save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if self.dlg_cancel {
            cancel = true;
        }
        if self.dlg_confirm {
            do_save = true; // Enter confirms the default (Save All)
        }
        if do_save {
            self.save_all();
            // save_all clears `modified` on success; if anything is still
            // unsaved the user cancelled a Save As, so stay open.
            if !self.any_unsaved() {
                self.force_quit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            self.show_quit_dialog = false;
        } else if dont_save {
            self.force_quit = true;
            self.show_quit_dialog = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if cancel {
            self.show_quit_dialog = false;
        }
    }

    fn discard_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_discard_dialog {
            return;
        }
        let (mut do_save, mut dont_save, mut cancel) = (false, false, false);
        egui::Window::new("Unsaved changes")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label("You have unsaved changes. Save them first?");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        do_save = true;
                    }
                    if ui.button("Don't Save").clicked() {
                        dont_save = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if self.dlg_cancel {
            cancel = true;
        }
        if self.dlg_confirm {
            do_save = true;
        }
        if do_save {
            self.save();
            // proceed only if the save actually succeeded
            if !self.modified {
                self.show_discard_dialog = false;
                if let Some(action) = self.pending.take() {
                    self.perform_pending(action);
                }
            } else {
                self.show_discard_dialog = false;
                self.pending = None;
            }
        } else if dont_save {
            self.show_discard_dialog = false;
            if let Some(action) = self.pending.take() {
                self.perform_pending(action);
            }
        } else if cancel {
            self.show_discard_dialog = false;
            self.pending = None;
        }
    }

    fn text_window(&mut self, ctx: &egui::Context) {
        if self.tool != Tool::Text || self.text_pos.is_none() {
            return;
        }
        let mut place = false;
        let mut cancel = false;
        egui::Window::new("Text")
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                let edit = ui.add(
                    egui::TextEdit::multiline(&mut self.text_buf)
                        .desired_rows(3)
                        .desired_width(220.0)
                        .hint_text("Type text…"),
                );
                edit.request_focus();
                ui.add(egui::Slider::new(&mut self.text_size, 8.0..=200.0).text("size"));
                ui.horizontal(|ui| {
                    if ui.button("Place").clicked() {
                        place = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if place {
            self.commit_text(ctx);
        } else if cancel {
            self.text_pos = None;
            self.text_buf.clear();
        }
    }

    fn settings(&self) -> Settings {
        Settings {
            tool: self.tool,
            fg: self.fg.to_array(),
            bg: self.bg.to_array(),
            brush_size: self.brush_size,
            fill_shapes: self.fill_shapes,
            corner_radius: self.corner_radius,
            text_size: self.text_size,
            theme: self.theme,
            show_grid: self.show_grid,
            antialias: self.antialias,
            fill_tolerance: self.fill_tolerance,
            open_tabs: self.open_tabs_state(),
            active_tab: self.active_among_saved(),
        }
    }

    /// File-backed tabs (in tab order) with their per-tab zoom, for persistence.
    fn open_tabs_state(&self) -> Vec<TabPersist> {
        (0..self.docs.len())
            .filter_map(|i| {
                let (path, zoom) = if i == self.active {
                    (&self.file_path, self.zoom)
                } else {
                    (&self.docs[i].file_path, self.docs[i].zoom)
                };
                path.as_ref().map(|p| TabPersist {
                    path: p.to_string_lossy().into_owned(),
                    zoom,
                })
            })
            .collect()
    }

    /// Index of the active tab within the saved (file-backed) tab list.
    fn active_among_saved(&self) -> usize {
        (0..self.active)
            .filter(|&i| {
                if i == self.active {
                    self.file_path.is_some()
                } else {
                    self.docs[i].file_path.is_some()
                }
            })
            .count()
    }

    /// Reopen the tabs saved from a previous run (skips files that no longer
    /// load), restoring each tab's zoom.
    fn restore_tabs(&mut self, tabs: Vec<TabPersist>, active_tab: usize) {
        let mut loaded = false;
        for t in tabs {
            let path = PathBuf::from(&t.path);
            let Ok(img) = image::open(&path) else {
                continue;
            };
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width() as usize, rgba.height() as usize);
            let ci = ColorImage::from_rgba_unmultiplied([w, h], rgba.as_raw());
            let zoom = if t.zoom > 0.0 { t.zoom } else { 1.0 };
            if loaded {
                self.new_tab_with(ci, Some(path), false);
            } else {
                // Replace the initial blank tab with the first restored file.
                self.image = ci;
                self.file_path = Some(path);
                self.modified = false;
                self.undo_stack.clear();
                self.redo_stack.clear();
                self.dirty = true;
                loaded = true;
            }
            self.zoom = zoom; // applies to the now-active restored tab
        }
        if loaded {
            self.switch_to(active_tab.min(self.docs.len().saturating_sub(1)));
        }
    }

    fn apply_settings(&mut self, s: Settings) {
        self.tool = s.tool;
        self.last_tool = s.tool;
        self.fg = Color32::from_rgba_premultiplied(s.fg[0], s.fg[1], s.fg[2], s.fg[3]);
        self.bg = Color32::from_rgba_premultiplied(s.bg[0], s.bg[1], s.bg[2], s.bg[3]);
        self.brush_size = s.brush_size.clamp(1, 64);
        self.fill_shapes = s.fill_shapes;
        self.corner_radius = s.corner_radius.clamp(0, 200);
        self.text_size = s.text_size.clamp(8.0, 200.0);
        // zoom is per-tab (restored by restore_tabs); new tabs default to 100%.
        self.theme = s.theme;
        self.show_grid = s.show_grid;
        self.antialias = s.antialias;
        self.fill_tolerance = s.fill_tolerance.clamp(0, 255);
    }

    /// Record a recently-used (opaque) foreground color, most-recent first.
    fn push_recent(&mut self, c: Color32) {
        if c.a() == 0 {
            return;
        }
        if self.recent_colors.first() == Some(&c) {
            return;
        }
        self.recent_colors.retain(|x| *x != c);
        self.recent_colors.insert(0, c);
        self.recent_colors.truncate(12);
    }

    fn any_dialog_open(&self) -> bool {
        self.show_new_dialog
            || self.show_resize_dialog
            || self.show_paste_dialog
            || self.show_paste_dest_dialog
            || self.show_quit_dialog
            || self.show_discard_dialog
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // "Typing" = a focused widget other than the canvas (e.g. the color
        // picker's hex box). Single-key shortcuts are suppressed then so the
        // field keeps its keystrokes.
        let focused = ctx.memory(|m| m.focused());
        let typing = focused.is_some() && focused != self.canvas_id;
        let editing_text =
            typing || (self.tool == Tool::Text && self.text_pos.is_some());
        let dialog_open = self.any_dialog_open();
        // Cmd+C/X/V are delivered by egui-winit as Copy/Cut/Paste *events*, not
        // key presses, so we collect them here and act after the input lock.
        let (mut do_copy, mut do_cut, mut do_paste) = (false, false, false);
        let (mut dlg_cancel, mut dlg_confirm, mut want_quit) = (false, false, false);
        ctx.input_mut(|i| {
            use egui::{Event, Key, Modifiers};
            if i.consume_key(Modifiers::COMMAND, Key::Q) {
                want_quit = true;
            }
            if i.consume_key(Modifiers::COMMAND, Key::Z) {
                self.undo();
            }
            if i.consume_key(Modifiers::COMMAND, Key::Y)
                || i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z)
            {
                self.redo();
            }
            if i.consume_key(Modifiers::COMMAND, Key::S) {
                self.save();
            }
            if i.consume_key(Modifiers::COMMAND, Key::O) {
                self.request_open(None);
            }
            if i.consume_key(Modifiers::COMMAND, Key::N) {
                self.request_new();
            }
            if i.consume_key(Modifiers::COMMAND, Key::T) {
                self.new_tab();
            }
            if i.consume_key(Modifiers::COMMAND, Key::W) {
                self.request_close_tab(self.active);
            }
            if i.consume_key(Modifiers::COMMAND, Key::E) {
                self.open_resize_dialog();
            }
            if i.consume_key(Modifiers::COMMAND, Key::G) {
                self.show_grid = !self.show_grid;
            }
            if i.consume_key(Modifiers::COMMAND, Key::Plus)
                || i.consume_key(Modifiers::COMMAND, Key::Equals)
            {
                self.zoom_req = Some(self.zoom * 1.25);
                self.zoom_anchor = None;
            }
            if i.consume_key(Modifiers::COMMAND, Key::Minus) {
                self.zoom_req = Some(self.zoom / 1.25);
                self.zoom_anchor = None;
            }
            if i.consume_key(Modifiers::COMMAND, Key::Num0) {
                self.zoom_req = Some(1.0);
                self.zoom_anchor = None;
            }
            if !editing_text && i.consume_key(Modifiers::NONE, Key::X) {
                std::mem::swap(&mut self.fg, &mut self.bg);
            }
            // Single-key tool switching and brush sizing.
            if !editing_text && !dialog_open {
                for t in Tool::ALL {
                    if i.consume_key(Modifiers::NONE, t.hotkey().0) {
                        self.tool = t;
                    }
                }
                if i.consume_key(Modifiers::NONE, Key::OpenBracket) {
                    self.brush_size = (self.brush_size - 1).max(1);
                }
                if i.consume_key(Modifiers::NONE, Key::CloseBracket) {
                    self.brush_size = (self.brush_size + 1).min(64);
                }
            }
            if !editing_text && i.consume_key(Modifiers::COMMAND, Key::A) {
                self.select_all();
            }
            // Arrow keys nudge an active selection by 1px.
            if !editing_text && self.floating.is_some() {
                let mut d = (0, 0);
                if i.consume_key(Modifiers::NONE, Key::ArrowLeft) {
                    d.0 -= 1;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowRight) {
                    d.0 += 1;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    d.1 -= 1;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    d.1 += 1;
                }
                if d != (0, 0) {
                    self.nudge_selection(d);
                }
            }
            if !editing_text {
                for e in &i.events {
                    match e {
                        Event::Copy => do_copy = true,
                        Event::Cut => do_cut = true,
                        Event::Paste(_) => do_paste = true,
                        _ => {}
                    }
                }
            }
            if !editing_text && i.consume_key(Modifiers::NONE, Key::Delete) {
                if self.floating.is_some() {
                    self.delete_selection();
                }
            }
            if dialog_open {
                // Consume here — before the canvas is drawn — because a focused
                // click-widget would otherwise swallow Enter/Escape. Dialogs act
                // on these flags.
                dlg_cancel = i.consume_key(Modifiers::NONE, Key::Escape);
                dlg_confirm = i.consume_key(Modifiers::NONE, Key::Enter)
                    || i.consume_key(Modifiers::COMMAND, Key::Enter);
            } else {
                if i.consume_key(Modifiers::NONE, Key::Escape) {
                    self.poly_points.clear();
                    self.curve = None;
                    if self.tool == Tool::Text {
                        self.text_pos = None;
                        self.text_buf.clear();
                    }
                    self.commit_floating();
                }
                if !editing_text
                    && (i.consume_key(Modifiers::NONE, Key::Enter)
                        || i.consume_key(Modifiers::COMMAND, Key::Enter))
                {
                    if self.tool == Tool::Polygon {
                        self.commit_polygon();
                    }
                }
            }
        });
        self.dlg_cancel = dlg_cancel;
        self.dlg_confirm = dlg_confirm;
        if want_quit {
            // Route Cmd+Q through the unsaved-changes prompt.
            if self.any_unsaved() {
                self.show_quit_dialog = true;
            } else {
                self.force_quit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // copy/cut/paste write/read real image data on the OS clipboard.
        if do_cut {
            self.cut_selection();
        } else if do_copy {
            self.copy_selection();
        }
        if do_paste {
            self.paste_clipboard();
        }
    }
}

impl eframe::App for PaintApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings());
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.set_theme(self.theme.to_pref());
        self.push_recent(self.fg);

        // Open an image dropped onto the window (prompts if there are changes).
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .find_map(|f| f.path.clone())
        });
        if let Some(path) = dropped {
            self.request_open(Some(path));
        }

        // macOS app-menu / Cmd+Q termination -> route through our prompt.
        #[cfg(target_os = "macos")]
        {
            mac_quit::try_install(ctx);
            if mac_quit::QUIT_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst) {
                if self.any_unsaved() {
                    self.show_quit_dialog = true;
                } else {
                    self.force_quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        // Intercept window close when there are unsaved changes (any tab).
        if ctx.input(|i| i.viewport().close_requested()) && self.any_unsaved() && !self.force_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_quit_dialog = true;
        }

        self.handle_shortcuts(ctx);

        // Clipboard marker management (macOS/Windows): egui/winit only forwards
        // Cmd/Ctrl+V when the clipboard holds text, so we attach a tiny text
        // marker to image clipboards *while focused*. On focus loss we strip it,
        // so other apps (e.g. Teams) paste the image, not the marker text.
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
            if focused {
                let cc = clipboard_seq();
                let gained = !self.was_focused;
                if gained || cc != self.last_change_count {
                    self.ensure_clipboard_marker();
                    self.last_change_count = clipboard_seq();
                }
            } else if self.was_focused {
                self.strip_clipboard_marker();
            }
            self.was_focused = focused;
        }

        // Switching tools commits any in-progress work.
        if self.tool != self.last_tool {
            self.commit_polygon();
            self.curve = None;
            if self.last_tool == Tool::Text {
                self.commit_text(ctx);
            }
            if self.last_tool == Tool::Select {
                self.commit_floating();
            }
            self.resize_grip = None;
            self.resize_orig = None;
            self.canvas_grip = None;
            self.canvas_orig = None;
            self.last_tool = self.tool;
        }

        if self.dirty || self.texture.is_none() {
            self.texture =
                Some(ctx.load_texture("canvas", self.image.clone(), canvas_tex_options()));
            self.dirty = false;
        }
        if self.checker_tex.is_none() {
            // 2×2 checkerboard tile, repeated to show transparency.
            let (a, b) = (Color32::from_gray(205), Color32::from_gray(165));
            let mut tile = ColorImage::new([2, 2], a);
            tile.pixels = vec![a, b, b, a];
            self.checker_tex =
                Some(ctx.load_texture("checker", tile, TextureOptions::NEAREST_REPEAT));
        }
        if self.floating_dirty {
            self.floating_tex = self
                .floating
                .as_ref()
                .map(|f| ctx.load_texture("sel", f.img.clone(), canvas_tex_options()));
            self.floating_dirty = false;
        }

        egui::TopBottomPanel::top("menu").show(ctx, |ui| self.menu_bar(ui));
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| self.tab_bar(ui));
        egui::SidePanel::left("tools")
            .resizable(false)
            .exact_width(168.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.toolbox(ui));
            });
        egui::TopBottomPanel::bottom("statusbar").show(ctx, |ui| self.status_bar(ui));
        egui::TopBottomPanel::bottom("palette").show(ctx, |ui| self.palette(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.canvas(ui));

        self.new_dialog(ctx);
        self.resize_dialog(ctx);
        self.paste_dialog(ctx);
        self.paste_dest_dialog(ctx);
        self.quit_dialog(ctx);
        self.discard_dialog(ctx);
        self.text_window(ctx);

        // Reflect the file name and unsaved state in the window title.
        let name = self
            .file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".to_owned());
        let title = format!(
            "{}{} — rs-paint",
            if self.modified { "• " } else { "" },
            name
        );
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        // egui runs reactively: it only repaints on input or an explicit
        // request_repaint(). We have no time-based animation (marching ants and
        // the text caret are static; the rubber-band/cursor previews follow the
        // pointer, which already triggers repaints), so we request nothing here —
        // the app stays idle (≈0% CPU) when there's no interaction.
    }
}

/// Decode the embedded app icon for the window / taskbar / dock. Without this,
/// eframe applies its default egui icon at runtime (overriding the .app icon).
fn load_icon() -> egui::IconData {
    let png = include_bytes!("../assets/icon_1024.png");
    match image::load_from_memory(png) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        }
        Err(_) => egui::IconData {
            rgba: Vec::new(),
            width: 0,
            height: 0,
        },
    }
}

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 800.0])
            .with_min_inner_size([720.0, 540.0])
            .with_title("rs-paint")
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "rs-paint",
        native_options,
        Box::new(|cc| {
            let mut app = PaintApp::default();
            if let Some(storage) = cc.storage {
                if let Some(mut s) = eframe::get_value::<Settings>(storage, eframe::APP_KEY) {
                    let (tabs, active_tab) = (std::mem::take(&mut s.open_tabs), s.active_tab);
                    app.apply_settings(s);
                    app.restore_tabs(tabs, active_tab);
                }
            }
            Ok(Box::new(app))
        }),
    )
}
