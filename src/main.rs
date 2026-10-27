mod cr3;
mod exposure;
mod pixels;
mod raw;

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, ColorImage, Key, Layout, Pos2, Rect, RichText, Sense, Stroke,
    TextureHandle, TextureOptions, UiBuilder, Vec2,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};

const SIDECAR: &str = ".slopshot.json";
/// Ratings file written before the rename; still read if the new one is absent.
const LEGACY_SIDECAR: &str = ".pickk.json";
/// Full-size previews kept decoded around the current photo (current, ±1, ±2).
const PREFETCH: [isize; 5] = [0, 1, -1, 2, -2];
const CARD_W: f32 = 220.0;
const IMG_H: f32 = 150.0;
const CARD_H: f32 = IMG_H + 78.0;
const GAP: f32 = 8.0;
const GREEN: Color32 = Color32::from_rgb(70, 190, 90);
const RED: Color32 = Color32::from_rgb(220, 70, 60);
const GOLD: Color32 = Color32::from_rgb(240, 190, 40);
/// Film strip under the large view: total height, sprocket band height (top and
/// bottom), photos shown each side of the current one.
const STRIP_H: f32 = 92.0;
const SPROCKET_H: f32 = 14.0;
const STRIP_SIDE: (usize, usize) = (3, 6);

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1300.0, 850.0])
            .with_title("slopshot"),
        ..Default::default()
    };
    eframe::run_native("slopshot", opts, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}

// ---------- data ----------

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
enum Flag {
    #[default]
    None,
    Picked,
    Rejected,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
struct Mark {
    rating: u8,
    flag: Flag,
}

/// Decoded pixels as they came out of the JPEG (unrotated, unexposed), kept so the
/// texture can be re-rendered when the display exposure changes.
struct Src {
    rgb: image::RgbImage,
    orientation: u16,
}

impl Src {
    fn render(&self, ev: f32) -> ColorImage {
        pixels::render(&self.rgb, self.orientation, &exposure::lut(ev))
    }
}

/// A texture plus its source pixels and the exposure it was rendered at.
struct Shown {
    tex: TextureHandle,
    src: Src,
    ev: f32,
}

impl Shown {
    fn new(ctx: &egui::Context, name: &str, src: Src, ev: f32) -> Self {
        Self::prepared(ctx, name, src.render(ev), src, ev)
    }
    /// For images already rendered off the UI thread.
    fn prepared(ctx: &egui::Context, name: &str, shown: ColorImage, src: Src, ev: f32) -> Self {
        let tex = ctx.load_texture(name, shown, TextureOptions::LINEAR);
        Self { tex, src, ev }
    }
    fn set_ev(&mut self, ev: f32) {
        if (self.ev - ev).abs() > 0.005 {
            self.tex.set(self.src.render(ev), TextureOptions::LINEAR);
            self.ev = ev;
        }
    }
}

enum Thumb {
    Pending,
    Ready(Shown),
    Failed,
}

struct Photo {
    path: PathBuf,
    name: String,
    date: String,
    mark: Mark,
    thumb: Thumb,
    auto_ev: f32,
    exif: raw::Exif,
    orientation: u16,
    sibling_jpg: bool,
}

impl Photo {
    /// Focus point centre and frame size as fractions of the displayed image.
    fn focus_uv(&self) -> Option<(Vec2, Vec2)> {
        let ((u, v), (fw, fh)) = self.exif.focus?.uv(self.orientation);
        Some((vec2(u, v), vec2(fw, fh)))
    }
    /// Full resolution comes from a JPEG (sibling or embedded) rather than a slow
    /// RAW develop, so it's cheap enough to load before the user zooms.
    fn cheap_hires(&self) -> bool {
        self.sibling_jpg || self.exif.full_jpeg
    }
    /// Full sensor resolution as displayed (after rotation), if known.
    fn native_size(&self) -> Option<Vec2> {
        let (w, h) = (self.exif.width as f32, self.exif.height as f32);
        let size = if (5..=8).contains(&self.orientation) { vec2(h, w) } else { vec2(w, h) };
        (w > 0.0 && h > 0.0).then_some(size)
    }
}

fn is_raw(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["arw", "awr", "cr3"].iter().any(|x| e.eq_ignore_ascii_case(x)))
}

// ---------- background decoding ----------
//
// Two pools: thumbnails on half the cores at low priority (keeps the machine
// responsive while a folder loads), and a small normal-priority pool that
// decodes full-size previews for the large view, current photo first, then
// its neighbours so stepping through is instant.

enum Job {
    Thumb(u64, usize, PathBuf),
    /// epoch, idx, path, display EV to pre-apply
    Full(u64, usize, PathBuf, f32),
    /// Full-resolution image for zooming: epoch, idx, path, orientation, manual EV,
    /// auto brightness on, the preview's auto EV
    HiRes(u64, usize, PathBuf, u16, f32, bool, f32),
}

impl Job {
    fn idx(&self) -> usize {
        match self {
            Job::Thumb(_, i, _) | Job::Full(_, i, _, _) | Job::HiRes(_, i, ..) => *i,
        }
    }
    fn epoch(&self) -> u64 {
        match self {
            Job::Thumb(e, ..) | Job::Full(e, ..) | Job::HiRes(e, ..) => *e,
        }
    }
}

enum Done {
    Thumb { epoch: u64, idx: usize, img: Option<image::RgbImage>, meta: Option<raw::RawMeta>, auto_ev: f32, sibling: bool },
    Full { epoch: u64, idx: usize, img: Option<(ColorImage, Src, f32)> },
    HiRes { epoch: u64, idx: usize, img: Option<HiResImg> },
}

struct HiResImg {
    shown: ColorImage,
    src: Src,
    ev: f32,
    auto_ev: f32,
    source: &'static str,
}

#[derive(Default)]
struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    cv: Condvar,
    epoch: AtomicU64,
}

impl Queue {
    fn push_back(&self, j: Job) {
        self.jobs.lock().unwrap().push_back(j);
        self.cv.notify_one();
    }
    fn push_front(&self, j: Job) {
        self.jobs.lock().unwrap().push_front(j);
        self.cv.notify_one();
    }
    /// Swap in a new job list, returning the jobs that hadn't started yet.
    fn replace(&self, jobs: impl IntoIterator<Item = Job>) -> Vec<Job> {
        let mut q = self.jobs.lock().unwrap();
        let old = q.drain(..).collect();
        q.extend(jobs);
        self.cv.notify_all();
        old
    }
    fn pop(&self) -> Job {
        let mut q = self.jobs.lock().unwrap();
        loop {
            if let Some(j) = q.pop_front() {
                return j;
            }
            q = self.cv.wait(q).unwrap();
        }
    }
}

/// Decoded preview (unrotated) plus the file's metadata. `shrink` = 4 averages 4x4
/// blocks (a quarter-resolution thumbnail).
fn decode(path: &Path, shrink: u32) -> (Option<image::RgbImage>, Option<raw::RawMeta>) {
    let Some(mut meta) = raw::read(path, raw::Pick::Preview) else { return (None, None) };
    let img = meta
        .preview
        .take()
        .and_then(|j| image::load_from_memory_with_format(&j, image::ImageFormat::Jpeg).ok())
        .map(|im| {
            let rgb = im.into_rgb8();
            if shrink > 1 { pixels::box_shrink(&rgb, shrink) } else { rgb }
        });
    (img, Some(meta))
}

/// Full resolution for zooming; the ~1.6k preview is too small. In order of preference
/// (fastest, and same look as the preview first): the camera JPG shot alongside,
/// a full-size JPEG embedded in the RAW (newer Sony bodies, every CR3), or developing the RAW.
fn decode_hires(path: &Path) -> Option<(image::RgbImage, &'static str)> {
    let jpg = sibling_jpg(path);
    let embedded = || {
        let j = raw::read(path, raw::Pick::FullSize)?.preview?;
        image::load_from_memory_with_format(&j, image::ImageFormat::Jpeg).ok()
    };
    jpg.and_then(|j| image::open(j).ok().map(|im| (im, "camera JPG")))
        .or_else(|| embedded().map(|im| (im, "embedded JPG")))
        .or_else(|| rawler::analyze::raw_to_srgb(path, &Default::default()).ok().map(|im| (im, "RAW")))
        .map(|(im, source)| (im.into_rgb8(), source))
}

/// A camera JPG saved alongside the RAW (RAW+JPEG shooting).
fn sibling_jpg(path: &Path) -> Option<PathBuf> {
    ["JPG", "jpg", "JPEG", "jpeg"].iter().map(|e| path.with_extension(e)).find(|p| p.is_file())
}

/// Display EV for a full-res image. A developed RAW has no camera tone curve and comes
/// out darker than the preview, so with auto off we still offset it to match the
/// preview's brightness (the difference of their auto EVs; ~0 for camera JPGs).
fn hires_ev(manual: f32, auto: bool, hires_auto: f32, preview_auto: f32) -> f32 {
    manual + hires_auto - if auto { 0.0 } else { preview_auto }
}

fn lower_thread_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        // On Linux nice values are per-thread; this only affects the calling worker.
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}

fn spawn_workers(ctx: &egui::Context, thumbs: &Arc<Queue>, fulls: &Arc<Queue>) -> mpsc::Receiver<Done> {
    let (tx, rx) = mpsc::channel();
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let pools = [(thumbs, (cores / 2).max(1), true), (fulls, 2, false)];
    for (queue, n, low_prio) in pools {
        for _ in 0..n {
            let (queue, tx, ctx) = (queue.clone(), tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                if low_prio {
                    lower_thread_priority();
                }
                loop {
                    let job = queue.pop();
                    let cur = queue.epoch.load(Ordering::Relaxed);
                    if job.epoch() != cur {
                        continue;
                    }
                    let done = match job {
                        Job::Thumb(epoch, idx, path) => {
                            let (img, meta) = decode(&path, 4);
                            let auto_ev = img.as_ref().map_or(0.0, exposure::auto_ev);
                            let sibling = sibling_jpg(&path).is_some();
                            Done::Thumb { epoch, idx, img, meta, auto_ev, sibling }
                        }
                        Job::HiRes(epoch, idx, path, orientation, manual_ev, auto, preview_auto) => {
                            let img = decode_hires(&path).map(|(rgb, source)| {
                                let auto_ev = exposure::auto_ev(&rgb);
                                let ev = hires_ev(manual_ev, auto, auto_ev, preview_auto);
                                let src = Src { rgb, orientation };
                                HiResImg { shown: src.render(ev), src, ev, auto_ev, source }
                            });
                            Done::HiRes { epoch, idx, img }
                        }
                        Job::Full(epoch, idx, path, ev) => {
                            let (rgb, meta) = decode(&path, 1);
                            let img = rgb.zip(meta).map(|(rgb, m)| {
                                let src = Src { rgb, orientation: m.orientation };
                                (src.render(ev), src, ev)
                            });
                            Done::Full { epoch, idx, img }
                        }
                    };
                    if tx.send(done).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            });
        }
    }
    rx
}

// ---------- export ----------

struct Export {
    total: usize,
    done: Arc<AtomicUsize>,
    result: mpsc::Receiver<String>,
}

fn run_export(files: Vec<PathBuf>, dest: PathBuf, done: Arc<AtomicUsize>, ctx: egui::Context) -> String {
    let (mut copied, mut skipped, mut errors) = (0, 0, Vec::new());
    for src in &files {
        let target = dest.join(src.file_name().unwrap());
        if target.exists() {
            skipped += 1;
        } else {
            match std::fs::copy(src, &target) {
                Ok(_) => copied += 1,
                Err(e) => errors.push(format!("{}: {e}", src.display())),
            }
        }
        done.fetch_add(1, Ordering::Relaxed);
        ctx.request_repaint();
    }
    let mut msg = format!("Exported {copied} file(s) to {}", dest.display());
    if skipped > 0 {
        msg += &format!(", skipped {skipped} already present");
    }
    if !errors.is_empty() {
        msg += &format!(", {} failed: {}", errors.len(), errors[0]);
    }
    msg
}

// ---------- app ----------

#[derive(Clone, Copy, PartialEq)]
enum FlagFilter {
    All,
    Picked,
    Rejected,
    Unflagged,
    NotRejected,
}

#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Name,
    Date,
    Rating,
}

#[derive(Clone, Copy)]
enum ZoomCmd {
    /// Multiply zoom, keeping the given screen point (or the view centre) fixed.
    By(f32, Option<Pos2>),
    Fit,
    OneToOne(Option<Pos2>),
    /// Fit if zoomed in, otherwise 100% at the given point.
    Toggle(Option<Pos2>),
    /// Back to exactly 100% around the current centre (after the real size is known).
    Resnap,
}

enum Action {
    Select(usize),
    /// Select from the film strip, scrolling the grid to it.
    Jump(usize),
    Open(usize),
    Rate(usize, u8),
    Flag(usize, Flag),
}

struct App {
    folder: Option<PathBuf>,
    photos: Vec<Photo>,
    epoch: u64,
    thumbs: Arc<Queue>,
    fulls: Arc<Queue>,
    done_rx: mpsc::Receiver<Done>,
    selected: Option<usize>,
    viewing: bool,
    /// Decoded full-size previews near the selection, by photo index.
    full: HashMap<usize, Shown>,
    /// Full decodes queued or in flight.
    full_requested: HashSet<usize>,
    prefetch_for: Vec<usize>,
    /// Last place the selection was seen in the visible list: (photo, position).
    /// Lets the film strip and the arrow keys carry on from there after the
    /// photo drops out of the filter (reject it while filtered to Unflagged).
    anchor: Option<(usize, usize)>,
    /// Full-resolution image for the photo in the large view: idx, texture, its auto EV, source.
    hires: Option<(usize, Shown, f32, &'static str)>,
    hires_requested: Option<usize>,
    hires_failed: HashSet<usize>,
    /// 1.0 = fit to window.
    zoom: f32,
    /// Point of the image (0..1 in each axis) at the centre of the view.
    center: Vec2,
    zoom_cmd: Option<ZoomCmd>,
    /// Zoom relative to full resolution, from the last frame (for the header).
    zoom_pct: f32,
    show_info: bool,
    show_focus: bool,
    /// Film strip of neighbouring photos under the large view.
    show_strip: bool,
    /// Photo in the large view and when it got there (full res loads after a short dwell).
    viewer_at: Option<(usize, std::time::Instant)>,
    export: Option<Export>,
    status: String,

    ev: f32,
    auto_bright: bool,

    f_name: String,
    f_min_rating: u8,
    f_flag: FlagFilter,
    f_from: String,
    f_to: String,
    sort: SortBy,
    sort_desc: bool,

    cols: usize,
    scroll_off: f32,
    view_h: f32,
    scroll_to_selected: bool,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let (thumbs, fulls) = (Arc::new(Queue::default()), Arc::new(Queue::default()));
        let done_rx = spawn_workers(ctx, &thumbs, &fulls);
        let mut app = Self {
            folder: None,
            photos: Vec::new(),
            epoch: 0,
            thumbs,
            fulls,
            done_rx,
            selected: None,
            viewing: false,
            full: HashMap::new(),
            full_requested: HashSet::new(),
            prefetch_for: Vec::new(),
            anchor: None,
            hires: None,
            hires_requested: None,
            hires_failed: HashSet::new(),
            zoom: 1.0,
            center: vec2(0.5, 0.5),
            zoom_cmd: None,
            zoom_pct: 0.0,
            show_info: true,
            show_focus: true,
            show_strip: true,
            viewer_at: None,
            export: None,
            status: String::new(),
            ev: 0.0,
            auto_bright: true,
            f_name: String::new(),
            f_min_rating: 0,
            f_flag: FlagFilter::All,
            f_from: String::new(),
            f_to: String::new(),
            sort: SortBy::Name,
            sort_desc: false,
            cols: 1,
            scroll_off: 0.0,
            view_h: 0.0,
            scroll_to_selected: false,
        };
        if let Some(dir) = std::env::args_os().nth(1) {
            app.open_folder(PathBuf::from(dir));
        }
        app
    }

    fn open_folder(&mut self, dir: PathBuf) {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                self.status = format!("Can't open {}: {e}", dir.display());
                return;
            }
        };
        let marks: HashMap<String, Mark> = std::fs::read_to_string(dir.join(SIDECAR))
            .or_else(|_| std::fs::read_to_string(dir.join(LEGACY_SIDECAR)))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        let mut photos: Vec<Photo> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_raw(p))
            .map(|path| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                let date = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                let mark = marks.get(&name).copied().unwrap_or_default();
                Photo {
                    path,
                    name,
                    date,
                    mark,
                    thumb: Thumb::Pending,
                    auto_ev: 0.0,
                    exif: raw::Exif::default(),
                    orientation: 1,
                    sibling_jpg: false,
                }
            })
            .collect();
        photos.sort_by(|a, b| a.name.cmp(&b.name));

        self.epoch += 1;
        for q in [&self.thumbs, &self.fulls] {
            q.epoch.store(self.epoch, Ordering::Relaxed);
            q.replace([]);
        }
        for (i, p) in photos.iter().enumerate() {
            self.thumbs.push_back(Job::Thumb(self.epoch, i, p.path.clone()));
        }

        self.status = format!("Loaded {} RAW file(s) from {}", photos.len(), dir.display());
        self.photos = photos;
        self.folder = Some(dir);
        self.selected = (!self.photos.is_empty()).then_some(0);
        self.viewing = false;
        self.full.clear();
        self.full_requested.clear();
        self.prefetch_for.clear();
        self.hires = None;
        self.hires_requested = None;
        self.hires_failed.clear();
        self.scroll_off = 0.0;
    }

    fn display_ev(&self, idx: usize) -> f32 {
        self.ev + if self.auto_bright { self.photos[idx].auto_ev } else { 0.0 }
    }

    /// Keep full-size previews decoded for the selection (and its neighbours in
    /// the large view), dropping ones that scrolled out of range.
    fn prefetch(&mut self, vis: &[usize]) {
        let Some(pos) = self.selected.and_then(|s| vis.iter().position(|&i| i == s)) else { return };
        let span = if self.viewing { &PREFETCH[..] } else { &PREFETCH[..2] };
        let want: Vec<usize> = span
            .iter()
            .filter_map(|d| vis.get(pos.checked_add_signed(*d)?).copied())
            .collect();
        if want == self.prefetch_for {
            return;
        }
        self.full.retain(|i, _| want.contains(i));
        // Drop queued-but-unstarted decodes; whatever's left in `full_requested` is in flight.
        for j in self.fulls.replace([]) {
            match j {
                Job::HiRes(..) => self.hires_requested = None,
                j => _ = self.full_requested.remove(&j.idx()),
            }
        }
        let jobs: Vec<Job> = want
            .iter()
            .filter(|&&i| !self.full.contains_key(&i) && !self.full_requested.contains(&i))
            .map(|&i| Job::Full(self.epoch, i, self.photos[i].path.clone(), self.display_ev(i)))
            .collect();
        self.full_requested.extend(jobs.iter().map(Job::idx));
        self.fulls.replace(jobs);
        self.prefetch_for = want;
    }

    fn save_marks(&mut self) {
        let Some(dir) = &self.folder else { return };
        let marks: HashMap<&str, Mark> = self
            .photos
            .iter()
            .filter(|p| p.mark != Mark::default())
            .map(|p| (p.name.as_str(), p.mark))
            .collect();
        let json = serde_json::to_string_pretty(&marks).unwrap();
        if let Err(e) = std::fs::write(dir.join(SIDECAR), json) {
            self.status = format!("Couldn't save ratings: {e}");
        }
    }

    fn visible(&self) -> Vec<usize> {
        let name = self.f_name.to_lowercase();
        let (from, to) = (self.f_from.trim(), self.f_to.trim());
        let mut v: Vec<usize> = (0..self.photos.len())
            .filter(|&i| {
                let p = &self.photos[i];
                (name.is_empty() || p.name.to_lowercase().contains(&name))
                    && p.mark.rating >= self.f_min_rating
                    && match self.f_flag {
                        FlagFilter::All => true,
                        FlagFilter::Picked => p.mark.flag == Flag::Picked,
                        FlagFilter::Rejected => p.mark.flag == Flag::Rejected,
                        FlagFilter::Unflagged => p.mark.flag == Flag::None,
                        FlagFilter::NotRejected => p.mark.flag != Flag::Rejected,
                    }
                    // ISO dates compare as strings; prefixes like "2026-03" work too.
                    && (from.is_empty() || p.date.as_str() >= from)
                    && (to.is_empty() || &p.date[..to.len().min(p.date.len())] <= to)
            })
            .collect();
        v.sort_by(|&a, &b| {
            let (pa, pb) = (&self.photos[a], &self.photos[b]);
            let o = match self.sort {
                SortBy::Name => pa.name.cmp(&pb.name),
                SortBy::Date => pa.date.cmp(&pb.date).then(pa.name.cmp(&pb.name)),
                SortBy::Rating => pa.mark.rating.cmp(&pb.mark.rating).then(pa.name.cmp(&pb.name)),
            };
            if self.sort_desc { o.reverse() } else { o }
        });
        v
    }

    fn apply(&mut self, a: Action) {
        match a {
            Action::Select(i) => self.selected = Some(i),
            Action::Jump(i) => {
                // From the film strip: the grid isn't on screen, so bring it along.
                self.selected = Some(i);
                self.scroll_to_selected = true;
            }
            Action::Open(i) => {
                self.selected = Some(i);
                self.viewing = true;
            }
            Action::Rate(i, r) => {
                self.photos[i].mark.rating = r;
                self.save_marks();
            }
            Action::Flag(i, f) => {
                self.photos[i].mark.flag = f;
                self.save_marks();
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context, vis: &[usize]) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let at = self.selected.and_then(|s| locate(vis, s, self.anchor));
        let mut actions = Vec::new();
        let mut step: isize = 0;
        ctx.input(|i| {
            let row = if self.viewing { 1 } else { self.cols as isize };
            if i.key_pressed(Key::ArrowRight) { step = 1 }
            if i.key_pressed(Key::ArrowLeft) { step = -1 }
            if i.key_pressed(Key::ArrowDown) { step = row }
            if i.key_pressed(Key::ArrowUp) { step = -row }
            if i.key_pressed(Key::Escape) { self.viewing = false }
            if i.key_pressed(Key::A) { self.auto_bright = !self.auto_bright }
            if i.key_pressed(Key::I) { self.show_info = !self.show_info }
            if i.key_pressed(Key::F) { self.show_focus = !self.show_focus }
            if self.viewing {
                if i.key_pressed(Key::S) { self.show_strip = !self.show_strip }
                if i.key_pressed(Key::Z) { self.zoom_cmd = Some(ZoomCmd::Toggle(None)) }
                if i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals) { self.zoom_cmd = Some(ZoomCmd::By(1.25, None)) }
                if i.key_pressed(Key::Minus) { self.zoom_cmd = Some(ZoomCmd::By(0.8, None)) }
            }
            if (i.key_pressed(Key::Enter) || i.key_pressed(Key::Space)) && self.selected.is_some() {
                self.viewing = !self.viewing;
            }
            let Some(sel) = self.selected else { return };
            let keys = [Key::Num0, Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5];
            for (r, k) in keys.into_iter().enumerate() {
                if i.key_pressed(k) {
                    actions.push(Action::Rate(sel, r as u8));
                }
            }
            // P/X toggle like the ✔/✖ buttons: pressing again clears the flag.
            let toggle = |f| if self.photos[sel].mark.flag == f { Flag::None } else { f };
            if i.key_pressed(Key::P) { actions.push(Action::Flag(sel, toggle(Flag::Picked))) }
            if i.key_pressed(Key::X) { actions.push(Action::Flag(sel, toggle(Flag::Rejected))) }
            if i.key_pressed(Key::U) { actions.push(Action::Flag(sel, Flag::None)) }
        });
        for a in actions {
            self.apply(a);
        }
        if step != 0 && !vis.is_empty() {
            self.selected = Some(vis[step_from(at, step, vis.len())]);
            self.scroll_to_selected = true;
        }
    }

    fn start_export(&mut self, ctx: &egui::Context) {
        let files: Vec<PathBuf> = self
            .photos
            .iter()
            .filter(|p| p.mark.flag == Flag::Picked)
            .map(|p| p.path.clone())
            .collect();
        if files.is_empty() {
            self.status = "Nothing is picked yet — press P on a photo to pick it.".into();
            return;
        }
        let Some(dest) = rfd::FileDialog::new().set_title("Export picked photos to…").pick_folder() else {
            return;
        };
        if self.folder.as_deref() == Some(dest.as_path()) {
            self.status = "Destination is the source folder — pick a different one.".into();
            return;
        }
        let (tx, rx) = mpsc::channel();
        let done = Arc::new(AtomicUsize::new(0));
        self.export = Some(Export { total: files.len(), done: done.clone(), result: rx });
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(run_export(files, dest, done, ctx));
        });
    }

    fn receive(&mut self, ctx: &egui::Context) {
        while let Ok(d) = self.done_rx.try_recv() {
            match d {
                Done::Thumb { epoch, idx, img, meta, auto_ev, sibling } if epoch == self.epoch => {
                    let p = &mut self.photos[idx];
                    p.auto_ev = auto_ev;
                    p.sibling_jpg = sibling;
                    if let Some(m) = meta {
                        if let Some(d) = m.date {
                            p.date = d;
                        }
                        p.exif = m.exif;
                        p.orientation = m.orientation;
                        // Stars given in camera, unless already rated/flagged in slopshot.
                        if let Some(r) = p.exif.rating.filter(|&r| r > 0 && p.mark == Mark::default()) {
                            p.mark.rating = r;
                        }
                    }
                    let ev = self.ev + if self.auto_bright { auto_ev } else { 0.0 };
                    p.thumb = match img {
                        Some(rgb) => Thumb::Ready(Shown::new(ctx, &p.name, Src { rgb, orientation: p.orientation }, ev)),
                        None => Thumb::Failed,
                    };
                }
                Done::HiRes { epoch, idx, img } if epoch == self.epoch => {
                    if self.hires_requested == Some(idx) {
                        self.hires_requested = None;
                    }
                    match img {
                        Some(h) if self.viewing && self.selected == Some(idx) => {
                            let mut s = Shown::prepared(ctx, "hires", h.shown, h.src, h.ev);
                            s.set_ev(hires_ev(self.ev, self.auto_bright, h.auto_ev, self.photos[idx].auto_ev));
                            self.hires = Some((idx, s, h.auto_ev, h.source));
                            // 100% was estimated from the sensor size; re-snap to the real image.
                            if (self.zoom_pct - 100.0).abs() < 3.0 {
                                self.zoom_cmd = Some(ZoomCmd::Resnap);
                            }
                        }
                        Some(_) => {}
                        None => {
                            self.hires_failed.insert(idx);
                            self.status = format!("Couldn't load full resolution for {}", self.photos[idx].name);
                        }
                    }
                }
                Done::Full { epoch, idx, img } if epoch == self.epoch => {
                    self.full_requested.remove(&idx);
                    if let Some((shown, src, ev)) = img.filter(|_| self.prefetch_for.contains(&idx)) {
                        let mut s = Shown::prepared(ctx, &format!("full{idx}"), shown, src, ev);
                        s.set_ev(self.display_ev(idx)); // in case EV changed meanwhile
                        self.full.insert(idx, s);
                    }
                }
                _ => {}
            }
        }
        if let Some(ex) = &self.export {
            if let Ok(msg) = ex.result.try_recv() {
                self.status = msg;
                self.export = None;
            }
        }
    }

    // ---------- UI pieces ----------

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("📂 Open folder…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().set_title("Open folder with RAW files").pick_folder() {
                        self.open_folder(dir);
                    }
                }
                ui.separator();
                ui.label("☀");
                ui.add(egui::Slider::new(&mut self.ev, -2.0..=3.0).step_by(0.1).suffix(" EV"))
                    .on_hover_text("Display brightness only — files are never changed");
                ui.checkbox(&mut self.auto_bright, "Auto")
                    .on_hover_text("Brighten/darken each photo to a normal exposure (A)");
                if ui.add_enabled(self.ev != 0.0, egui::Button::new("Reset").small()).clicked() {
                    self.ev = 0.0;
                }
                ui.separator();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let picked = self.photos.iter().filter(|p| p.mark.flag == Flag::Picked).count();
                    if let Some(ex) = &self.export {
                        let d = ex.done.load(Ordering::Relaxed);
                        ui.add(
                            egui::ProgressBar::new(d as f32 / ex.total as f32)
                                .desired_width(200.0)
                                .text(format!("Copying {d}/{}", ex.total)),
                        );
                    } else if ui
                        .add_enabled(picked > 0, egui::Button::new(format!("⮫ Export ({picked} picked)…")))
                        .clicked()
                    {
                        self.start_export(ctx);
                    }
                    // Path last, so it truncates into whatever space the buttons leave.
                    if let Some(f) = &self.folder {
                        ui.add(egui::Label::new(RichText::new(f.display().to_string()).weak()).truncate())
                            .on_hover_text(f.display().to_string());
                    }
                });
            });
            ui.add_space(4.0);
        });
    }

    fn bottom_bar(&self, ctx: &egui::Context, shown: usize) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let count = |f| self.photos.iter().filter(|p| p.mark.flag == f).count();
                ui.label(format!(
                    "{} photos · {shown} shown · {} picked · {} rejected",
                    self.photos.len(),
                    count(Flag::Picked),
                    count(Flag::Rejected)
                ));
                ui.separator();
                ui.label(RichText::new(&self.status).weak());
            });
        });
    }

    fn filter_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("filters").resizable(false).exact_width(200.0).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Filter");
            ui.add_space(4.0);
            ui.label("Name contains");
            ui.add(egui::TextEdit::singleline(&mut self.f_name).hint_text("e.g. 0067"));

            ui.add_space(6.0);
            ui.label("Minimum rating");
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for r in 0..=5u8 {
                    let txt = if r == 0 { "any".to_string() } else { format!("{r}★") };
                    if ui.selectable_label(self.f_min_rating == r, txt).clicked() {
                        self.f_min_rating = r;
                    }
                }
            });

            ui.add_space(6.0);
            ui.label("Flag");
            egui::ComboBox::from_id_salt("flag")
                .width(180.0)
                .selected_text(match self.f_flag {
                    FlagFilter::All => "All",
                    FlagFilter::Picked => "Picked",
                    FlagFilter::Rejected => "Rejected",
                    FlagFilter::Unflagged => "Unflagged",
                    FlagFilter::NotRejected => "Not rejected",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.f_flag, FlagFilter::All, "All");
                    ui.selectable_value(&mut self.f_flag, FlagFilter::Picked, "Picked");
                    ui.selectable_value(&mut self.f_flag, FlagFilter::Rejected, "Rejected");
                    ui.selectable_value(&mut self.f_flag, FlagFilter::Unflagged, "Unflagged");
                    ui.selectable_value(&mut self.f_flag, FlagFilter::NotRejected, "Not rejected");
                });

            ui.add_space(6.0);
            ui.label("Date from");
            ui.add(egui::TextEdit::singleline(&mut self.f_from).hint_text("YYYY-MM-DD"));
            ui.label("Date to");
            ui.add(egui::TextEdit::singleline(&mut self.f_to).hint_text("YYYY-MM-DD"));

            ui.add_space(6.0);
            if ui.button("Reset filters").clicked() {
                self.f_name.clear();
                self.f_min_rating = 0;
                self.f_flag = FlagFilter::All;
                self.f_from.clear();
                self.f_to.clear();
            }

            ui.separator();
            ui.heading("Sort");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.sort, SortBy::Name, "Name");
                ui.radio_value(&mut self.sort, SortBy::Date, "Date");
                ui.radio_value(&mut self.sort, SortBy::Rating, "Rating");
            });
            ui.checkbox(&mut self.sort_desc, "Descending");

            ui.separator();
            ui.label(
                RichText::new(
                    "Keys\n\
                     Arrow keys  move\n\
                     1–5  rate · 0 clear\n\
                     P pick · X reject · U unflag\n\
                     Enter/Space  view large\n\
                     Esc  back to grid\n\
                     A  toggle auto brightness\n\
                     I  toggle info panel\n\
                     Double-click  view large\n\
                     \n\
                     Large view\n\
                     Scroll or +/-  zoom\n\
                     Z  100% at focus point / fit\n\
                     Double-click  100% here / fit\n\
                     F  show focus point\n\
                     S  film strip\n\
                     Drag  pan",
                )
                .small()
                .weak(),
            );
        });
    }

    fn grid(&mut self, ui: &mut egui::Ui, vis: &[usize]) -> Vec<Action> {
        let mut actions = Vec::new();
        let cols = ((ui.available_width() + GAP) / (CARD_W + GAP)).floor().max(1.0) as usize;
        self.cols = cols;
        let rows = vis.len().div_ceil(cols);
        ui.spacing_mut().item_spacing = vec2(GAP, GAP);
        let row_h = CARD_H + GAP;

        let mut area = egui::ScrollArea::vertical().auto_shrink(false);
        if std::mem::take(&mut self.scroll_to_selected) {
            if let Some(pos) = self.selected.and_then(|s| vis.iter().position(|&i| i == s)) {
                let top = (pos / cols) as f32 * row_h;
                let mut off = self.scroll_off;
                if top < off {
                    off = top;
                } else if top + CARD_H > off + self.view_h {
                    off = top + CARD_H - self.view_h;
                }
                area = area.vertical_scroll_offset(off);
            }
        }
        let (selected, ev, auto, show_focus) = (self.selected, self.ev, self.auto_bright, self.show_focus);
        let photos = &mut self.photos;
        let out = area.show_rows(ui, CARD_H, rows, |ui, range| {
            for r in range {
                ui.horizontal(|ui| {
                    for &idx in vis.iter().skip(r * cols).take(cols) {
                        let p = &mut photos[idx];
                        if let Thumb::Ready(s) = &mut p.thumb {
                            s.set_ev(ev + if auto { p.auto_ev } else { 0.0 });
                        }
                        card(ui, p, idx, selected == Some(idx), show_focus, &mut actions);
                    }
                });
            }
        });
        self.scroll_off = out.state.offset.y;
        self.view_h = out.inner_rect.height();
        actions
    }

    fn viewer(&mut self, ui: &mut egui::Ui, idx: usize, vis: &[usize]) -> Vec<Action> {
        let mut actions = Vec::new();
        let ev = self.display_ev(idx);
        if let Thumb::Ready(s) = &mut self.photos[idx].thumb {
            s.set_ev(ev);
        }
        if let Some(s) = self.full.get_mut(&idx) {
            s.set_ev(ev);
        }
        if self.hires.as_ref().is_some_and(|h| h.0 != idx) {
            self.hires = None; // moved on; free the ~100 MB
        }
        if self.viewer_at.is_none_or(|(i, _)| i != idx) {
            self.viewer_at = Some((idx, std::time::Instant::now()));
            // Stepping through while zoomed: jump to where this shot is focused.
            if let (true, Some((c, _))) = (self.zoom > 1.0, self.photos[idx].focus_uv()) {
                self.center = c;
            }
        }
        let (manual, auto, preview_auto) = (self.ev, self.auto_bright, self.photos[idx].auto_ev);
        if let Some((_, s, auto_ev, _)) = &mut self.hires {
            s.set_ev(hires_ev(manual, auto, *auto_ev, preview_auto));
        }

        // Film strip: the current photo and its neighbours in the grid's order
        // (filters and sort apply), as many per side as fit, within STRIP_SIDE.
        let strip = match locate(vis, idx, self.anchor) {
            Some(at) if self.show_strip => strip_slots(vis, idx, at, ui.available_width()),
            _ => (Vec::new(), 0.0),
        };
        let gone = matches!(locate(vis, idx, self.anchor), Some(At::Gone(_)));
        for &i in strip.0.iter().flatten() {
            let ev = self.display_ev(i);
            if let Thumb::Ready(s) = &mut self.photos[i].thumb {
                s.set_ev(ev);
            }
        }

        let p = &self.photos[idx];
        let mut cmd = self.zoom_cmd.take();
        let mut back = false;
        ui.horizontal_wrapped(|ui| {
            back = ui.button("⬅ Grid (Esc)").clicked();
            ui.separator();
            ui.label(RichText::new(&p.name).strong());
            ui.label(RichText::new(&p.date).weak());
            ui.separator();
            ui.horizontal(|ui| mark_controls(ui, p, idx, 20.0, &mut actions));
            ui.separator();
            if ui.selectable_label(self.zoom <= 1.0, "Fit").clicked() {
                cmd = Some(ZoomCmd::Fit);
            }
            if ui.button("100%").clicked() {
                cmd = Some(ZoomCmd::OneToOne(None));
            }
            ui.label(format!("{:.0}%", self.zoom_pct));
            if p.exif.focus.is_some() {
                ui.toggle_value(&mut self.show_focus, "◎ Focus").on_hover_text("Show the camera's focus point (F)");
            }
            if self.hires_requested == Some(idx) {
                ui.spinner();
                ui.label(RichText::new("loading full resolution…").weak());
            } else if let Some((_, _, _, source)) = &self.hires {
                ui.label(RichText::new(format!("full res ({source})")).weak());
            }
            ui.toggle_value(&mut self.show_strip, "🎞 Strip").on_hover_text("Show neighbouring photos below (S)");
            ui.separator();
            ui.label(RichText::new(format!("display {ev:+.1} EV")).weak());
        });
        if back {
            self.viewing = false;
        }
        ui.add_space(4.0);

        let preview = self.full.get(&idx).map(|s| &s.tex).or(match &p.thumb {
            Thumb::Ready(s) => Some(&s.tex),
            _ => None,
        });
        let hires_tex = self.hires.as_ref().map(|h| &h.1.tex);
        let mut area = ui.available_size();
        if !strip.0.is_empty() {
            area.y = (area.y - STRIP_H - 4.0).max(0.0);
        }
        let (rect, resp) = ui.allocate_exact_size(area, Sense::click_and_drag());
        if !strip.0.is_empty() {
            ui.add_space(4.0);
            let (strip_rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), STRIP_H), Sense::hover());
            let strip = Strip { slots: strip.0, frame_w: strip.1, current: idx, gone };
            film_strip(ui, &self.photos, &strip, strip_rect, &mut actions);
        }
        let Some(layout_tex) = preview.or(hires_tex) else {
            ui.put(rect, egui::Spinner::new().size(32.0));
            return actions;
        };

        // Geometry: `base` is the image size (in points) when fitted to the view.
        let size = layout_tex.size_vec2();
        let base = size * (rect.width() / size.x).min(rect.height() / size.y);
        let native = match hires_tex {
            Some(t) => t.size_vec2(),
            None => p.native_size().unwrap_or(size),
        };
        let focus = p.focus_uv();
        let one_to_one = native.x / (base.x * ui.ctx().pixels_per_point());
        let max_zoom = (one_to_one * 4.0).max(4.0);

        let pointer = resp.hover_pos();
        if resp.hovered() {
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let f = (scroll / 300.0).exp() * pinch;
            if (f - 1.0).abs() > 1e-4 {
                cmd = Some(ZoomCmd::By(f, pointer));
            }
        }
        if resp.double_clicked() {
            cmd = Some(ZoomCmd::Toggle(pointer));
        }
        let target = match cmd {
            Some(ZoomCmd::By(f, at)) => Some((at, self.zoom * f)),
            Some(ZoomCmd::OneToOne(at)) => Some((at, one_to_one)),
            Some(ZoomCmd::Toggle(at)) if self.zoom <= 1.0 => Some((at, one_to_one)),
            Some(ZoomCmd::Fit | ZoomCmd::Toggle(_)) => Some((None, 1.0)),
            Some(ZoomCmd::Resnap) => Some((None, one_to_one)),
            None => None,
        };
        let zoom_to_focus = matches!(cmd, Some(ZoomCmd::OneToOne(None)))
            || matches!(cmd, Some(ZoomCmd::Toggle(None)) if self.zoom <= 1.0);
        if let Some((at, z)) = target {
            // Zoom about `at`, keeping the image point under it fixed on screen.
            let d = at.unwrap_or(rect.center()) - rect.center();
            let uv = self.center + d / (base * self.zoom);
            self.zoom = z.clamp(1.0, max_zoom);
            self.center = uv - d / (base * self.zoom);
            if let (true, Some((c, _))) = (zoom_to_focus, focus) {
                self.center = c;
            }
        }
        if resp.dragged() {
            self.center -= resp.drag_delta() / (base * self.zoom);
        }
        // Keep the image covering the view on each axis where it's bigger than the view.
        let disp = base * self.zoom;
        let half = rect.size() / disp / 2.0;
        self.center.x = if half.x >= 0.5 { 0.5 } else { self.center.x.clamp(half.x, 1.0 - half.x) };
        self.center.y = if half.y >= 0.5 { 0.5 } else { self.center.y.clamp(half.y, 1.0 - half.y) };
        self.zoom_pct = self.zoom / one_to_one * 100.0;

        let img_rect = Rect::from_center_size(rect.center() - (self.center - vec2(0.5, 0.5)) * disp, disp);
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        // Full res only once the preview would be upscaled: drawing a 26 MP texture
        // shrunk to fit (no mipmaps) aliases, and the preview is sharp enough there.
        let tex = match (hires_tex, preview) {
            (Some(h), Some(pv)) if disp.x * ui.ctx().pixels_per_point() > pv.size_vec2().x * 1.05 => h,
            (Some(h), None) => h,
            _ => layout_tex,
        };
        let painter = ui.painter().with_clip_rect(rect);
        painter.image(tex.id(), img_rect, uv, dim(p));
        if let (true, Some((c, frame))) = (self.show_focus, focus) {
            let size = (frame * disp).max(vec2(18.0, 18.0));
            let r = Rect::from_center_size(img_rect.min + c * disp, size);
            painter.rect_stroke(r, 2.0, Stroke::new(3.0, Color32::from_black_alpha(140)), egui::StrokeKind::Outside);
            painter.rect_stroke(r, 2.0, Stroke::new(1.5, GREEN), egui::StrokeKind::Outside);
        }

        if self.zoom > 1.0 {
            if resp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            } else if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
        // Full resolution: needed once zoomed in. When it's just a JPEG decode
        // (~0.1 s), start it as soon as the photo has been on screen briefly, so
        // zooming is instant; the dwell skips photos you're only flicking past.
        const DWELL: std::time::Duration = std::time::Duration::from_millis(300);
        let dwelt = self.viewer_at.map_or(std::time::Duration::ZERO, |(_, t)| t.elapsed());
        let want = self.zoom > 1.0 || (self.photos[idx].cheap_hires() && dwelt >= DWELL);
        if !want && self.photos[idx].cheap_hires() {
            ui.ctx().request_repaint_after(DWELL.saturating_sub(dwelt));
        }
        if want && self.hires.is_none() && self.hires_requested.is_none() && !self.hires_failed.contains(&idx) {
            let p = &self.photos[idx];
            self.hires_requested = Some(idx);
            self.fulls.push_front(Job::HiRes(self.epoch, idx, p.path.clone(), p.orientation, manual, auto, preview_auto));
        }
        actions
    }

    fn info_panel(&self, ctx: &egui::Context, idx: usize) {
        let p = &self.photos[idx];
        let e = &p.exif;
        egui::SidePanel::right("info").resizable(false).exact_width(240.0).show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Info");
            ui.add_space(4.0);
            let focal = e.focal_length().map(|f| match e.focal35 {
                Some(eq) if Some(eq as f32) != e.focal.map(f32::round) => format!("{f} ({eq}mm full-frame eq.)"),
                _ => f,
            });
            let bias = e.bias.map(|b| format!("{} EV", if b == 0.0 { "0".into() } else { format!("{b:+.1}") }));
            let res = p.native_size().map(|s| format!("{} × {} ({:.1} MP)", s.x, s.y, s.x * s.y / 1e6));
            let file_size = std::fs::metadata(&p.path).ok().map(|m| format!("{:.1} MB", m.len() as f64 / 1e6));
            let rows = [
                ("File", Some(p.name.clone())),
                ("Date", Some(p.date.clone())),
                ("Camera", e.camera()),
                ("Lens", e.lens.clone()),
                ("Focal length", focal),
                ("Aperture", e.aperture()),
                ("Shutter", e.shutter()),
                ("ISO", e.iso.map(|i| i.to_string())),
                ("Exposure comp.", bias),
                ("Focus point", e.focus.map(|f| match f.frame {
                    Some((w, h)) => format!("yes ({w}×{h} px frame)"),
                    None => "yes".into(),
                })),
                ("Camera rating", e.rating.filter(|&r| r > 0).map(|r| "★".repeat(r as usize))),
                ("Resolution", res),
                ("File size", file_size),
            ];
            egui::Grid::new("exif").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                for (k, v) in rows {
                    ui.label(RichText::new(k).weak());
                    ui.add(egui::Label::new(v.unwrap_or_else(|| "—".into())).wrap());
                    ui.end_row();
                }
            });
        });
    }
}

fn dim(p: &Photo) -> Color32 {
    if p.mark.flag == Flag::Rejected { Color32::from_gray(90) } else { Color32::WHITE }
}

fn paint_fit(ui: &egui::Ui, tex: &TextureHandle, rect: Rect, tint: Color32) -> Rect {
    let size = tex.size_vec2();
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    let fitted = Rect::from_center_size(rect.center(), size * scale);
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    ui.painter().image(tex.id(), fitted, uv, tint);
    fitted
}

fn mark_controls(ui: &mut egui::Ui, p: &Photo, idx: usize, size: f32, actions: &mut Vec<Action>) {
    ui.spacing_mut().item_spacing.x = 1.0;
    for s in 1..=5u8 {
        let on = s <= p.mark.rating;
        let star = RichText::new(if on { "★" } else { "☆" })
            .size(size)
            .color(if on { GOLD } else { Color32::GRAY });
        if ui.add(egui::Label::new(star).sense(Sense::click())).on_hover_text(format!("{s} star(s)")).clicked() {
            actions.push(Action::Rate(idx, if p.mark.rating == s { 0 } else { s }));
        }
    }
    ui.add_space(8.0);
    ui.spacing_mut().item_spacing.x = 4.0;
    let flag_btn = |ui: &mut egui::Ui, f: Flag, txt: &str, color: Color32, tip: &str| {
        let active = p.mark.flag == f;
        let text = RichText::new(txt).size(size * 0.8).color(if active { Color32::WHITE } else { color });
        let btn = egui::Button::new(text).fill(if active { color } else { Color32::TRANSPARENT });
        ui.add(btn).on_hover_text(tip).clicked().then(|| Action::Flag(idx, if active { Flag::None } else { f }))
    };
    actions.extend(flag_btn(ui, Flag::Picked, "✔", GREEN, "Pick (P)"));
    actions.extend(flag_btn(ui, Flag::Rejected, "✖", RED, "Reject (X)"));
}

/// Where a photo is in the visible list.
#[derive(Clone, Copy, PartialEq, Debug)]
enum At {
    /// At this position.
    In(usize),
    /// Filtered out; it was at this position (its old neighbours now meet there).
    Gone(usize),
}

fn locate(vis: &[usize], idx: usize, anchor: Option<(usize, usize)>) -> Option<At> {
    match vis.iter().position(|&i| i == idx) {
        Some(p) => Some(At::In(p)),
        None => anchor.filter(|a| a.0 == idx).map(|a| At::Gone(a.1.min(vis.len()))),
    }
}

/// Position in `vis` (of length `len` > 0) after an arrow-key `step`. A photo
/// that was filtered out has had its neighbours close the gap at its old spot
/// `p`, so the next photo is the one now at `p` and the previous one is at `p - 1`.
fn step_from(at: Option<At>, step: isize, len: usize) -> usize {
    let new = match at {
        Some(At::In(p)) => p as isize + step,
        Some(At::Gone(p)) => p as isize + if step > 0 { step - 1 } else { step },
        None => 0,
    };
    new.clamp(0, len as isize - 1) as usize
}

/// Slots for the film strip: the current photo in the middle and `n` neighbours
/// per side in `vis` order (`None` past either end, so the current photo stays
/// centred), plus the frame width. A photo that has just been filtered out keeps
/// its old slot between its old neighbours. `n` is as many as fit in `width`,
/// within STRIP_SIDE; at the minimum the frames shrink instead.
fn strip_slots(vis: &[usize], idx: usize, at: At, width: f32) -> (Vec<Option<usize>>, f32) {
    let frame_h = STRIP_H - 2.0 * SPROCKET_H - 8.0;
    let cell = frame_h * 1.5 + GAP;
    let fit = ((width / cell).floor() as usize).saturating_sub(1) / 2;
    let n = fit.clamp(STRIP_SIDE.0, STRIP_SIDE.1);
    let frame_w = ((width - GAP) / (2 * n + 1) as f32 - GAP).min(frame_h * 1.5);
    let (pos, gap) = match at {
        At::In(p) => (p, 0),
        At::Gone(p) => (p, 1), // nothing to skip at `p`: the next photo is already there
    };
    let slot = |d: isize| match d {
        0 => Some(idx),
        d if d < 0 => pos.checked_add_signed(d).and_then(|p| vis.get(p).copied()),
        d => vis.get(pos + d as usize - gap).copied(),
    };
    ((-(n as isize)..=n as isize).map(slot).collect(), frame_w)
}

/// A strip of film: dark base, sprocket holes along both edges, and a frame per
/// slot holding that photo's thumbnail (so it works for any format slopshot can
/// preview). Clicking a frame moves the large view to it.
/// What the film strip shows: slots from `strip_slots`, the frame width, the
/// photo in the large view, and whether it has just been filtered out.
struct Strip {
    slots: Vec<Option<usize>>,
    frame_w: f32,
    current: usize,
    gone: bool,
}

fn film_strip(ui: &egui::Ui, photos: &[Photo], strip: &Strip, rect: Rect, actions: &mut Vec<Action>) {
    let Strip { ref slots, frame_w, current, gone } = *strip;
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 4.0, Color32::from_gray(14));
    // Sprocket holes, spaced like 35mm film relative to the strip height.
    let hole = vec2(8.0, 6.0);
    let pitch = 16.0;
    let mut x = rect.left() + pitch / 2.0;
    while x < rect.right() {
        for y in [rect.top() + SPROCKET_H / 2.0, rect.bottom() - SPROCKET_H / 2.0] {
            painter.rect_filled(Rect::from_center_size(pos2(x, y), hole), 1.5, Color32::from_gray(52));
        }
        x += pitch;
    }

    let frame_h = rect.height() - 2.0 * SPROCKET_H - 8.0;
    let total = slots.len() as f32 * (frame_w + GAP) - GAP;
    let mut left = rect.center().x - total / 2.0;
    for (k, slot) in slots.iter().enumerate() {
        let frame = Rect::from_min_size(pos2(left, rect.top() + SPROCKET_H + 4.0), vec2(frame_w, frame_h));
        left += frame_w + GAP;
        painter.rect_filled(frame, 2.0, Color32::from_gray(26));
        let Some(i) = *slot else { continue };
        let p = &photos[i];
        let resp = ui.interact(frame, ui.id().with(("strip", k)), Sense::click()).on_hover_text(&p.name);
        let pic = match &p.thumb {
            // Greyed out right down when it has just dropped out of the filter.
            Thumb::Ready(s) => paint_fit(ui, &s.tex, frame.shrink(2.0), if i == current && gone { Color32::from_gray(60) } else { dim(p) }),
            Thumb::Pending => {
                painter.text(frame.center(), Align2::CENTER_CENTER, "…", egui::FontId::proportional(14.0), Color32::GRAY);
                frame
            }
            Thumb::Failed => {
                painter.text(frame.center(), Align2::CENTER_CENTER, "—", egui::FontId::proportional(14.0), Color32::GRAY);
                frame
            }
        };
        if p.mark.rating > 0 {
            let stars = "★".repeat(p.mark.rating as usize);
            let at = pic.left_bottom() + vec2(3.0, -2.0);
            for (d, c) in [(vec2(1.0, 1.0), Color32::from_black_alpha(200)), (Vec2::ZERO, GOLD)] {
                painter.text(at + d, Align2::LEFT_BOTTOM, &stars, egui::FontId::proportional(10.0), c);
            }
        }
        // Flag colour on the frame itself, so the photo being judged shows its
        // verdict too; the current one also gets the selection ring around it.
        let flag = match p.mark.flag {
            Flag::Picked => Some(Stroke::new(2.0_f32, GREEN)),
            Flag::Rejected => Some(Stroke::new(2.0_f32, RED)),
            Flag::None if resp.hovered() && i != current => Some(Stroke::new(1.0_f32, Color32::from_gray(160))),
            Flag::None => None,
        };
        if let Some(b) = flag {
            painter.rect_stroke(frame, 2.0, b, egui::StrokeKind::Inside);
        }
        if i == current {
            let ring = if gone { Color32::from_gray(110) } else { ui.visuals().selection.stroke.color };
            painter.rect_stroke(frame.expand(1.0), 3.0, Stroke::new(2.0_f32, ring), egui::StrokeKind::Outside);
        }
        if resp.clicked() && i != current {
            actions.push(Action::Jump(i));
        }
    }
}

fn card(ui: &mut egui::Ui, p: &Photo, idx: usize, selected: bool, show_focus: bool, actions: &mut Vec<Action>) {
    let (rect, _) = ui.allocate_exact_size(vec2(CARD_W, CARD_H), Sense::hover());
    let vis = ui.visuals();
    let border = match p.mark.flag {
        Flag::Picked => Stroke::new(2.0, GREEN),
        Flag::Rejected => Stroke::new(2.0, RED),
        Flag::None => Stroke::new(1.0, vis.widgets.noninteractive.bg_stroke.color),
    };
    let fill = if selected { vis.selection.bg_fill.gamma_multiply(0.45) } else { vis.faint_bg_color };
    ui.painter().rect(rect, 6.0, fill, border, egui::StrokeKind::Inside);
    if selected {
        ui.painter().rect_stroke(rect.expand(2.0), 8.0, Stroke::new(2.0, vis.selection.stroke.color), egui::StrokeKind::Outside);
    }

    let mut ui = ui.new_child(UiBuilder::new().max_rect(rect.shrink(6.0)).layout(Layout::top_down(Align::Min)));
    ui.spacing_mut().item_spacing.y = 2.0;
    let (img_rect, resp) = ui.allocate_exact_size(vec2(CARD_W - 12.0, IMG_H), Sense::click());
    match &p.thumb {
        Thumb::Ready(s) => {
            let fitted = paint_fit(&ui, &s.tex, img_rect, dim(p));
            if let (true, Some((c, _))) = (show_focus, p.focus_uv()) {
                let at = fitted.min + c * fitted.size();
                ui.painter().circle(at, 3.5, GREEN, Stroke::new(1.0, Color32::from_black_alpha(160)));
            }
        }
        Thumb::Pending => {
            ui.put(img_rect, egui::Spinner::new());
        }
        Thumb::Failed => {
            ui.put(img_rect, egui::Label::new(RichText::new("no preview").weak()));
        }
    }
    if resp.double_clicked() {
        actions.push(Action::Open(idx));
    } else if resp.clicked() {
        actions.push(Action::Select(idx));
    }

    ui.horizontal(|ui| {
        ui.add(egui::Label::new(RichText::new(&p.name).strong()).truncate());
    });
    ui.label(RichText::new(&p.date).small().weak());
    ui.label(RichText::new(p.exif.summary()).small().weak());
    ui.horizontal(|ui| mark_controls(ui, p, idx, 16.0, actions));
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive(ctx);
        let vis = self.visible();
        if let Some(sel) = self.selected
            && let Some(p) = vis.iter().position(|&i| i == sel)
        {
            self.anchor = Some((sel, p));
        }
        self.handle_keys(ctx, &vis);
        self.prefetch(&vis);
        if self.selected.is_none() {
            self.viewing = false;
        }
        if !self.viewing {
            self.zoom = 1.0;
            self.center = vec2(0.5, 0.5);
            self.hires = None;
            self.viewer_at = None;
        }

        self.top_bar(ctx);
        self.bottom_bar(ctx, vis.len());
        self.filter_panel(ctx);
        if let (true, Some(idx)) = (self.show_info && !self.photos.is_empty(), self.selected) {
            self.info_panel(ctx, idx);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.folder.is_none() {
                ui.centered_and_justified(|ui| {
                    if ui.button(RichText::new("📂 Open a folder of ARW/CR3 files…").size(20.0)).clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.open_folder(dir);
                        }
                    }
                });
                return;
            }
            if self.photos.is_empty() {
                ui.centered_and_justified(|ui| ui.label("No .ARW or .CR3 files in this folder."));
                return;
            }
            let actions = match (self.viewing, self.selected) {
                (true, Some(idx)) => self.viewer(ui, idx, &vis),
                _ => self.grid(ui, &vis),
            };
            for a in actions {
                self.apply(a);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_neighbour_count() {
        let vis: Vec<usize> = (0..40).collect();
        // Narrow: still 3 per side, frames shrink to fit.
        let (s, w) = strip_slots(&vis, 20, At::In(20), 300.0);
        assert_eq!(s.len(), 7);
        assert!(w * 7.0 + GAP * 8.0 <= 300.0 + 0.01);
        // Wide: capped at 6 per side.
        let (s, _) = strip_slots(&vis, 20, At::In(20), 4000.0);
        assert_eq!(s, (14..=26).map(Some).collect::<Vec<_>>());
        // Medium width picks in between.
        let (s, _) = strip_slots(&vis, 20, At::In(20), 844.0);
        assert_eq!(s.len(), 9);
    }

    #[test]
    fn strip_follows_visible_order_and_pads_ends() {
        let vis = vec![7, 3, 9, 1];
        let (s, _) = strip_slots(&vis, 7, At::In(0), 300.0);
        assert_eq!(s, vec![None, None, None, Some(7), Some(3), Some(9), Some(1)]);
    }

    /// The review's culling flow: filter to Unflagged, open the large view, press
    /// X. Photo 5 leaves `vis`; the strip keeps it in its old slot (between 4 and
    /// 6) instead of vanishing, and the arrows carry on from there.
    #[test]
    fn strip_and_arrows_survive_the_current_photo_being_filtered_out() {
        let before: Vec<usize> = (0..10).collect();
        let anchor = Some((5, before.iter().position(|&i| i == 5).unwrap()));
        let vis: Vec<usize> = before.into_iter().filter(|&i| i != 5).collect();
        let at = locate(&vis, 5, anchor);
        assert_eq!(at, Some(At::Gone(5)));
        let (s, _) = strip_slots(&vis, 5, at.unwrap(), 300.0);
        assert_eq!(s, [2, 3, 4, 5, 6, 7, 8].map(Some).to_vec());
        // Right goes to 6, left to 4, not back to the first photo.
        assert_eq!(vis[step_from(at, 1, vis.len())], 6);
        assert_eq!(vis[step_from(at, -1, vis.len())], 4);
        assert_eq!(vis[step_from(at, 3, vis.len())], 8); // a grid row down
        assert_eq!(step_from(Some(At::In(2)), 1, 9), 3);
        // A stale anchor for some other photo doesn't apply; at the end, it clamps.
        assert_eq!(locate(&vis, 5, Some((8, 2))), None);
        assert_eq!(locate(&[0, 1], 9, Some((9, 7))), Some(At::Gone(2)));
    }
}
