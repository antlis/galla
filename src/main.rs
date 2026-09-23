//! galla — a minimal image/video thumbnail gallery.
//!
//! Grid of thumbnails for the images and videos in the paths given on the
//! command line (defaults to the current directory). Images open in a built-in
//! viewer with zoom/pan; videos are handed to a configurable external player
//! (mpv by default). `y` copies the selected file's path to the clipboard.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::time::UNIX_EPOCH;

use eframe::egui;
use egui::{
    load::SizedTexture, Align, Align2, Color32, ColorImage, FontId, Rect, Stroke, TextureHandle,
    TextureOptions, Vec2,
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
    Some(to_color_image(&img.thumbnail(THUMB as u32, THUMB as u32)))
}

/// Resolve the external player command, most-specific source first:
/// `--player` CLI flag > `$GALLA_PLAYER` > config file > "mpv".
fn resolve_player(cli: Option<String>) -> Vec<String> {
    let raw = cli
        .or_else(|| std::env::var("GALLA_PLAYER").ok())
        .or_else(config_player)
        .unwrap_or_else(|| "mpv".to_string());
    let parts: Vec<String> = raw.split_whitespace().map(|s| s.to_string()).collect();
    if parts.is_empty() {
        vec!["mpv".to_string()]
    } else {
        parts
    }
}

/// Read `player = ...` from ~/.config/galla/config.toml (value may be quoted).
fn config_player() -> Option<String> {
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
        if let Some(rest) = line.strip_prefix("player") {
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
    if let Some((cmd, args)) = player.split_first() {
        let _ = Command::new(cmd).args(args).arg(path).spawn();
    }
}

fn copy_path(path: &Path) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(path.to_string_lossy().to_string());
    }
}

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
    selected: usize,
    cols: usize,
    scroll_to_selected: bool,
    mode: Mode,
    rx: Receiver<(usize, ColorImage)>,
}

impl GallaApp {
    fn new(cc: &eframe::CreationContext<'_>, entries: Vec<Entry>, player: Vec<String>) -> Self {
        let image_indices = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == Kind::Image)
            .map(|(i, _)| i)
            .collect();

        // Generate thumbnails on a background thread so the UI stays responsive.
        let (tx, rx) = channel();
        let ctx = cc.egui_ctx.clone();
        let jobs: Vec<(usize, PathBuf, Kind)> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (i, e.path.clone(), e.kind))
            .collect();
        std::thread::spawn(move || {
            let cache = cache_dir();
            for (i, path, kind) in jobs {
                let ci = make_thumb(&path, kind, &cache).unwrap_or_else(|| placeholder(kind));
                if tx.send((i, ci)).is_err() {
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
            selected: 0,
            cols: 1,
            scroll_to_selected: false,
            mode,
            rx,
        }
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
}

impl eframe::App for GallaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain freshly generated thumbnails and upload them as textures.
        while let Ok((i, ci)) = self.rx.try_recv() {
            if let Some(e) = self.entries.get_mut(i) {
                e.tex = Some(ctx.load_texture(format!("thumb{i}"), ci, TextureOptions::LINEAR));
            }
        }

        match self.mode {
            Mode::Grid => self.grid_ui(ctx),
            Mode::Single { .. } => self.single_ui(ctx),
        }
    }
}

impl GallaApp {
    fn grid_ui(&mut self, ctx: &egui::Context) {
        let n = self.entries.len();

        // Keyboard navigation (uses last frame's column count).
        let (mut sel, cols) = (self.selected, self.cols.max(1));
        ctx.input(|i| {
            if i.key_pressed(egui::Key::ArrowRight) {
                sel += 1;
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
                sel = sel.saturating_sub(1);
            }
            if i.key_pressed(egui::Key::ArrowDown) {
                sel += cols;
            }
            if i.key_pressed(egui::Key::ArrowUp) && sel >= cols {
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
        let mut want_copy = false;
        ctx.input(|i| {
            want_open = i.key_pressed(egui::Key::Enter);
            want_quit = i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Q);
            want_copy = i.key_pressed(egui::Key::Y);
        });

        if want_quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if want_copy && n > 0 {
            copy_path(&self.entries[self.selected].path);
        }

        let selected = self.selected;
        let scroll_to = self.scroll_to_selected;
        let entries = &self.entries;
        let mut clicked: Option<usize> = None;
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
                    let mut i = 0;
                    while i < entries.len() {
                        ui.horizontal(|ui| {
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
            self.selected = i;
            self.open(ctx, i);
        } else if want_open && n > 0 {
            self.open(ctx, self.selected);
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
        let mut copy = false;
        let mut reset = false;
        let mut step = 0isize;
        let mut zoom_key = 0.0f32;
        ctx.input(|i| {
            back = i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Q);
            copy = i.key_pressed(egui::Key::Y);
            reset = i.key_pressed(egui::Key::Num0);
            if i.key_pressed(egui::Key::ArrowRight) {
                step += 1;
            }
            if i.key_pressed(egui::Key::ArrowLeft) {
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
            self.mode = Mode::Grid;
            self.selected = cur_idx;
            self.scroll_to_selected = true;
            return;
        }
        if copy {
            copy_path(&self.entries[cur_idx].path);
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
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--player" | "-p" => cli_player = args.next(),
            "--help" | "-h" => {
                println!(
                    "galla — minimal image/video gallery\n\n\
                     Usage: galla [--player CMD] [PATH ...]\n\n\
                     PATH may be image/video files or directories (scanned one level).\n\
                     Defaults to the current directory.\n\n\
                     Keys: arrows move · Enter open · y copy path · q/Esc back/quit\n\
                     Single view: scroll/+/- zoom · drag pan · 0 reset · ←/→ prev/next\n\n\
                     Player resolves: --player > $GALLA_PLAYER > ~/.config/galla/config.toml > mpv"
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
    let player = resolve_player(cli_player);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 750.0])
            .with_title("galla"),
        ..Default::default()
    };
    eframe::run_native(
        "galla",
        native_options,
        Box::new(move |cc| Ok(Box::new(GallaApp::new(cc, entries, player)))),
    )
}
