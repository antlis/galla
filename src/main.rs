//! galla — a minimal image/video thumbnail gallery.
//!
//! Grid of thumbnails for the images and videos in the paths given on the
//! command line (defaults to the current directory). Images open in a built-in
//! viewer with zoom/pan; videos are handed to a configurable external player
//! (mpv by default). `y` copies the selected file's path to the clipboard.

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Instant, UNIX_EPOCH};

use eframe::egui;
use egui::{
    load::SizedTexture, Align, Align2, Color32, ColorImage, FontId, Rect, RichText, Stroke,
    TextureHandle, TextureOptions, Vec2,
};

const THUMB: usize = 192;

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "svg", "ico", "tiff", "tif", "heic", "heif",
    "avif", "jxl",
];
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "avi", "wmv", "flv", "m4v", "mpg", "mpeg", "ts", "m2ts", "3gp",
    "ogv",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Image,
    Video,
}

struct Entry {
    path: PathBuf,
    kind: Kind,
    tex: Option<TextureHandle>,
}

fn ext_of(p: &Path) -> Option<String> {
    p.extension().map(|e| e.to_string_lossy().to_lowercase())
}

fn kind_of(p: &Path) -> Option<Kind> {
    let e = ext_of(p)?;
    if IMAGE_EXTS.contains(&e.as_str()) {
        Some(Kind::Image)
    } else if VIDEO_EXTS.contains(&e.as_str()) {
        Some(Kind::Video)
    } else {
        None
    }
}

/// Expand the CLI paths into a flat, sorted list of media files.
/// Files are taken as-is; directories are scanned one level deep.
fn collect(paths: &[PathBuf]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut push = |p: PathBuf| {
        if let Some(kind) = kind_of(&p) {
            out.push(Entry {
                path: p,
                kind,
                tex: None,
            });
        }
    };
    for p in paths {
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
                files.sort();
                for f in files {
                    if f.is_file() {
                        push(f);
                    }
                }
            }
        } else if p.is_file() {
            push(p.clone());
        }
    }
    out
}

fn cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let mut h = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            h.push(".cache");
            h
        });
    base.join("galla")
}

fn mtime_secs(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Path to the cached still frame for a video, invalidated by its mtime.
fn video_thumb_path(dir: &Path, video: &Path) -> PathBuf {
    let mut h = DefaultHasher::new();
    video.to_string_lossy().hash(&mut h);
    mtime_secs(video).hash(&mut h);
    dir.join(format!("{:016x}.jpg", h.finish()))
}

fn to_color_image(img: &image::DynamicImage) -> ColorImage {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw())
}

fn placeholder(kind: Kind) -> ColorImage {
    let shade = match kind {
        Kind::Image => 45,
        Kind::Video => 30,
    };
    ColorImage::new([THUMB, THUMB], Color32::from_gray(shade))
}

/// Produce a thumbnail-sized `ColorImage`. Videos are first turned into a
/// still frame by ffmpegthumbnailer, cached on disk.
fn make_thumb(path: &Path, kind: Kind, cache: &Path) -> Option<ColorImage> {
    let source = match kind {
        Kind::Image => path.to_path_buf(),
        Kind::Video => {
            let out = video_thumb_path(cache, path);
            if !out.exists() {
                let _ = std::fs::create_dir_all(cache);
                let ok = Command::new("ffmpegthumbnailer")
                    .args(["-i", &path.to_string_lossy()])
                    .args(["-o", &out.to_string_lossy()])
                    .args(["-s", &THUMB.to_string()])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    // ffmpegthumbnailer may leave an empty file behind.
                    let _ = std::fs::remove_file(&out);
                    return None;
                }
            }
            out
        }
    };
    let img = image::open(&source).ok()?;
    let thumb = img.thumbnail(THUMB as u32, THUMB as u32).to_rgba8();
    Some(square_thumb(&thumb, kind))
}

/// Letterbox an aspect-preserved thumbnail onto a square THUMB×THUMB canvas so
/// grid cells stay uniform and images are never stretched.
fn square_thumb(thumb: &image::RgbaImage, kind: Kind) -> ColorImage {
    let shade = match kind {
        Kind::Image => 45,
        Kind::Video => 30,
    };
    let mut canvas = image::RgbaImage::from_pixel(
        THUMB as u32,
        THUMB as u32,
        image::Rgba([shade, shade, shade, 255]),
    );
    let (tw, th) = thumb.dimensions();
    let ox = ((THUMB as u32).saturating_sub(tw) / 2) as i64;
    let oy = ((THUMB as u32).saturating_sub(th) / 2) as i64;
    image::imageops::overlay(&mut canvas, thumb, ox, oy);
    ColorImage::from_rgba_unmultiplied([THUMB, THUMB], canvas.as_raw())
}

/// Resolve an external command, most-specific source first:
/// `--<name>` CLI flag > `$GALLA_<NAME>` > config file `<name> = ...` > default.
fn resolve_command(cli: Option<String>, name: &str, default: &str) -> Vec<String> {
    let raw = cli
        .or_else(|| std::env::var(format!("GALLA_{}", name.to_uppercase())).ok())
        .or_else(|| config_value(name))
        .unwrap_or_else(|| default.to_string());
    let parts: Vec<String> = raw.split_whitespace().map(|s| s.to_string()).collect();
    if parts.is_empty() {
        default.split_whitespace().map(|s| s.to_string()).collect()
    } else {
        parts
    }
}

/// Read `<key> = ...` from ~/.config/galla/config.toml (value may be quoted).
fn config_value(key: &str) -> Option<String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let mut h = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            h.push(".config");
            h
        });
    let cfg = base.join("galla").join("config.toml");
    let text = std::fs::read_to_string(cfg).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(val) = rest.strip_prefix('=') {
                let val = val.trim().trim_matches(|c| c == '"' || c == '\'');
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

fn spawn_player(player: &[String], path: &Path) {
    spawn_with(player, std::slice::from_ref(&path.to_path_buf()));
}

/// Spawn `cmd` with one or more file paths appended. Handing several video paths
/// to mpv opens them as a playlist; handing several files to dragon-drop shows
/// one draggable tile per file.
fn spawn_with(cmd: &[String], paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    if let Some((c, args)) = cmd.split_first() {
        let _ = Command::new(c).args(args).args(paths).spawn();
    }
}

/// Hand the file to an external drag-and-drop source (default `dragon-drop`) so
/// it can be dragged into other apps (Telegram, browsers, file managers…).
fn spawn_drag(drag: &[String], path: &Path) {
    spawn_with(drag, std::slice::from_ref(&path.to_path_buf()));
}

fn copy_path(path: &Path) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(path.to_string_lossy().to_string());
    }
}

/// Copy the decoded image itself to the clipboard as raw RGBA pixels.
fn copy_image(path: &Path) {
    let Ok(img) = image::open(path) else { return };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_image(arboard::ImageData {
            width: w as usize,
            height: h as usize,
            bytes: Cow::Owned(rgba.into_raw()),
        });
    }
}

/// Keybindings shown in the `?` overlay.
const HELP_KEYS: &[(&str, &str)] = &[
    ("←/→   h / l", "move selection · prev/next image"),
    ("↑/↓   k / j", "move selection (grid)"),
    ("Space", "mark / unmark the current tile"),
    ("Shift+click", "mark a range of tiles"),
    ("Enter", "open image · play video(s) (marked or hovered)"),
    ("y", "copy file path to clipboard"),
    ("Y", "copy image to clipboard"),
    ("d", "drag-and-drop file(s) into another app"),
    ("D", "move file(s) to trash — asks to confirm"),
    ("+ / - / scroll", "zoom (single view)"),
    ("mouse drag", "pan (single view)"),
    ("0", "reset zoom (single view)"),
    ("?", "toggle this help"),
    ("q / Esc", "clear selection · back · quit"),
];

enum Mode {
    Grid,
    Single {
        idx: usize,
        tex: Option<TextureHandle>,
        zoom: f32,
        offset: Vec2,
    },
}

struct GallaApp {
    entries: Vec<Entry>,
    image_indices: Vec<usize>,
    player: Vec<String>,
    drag: Vec<String>,
    selected: usize,
    marked: HashSet<usize>,
    cols: usize,
    scroll_to_selected: bool,
    started_single: bool,
    show_help: bool,
    confirm_delete: Option<Vec<usize>>,
    toast: Option<(String, Instant)>,
    mode: Mode,
    rx: Receiver<(PathBuf, ColorImage)>,
}

impl GallaApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        entries: Vec<Entry>,
        player: Vec<String>,
        drag: Vec<String>,
    ) -> Self {
        let image_indices = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == Kind::Image)
            .map(|(i, _)| i)
            .collect();

        // Generate thumbnails on a background thread so the UI stays responsive.
        let (tx, rx) = channel();
        let ctx = cc.egui_ctx.clone();
        // Keyed by path (not index) so a later deletion can't misassign a
        // thumbnail that is still being generated.
        let jobs: Vec<(PathBuf, Kind)> = entries
            .iter()
            .map(|e| (e.path.clone(), e.kind))
            .collect();
        std::thread::spawn(move || {
            let cache = cache_dir();
            for (path, kind) in jobs {
                let ci = make_thumb(&path, kind, &cache).unwrap_or_else(|| placeholder(kind));
                if tx.send((path, ci)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });

        // Opening exactly one image jumps straight to the viewer.
        let mode = if entries.len() == 1 && entries[0].kind == Kind::Image {
            Mode::Single {
                idx: 0,
                tex: None,
                zoom: 1.0,
                offset: Vec2::ZERO,
            }
        } else {
            Mode::Grid
        };

        Self {
            entries,
            image_indices,
            player,
            drag,
            selected: 0,
            marked: HashSet::new(),
            cols: 1,
            scroll_to_selected: false,
            started_single: matches!(mode, Mode::Single { .. }),
            show_help: false,
            confirm_delete: None,
            toast: None,
            mode,
            rx,
        }
    }

    /// Show a transient status message (auto-hides after a couple seconds).
    fn notify(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn open(&mut self, ctx: &egui::Context, i: usize) {
        match self.entries[i].kind {
            Kind::Image => {
                self.mode = Mode::Single {
                    idx: i,
                    tex: None,
                    zoom: 1.0,
                    offset: Vec2::ZERO,
                };
                ctx.request_repaint();
            }
            Kind::Video => spawn_player(&self.player, &self.entries[i].path),
        }
    }

    /// Move to another image while in single-view (delta is +1 / -1, wrapping).
    fn step_image(&mut self, cur: usize, delta: isize) {
        if self.image_indices.is_empty() {
            return;
        }
        let pos = self
            .image_indices
            .iter()
            .position(|&x| x == cur)
            .unwrap_or(0) as isize;
        let n = self.image_indices.len() as isize;
        let np = ((pos + delta) % n + n) % n;
        let idx = self.image_indices[np as usize];
        self.mode = Mode::Single {
            idx,
            tex: None,
            zoom: 1.0,
            offset: Vec2::ZERO,
        };
    }

    /// The entries an action applies to: the marked set if any, else the cursor.
    fn targets(&self) -> Vec<usize> {
        if self.marked.is_empty() {
            if self.entries.is_empty() {
                vec![]
            } else {
                vec![self.selected]
            }
        } else {
            let mut v: Vec<usize> = self
                .marked
                .iter()
                .copied()
                .filter(|&i| i < self.entries.len())
                .collect();
            v.sort_unstable();
            v
        }
    }

    fn rebuild_image_indices(&mut self) {
        self.image_indices = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == Kind::Image)
            .map(|(i, _)| i)
            .collect();
    }

    /// Move the files at `idxs` to the trash and drop them from the gallery.
    fn perform_delete(&mut self, mut idxs: Vec<usize>) {
        idxs.retain(|&i| i < self.entries.len());
        idxs.sort_unstable();
        idxs.dedup();
        if idxs.is_empty() {
            return;
        }

        let was_single = matches!(self.mode, Mode::Single { .. });
        // Only drop entries that actually made it to the trash; keep the rest.
        let mut removed: Vec<usize> = Vec::new();
        let mut fail = 0u32;
        for &i in &idxs {
            if trash::delete(&self.entries[i].path).is_ok() {
                removed.push(i);
            } else {
                fail += 1;
            }
        }
        let ok = removed.len() as u32;
        // Drop removed entries (descending so earlier indices stay valid).
        for &i in removed.iter().rev() {
            self.entries.remove(i);
        }
        self.rebuild_image_indices();
        self.marked.clear();
        self.selected = self
            .selected
            .saturating_sub(removed.iter().filter(|&&i| i < self.selected).count())
            .min(self.entries.len().saturating_sub(1));

        // A single-view delete drops back to the grid near the deleted spot.
        if was_single {
            self.mode = Mode::Grid;
        }
        if self.entries.is_empty() {
            self.notify("Trashed last file — empty");
        } else if fail == 0 {
            self.notify(format!("Trashed {ok} file(s)"));
        } else {
            self.notify(format!("Trashed {ok}, failed {fail}"));
        }
    }

    fn draw_confirm(&self, ctx: &egui::Context) {
        let Some(targets) = &self.confirm_delete else {
            return;
        };
        let n = targets.len();
        let names: Vec<String> = targets
            .iter()
            .filter_map(|&i| self.entries.get(i))
            .take(6)
            .map(|e| {
                e.path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect();
        egui::Area::new(egui::Id::new("galla_confirm"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_gray(20))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(200, 90, 90)))
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("Move {n} file(s) to trash?"))
                                .heading()
                                .color(Color32::from_rgb(230, 120, 120)),
                        );
                        ui.add_space(8.0);
                        for name in &names {
                            ui.label(RichText::new(name).monospace());
                        }
                        if n > names.len() {
                            ui.label(RichText::new(format!("… and {} more", n - names.len())).monospace());
                        }
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new("y / Enter = trash    n / Esc = cancel")
                                .monospace()
                                .color(Color32::from_gray(160)),
                        );
                    });
            });
    }
}

impl eframe::App for GallaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain freshly generated thumbnails and upload them as textures.
        while let Ok((path, ci)) = self.rx.try_recv() {
            if let Some(e) = self.entries.iter_mut().find(|e| e.path == path) {
                let name = path.to_string_lossy().into_owned();
                e.tex = Some(ctx.load_texture(name, ci, TextureOptions::LINEAR));
            }
        }

        // `?` toggles the help overlay. While it is open, Esc/q close it and are
        // consumed so the underlying view does not also act on them.
        if ctx.input(|i| i.key_pressed(egui::Key::Questionmark)) {
            self.show_help = !self.show_help;
        }
        if self.show_help {
            let closed = ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::Q)
            });
            if closed {
                self.show_help = false;
            }
        }

        // Delete-confirmation modal: swallow its keys so the view underneath
        // doesn't also react to Enter/Esc/q.
        if self.confirm_delete.is_some() {
            let confirm = ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Y)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
            });
            let cancel = ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::N)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::Q)
            });
            if confirm {
                if let Some(targets) = self.confirm_delete.take() {
                    self.perform_delete(targets);
                }
            } else if cancel {
                self.confirm_delete = None;
            }
        }

        match self.mode {
            Mode::Grid => self.grid_ui(ctx),
            Mode::Single { .. } => self.single_ui(ctx),
        }

        if self.show_help {
            self.draw_help(ctx);
        }
        if self.confirm_delete.is_some() {
            self.draw_confirm(ctx);
        }
        self.draw_toast(ctx);
    }
}

impl GallaApp {
    fn draw_toast(&mut self, ctx: &egui::Context) {
        const SHOW: f32 = 2.0;
        let Some((msg, at)) = &self.toast else { return };
        let elapsed = at.elapsed().as_secs_f32();
        if elapsed > SHOW {
            self.toast = None;
            return;
        }
        let msg = msg.clone();
        egui::Area::new(egui::Id::new("galla_toast"))
            .anchor(Align2::CENTER_BOTTOM, Vec2::new(0.0, -24.0))
            .order(egui::Order::Tooltip)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_gray(25))
                    .stroke(Stroke::new(1.0, Color32::from_gray(70)))
                    .inner_margin(egui::vec2(14.0, 8.0))
                    .show(ui, |ui| {
                        ui.label(RichText::new(msg).monospace().color(Color32::from_gray(230)));
                    });
            });
        ctx.request_repaint(); // keep animating until it expires
    }

    fn draw_help(&self, ctx: &egui::Context) {
        let accent = Color32::from_rgb(137, 180, 250);
        egui::Area::new(egui::Id::new("galla_help"))
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_gray(20))
                    .stroke(Stroke::new(1.0, Color32::from_gray(80)))
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        ui.label(RichText::new("galla — keys").heading().color(accent));
                        ui.add_space(8.0);
                        egui::Grid::new("galla_help_grid")
                            .spacing([20.0, 6.0])
                            .show(ui, |ui| {
                                for (k, d) in HELP_KEYS {
                                    ui.label(RichText::new(*k).monospace().color(accent));
                                    ui.label(RichText::new(*d).monospace());
                                    ui.end_row();
                                }
                            });
                    });
            });
    }

    fn grid_ui(&mut self, ctx: &egui::Context) {
        let n = self.entries.len();

        // Keyboard navigation (uses last frame's column count).
        let (mut sel, cols) = (self.selected, self.cols.max(1));
        ctx.input(|i| {
            if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::L) {
                sel += 1;
            }
            if i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::H) {
                sel = sel.saturating_sub(1);
            }
            if i.key_pressed(egui::Key::ArrowDown) || i.key_pressed(egui::Key::J) {
                sel += cols;
            }
            if (i.key_pressed(egui::Key::ArrowUp) || i.key_pressed(egui::Key::K)) && sel >= cols {
                sel -= cols;
            }
        });
        if n > 0 {
            sel = sel.min(n - 1);
        } else {
            sel = 0;
        }
        if sel != self.selected {
            self.selected = sel;
            self.scroll_to_selected = true;
        }

        let mut want_open = false;
        let mut want_quit = false;
        let mut want_copy_path = false;
        let mut want_copy_img = false;
        let mut want_drag = false;
        let mut want_delete = false;
        let mut want_mark = false;
        ctx.input(|i| {
            want_open = i.key_pressed(egui::Key::Enter);
            want_quit = i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Q);
            let y = i.key_pressed(egui::Key::Y);
            want_copy_path = y && !i.modifiers.shift;
            want_copy_img = y && i.modifiers.shift;
            let d = i.key_pressed(egui::Key::D);
            want_drag = d && !i.modifiers.shift;
            want_delete = d && i.modifiers.shift;
            want_mark = i.key_pressed(egui::Key::Space);
        });

        if want_quit {
            // Esc/q clears a selection first, only quitting when there is none.
            if self.marked.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            self.marked.clear();
        }
        if want_mark && n > 0 && !self.marked.remove(&self.selected) {
            self.marked.insert(self.selected);
        }
        if n > 0 {
            let path = self.entries[self.selected].path.clone();
            let is_image = self.entries[self.selected].kind == Kind::Image;
            if want_copy_path {
                copy_path(&path);
                self.notify("Copied path");
            }
            if want_copy_img && is_image {
                copy_image(&path);
                self.notify("Copied image");
            }
            if want_drag {
                let targets = self.targets();
                let paths: Vec<PathBuf> =
                    targets.iter().map(|&i| self.entries[i].path.clone()).collect();
                spawn_with(&self.drag, &paths);
                self.notify(format!("Dragging {} file(s)", paths.len()));
            }
            if want_delete {
                self.confirm_delete = Some(self.targets());
            }
        }

        let selected = self.selected;
        let scroll_to = self.scroll_to_selected;
        let entries = &self.entries;
        let marked = &self.marked;
        let mut clicked: Option<usize> = None;
        let mut click_shift = false;
        let mut new_cols = 1usize;

        egui::CentralPanel::default().show(ctx, |ui| {
            if entries.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No images or videos found.");
                });
                return;
            }
            let spacing = ui.spacing().item_spacing.x;
            let cell = THUMB as f32 + spacing;
            new_cols = ((ui.available_width() / cell).floor() as usize).max(1);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let size = Vec2::splat(THUMB as f32);
                    // Centre the grid so leftover width is split evenly, not all
                    // dumped on the right edge.
                    let row_w = new_cols as f32 * THUMB as f32
                        + new_cols.saturating_sub(1) as f32 * spacing;
                    let pad = ((ui.available_width() - row_w) / 2.0).max(0.0);
                    let mut i = 0;
                    while i < entries.len() {
                        ui.horizontal(|ui| {
                            ui.add_space(pad);
                            for _ in 0..new_cols {
                                if i >= entries.len() {
                                    break;
                                }
                                let e = &entries[i];
                                let resp = match &e.tex {
                                    Some(tex) => ui.add_sized(
                                        size,
                                        egui::ImageButton::new(
                                            egui::Image::new(SizedTexture::new(tex.id(), size))
                                                .fit_to_exact_size(size),
                                        ),
                                    ),
                                    None => ui.add_sized(size, egui::Button::new("…")),
                                };
                                if resp.clicked() {
                                    clicked = Some(i);
                                    click_shift = ui.input(|inp| inp.modifiers.shift);
                                }
                                if e.kind == Kind::Video {
                                    ui.painter().text(
                                        resp.rect.center_bottom() - Vec2::new(0.0, 16.0),
                                        Align2::CENTER_CENTER,
                                        "▶",
                                        FontId::proportional(22.0),
                                        Color32::WHITE,
                                    );
                                }
                                if marked.contains(&i) {
                                    ui.painter().rect_filled(
                                        resp.rect,
                                        4.0,
                                        Color32::from_rgba_unmultiplied(120, 200, 120, 60),
                                    );
                                    ui.painter().text(
                                        resp.rect.left_top() + Vec2::new(6.0, 4.0),
                                        Align2::LEFT_TOP,
                                        "✓",
                                        FontId::proportional(20.0),
                                        Color32::from_rgb(150, 230, 150),
                                    );
                                }
                                if i == selected {
                                    ui.painter().rect_stroke(
                                        resp.rect.expand(2.0),
                                        4.0,
                                        Stroke::new(3.0, Color32::from_rgb(137, 180, 250)),
                                    );
                                    if scroll_to {
                                        resp.scroll_to_me(Some(Align::Center));
                                    }
                                }
                                i += 1;
                            }
                        });
                    }
                });
        });

        self.cols = new_cols;
        self.scroll_to_selected = false;
        if let Some(i) = clicked {
            if click_shift {
                // Shift-click marks the range from the cursor to the clicked tile.
                let (a, b) = (self.selected.min(i), self.selected.max(i));
                for k in a..=b {
                    self.marked.insert(k);
                }
                self.selected = i;
            } else {
                self.selected = i;
                self.open(ctx, i);
            }
        } else if want_open && n > 0 {
            self.open_targets(ctx);
        }
    }

    /// Enter with a selection: play the marked videos in one player, otherwise
    /// open the first marked image in the viewer. With no selection, act on the
    /// cursor as before.
    fn open_targets(&mut self, ctx: &egui::Context) {
        if self.marked.is_empty() {
            self.open(ctx, self.selected);
            return;
        }
        let targets = self.targets();
        let videos: Vec<PathBuf> = targets
            .iter()
            .filter(|&&i| self.entries[i].kind == Kind::Video)
            .map(|&i| self.entries[i].path.clone())
            .collect();
        if !videos.is_empty() {
            spawn_with(&self.player, &videos);
            self.notify(format!("Playing {} video(s)", videos.len()));
        } else if let Some(&first) = targets.first() {
            self.open(ctx, first);
        }
    }

    fn single_ui(&mut self, ctx: &egui::Context) {
        // Pull out the current image's state (disjoint field borrows).
        let cur_idx = if let Mode::Single { idx, .. } = self.mode {
            idx
        } else {
            return;
        };

        // Handle navigation / actions first (may replace self.mode).
        let mut back = false;
        let mut copy_path_key = false;
        let mut copy_img_key = false;
        let mut drag_key = false;
        let mut delete_key = false;
        let mut reset = false;
        let mut step = 0isize;
        let mut zoom_key = 0.0f32;
        ctx.input(|i| {
            back = i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Q);
            let y = i.key_pressed(egui::Key::Y);
            copy_path_key = y && !i.modifiers.shift;
            copy_img_key = y && i.modifiers.shift;
            let d = i.key_pressed(egui::Key::D);
            drag_key = d && !i.modifiers.shift;
            delete_key = d && i.modifiers.shift;
            reset = i.key_pressed(egui::Key::Num0);
            if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::L) {
                step += 1;
            }
            if i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::H) {
                step -= 1;
            }
            if i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals) {
                zoom_key += 1.0;
            }
            if i.key_pressed(egui::Key::Minus) {
                zoom_key -= 1.0;
            }
        });

        if back {
            // If galla was launched as a single-image viewer there is no grid to
            // return to, so closing the image quits instead of showing a 1-tile
            // grid. Drilling in from the grid still returns to the grid.
            if self.started_single {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.mode = Mode::Grid;
                self.selected = cur_idx;
                self.scroll_to_selected = true;
            }
            return;
        }
        if copy_path_key {
            copy_path(&self.entries[cur_idx].path);
            self.notify("Copied path");
        }
        if copy_img_key {
            copy_image(&self.entries[cur_idx].path);
            self.notify("Copied image");
        }
        if drag_key {
            spawn_drag(&self.drag, &self.entries[cur_idx].path);
            self.notify("Drag started");
        }
        if delete_key {
            self.confirm_delete = Some(vec![cur_idx]);
        }
        if step != 0 {
            self.step_image(cur_idx, step);
            return;
        }

        // Lazily load the full-resolution texture for the current image.
        if let Mode::Single { idx, tex, .. } = &mut self.mode {
            if tex.is_none() {
                if let Ok(img) = image::open(&self.entries[*idx].path) {
                    let ci = to_color_image(&img);
                    *tex = Some(ctx.load_texture("full", ci, TextureOptions::LINEAR));
                }
            }
        }

        if let Mode::Single {
            tex, zoom, offset, ..
        } = &mut self.mode
        {
            if reset {
                *zoom = 1.0;
                *offset = Vec2::ZERO;
            }
            if zoom_key != 0.0 {
                *zoom = (*zoom * 1.25f32.powf(zoom_key)).clamp(0.05, 40.0);
            }

            let tex = tex.clone();
            let title = self.entries[cur_idx]
                .path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();

            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(Color32::from_gray(16)))
                .show(ctx, |ui| {
                    let (rect, resp) =
                        ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());

                    if resp.dragged() {
                        *offset += resp.drag_delta();
                    }
                    let scroll = ui.input(|i| i.raw_scroll_delta.y);
                    if resp.hovered() && scroll != 0.0 {
                        *zoom = (*zoom * 1.1f32.powf(scroll / 40.0)).clamp(0.05, 40.0);
                    }

                    match &tex {
                        Some(tex) => {
                            let isize = tex.size_vec2();
                            let fit =
                                (rect.width() / isize.x).min(rect.height() / isize.y).min(1.0);
                            let scale = fit * *zoom;
                            let draw = isize * scale;
                            let center = rect.center() + *offset;
                            let img_rect = Rect::from_center_size(center, draw);
                            ui.painter().image(
                                tex.id(),
                                img_rect,
                                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }
                        None => {
                            ui.painter().text(
                                rect.center(),
                                Align2::CENTER_CENTER,
                                "Cannot load image",
                                FontId::proportional(18.0),
                                Color32::LIGHT_GRAY,
                            );
                        }
                    }

                    // Filename caption, top-left.
                    ui.painter().text(
                        rect.left_top() + Vec2::new(10.0, 8.0),
                        Align2::LEFT_TOP,
                        title,
                        FontId::proportional(16.0),
                        Color32::from_gray(220),
                    );
                });
        }
    }
}

fn main() -> eframe::Result<()> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut cli_player: Option<String> = None;
    let mut cli_drag: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--player" | "-p" => cli_player = args.next(),
            "--drag" | "-d" => cli_drag = args.next(),
            "--help" | "-h" => {
                println!(
                    "galla — minimal image/video gallery\n\n\
                     Usage: galla [--player CMD] [--drag CMD] [PATH ...]\n\n\
                     PATH may be image/video files or directories (scanned one level).\n\
                     Defaults to the current directory.\n\n\
                     Keys: arrows/hjkl move · Space/Shift+click select · Enter open/play · y copy path · Y copy image\n\
                     d drag-out · D trash (confirm) · ? help · q/Esc clear-sel/back/quit\n\
                     Single view: scroll/+/- zoom · mouse-drag pan · 0 reset · ←/→ or h/l prev/next\n\n\
                     Player resolves: --player > $GALLA_PLAYER > ~/.config/galla/config.toml (player) > mpv\n\
                     Drag resolves:   --drag   > $GALLA_DRAG   > ~/.config/galla/config.toml (drag)   > dragon-drop --and-exit --all"
                );
                return Ok(());
            }
            _ => paths.push(PathBuf::from(a)),
        }
    }
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }

    let entries = collect(&paths);
    let player = resolve_command(cli_player, "player", "mpv");
    let drag = resolve_command(cli_drag, "drag", "dragon-drop --and-exit --all");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 750.0])
            .with_title("galla"),
        ..Default::default()
    };
    eframe::run_native(
        "galla",
        native_options,
        Box::new(move |cc| Ok(Box::new(GallaApp::new(cc, entries, player, drag)))),
    )
}
