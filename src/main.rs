//! ResMon: a resource-monitor widget docked on the right screen edge (egui).
//!
//! One window morphs between three faces: tab (at rest) -> panel (hover) -> full view (click).
//! A sampler thread reads machine totals every 0.5 s and processes every 1.5 s (only while the
//! full view is open); a watcher thread polls the cursor for the tab <-> panel hover. The UI
//! repaints only when one of them asks, or while something is moving.
#![windows_subsystem = "windows"]

mod net;
mod raster;
mod shell;
mod settings;
mod space;
mod sys;
mod theme;
mod treemap;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Id, Layout, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, Vec2, ViewportCommand, pos2,
    vec2,
};
use sys::{Field, History, Proc, Totals};
use theme::{ac, th};
use treemap::{Hit, Key, Sticky, Tile};
use windows::Win32::Foundation::HWND;

const REFLOW: f64 = 8.0; // seconds between two treemap reflows
const TAB: Vec2 = vec2(34.0, 178.0); // docked tab (logical px): three bars with their icons
const PANEL: Vec2 = vec2(264.0, 790.0); // hover panel
const FULL: Vec2 = vec2(1440.0, 900.0); // full view (clamped to the work area)
const UI_ANIM: f64 = 0.28; // shell transitions
const FADE: f64 = 0.22; // full view content fade
const TILE_ANIM: f64 = 0.55; // treemap tiles glide
const SPLIT_GAP: f32 = 10.0; // split view (disk read | write, network down | up): space between halves
const SPLIT_CAPTION: f32 = 28.0; // and the caption band over each half

const DANGER: Color32 = Color32::from_rgb(0xF0, 0x62, 0x72);
// Segoe Fluent Icons glyphs
const I_CPU: &str = "\u{EEA1}";
const I_RAM: &str = "\u{EEA0}";
const I_COMMIT: &str = "\u{EEA3}";
const I_GPU: &str = "\u{E7F4}";
const I_VRAM: &str = "\u{E964}";
const I_DISK: &str = "\u{EDA2}";
const I_NET: &str = "\u{E774}";
const I_DRIVES: &str = "\u{E8B7}";
const I_READ: &str = "\u{E7B3}"; // eye
const I_WRITE: &str = "\u{E70F}"; // pencil
const I_DOWN: &str = "\u{E896}";
const I_UP: &str = "\u{E898}";
const PALETTE: [u32; 12] = [0x3D7FD9, 0x8C5BD6, 0xD6508F, 0xD99A2B, 0x2FA57A, 0x2AA6BF, 0xD9573D, 0x7DAA2F, 0x5B63D6, 0x2F9C94, 0xC2732E, 0xA24FC9];

// ------------------------------------------------------------------ formatting / colour

fn fmt_bytes(b: f64) -> String {
    let mut b = b.max(0.0);
    for u in ["B", "KB", "MB", "GB", "TB"] {
        if b < 1024.0 || u == "TB" {
            return if u == "B" || u == "KB" || b >= 100.0 { format!("{b:.0} {u}") } else { format!("{b:.1} {u}") };
        }
        b /= 1024.0;
    }
    unreachable!()
}

fn fmt_rate(b: f64) -> String {
    fmt_bytes(b) + "/s"
}

fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

fn crc32(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in b {
        c ^= x as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

fn app_color(name: &str) -> Color32 {
    rgb(PALETTE[crc32(name.to_lowercase().as_bytes()) as usize % PALETTE.len()]) // same colours as the Python version
}

fn heat_color(cpu: f64) -> Color32 {
    // 0..~25% of the machine -> cold slate to hot orange, in 8 steps so tiles do not flicker
    let q = (cpu * 8.0 / 25.0).round() * 25.0 / 8.0;
    mix(rgb(0x2A3445), rgb(0xFF6A3D), ((q / 25.0).min(1.0) as f32).powf(0.6))
}

const FIELD_NAMES: [(Field, &str); 9] = [
    (Field::Ws, "ws"),
    (Field::Private, "priv"),
    (Field::Cpu, "cpu"),
    (Field::GpuMem, "vram"),
    (Field::Gpu, "gpu"),
    (Field::IoRead, "disk"),
    (Field::IoWrite, "disk"),
    (Field::NetDown, "net"),
    (Field::NetUp, "net"),
];

fn field_name(f: Field) -> &'static str {
    FIELD_NAMES.iter().find(|e| e.0 == f).map_or("ws", |e| e.1)
}

fn field_of(name: &str) -> Field {
    FIELD_NAMES.iter().find(|e| e.1 == name).map_or(Field::Ws, |e| e.0)
}

fn ease(t: f64) -> f32 {
    let t = t.clamp(0.0, 1.0) as f32;
    1.0 - (1.0 - t).powi(3) // ease-out cubic
}

fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

fn r4(r: [f32; 4], origin: Pos2) -> Rect {
    Rect::from_min_size(origin + vec2(r[0], r[1]), vec2(r[2], r[3]))
}

fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semi".into()))
}

fn icons(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("icons".into()))
}

/// Text cut to one line with an ellipsis.
fn text(p: &egui::Painter, pos: Pos2, anchor: Align2, s: &str, f: FontId, c: Color32, max_w: f32) -> Rect {
    let mut job = egui::text::LayoutJob::simple_singleline(s.to_string(), f, c);
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(1.0));
    let g = p.layout_job(job);
    let rect = anchor.anchor_size(pos, g.size());
    p.galley(rect.min, g, c);
    rect
}

fn smooth(s: &VecDeque<f32>) -> Vec<f32> {
    let mut acc: Option<f32> = None;
    s.iter()
        .map(|&v| {
            let a = acc.map_or(v, |a| a + 0.45 * (v - a));
            acc = Some(a);
            a
        })
        .collect()
}

fn spark(p: &egui::Painter, r: Rect, s: &VecDeque<f32>, top: f32, c: Color32, fill: bool) {
    let n = s.len();
    if n < 3 {
        return;
    }
    let step = r.width() / (sys::HISTORY - 1) as f32; // right-aligned: a young history grows from the right edge
    let pts: Vec<Pos2> = smooth(s)
        .iter()
        .enumerate()
        .map(|(i, &v)| pos2(r.right() - (n - 1 - i) as f32 * step, r.bottom() - 2.0 - (v / top).clamp(0.0, 1.0) * (r.height() - 4.0)))
        .collect();
    if fill {
        let mut m = egui::Mesh::default();
        for (i, q) in pts.iter().enumerate() {
            let t = (r.bottom() - q.y) / r.height();
            m.colored_vertex(*q, alpha(c, (0x05 as f32 + t * (0x55 - 0x05) as f32) as u8));
            m.colored_vertex(pos2(q.x, r.bottom()), alpha(c, 0x05));
            if i > 0 {
                let k = (2 * i) as u32;
                m.add_triangle(k - 2, k - 1, k);
                m.add_triangle(k - 1, k, k + 1);
            }
        }
        p.add(Shape::mesh(m));
    }
    p.add(Shape::line(pts, Stroke::new(1.6, c)));
}

// ------------------------------------------------------------------ shared state (threads <-> UI)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Tab,
    Panel,
    Full,
}

struct Shared {
    tot: Option<Totals>,
    hist: History,
    tot_seq: u64,
    procs: Option<Arc<Vec<Proc>>>,
    procs_seq: u64,
    want_procs: bool,
    procs_now: bool,
    want_split: bool, // per-core view open: sample threads too
    // hover watcher
    mode: Mode,
    bounds: (i32, i32, i32, i32),
    busy: bool,
    hover_fire: bool,
    hover: f32, // seconds resting on the tab before the panel opens (a setting)
}

fn spawn_threads(sh: Arc<Mutex<Shared>>, ctx: egui::Context) {
    let (s1, c1) = (sh.clone(), ctx.clone());
    std::thread::spawn(move || {
        let mut s = sys::Sampler::new();
        let mut hist = History::default();
        let mut next_tot = Instant::now();
        let mut last_procs: Option<Instant> = None;
        loop {
            let now = Instant::now();
            if now >= next_tot {
                let t = s.sample_totals(&mut hist);
                let mut g = s1.lock().unwrap();
                (g.tot, g.hist, g.tot_seq) = (Some(t), hist.clone(), g.tot_seq + 1);
                drop(g);
                next_tot = now + Duration::from_millis(500);
                c1.request_repaint();
            }
            let (want, asap) = {
                let mut g = s1.lock().unwrap();
                s.split = g.want_split;
                (g.want_procs, std::mem::take(&mut g.procs_now))
            };
            s.set_net(want);
            if want && (asap || last_procs.is_none_or(|t| now - t >= Duration::from_millis(1500))) {
                let p = Arc::new(s.sample_procs());
                last_procs = Some(now);
                let mut g = s1.lock().unwrap();
                (g.procs, g.procs_seq) = (Some(p), g.procs_seq + 1);
                drop(g);
                c1.request_repaint();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    // Cursor polling with hysteresis: rest on the tab for the hover setting -> panel; leave the panel for 0.4 s -> tab.
    // Polling (not hover events) because a frameless window that resizes under the cursor misses exits.
    std::thread::spawn(move || {
        if std::env::var_os("RESMON_OPEN").is_some() {
            return; // a face opened for a check stays open
        }
        let mut since: Option<Instant> = None;
        loop {
            std::thread::sleep(Duration::from_millis(80));
            let (mode, (l, t, w, h), busy, hover_open) = {
                let g = sh.lock().unwrap();
                (g.mode, g.bounds, g.busy || g.hover_fire, g.hover)
            };
            if busy || mode == Mode::Full || sys::left_button_down() {
                since = None; // a held button is a drag in the making, not a hover
                continue;
            }
            let (x, y) = sys::cursor_pos();
            let m = if mode == Mode::Tab { 0 } else { 14 };
            let inside = l - m <= x && x <= l + w + m && t - m <= y && y <= t + h + m;
            if inside != (mode == Mode::Tab) {
                since = None;
                continue;
            }
            let s0 = *since.get_or_insert_with(Instant::now);
            let need = if mode == Mode::Tab { hover_open } else { 0.4 };
            if s0.elapsed().as_secs_f32() >= need {
                since = None;
                sh.lock().unwrap().hover_fire = true;
                ctx.request_repaint();
            }
        }
    });
}

// ------------------------------------------------------------------ the app

#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Name,
    Pid,
    Cpu,
    Ws,
    Private,
    GpuMem,
    Gpu,
    IoR,
    IoW,
    NetD,
    NetU,
    Threads,
    Create,
    Cmd,
}

const COLS: [(Col, &str, f32, bool); 14] = [
    (Col::Name, "Name", 190.0, false),
    (Col::Pid, "PID", 62.0, true),
    (Col::Cpu, "CPU", 62.0, true),
    (Col::Ws, "RAM", 76.0, true),
    (Col::Private, "Private", 76.0, true),
    (Col::GpuMem, "VRAM", 76.0, true),
    (Col::Gpu, "GPU", 52.0, true),
    (Col::IoR, "Disk read", 80.0, true),
    (Col::IoW, "Disk write", 80.0, true),
    (Col::NetD, "Download", 80.0, true),
    (Col::NetU, "Upload", 80.0, true),
    (Col::Threads, "Threads", 60.0, true),
    (Col::Create, "Started", 100.0, true),
    (Col::Cmd, "Command line", 0.0, false),
];

/// The disk space map: a background scan of one drive, viewed one folder at a time.
struct SpaceView {
    scan: space::Scan,
    at: u32,      // folder shown
    used: u64,    // the drive's used and total bytes, for "Not scanned yet" and "Free"
    total: u64,
}

struct Trans {
    to: Mode,
    t0: f64,
    from: Vec2, // size (points) of the face we come from
}

struct KillAsk {
    pids: Vec<u32>,
    what: String,
    me: bool,
    tree: bool,
}

struct App {
    sh: Arc<Mutex<Shared>>,
    hwnd: Option<HWND>,
    shown: bool,
    mode: Mode,
    trans: Option<Trans>,
    anchor: (i32, i32),
    bounds: (i32, i32, i32, i32),
    enforce_until: f64,
    drag: bool,
    dragged: bool,
    // data
    tot: Option<Totals>,
    hist: History,
    seq: (u64, u64),
    procs: Arc<Vec<Proc>>,
    by_pid: HashMap<u32, usize>,
    groups: HashMap<String, Vec<usize>>,
    // treemap
    metric: Field,
    heat: bool,
    relative: bool,
    per_core: bool,
    zoom: Option<String>,
    sel: Option<Hit>,
    hover: Option<Hit>,
    tiles: Vec<Tile>,
    anim: HashMap<Key, ([f32; 4], [f32; 4])>,
    anim_t0: f64,
    sticky: Sticky,
    sticky2: Sticky, // the right half of a split view (writes, upload)
    last_reflow: f64,
    tm_size: Vec2,
    // table
    sort: (Col, bool),
    query: String,
    rows: Vec<usize>,
    space: Option<SpaceView>, // Some: the treemap and table show disk space instead of processes
    cores: Vec<(Vec<usize>, u8)>, // physical cores: their threads (logical processors) and efficiency class
    space_seen: u64,          // folders count at the last space layout, to relayout as the scan grows
    // misc
    toast: Option<(String, bool, f64)>,
    kill_ask: Option<KillAsk>,
    startup: bool,
    users: HashMap<u32, String>,
    quit: bool,
    table_open: bool,    // the process list under the map
    settings_open: bool, // the settings sheet over the full view
    cfg: settings::Settings,
}

impl App {
    fn new(ctx: &egui::Context, hwnd: HWND) -> App {
        setup_style(ctx);
        let cfg = settings::Settings::load();
        theme::set(&cfg.theme, &cfg.accents);
        let anchor = cfg.anchor.unwrap_or_else(|| {
            let (_, t, r, b, _) = sys::monitor_at(0, 0); // (0, 0) is always on the primary monitor
            (r - 1, (t + b) / 2)
        });
        let sh = Arc::new(Mutex::new(Shared {
            tot: None,
            hist: History::default(),
            tot_seq: 0,
            procs: None,
            procs_seq: 0,
            want_procs: false,
            procs_now: false,
            want_split: false,
            mode: Mode::Tab,
            bounds: (0, 0, 0, 0),
            busy: true,
            hover_fire: false,
            hover: cfg.hover,
        }));
        spawn_threads(sh.clone(), ctx.clone());
        App {
            sh,
            hwnd: Some(hwnd),
            shown: false,
            mode: Mode::Tab,
            trans: None,
            anchor,
            bounds: (0, 0, 0, 0),
            enforce_until: 0.0,
            drag: false,
            dragged: false,
            tot: None,
            hist: History::default(),
            seq: (0, 0),
            procs: Arc::new(Vec::new()),
            by_pid: HashMap::new(),
            groups: HashMap::new(),
            metric: field_of(&cfg.metric),
            heat: cfg.heat,
            relative: cfg.relative,
            per_core: cfg.per_core,
            zoom: None,
            sel: None,
            hover: None,
            tiles: Vec::new(),
            anim: HashMap::new(),
            anim_t0: 0.0,
            sticky: Sticky::new(0.15),
            sticky2: Sticky::new(0.15),
            last_reflow: -1e9,
            tm_size: Vec2::ZERO,
            sort: (Col::Ws, true),
            query: String::new(),
            rows: Vec::new(),
            space: None,
            cores: sys::core_topology(),
            space_seen: 0,
            toast: None,
            kill_ask: None,
            startup: sys::startup_enabled(),
            users: HashMap::new(),
            quit: false,
            table_open: cfg.table_open,
            settings_open: false,
            cfg,
        }
    }

    // -------------------------------------------------------------- shell: tab <-> panel <-> full (one window)

    /// Bounds in physical px on the anchor's monitor, flush with its right edge; tab and panel are centred on
    /// the anchor's height, the full view on the monitor.
    fn geometry(&self, mode: Mode) -> (i32, i32, i32, i32) {
        let (l, t, r, b, s) = sys::monitor_at(self.anchor.0, self.anchor.1);
        let (l, t, r, b) = (l as f32, t as f32, r as f32, b as f32);
        let (w, h, y) = match mode {
            Mode::Full => {
                let (w, h) = ((FULL.x * s).min(r - l - 12.0 * s), (FULL.y * s).min(b - t - 24.0 * s));
                (w, h, (t + b - h) / 2.0)
            }
            _ => {
                let v = if mode == Mode::Tab { self.tab_size() } else { PANEL };
                let (w, h) = (v.x * s, (v.y * s).min(b - t));
                (w, h, (self.anchor.1 as f32 - h / 2.0).clamp(t, b - h))
            }
        };
        ((r - w).round() as i32, y.round() as i32, w.round() as i32, h.round() as i32)
    }

    fn set_bounds(&mut self, b: (i32, i32, i32, i32), now: f64) {
        self.bounds = b;
        self.enforce_until = now + 0.6; // a monitor with another scale makes winit resize the window: place it again
        if let Some(h) = self.hwnd {
            sys::place(h, b);
        }
    }

    fn face_size(&self, mode: Mode) -> Vec2 {
        match mode {
            Mode::Tab => self.tab_size(),
            Mode::Panel => PANEL,
            Mode::Full => FULL,
        }
    }

    fn go(&mut self, to: Mode, now: f64) {
        if self.trans.is_some() || self.drag || to == self.mode {
            return;
        }
        let from = self.face_size(self.mode);
        if to != Mode::Tab {
            let b = self.geometry(to);
            self.set_bounds(b, now);
            self.mode = to;
            if to == Mode::Full {
                let mut g = self.sh.lock().unwrap();
                (g.want_procs, g.procs_now) = (true, true);
            }
        }
        self.trans = Some(Trans { to, t0: now, from });
        self.hover = None;
    }

    fn finish_trans(&mut self, ctx: &egui::Context, now: f64) {
        let Some(tr) = &self.trans else { return };
        let total = if tr.to == Mode::Tab && self.mode == Mode::Full { FADE + UI_ANIM } else if tr.to == Mode::Full { UI_ANIM + FADE } else { UI_ANIM };
        if now - tr.t0 < total {
            ctx.request_repaint();
            return;
        }
        if tr.to == Mode::Tab {
            self.mode = Mode::Tab;
            let b = self.geometry(Mode::Tab);
            self.set_bounds(b, now);
            self.sh.lock().unwrap().want_procs = false;
            self.space = None; // the folder tree can be tens of MB: docked, give it back
        }
        self.trans = None;
    }

    /// Pull the newest samples from the sampler thread.
    fn pull(&mut self, now: f64) {
        let (tot, procs, fire) = {
            let mut g = self.sh.lock().unwrap();
            let tot = (g.tot_seq != self.seq.0).then(|| (g.tot.clone(), g.hist.clone()));
            let procs = (g.procs_seq != self.seq.1).then(|| g.procs.clone()).flatten();
            self.seq = (g.tot_seq, g.procs_seq);
            (tot, procs, std::mem::take(&mut g.hover_fire))
        };
        if let Some((t, h)) = tot {
            (self.tot, self.hist) = (t, h);
        }
        if let Some(p) = procs
            && self.mode == Mode::Full
        {
            self.set_procs(p, now);
        }
        if fire {
            self.go(if self.mode == Mode::Tab { Mode::Panel } else { Mode::Tab }, now);
        }
    }

    fn push_shared(&self) {
        let mut g = self.sh.lock().unwrap();
        g.mode = self.mode;
        g.bounds = self.bounds;
        g.busy = self.trans.is_some() || self.drag || !self.shown;
        g.hover = self.cfg.hover;
        g.want_split = self.mode == Mode::Full && self.per_core && self.metric == Field::Cpu && self.space.is_none();
    }

    // -------------------------------------------------------------- processes / treemap model

    fn set_procs(&mut self, procs: Arc<Vec<Proc>>, now: f64) {
        self.procs = procs;
        self.by_pid = self.procs.iter().enumerate().map(|(i, q)| (q.pid, i)).collect();
        self.groups.clear();
        for (i, q) in self.procs.iter().enumerate() {
            self.groups.entry(q.name.clone()).or_default().push(i);
        }
        if self.zoom.as_ref().is_some_and(|z| !self.groups.contains_key(z)) {
            self.zoom = None;
        }
        let gone = match &self.sel {
            Some(Hit::Pid(p)) => !self.by_pid.contains_key(p),
            Some(Hit::Group(n)) => !self.groups.contains_key(n),
            Some(Hit::Dir(_)) | None => false,
        };
        if gone {
            self.sel = None;
        }
        if self.space.is_some() {
            return; // the process list is kept fresh for the cards; the map shows the disk
        }
        let m = self.layout();
        let reflow = now - self.last_reflow >= REFLOW && self.geometry_changed(&m);
        self.apply(m, reflow, now);
        self.refresh_rows();
    }

    fn layout(&mut self) -> Vec<Tile> {
        if self.tm_size.x < 1.0 {
            return Vec::new();
        }
        if self.space.is_some() {
            return self.space_layout();
        }
        if let Some((a, b)) = self.split_fields() {
            // two maps side by side, each under a caption band
            let (w, h) = ((self.tm_size.x - SPLIT_GAP) / 2.0, self.tm_size.y - SPLIT_CAPTION);
            let zoom = self.zoom.clone();
            let mut tiles = Vec::new();
            for (side, f) in [(0u8, a), (1u8, b)] {
                let st = if side == 0 { &mut self.sticky } else { &mut self.sticky2 };
                for mut t in treemap::model(&self.procs, f, zoom.as_deref(), w, h, st, None) {
                    t.rect[0] += side as f32 * (w + SPLIT_GAP);
                    t.rect[1] += SPLIT_CAPTION;
                    t.key = Key::Side(side, Box::new(t.key));
                    tiles.push(t);
                }
            }
            return tiles;
        }
        let whole = self.whole();
        treemap::model(&self.procs, self.metric, self.zoom.as_deref(), self.tm_size.x, self.tm_size.y, &mut self.sticky, whole)
    }

    // -------------------------------------------------------------- disk space map

    /// Folders of the current one (each with its own subfolders inside), plus "Files" (directly in it) and,
    /// at the drive root, "Not scanned yet" (used space the scan has not reached) and, with Share of total,
    /// "Free". As the scan advances the scanned folders grow and the unscanned part shrinks.
    fn space_layout(&mut self) -> Vec<Tile> {
        let Some(sv) = &self.space else { return Vec::new() };
        let g = sv.scan.tree.lock().unwrap();
        let node = &g.nodes[sv.at as usize];
        let files_key = |id: u32| Key::Dir(id | 0x8000_0000); // "Files" of a folder: its own tile key
        let kid = |c: u32| {
            let n = &g.nodes[c as usize];
            let mut children: Vec<treemap::Item> = n
                .children
                .iter()
                .map(|&cc| treemap::Item { key: Key::Dir(cc), hit: Some(Hit::Dir(cc)), title: g.nodes[cc as usize].name.to_string(), value: g.nodes[cc as usize].total as f64, free: false, children: Vec::new() })
                .collect();
            if n.files > 0 && !children.is_empty() {
                children.push(treemap::Item { key: files_key(c), hit: Some(Hit::Dir(c)), title: "Files".into(), value: n.files as f64, free: true, children: Vec::new() });
            }
            treemap::Item { key: Key::Dir(c), hit: Some(Hit::Dir(c)), title: n.name.to_string(), value: n.total as f64, free: false, children }
        };
        let mut items: Vec<treemap::Item> = node.children.iter().map(|&c| kid(c)).collect();
        if node.files > 0 {
            items.push(treemap::Item { key: files_key(sv.at), hit: None, title: "Files".into(), value: node.files as f64, free: true, children: Vec::new() });
        }
        if sv.at == 0 {
            let missing = sv.used.saturating_sub(g.nodes[0].total);
            if g.scanned_at.is_none() && missing > 0 {
                items.push(treemap::Item { key: Key::Part("Not scanned yet"), hit: None, title: "Not scanned yet".into(), value: missing as f64, free: true, children: Vec::new() });
            }
            if self.relative {
                items.push(treemap::Item { key: Key::Part("Free"), hit: None, title: "Free".into(), value: sv.total.saturating_sub(sv.used) as f64, free: true, children: Vec::new() });
            }
        }
        drop(g);
        treemap::nested(items, self.tm_size.x, self.tm_size.y, &mut self.sticky)
    }

    /// Open the disk space map of `drive` ("C:"), starting its scan if needed.
    fn open_space(&mut self, drive: &str, now: f64, ctx: &egui::Context) {
        let (used, total) = self.tot.as_ref().and_then(|t| t.drives.iter().find(|d| d.0 == drive).map(|d| (d.1, d.2))).unwrap_or((0, 0));
        if self.space.as_ref().is_none_or(|s| s.scan.root != drive) {
            let c = ctx.clone();
            self.space = Some(SpaceView { scan: space::Scan::open(drive, move || c.request_repaint()), at: 0, used, total });
        } else if let Some(s) = &mut self.space {
            s.at = 0;
        }
        (self.sel, self.zoom, self.space_seen) = (None, None, 0);
        self.relayout(now);
    }

    fn space_enter(&mut self, id: u32, now: f64) {
        if let Some(s) = &mut self.space {
            let has_kids = !s.scan.tree.lock().unwrap().nodes[id as usize].children.is_empty();
            if has_kids {
                s.at = id;
                self.sel = None;
                self.relayout(now);
            }
        }
    }

    /// The scan moved on: lay the map out again (with a glide), at most once a second.
    fn space_tick(&mut self, now: f64) {
        let Some(s) = &self.space else { return };
        let n = s.scan.tree.lock().unwrap().version; // bumps on every change the walk makes
        if n != self.space_seen && now - self.last_reflow >= 1.0 {
            self.space_seen = n;
            let m = self.layout();
            self.apply(m, true, now);
            self.refresh_rows();
        }
    }

    /// Disk and network are shown as one split view: (left, right) measures.
    fn split_fields(&self) -> Option<(Field, Field)> {
        if self.space.is_some() {
            return None; // the drive map has its own header: no read/write or down/up captions
        }
        match self.metric {
            Field::IoRead | Field::IoWrite => Some((Field::IoRead, Field::IoWrite)),
            Field::NetDown | Field::NetUp => Some((Field::NetDown, Field::NetUp)),
            _ => None,
        }
    }

    /// (total, used, free label) of the machine in the metric's units, for "Share of total"; None when off or
    /// meaningless (disk I/O has no fixed maximum).
    fn whole(&self) -> Option<(f64, f64, &'static str)> {
        let s = self.tot.as_ref().filter(|_| self.relative)?;
        let g = &s.gpu;
        match self.metric {
            Field::Ws => Some((s.ram_total as f64, s.ram_used as f64, "Free")),
            Field::Private => Some((s.commit_limit as f64, s.commit as f64, "Free")),
            Field::Cpu => Some((100.0, s.cpu as f64, "Idle")), // processes are in % of the machine whatever the display
            Field::Gpu => g.util.map(|u| (100.0, u as f64, "Idle")),
            Field::GpuMem => (g.mem_total > 0).then(|| (g.mem_total as f64, g.mem_used as f64, "Free")),
            // rates have no fixed maximum (and process file I/O counts cache hits the disk never sees)
            Field::IoRead | Field::IoWrite | Field::NetDown | Field::NetUp => None,
        }
    }

    fn geometry_changed(&self, m: &[Tile]) -> bool {
        let old: HashMap<&Key, [f32; 4]> = self.tiles.iter().map(|t| (&t.key, t.rect)).collect();
        m.len() != old.len() || m.iter().any(|t| old.get(&t.key).is_none_or(|o| (0..4).any(|i| (o[i] - t.rect[i]).abs() > 1.0)))
    }

    /// Install a new layout. reflow=false keeps every tile where it is (labels and colours only): reflows are
    /// rate-limited, because tiles that keep moving are hard to read and to click.
    fn apply(&mut self, mut m: Vec<Tile>, reflow: bool, now: f64) {
        if reflow {
            let mut moves = false;
            for t in &m {
                let from = self.shown_rect(&t.key, now).unwrap_or(t.rect);
                moves |= (0..4).any(|i| (from[i] - t.rect[i]).abs() > 0.5);
                self.anim.insert(t.key.clone(), (from, t.rect));
            }
            let keys: HashSet<&Key> = m.iter().map(|t| &t.key).collect();
            self.anim.retain(|k, _| keys.contains(k));
            // nothing moves (first layout, same geometry): no glide, so no 0.5 s of frames for nothing
            (self.anim_t0, self.last_reflow) = (if moves { now } else { -1e9 }, now);
        } else {
            // same geometry, fresh labels; a tile whose process dropped out of the measure keeps its place
            // until the next reflow (dropping it left a dark hole in the map)
            let mut new: HashMap<Key, Tile> = m.into_iter().map(|t| (t.key.clone(), t)).collect();
            m = self
                .tiles
                .iter()
                .map(|o| match new.remove(&o.key) {
                    Some(mut t) => {
                        (t.rect, t.header) = (o.rect, o.header);
                        t
                    }
                    None => Tile { value: 0.0, cpu: 0.0, ..o.clone() }, // says what it uses now: nothing
                })
                .collect();
        }
        self.tiles = m;
    }

    fn shown_rect(&self, k: &Key, now: f64) -> Option<[f32; 4]> {
        let (a, b) = self.anim.get(k)?;
        Some(lerp4(*a, *b, ease((now - self.anim_t0) / TILE_ANIM)))
    }

    /// User changed metric / zoom / share: new layout now, tiles glide to it.
    fn relayout(&mut self, now: f64) {
        (self.sticky, self.sticky2) = (Sticky::new(0.15), Sticky::new(0.15));
        let m = self.layout();
        self.apply(m, true, now);
        self.refresh_rows();
    }

    fn cpu_scale(&self) -> f64 {
        1.0 // CPU % is always of the whole machine; "Per core" shows the cores themselves (draw_cores)
    }

    fn fmt_metric(&self, v: f64) -> String {
        if self.space.is_some() {
            return fmt_bytes(v);
        }
        match self.metric {
            Field::Cpu => format!("{:.1}%", v * self.cpu_scale()),
            Field::Gpu => format!("{v:.1}%"),
            f if f.is_rate() => fmt_rate(v),
            _ => fmt_bytes(v),
        }
    }

    fn metric_label(&self) -> &'static str {
        match self.metric {
            Field::Ws => "memory",
            Field::Private => "private bytes",
            Field::Cpu => "CPU",
            Field::GpuMem => "VRAM",
            Field::Gpu => "GPU load",
            Field::IoRead | Field::IoWrite => "disk reads | writes",

            Field::NetDown | Field::NetUp => "download | upload",

        }
    }

    fn tile_color(&self, t: &Tile) -> Color32 {
        if t.free {
            return match t.name.as_str() {
                "Other" | "Files" => th().raised,
                "Not scanned yet" => th().unscanned,
                _ => th().free,
            };
        }
        if self.heat { heat_color(t.cpu) } else { mix(app_color(&t.name), th().bg, 0.25) }
    }

    fn selected(&self) -> Vec<&Proc> {
        match &self.sel {
            Some(Hit::Pid(p)) => self.by_pid.get(p).map(|&i| vec![&self.procs[i]]).unwrap_or_default(),
            Some(Hit::Group(n)) => self.groups.get(n).map(|v| v.iter().map(|&i| &self.procs[i]).collect()).unwrap_or_default(),
            Some(Hit::Dir(_)) | None => Vec::new(),
        }
    }

    fn refresh_rows(&mut self) {
        let ql = self.query.trim().to_lowercase();
        let base: Vec<usize> = match &self.zoom {
            Some(z) => self.groups.get(z).cloned().unwrap_or_default(),
            None => (0..self.procs.len()).collect(),
        };
        let p = &self.procs;
        let mut rows: Vec<usize> =
            base.into_iter().filter(|&i| ql.is_empty() || p[i].name.to_lowercase().contains(&ql) || p[i].pid.to_string() == ql || p[i].cmd.to_lowercase().contains(&ql)).collect();
        let (col, desc) = self.sort;
        let num = |q: &Proc| -> f64 {
            match col {
                Col::Pid => q.pid as f64,
                Col::Cpu => q.cpu,
                Col::Ws => q.ws,
                Col::Private => q.private,
                Col::GpuMem => q.gpu_mem,
                Col::Gpu => q.gpu,
                Col::IoR => q.io_r,
                Col::IoW => q.io_w,
                Col::NetD => q.net_d,
                Col::NetU => q.net_u,
                Col::Threads => q.threads as f64,
                Col::Create => q.create as f64,
                Col::Name | Col::Cmd => 0.0,
            }
        };
        rows.sort_by(|&a, &b| {
            let o = match col {
                Col::Name => p[a].name.to_lowercase().cmp(&p[b].name.to_lowercase()),
                Col::Cmd => p[a].cmd.to_lowercase().cmp(&p[b].cmd.to_lowercase()),
                _ => num(&p[a]).total_cmp(&num(&p[b])),
            };
            if desc { o.reverse() } else { o }
        });
        self.rows = rows;
    }

    fn toast(&mut self, msg: impl Into<String>, error: bool, now: f64) {
        self.toast = Some((msg.into(), error, now));
    }

    fn username(&mut self, pid: u32) -> String {
        self.users.entry(pid).or_insert_with(|| sys::username(pid)).clone()
    }

    fn describe(&mut self, h: &Hit) -> (String, String) {
        let pdh = self.tot.as_ref().is_some_and(|t| t.pdh_ok);
        let k = self.cpu_scale();
        match h {
            Hit::Pid(pid) => {
                let Some(&i) = self.by_pid.get(pid) else { return Default::default() };
                let procs = self.procs.clone();
                let q = &procs[i];
                let gm = if pdh { format!("{} VRAM · {:.0}% GPU", fmt_bytes(q.gpu_mem), q.gpu) } else { "GPU n/a".into() };
                let cmd: String = if q.cmd.is_empty() { "(command line not accessible)".into() } else { q.cmd.chars().take(300).collect() };
                let body = format!(
                    "PID {}  ·  parent {}  ·  {}\nRAM {}  ·  private {}\nCPU {:.1}%  ·  {gm}  ·  {} threads\nDisk read {}  ·  write {}\nNetwork down {}  ·  up {}\n{cmd}",
                    q.pid,
                    q.ppid,
                    self.username(q.pid),
                    fmt_bytes(q.ws),
                    fmt_bytes(q.private),
                    q.cpu * k,
                    q.threads,
                    fmt_rate(q.io_r),
                    fmt_rate(q.io_w),
                    fmt_rate(q.net_d),
                    fmt_rate(q.net_u)
                );
                (q.name.clone(), body)
            }
            Hit::Group(name) => {
                let qs: Vec<&Proc> = self.groups.get(name).map(|v| v.iter().map(|&i| &self.procs[i]).collect()).unwrap_or_default();
                let mut pids: Vec<String> = qs.iter().take(12).map(|q| q.pid.to_string()).collect();
                if qs.len() > 12 {
                    pids.push("…".into());
                }
                let sum = |f: fn(&Proc) -> f64| qs.iter().map(|q| f(q)).sum::<f64>();
                let body = format!(
                    "{} processes  ·  PIDs {}\nRAM {}  ·  private {}\nCPU {:.1}%  ·  {} VRAM  ·  {:.0}% GPU\nDisk read {}  ·  write {}\nNetwork down {}  ·  up {}\nDouble-click to show its processes",
                    qs.len(),
                    pids.join(", "),
                    fmt_bytes(sum(|q| q.ws)),
                    fmt_bytes(sum(|q| q.private)),
                    sum(|q| q.cpu) * k,
                    fmt_bytes(sum(|q| q.gpu_mem)),
                    qs.iter().map(|q| q.gpu).fold(0.0, f64::max),
                    fmt_rate(sum(|q| q.io_r)),
                    fmt_rate(sum(|q| q.io_w)),
                    fmt_rate(sum(|q| q.net_d)),
                    fmt_rate(sum(|q| q.net_u))
                );
                (name.clone(), body)
            }
            Hit::Dir(id) => {
                let Some(s) = &self.space else { return Default::default() };
                let g = s.scan.tree.lock().unwrap();
                let n = &g.nodes[*id as usize];
                let parent = if *id == 0 { n.total } else { g.nodes[n.parent as usize].total };
                let share = if parent > 0 { n.total as f64 / parent as f64 * 100.0 } else { 0.0 };
                let body = format!(
                    "{}\n{}  ·  {share:.0}% of its parent folder\n{} subfolders  ·  {} in files directly here{}",
                    g.path(*id),
                    fmt_bytes(n.total as f64),
                    n.children.len(),
                    fmt_bytes(n.files as f64),
                    if n.children.is_empty() { "" } else { "\nDouble-click to open it" }
                );
                (n.name.to_string(), body)
            }
        }
    }

    // -------------------------------------------------------------- actions

    fn kill(&mut self, pids: &[u32], tree: bool, now: f64) {
        let mut targets: Vec<u32> = pids.to_vec();
        if tree {
            // descendants by parent pid; a child is never older than its parent (guards against pid reuse)
            let mut i = 0;
            while i < targets.len() {
                let (pid, born) = (targets[i], self.by_pid.get(&targets[i]).map_or(0, |&k| self.procs[k].create));
                for q in self.procs.iter() {
                    if q.ppid == pid && q.create >= born && q.pid != pid && !targets.contains(&q.pid) {
                        targets.push(q.pid);
                    }
                }
                i += 1;
            }
        }
        let (mut killed, mut denied) = (0, 0);
        for pid in targets {
            match sys::kill(pid) {
                Ok(true) => killed += 1,
                Ok(false) => {}
                Err(()) => denied += 1,
            }
        }
        let s = if killed != 1 { "es" } else { "" };
        if denied > 0 {
            self.toast(format!("Ended {killed}, access denied on {denied}: run ResMon as administrator for system processes"), true, now);
        } else {
            self.toast(format!("Ended {killed} process{s}"), false, now);
        }
    }

    fn open_location(&mut self, pid: u32, now: f64) {
        match sys::exe_path(pid) {
            Some(path) => {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{path}\"")).spawn();
            }
            None => self.toast("Access denied: run ResMon as administrator to locate this file", true, now),
        }
    }

    /// Copy the remembered choices into the settings file.
    fn save_settings(&mut self) -> std::io::Result<()> {
        let c = &mut self.cfg;
        (c.anchor, c.metric, c.heat, c.relative, c.per_core, c.table_open) =
            (Some(self.anchor), field_name(self.metric).into(), self.heat, self.relative, self.per_core, self.table_open);
        c.save()
    }

    fn toggle_startup(&mut self, now: f64) {
        if let Err(e) = sys::set_startup(!self.startup) {
            self.toast(format!("Cannot change start with Windows: {e}"), true, now);
        }
        self.startup = sys::startup_enabled();
    }
}

// ------------------------------------------------------------------ drawing

struct Gauge<'a> {
    icon: &'static str,
    vi: &'static str, // optional icons before the value and the sub line (disk: read / write, network: down / up)
    si: &'static str,
    title: String,
    value: String,
    sub: String,
    cores: Option<&'a [f32]>,
    s1: (&'a VecDeque<f32>, Color32),
    s2: Option<(&'a VecDeque<f32>, Color32)>,
    top: f32,
}

fn card(p: &egui::Painter, r: Rect, selected: bool) {
    p.rect(r, 12.0, if selected { th().sel_bg } else { th().card }, Stroke::new(1.0, if selected { th().sel_line } else { th().card }), StrokeKind::Inside);
}

fn draw_gauge(ui: &Ui, r: Rect, g: &Gauge, small: bool, selected: bool) {
    let p = ui.painter().with_clip_rect(r);
    card(&p, r, selected);
    let inner = Rect::from_min_max(r.min + vec2(12.0, if small { 8.0 } else { 10.0 }), r.max - vec2(12.0, if small { 6.0 } else { 8.0 }));
    let w = inner.width();
    let tc = if selected { th().text } else { th().muted };
    let mut y = inner.top();
    // the icon in the card's colour, then the label
    let ic = if selected { g.s1.1 } else { mix(g.s1.1, th().muted, 0.35) };
    if small {
        p.text(pos2(inner.left(), y + 9.0), Align2::LEFT_CENTER, g.icon, icons(12.0), ic);
        text(&p, pos2(inner.left() + 18.0, y + 9.0), Align2::LEFT_CENTER, &g.title, bold(10.0), tc, w * 0.5);
        let vr = text(&p, pos2(inner.right(), y + 9.0), Align2::RIGHT_CENTER, &g.value, bold(15.0), th().text, w * 0.6);
        if !g.vi.is_empty() {
            p.text(pos2(vr.left() - 5.0, y + 9.0), Align2::RIGHT_CENTER, g.vi, icons(12.0), g.s1.1);
        }
        y += 21.0;
    } else {
        p.text(pos2(inner.left(), y + 6.0), Align2::LEFT_CENTER, g.icon, icons(12.0), ic);
        text(&p, pos2(inner.left() + 18.0, y), Align2::LEFT_TOP, &g.title, bold(10.0), tc, w - 18.0);
        y += 16.0;
        let dx = if g.vi.is_empty() { 0.0 } else { 22.0 };
        if !g.vi.is_empty() {
            p.text(pos2(inner.left(), y + 13.0), Align2::LEFT_CENTER, g.vi, icons(15.0), g.s1.1);
        }
        text(&p, pos2(inner.left() + dx, y), Align2::LEFT_TOP, &g.value, bold(((w - dx) / 7.0).clamp(13.0, 20.0)), th().text, w - dx);
        y += 28.0;
    }
    match g.cores {
        Some(cores) if !cores.is_empty() => {
            let n = cores.len() as f32;
            let bw = (w - 3.0 * (n - 1.0)) / n;
            for (i, &v) in cores.iter().enumerate() {
                // values jump to each sample: easing them kept the CPU renderer at ~40 fps (half a core)
                let tr = Rect::from_min_size(pos2(inner.left() + i as f32 * (bw + 3.0), y + 1.0), vec2(bw, 12.0));
                p.rect_filled(tr, 2.0, th().line);
                let fh = (v / 100.0).clamp(0.1, 1.0) * 12.0;
                p.rect_filled(Rect::from_min_max(pos2(tr.left(), tr.bottom() - fh), tr.max), 2.0, g.s1.1);
            }
        }
        _ => {
            let dx = if g.si.is_empty() { 0.0 } else { 17.0 };
            if let (false, Some((_, c2))) = (g.si.is_empty(), g.s2) {
                p.text(pos2(inner.left() + 1.0, y + 7.0), Align2::LEFT_CENTER, g.si, icons(11.0), c2);
            }
            text(&p, pos2(inner.left() + dx, y), Align2::LEFT_TOP, &g.sub, font(11.0), th().muted, w - dx);
        }
    }
    y += 17.0;
    let sr = Rect::from_min_max(pos2(inner.left(), y), pos2(inner.right(), inner.bottom()));
    spark(&p, sr, g.s1.0, g.top, g.s1.1, true);
    if let Some((s, c)) = g.s2 {
        spark(&p, sr, s, g.top, c, false);
    }
}

/// Which processes ran on each logical CPU: [cpu] -> [(process index, % of that CPU)].
///
/// Windows does not say where a thread ran (that needs a kernel trace, i.e. administrator). We know each
/// process's CPU time and each CPU's load exactly, and each busy thread's ideal processor (where the
/// scheduler prefers it; under load it often runs elsewhere). So the ideal processors are only the
/// starting guess, and the table is balanced (iterative proportional fitting) until every process row
/// sums to its measured CPU and every CPU column to its measured load.
fn split_by_cpu(procs: &[Proc], loads: &[f32]) -> Vec<Vec<(usize, f32)>> {
    let k = loads.len();
    let busy: Vec<usize> = (0..procs.len()).filter(|&i| procs[i].cpu > 0.05).collect();
    let mut out = vec![Vec::new(); k];
    let total_load: f64 = loads.iter().map(|&l| l as f64).sum();
    if busy.is_empty() || k == 0 || total_load <= 0.0 {
        return out;
    }
    let rows: Vec<f64> = busy.iter().map(|&i| procs[i].cpu * k as f64).collect(); // in "% of one CPU"
    let scale = total_load / rows.iter().sum::<f64>().max(1e-9);
    let cols: Vec<f64> = loads.iter().map(|&l| l as f64 / scale.max(1.0)).collect(); // processes may not explain all load
    let mut m: Vec<Vec<f64>> = busy
        .iter()
        .zip(&rows)
        .map(|(&i, &row)| {
            (0..k)
                .map(|c| {
                    let ideal = procs[i].cpus.iter().find(|e| e.0 as usize == c).map_or(0.0, |e| e.1 as f64);
                    ideal + 0.15 * row * loads[c] as f64 / total_load // anything can run anywhere
                })
                .collect()
        })
        .collect();
    for _ in 0..30 {
        for (r, &target) in m.iter_mut().zip(&rows) {
            let s: f64 = r.iter().sum();
            if s > 0.0 {
                r.iter_mut().for_each(|v| *v *= target / s);
            }
        }
        for c in 0..k {
            let s: f64 = m.iter().map(|r| r[c]).sum();
            if s > 0.0 {
                let f = cols[c] / s;
                m.iter_mut().for_each(|r| r[c] *= f);
            }
        }
    }
    for (r, &i) in m.iter().zip(&busy) {
        for (c, &v) in r.iter().enumerate() {
            if v > 0.05 {
                out[c].push((i, v as f32));
            }
        }
    }
    out
}

/// One drive in the full view: name, free space, a used-space bar, and its read / write rates.
fn draw_drive_card(ui: &Ui, r: Rect, mount: &str, used: u64, total: u64, rd: f64, wr: f64, selected: bool) {
    let p = ui.painter().with_clip_rect(r);
    card(&p, r, selected);
    let inner = Rect::from_min_max(r.min + vec2(12.0, 10.0), r.max - vec2(12.0, 8.0));
    let w = inner.width();
    let ic = if selected { th().text } else { th().muted };
    p.text(pos2(inner.left(), inner.top() + 6.0), Align2::LEFT_CENTER, I_DISK, icons(12.0), ic);
    text(&p, pos2(inner.left() + 18.0, inner.top()), Align2::LEFT_TOP, &format!("DRIVE {mount}"), bold(10.0), ic, w - 18.0);
    let f = if total > 0 { used as f32 / total as f32 } else { 0.0 };
    text(&p, pos2(inner.left(), inner.top() + 16.0), Align2::LEFT_TOP, &format!("{} free", fmt_bytes(total.saturating_sub(used) as f64)), bold((w / 7.5).clamp(13.0, 20.0)), th().text, w);
    text(&p, pos2(inner.left(), inner.top() + 44.0), Align2::LEFT_TOP, &format!("of {}  ·  {:.0}% used", fmt_bytes(total as f64), f * 100.0), font(11.0), th().muted, w);
    let bar = Rect::from_min_size(pos2(inner.left(), inner.top() + 62.0), vec2(w, 5.0));
    p.rect_filled(bar, 2.5, th().line);
    p.rect_filled(Rect::from_min_size(bar.min, vec2(w * f, 5.0)), 2.5, if f > 0.9 { DANGER } else { ac().disk });
    let y = inner.bottom() - 7.0;
    p.text(pos2(inner.left(), y), Align2::LEFT_CENTER, I_READ, icons(11.0), ac().disk);
    let t = text(&p, pos2(inner.left() + 16.0, y), Align2::LEFT_CENTER, &fmt_rate(rd), font(11.0), th().muted, w / 2.0 - 16.0);
    let x = (t.right() + 10.0).max(inner.left() + w / 2.0);
    p.text(pos2(x, y), Align2::LEFT_CENTER, I_WRITE, icons(11.0), ac().disk2);
    text(&p, pos2(x + 16.0, y), Align2::LEFT_CENTER, &fmt_rate(wr), font(11.0), th().muted, inner.right() - x - 16.0);
}

/// Space per volume. Returns each row's rect (the full view opens a drive's space map on click).
fn draw_drives(ui: &Ui, r: Rect, drives: &[(String, u64, u64)], small: bool, selected: Option<&str>) -> Vec<(String, Rect)> {
    let p = ui.painter().with_clip_rect(r);
    card(&p, r, selected.is_some());
    let inner = r.shrink2(vec2(12.0, 9.0));
    let ic = if selected.is_some() { th().text } else { th().muted };
    p.text(pos2(inner.left(), inner.top() + 6.0), Align2::LEFT_CENTER, I_DRIVES, icons(12.0), ic);
    let title = if small { "DRIVES" } else { "DRIVES · click for folders" };
    text(&p, inner.left_top() + vec2(18.0, 0.0), Align2::LEFT_TOP, if selected.is_some() { "DRIVES · FOLDERS" } else { title }, bold(10.0), ic, inner.width() - 18.0);
    let mut y = inner.top() + 22.0;
    let step = if small { 17.0 } else { 18.0 };
    let mut rows = Vec::new();
    for (mount, used, total) in drives {
        let cy = y + 6.0;
        rows.push((mount.clone(), Rect::from_min_max(pos2(r.left(), cy - step / 2.0), pos2(r.right(), cy + step / 2.0))));
        if selected == Some(mount.as_str()) {
            p.rect_filled(Rect::from_min_max(pos2(inner.left() - 6.0, cy - step / 2.0), pos2(inner.right() + 6.0, cy + step / 2.0)), 5.0, th().pill_on);
        }
        text(&p, pos2(inner.left(), cy), Align2::LEFT_CENTER, mount, font(11.0), th().text, 24.0);
        let free = format!("{} free", fmt_bytes((total - used) as f64));
        let fr = text(&p, pos2(inner.right(), cy), Align2::RIGHT_CENTER, &free, font(11.0), th().muted, 90.0);
        let bar = Rect::from_min_max(pos2(inner.left() + 28.0, cy - 2.0), pos2(fr.left() - 8.0, cy + 2.0));
        p.rect_filled(bar, 2.0, th().line);
        let f = if *total > 0 { *used as f32 / *total as f32 } else { 0.0 };
        p.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * f, 4.0)), 2.0, if f > 0.9 { DANGER } else { th().muted });
        y += step;
    }
    rows
}

/// A text button drawn by hand (labels only: they already say what they do).
fn link(ui: &mut Ui, label: &str, color: Color32, size: f32) -> egui::Response {
    let g = ui.painter().layout_no_wrap(label.into(), font(size), color);
    let (r, resp) = ui.allocate_exact_size(g.size() + vec2(20.0, 10.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, 6.0, alpha(th().text, 14));
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    ui.painter().galley(r.center() - g.size() / 2.0, g, color);
    resp
}

fn icon_button(ui: &mut Ui, glyph: &str, tip: &str, color: Color32) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, 8.0, alpha(th().text, 16));
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    ui.painter().text(r.center(), Align2::CENTER_CENTER, glyph, icons(14.0), color);
    resp.on_hover_text(tip)
}

/// Segmented pills; returns the index clicked.
fn pills(ui: &mut Ui, items: &[(&str, bool)]) -> Option<usize> {
    let gs: Vec<_> = items.iter().map(|(l, on)| ui.painter().layout_no_wrap(l.to_string(), font(12.0), if *on { th().text } else { th().muted })).collect();
    let w: f32 = gs.iter().map(|g| g.size().x + 20.0).sum::<f32>() + 2.0 * (gs.len() - 1) as f32 + 6.0;
    let (r, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
    ui.painter().rect_filled(r, 10.0, th().card);
    let mut x = r.left() + 3.0;
    let mut hit = None;
    for (i, g) in gs.into_iter().enumerate() {
        let pr = Rect::from_min_size(pos2(x, r.top() + 3.0), vec2(g.size().x + 20.0, 24.0));
        let resp = ui.interact(pr, ui.scope_id().with(("pill", items[i].0)), Sense::click());
        let bg = ui.ctx().animate_bool_with_time(resp.id.with("on"), items[i].1, 0.2);
        ui.painter().rect_filled(pr, 8.0, alpha(th().pill_on, (bg * 255.0) as u8));
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        ui.painter().galley(pr.center() - g.size() / 2.0, g, th().text);
        if resp.clicked() {
            hit = Some(i);
        }
        x = pr.right() + 2.0;
    }
    hit
}

impl App {
    fn gauges(&self, small: bool) -> Vec<(&'static str, Gauge<'_>)> {
        let Some(s) = &self.tot else { return Vec::new() };
        let h = &self.hist;
        let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
        let mut v = vec![
            ("cpu", Gauge { icon: I_CPU, vi: "", si: "", title: "CPU".into(), value: format!("{:.0}%", s.cpu), sub: String::new(), cores: Some(&s.per_core), s1: (&h.cpu, ac().cpu), s2: None, top: 100.0 }),
            (
                "ram",
                Gauge {
                    icon: I_RAM,
                    vi: "",
                    si: "",
                    title: if !small && self.metric == Field::Private { "MEMORY · PRIVATE".into() } else { "MEMORY".into() },
                    value: format!("{:.1} / {}", gb(s.ram_used), fmt_bytes(s.ram_total as f64)),
                    sub: format!("Commit {} of {}", fmt_bytes(s.commit as f64), fmt_bytes(s.commit_limit as f64)),
                    cores: None,
                    s1: (&h.ram, ac().ram),
                    s2: None,
                    top: 100.0,
                },
            ),
        ];
        if small {
            v.push((
                "commit",
                Gauge {
                    icon: I_COMMIT,
                    vi: "",
                    si: "",
                    title: "COMMIT".into(),
                    value: format!("{:.1} / {}", gb(s.commit), fmt_bytes(s.commit_limit as f64)),
                    sub: format!("{:.0}% of the commit limit", s.commit as f64 / s.commit_limit.max(1) as f64 * 100.0),
                    cores: None,
                    s1: (&h.commit, ac().commit),
                    s2: None,
                    top: 100.0,
                },
            ));
        }
        let g = &s.gpu;
        let (gv, gs) = match g.util {
            Some(u) => {
                let extra: Vec<String> = [g.temp.map(|t| format!("{t}°C")), g.power.map(|w| format!("{w:.0} W"))].into_iter().flatten().collect();
                (format!("{u:.0}%"), if extra.is_empty() { g.name.clone() } else { extra.join(" · ") })
            }
            None => ("n/a".into(), s.gpu_err.clone().unwrap_or_else(|| "No NVIDIA GPU".into())),
        };
        v.push(("gpu", Gauge { icon: I_GPU, vi: "", si: "", title: "GPU".into(), value: gv, sub: gs, cores: None, s1: (&h.gpu, ac().gpu), s2: None, top: 100.0 }));
        let (vv, vs) = if g.mem_total > 0 { (format!("{:.1} / {}", gb(g.mem_used), fmt_bytes(g.mem_total as f64)), g.name.replace("NVIDIA GeForce ", "")) } else { ("-".into(), String::new()) };
        v.push(("vram", Gauge { icon: I_VRAM, vi: "", si: "", title: "VRAM".into(), value: vv, sub: vs, cores: None, s1: (&h.vram, ac().vram), s2: None, top: 100.0 }));
        let mx = |a: &VecDeque<f32>, b: &VecDeque<f32>, floor: f32| a.iter().chain(b).fold(floor, |m, &x| m.max(x));
        v.push((
            "disk",
            Gauge {
                icon: I_DISK,
                title: "DISK".into(),
                value: fmt_rate(s.disk_r),
                sub: format!("{} write", fmt_rate(s.disk_w)),
                vi: I_READ,
                si: I_WRITE,
                cores: None,
                s1: (&h.disk_r, ac().disk),
                s2: Some((&h.disk_w, ac().disk2)),
                top: mx(&h.disk_r, &h.disk_w, 1048576.0),
            },
        ));
        v.push((
            "net",
            Gauge {
                icon: I_NET,
                title: "NETWORK".into(),
                value: fmt_rate(s.net_d),
                sub: format!("{} up", fmt_rate(s.net_u)),
                vi: I_DOWN,
                si: I_UP,
                cores: None,
                s1: (&h.net_d, ac().net),
                s2: Some((&h.net_u, ac().net2)),
                top: mx(&h.net_d, &h.net_u, 102400.0),
            },
        ));
        v.retain(|(k, _)| self.cfg.shows(if *k == "commit" { "ram" } else { k })); // cards switched off in Settings
        v
    }

    /// The docked tab: as wide as its bars need.
    fn tab_size(&self) -> Vec2 {
        vec2(16.0 + 9.0 * self.cfg.tab.len().max(1) as f32, TAB.y)
    }

    /// (fill %, colour, icon) of each bar of the docked tab. Rates are against the peak of the last minute.
    fn tab_bars(&self) -> Vec<(f32, Color32, &'static str)> {
        let Some(s) = &self.tot else { return Vec::new() };
        let h = &self.hist;
        let peak = |a: &VecDeque<f32>, b: &VecDeque<f32>, floor: f32| a.iter().chain(b).fold(floor, |m, &x| m.max(x));
        self.cfg
            .tab
            .iter()
            .filter_map(|k| match k.as_str() {
                "cpu" => Some((s.cpu, ac().cpu, I_CPU)),
                "ram" => Some((s.ram_used as f32 / s.ram_total.max(1) as f32 * 100.0, ac().ram, I_RAM)),
                "gpu" => Some((s.gpu.util.unwrap_or(0.0), ac().gpu, I_GPU)),
                "vram" => Some((s.gpu.mem_used as f32 / s.gpu.mem_total.max(1) as f32 * 100.0, ac().vram, I_VRAM)),
                "disk" => Some(((s.disk_r + s.disk_w) as f32 / peak(&h.disk_r, &h.disk_w, 1048576.0) * 100.0, ac().disk, I_DISK)),
                "net" => Some(((s.net_d + s.net_u) as f32 / peak(&h.net_d, &h.net_u, 102400.0) * 100.0, ac().net, I_NET)),
                _ => None,
            })
            .collect()
    }

    fn draw_tab(&mut self, ui: &mut Ui, win: Rect, now: f64) {
        let ctx = ui.ctx().clone();
        let tab = self.tab_size();
        let r = Rect::from_min_size(pos2(win.right() - tab.x, win.center().y - tab.y / 2.0), tab);
        let p = ui.painter();
        p.rect(r, CornerRadius { nw: 12, sw: 12, ne: 0, se: 0 }, alpha(th().tab_bg, 0xF2), Stroke::new(1.0, th().tab_line), StrokeKind::Inside);
        // the bars chosen in Settings, each with its icon underneath
        let bars = self.tab_bars();
        let (top, bh, pitch) = (r.top() + 14.0, tab.y - 44.0, 9.0);
        let x0 = r.center().x + 2.0 - (pitch * (bars.len() as f32 - 1.0) + 4.0) / 2.0;
        for (i, (v, c, ic)) in bars.into_iter().enumerate() {
            // no easing here: the docked tab is always on screen, and an animation every 0.5 s kept it at ~36 fps (3% of a core)
            let tr = Rect::from_min_size(pos2(x0 + i as f32 * pitch, top), vec2(4.0, bh));
            p.rect_filled(tr, 2.0, th().line);
            let fh = (v / 100.0).clamp(0.02, 1.0) * bh;
            p.rect_filled(Rect::from_min_max(pos2(tr.left(), tr.bottom() - fh), tr.max), 2.0, c);
            p.text(pos2(tr.center().x, tr.bottom() + 10.0), Align2::CENTER_CENTER, ic, icons(8.5), mix(c, th().muted, 0.25));
        }
        // hand on hover (it is a button); the move cursor only once it is held: a drag or a long press
        let resp = ui.interact(r, Id::unique("tab"), Sense::click_and_drag());
        let held = resp.is_pointer_button_down_on() && ctx.input(|i| i.pointer.press_start_time()).is_some_and(|t| now - t > 0.5);
        if !self.drag && self.trans.is_none() && (resp.drag_started() || held) {
            (self.drag, self.dragged) = (true, true);
        }
        if self.drag {
            ctx.set_cursor_icon(CursorIcon::Move);
            let (x, y) = sys::cursor_pos();
            if sys::left_button_down() {
                if let Some(h) = self.hwnd {
                    let (_, _, w, hh) = self.bounds;
                    sys::move_to(h, x - w / 2, y - hh / 2); // keep the grabbed tab centred under the cursor
                }
                ctx.request_repaint();
            } else {
                self.drag = false;
                self.anchor = (sys::monitor_at(x, y).2 - 1, y);
                if let Err(e) = self.save_settings() {
                    self.toast(format!("Cannot save the tab position: {e}"), true, now);
                }
                let b = self.geometry(Mode::Tab);
                self.set_bounds(b, now);
            }
            return;
        }
        if resp.hovered() {
            ctx.set_cursor_icon(CursorIcon::PointingHand);
        }
        if resp.clicked() && !self.dragged {
            self.go(Mode::Full, now);
        } else if resp.secondary_clicked() {
            self.go(Mode::Panel, now);
        }
        if !resp.is_pointer_button_down_on() {
            self.dragged = false;
        }
    }

    fn draw_panel(&mut self, ui: &mut Ui, win: Rect, now: f64) {
        let p = match &self.trans {
            Some(t) if t.to == Mode::Tab => 1.0 - ease((now - t.t0) / UI_ANIM),
            Some(t) => ease((now - t.t0) / UI_ANIM),
            None => 1.0,
        };
        let off = (1.0 - p) * win.width();
        let r = win.translate(vec2(off, 0.0));
        let mut ui = ui.new_child(UiBuilder::new().max_rect(r).layout(Layout::top_down(Align::Min)));
        ui.set_clip_rect(win);
        let bg = ui.interact(r, Id::unique("panel"), Sense::click());
        ui.painter().rect(r, CornerRadius { nw: 16, sw: 16, ne: 0, se: 0 }, alpha(th().panel_bg, 0xF7), Stroke::new(1.0, th().line), StrokeKind::Inside);
        let inner = Rect::from_min_max(r.min + vec2(12.0, 10.0), r.max - vec2(10.0, 4.0));
        let pt = ui.painter().clone();
        text(&pt, inner.left_top(), Align2::LEFT_TOP, "ResMon", bold(13.0), th().text, 100.0);
        text(&pt, pos2(inner.right(), inner.top() + 2.0), Align2::RIGHT_TOP, "click for details", font(10.0), th().faint, 120.0);
        let gauges = self.gauges(true);
        let mut y = inner.top() + 26.0;
        for (_, g) in &gauges {
            let gr = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), 82.0));
            draw_gauge(&ui, gr, g, true, false);
            y += 88.0;
        }
        if let Some(s) = &self.tot {
            let dh = 31.0 + 17.0 * s.drives.len() as f32;
            draw_drives(&ui, Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), dh)), &s.drives, true, None);
        }
        let foot = Rect::from_min_max(pos2(inner.left(), inner.bottom() - 34.0), inner.max);
        let mut fu = ui.new_child(UiBuilder::new().max_rect(foot).layout(Layout::left_to_right(Align::Center)));
        if icon_button(&mut fu, "\u{E740}", "Open the full monitor", th().text).clicked() {
            self.go(Mode::Full, now);
            return;
        }
        fu.with_layout(Layout::right_to_left(Align::Center), |fu| {
            if icon_button(fu, "\u{E8BB}", "Quit ResMon", th().muted).clicked() {
                self.quit = true;
            }
            let tip = format!("Start with Windows: {} (click to toggle)", if self.startup { "on" } else { "off" });
            if icon_button(fu, "\u{E7E8}", &tip, if self.startup { ac().gpu } else { th().muted }).clicked() {
                self.toggle_startup(now);
            }
        });
        if bg.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if bg.clicked() {
            self.go(Mode::Full, now);
        }
    }

    fn draw_full(&mut self, ui: &mut Ui, win: Rect, now: f64) {
        // the card grows out of the right edge inside the full-size transparent window, then the content fades in
        let (size_t, fade) = match &self.trans {
            Some(t) if t.to == Mode::Tab => {
                let e = now - t.t0;
                (1.0 - ease((e - FADE) / UI_ANIM), 1.0 - ((e / FADE) as f32).clamp(0.0, 1.0))
            }
            Some(t) => {
                let e = now - t.t0;
                (ease(e / UI_ANIM), (((e - UI_ANIM) / FADE) as f32).clamp(0.0, 1.0))
            }
            None => (1.0, 1.0),
        };
        let from = match &self.trans {
            Some(t) if t.to == Mode::Tab => self.tab_size(),
            Some(t) => t.from,
            None => win.size(),
        };
        let size = from + (win.size() - from) * size_t;
        let card_r = Rect::from_min_size(pos2(win.right() - size.x, win.center().y - size.y / 2.0), size);
        ui.painter().rect(card_r, CornerRadius { nw: 14, sw: 14, ne: 0, se: 0 }, th().bg, Stroke::new(1.0, th().line), StrokeKind::Inside);
        if fade <= 0.0 {
            return;
        }
        let inner = Rect::from_min_max(card_r.min + vec2(14.0, 6.0), card_r.max - vec2(14.0, 14.0));
        let mut ui = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
        ui.set_clip_rect(card_r);
        ui.multiply_opacity(fade);
        self.topbar(&mut ui, inner, now);
        let header = Rect::from_min_size(pos2(inner.left(), inner.top() + 42.0), vec2(inner.width(), 112.0));
        self.header(&ui, header, now);
        let tb = Rect::from_min_size(pos2(inner.left(), header.bottom() + 10.0), vec2(inner.width(), 30.0));
        self.toolbar(&mut ui, tb, now);
        // the process list can be folded away: the map takes its room, the selection bar stays
        let rest = inner.bottom() - tb.bottom() - 10.0 - 40.0 - if self.table_open { 20.0 } else { 10.0 };
        let tm_h = if self.table_open { (rest * 0.6).floor() } else { rest };
        let tm = Rect::from_min_size(pos2(inner.left(), tb.bottom() + 10.0), vec2(inner.width(), tm_h));
        self.space_tick(now);
        self.treemap(&mut ui, tm, now);
        let sb = Rect::from_min_size(pos2(inner.left(), tm.bottom() + 10.0), vec2(inner.width(), 40.0));
        self.selbar(&mut ui, sb, now);
        if self.table_open {
            let table = Rect::from_min_max(pos2(inner.left(), sb.bottom() + 10.0), inner.max);
            self.table(&mut ui, table, now);
        }
        if self.settings_open {
            let sheet = Rect::from_min_max(pos2(inner.right() - 380.0, header.top()), inner.max);
            self.settings_sheet(&mut ui, sheet, now);
        }
        self.kill_dialog(ui.ctx(), now);
        if let Some((msg, err, t0)) = &self.toast {
            let left = 4.0 - (now - t0);
            if left <= 0.0 {
                self.toast = None;
            } else {
                let p = ui.painter();
                let g = p.layout_no_wrap(msg.clone(), font(13.0), th().text);
                let r = Rect::from_center_size(pos2(card_r.center().x, card_r.bottom() - 40.0), g.size() + vec2(32.0, 20.0));
                p.rect_filled(r, 10.0, if *err { rgb(0x3A1F25) } else { rgb(0x1F2A24) });
                p.galley(r.center() - g.size() / 2.0, g, th().text);
                ui.ctx().request_repaint_after(Duration::from_secs_f64(left));
            }
        }
    }

    fn topbar(&mut self, ui: &mut Ui, inner: Rect, now: f64) {
        let bar = Rect::from_min_size(inner.min, vec2(inner.width(), 32.0));
        let drag = ui.interact(Rect::from_min_max(bar.min, pos2(bar.right() - 110.0, bar.bottom())), Id::unique("titlebar"), Sense::click_and_drag());
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
        }
        let p = ui.painter();
        let t = text(p, pos2(bar.left() + 4.0, bar.center().y), Align2::LEFT_CENTER, "ResMon", bold(13.0), th().text, 80.0);
        text(p, pos2(t.right() + 8.0, bar.center().y), Align2::LEFT_CENTER, "drag to move · Esc to dock", font(11.0), th().faint, 300.0);
        let mut bu = ui.new_child(UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Center)));
        if icon_button(&mut bu, "\u{E8BB}", "Dock to the edge", th().muted).clicked() || icon_button(&mut bu, "\u{E921}", "Dock to the edge", th().muted).clicked() {
            self.go(Mode::Tab, now);
        }
        if icon_button(&mut bu, "\u{E713}", "Settings", if self.settings_open { th().text } else { th().muted }).clicked() {
            self.settings_open = !self.settings_open;
        }
    }

    fn header(&mut self, ui: &Ui, r: Rect, now: f64) {
        let gauges = self.gauges(false);
        // the measure cards, then one card per drive
        let drives: Vec<(String, u64, u64)> = if self.cfg.shows("drives") { self.tot.as_ref().map(|t| t.drives.clone()).unwrap_or_default() } else { Vec::new() };
        let n = (gauges.len() + drives.len()).max(1) as f32;
        let w = (r.width() - 10.0 * (n - 1.0)) / n;
        let mut pick = None;
        for (i, (key, g)) in gauges.iter().enumerate() {
            let gr = Rect::from_min_size(pos2(r.left() + i as f32 * (w + 10.0), r.top()), vec2(w, r.height()));
            let metric = match *key {
                "cpu" => Some(Field::Cpu),
                "ram" => Some(Field::Ws),
                "gpu" => Some(Field::Gpu),
                "vram" => Some(Field::GpuMem),
                "disk" => Some(Field::IoRead),
                "net" => Some(Field::NetDown),
                _ => None,
            };
            // a card with two measures (memory, disk, network) lights up for either
            let pair = |a: Field, b: Field| self.metric == a || self.metric == b;
            let on = self.space.is_none()
                && match *key {
                    "ram" => pair(Field::Ws, Field::Private),
                    "disk" => pair(Field::IoRead, Field::IoWrite),
                    "net" => pair(Field::NetDown, Field::NetUp),
                    _ => metric == Some(self.metric),
                };
            draw_gauge(ui, gr, g, false, on);
            if let Some(m) = metric {
                let tip = match *key {
                    "ram" => "Size the treemap by memory (click again for private bytes)".to_string(),
                    "disk" => "Who reads and who writes the disk, side by side".to_string(),
                    "net" => "Who downloads and who uploads, side by side".to_string(),
                    _ => format!("Size the treemap by {}", g.title),
                };
                let resp = ui.interact(gr, Id::unique(("card", *key)), Sense::click()).on_hover_text(tip);
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    // clicked again, a two-measure card flips between its measures
                    pick = Some(match (m, self.metric, on) {
                        (Field::Ws, Field::Ws, true) => Field::Private,
                        _ => m,
                    });
                }
            }
        }
        let mut drive = None;
        let io = self.tot.as_ref().map(|t| t.drive_io.clone()).unwrap_or_default();
        let open = self.space.as_ref().map(|s| s.scan.root.clone());
        for (j, (mount, used, total)) in drives.iter().enumerate() {
            let i = gauges.len() + j;
            let dr = Rect::from_min_size(pos2(r.left() + i as f32 * (w + 10.0), r.top()), vec2(w, r.height()));
            let (rd, wr) = io.iter().find(|d| &d.0 == mount).map_or((0.0, 0.0), |d| (d.1, d.2));
            draw_drive_card(ui, dr, mount, *used, *total, rd, wr, open.as_deref() == Some(mount.as_str()));
            let resp = ui.interact(dr, Id::unique(("drive", mount.as_str())), Sense::click()).on_hover_text(format!("{mount}: what fills it, folder by folder"));
            if resp.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if resp.clicked() {
                drive = Some(mount.clone());
            }
        }
        if let Some(m) = pick {
            (self.metric, self.space) = (m, None);
            let _ = self.save_settings(); // the view comes back as it was left
            self.relayout(now);
        }
        if let Some(d) = drive {
            self.open_space(&d, now, &ui.ctx().clone());
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, r: Rect, now: f64) {
        let mut tu = ui.new_child(UiBuilder::new().max_rect(r).layout(Layout::left_to_right(Align::Center)));
        tu.spacing_mut().item_spacing.x = 4.0;
        if self.space.is_some() {
            self.space_toolbar(&mut tu, now);
            return;
        }
        if link(&mut tu, "All processes", if self.zoom.is_some() { th().muted } else { th().text }, 14.0).clicked() && self.zoom.is_some() {
            self.zoom = None;
            self.relayout(now);
        }
        if let Some(z) = &self.zoom {
            tu.label(egui::RichText::new("\u{E76C}").font(icons(11.0)).color(th().faint));
            tu.label(egui::RichText::new(z.as_str()).font(bold(14.0)).color(th().text));
        }
        let n = if self.zoom.is_some() { self.tiles.iter().filter(|t| t.hit.is_some()).count() } else { self.tiles.iter().filter(|t| t.hit.is_some()).map(|t| &t.name).collect::<HashSet<_>>().len() };
        let by = format!("Sized by {}{}", self.metric_label(), if self.whole().is_some() { " · share of total" } else { "" });
        let count = if self.per_core && self.metric == Field::Cpu {
            let threads: usize = self.cores.iter().map(|c| c.0.len()).sum();
            format!("Per core  ·  {} cores, {threads} threads", self.cores.len())
        } else if self.zoom.is_some() {
            format!("{by}  ·  {n} processes")
        } else {
            format!("{by}  ·  {n} apps · {} processes", self.procs.len())
        };
        tu.add_space(8.0);
        tu.label(egui::RichText::new(count).font(font(12.0)).color(th().faint));
        tu.with_layout(Layout::right_to_left(Align::Center), |tu| {
            tu.spacing_mut().item_spacing.x = 8.0;
            let cores_view = self.per_core && self.metric == Field::Cpu;
            // only the buttons that mean something for this view
            let glyph = if self.table_open { "\u{E70D}" } else { "\u{E70E}" }; // chevron down / up
            if icon_button(tu, glyph, if self.table_open { "Hide the process list" } else { "Show the process list" }, th().muted).clicked() {
                self.table_open = !self.table_open;
                let _ = self.save_settings();
            }
            if !cores_view && let Some(i) = pills(tu, &[("App", !self.heat), ("CPU heat", self.heat)]) {
                self.heat = i == 1;
                let _ = self.save_settings();
            }
            if self.metric == Field::Cpu && pills(tu, &[("Per core", self.per_core)]).is_some() {
                self.per_core = !self.per_core;
                let _ = self.save_settings();
            }
            // a share of the machine only where the machine has a meaningful total for the processes to share
            let shareable = matches!(self.metric, Field::Ws | Field::Private | Field::Gpu) || (self.metric == Field::Cpu && !cores_view);
            if shareable && pills(tu, &[("Share of total", self.relative)]).is_some() {
                self.relative = !self.relative;
                let _ = self.save_settings();
                self.relayout(now);
            }
        });
    }

    /// Disk space map: the folder path as a breadcrumb, the scan's progress, Share of total (free space).
    fn space_toolbar(&mut self, tu: &mut Ui, now: f64) {
        let Some(sv) = &self.space else { return };
        let (chain, status, running) = {
            let g = sv.scan.tree.lock().unwrap();
            let mut chain = vec![sv.at];
            while *chain.last().unwrap() != 0 {
                chain.push(g.nodes[*chain.last().unwrap() as usize].parent);
            }
            chain.reverse();
            let chain: Vec<(u32, String)> = chain.into_iter().map(|id| (id, g.nodes[id as usize].name.to_string())).collect();
            let found = fmt_bytes(g.nodes[0].total as f64);
            let denied = if g.denied > 0 { format!(" · {} folders not readable", g.denied) } else { String::new() };
            let status = match (g.running, g.scanned_at) {
                (true, None) => format!("Scanning… {} folders, {found} of {} used{denied}", g.checked, fmt_bytes(sv.used as f64)),
                (true, Some(_)) => format!("Updating… {} folders checked{denied}", g.checked),
                (false, Some(at)) => {
                    let ago = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()).saturating_sub(at);
                    let ago = match ago {
                        0..120 => "just now".to_string(),
                        120..7200 => format!("{} min ago", ago / 60),
                        7200..172800 => format!("{} h ago", ago / 3600),
                        _ => format!("{} days ago", ago / 86400),
                    };
                    format!("{found} in files · scanned {ago}{denied}")
                }
                (false, None) => String::new(),
            };
            (chain, status, g.running)
        };
        let mut go = None;
        let last = chain.len() - 1;
        for (i, (id, name)) in chain.iter().enumerate() {
            if i > 0 {
                tu.label(egui::RichText::new("\u{E76C}").font(icons(11.0)).color(th().faint));
            }
            if link(tu, name, if i == last { th().text } else { th().muted }, 14.0).clicked() && i != last {
                go = Some(*id);
            }
        }
        tu.add_space(8.0);
        tu.label(egui::RichText::new(status).font(font(12.0)).color(th().faint));
        let mut refresh = false;
        tu.with_layout(Layout::right_to_left(Align::Center), |tu| {
            tu.spacing_mut().item_spacing.x = 8.0;
            let glyph = if self.table_open { "\u{E70D}" } else { "\u{E70E}" };
            if icon_button(tu, glyph, if self.table_open { "Hide the folder list" } else { "Show the folder list" }, th().muted).clicked() {
                self.table_open = !self.table_open;
                let _ = self.save_settings();
            }
            if pills(tu, &[("Share of total", self.relative)]).is_some() {
                self.relative = !self.relative;
                let _ = self.save_settings();
                self.relayout(now);
            }
            // the drive is read from the cache; walking it again happens only when asked
            if !running && pills(tu, &[("\u{21BB}  Refresh", false)]).is_some() {
                refresh = true;
            }
        });
        if refresh && let Some(s) = &self.space {
            let c = tu.ctx().clone();
            s.scan.refresh(move || c.request_repaint());
        }
        if let Some(id) = go {
            if let Some(s) = &mut self.space {
                s.at = id;
            }
            self.sel = None;
            self.relayout(now);
        }
    }

    /// "Per core": the CPU split into its physical cores, each with its threads (logical processors) side by
    /// side, filled by load. On a hybrid CPU each core says whether it is a performance or efficiency core.
    fn draw_cores(&mut self, ui: &Ui, r: Rect) {
        let p = ui.painter().with_clip_rect(r);
        p.rect_filled(r, 10.0, th().tm_bg);
        let Some(t) = self.tot.clone() else { return };
        let on_cpu = split_by_cpu(&self.procs, &t.per_core);
        let mut segs: Vec<(Rect, usize)> = Vec::new(); // for hover and click
        let cores: Vec<(Vec<usize>, u8)> = if self.cores.is_empty() { (0..t.per_core.len()).map(|i| (vec![i], 0)).collect() } else { self.cores.clone() };
        let n = cores.len().max(1);
        let hybrid = cores.iter().any(|c| c.1 != cores[0].1);
        let top_class = cores.iter().map(|c| c.1).max().unwrap_or(0);
        let (w, h) = (r.width() - 8.0, r.height() - 8.0);
        // the grid whose cells are closest to square
        let cols = (1..=n).min_by_key(|&c| ((w / c as f32) / (h / n.div_ceil(c) as f32)).ln().abs().mul_add(1000.0, 0.0) as i64).unwrap_or(1);
        let rows = n.div_ceil(cols);
        let (cw, ch) = (w / cols as f32, h / rows as f32);
        for (i, (threads, class)) in cores.iter().enumerate() {
            let cell = Rect::from_min_size(r.min + vec2(4.0 + (i % cols) as f32 * cw, 4.0 + (i / cols) as f32 * ch), vec2(cw, ch)).shrink(3.0);
            let loads: Vec<f32> = threads.iter().map(|&k| t.per_core.get(k).copied().unwrap_or(0.0)).collect();
            let avg = loads.iter().sum::<f32>() / loads.len().max(1) as f32;
            p.rect_filled(cell, 6.0, mix(ac().cpu, th().bg, 0.86));
            let kind = if hybrid { if *class == top_class { " · P-core" } else { " · E-core" } } else { "" };
            text(&p, cell.min + vec2(8.0, 6.0), Align2::LEFT_TOP, &format!("Core {i}{kind}"), bold(12.0), th().text, cell.width() - 70.0);
            text(&p, pos2(cell.right() - 8.0, cell.top() + 6.0), Align2::RIGHT_TOP, &format!("{avg:.0}%"), bold(12.0), th().text, 60.0);
            let inner = Rect::from_min_max(cell.min + vec2(8.0, 28.0), cell.max - vec2(8.0, 8.0));
            let tn = loads.len().max(1) as f32;
            let tw = (inner.width() - 6.0 * (tn - 1.0)) / tn;
            for (j, (&v, &k)) in loads.iter().zip(threads).enumerate() {
                let tr = Rect::from_min_size(pos2(inner.left() + j as f32 * (tw + 6.0), inner.top()), vec2(tw, inner.height()));
                p.rect_filled(tr, 4.0, th().line);
                let fh = (v / 100.0).clamp(0.0, 1.0) * tr.height();
                if fh > 0.5 {
                    p.rect_filled(Rect::from_min_max(pos2(tr.left(), tr.bottom() - fh), tr.max), 4.0, mix(rgb(0x2B4F7A), ac().cpu, v / 100.0));
                }
                // the fill split into the processes that ran here, biggest at the bottom, scaled to the
                // measured load (the attribution is an estimate, the load is not)
                let mut procs = on_cpu.get(k).cloned().unwrap_or_default();
                procs.sort_by(|a, b| b.1.total_cmp(&a.1));
                let est: f32 = procs.iter().map(|x| x.1).sum();
                let scale = if est > v { v / est } else { 1.0 };
                let mut y = tr.bottom();
                for (pi, pct) in procs {
                    let sh = (pct * scale / 100.0) * tr.height();
                    if sh < 2.0 {
                        continue;
                    }
                    let sr = Rect::from_min_max(pos2(tr.left(), y - sh), pos2(tr.right(), y));
                    let name = &self.procs[pi].name;
                    p.rect_filled(sr.shrink2(vec2(0.0, 0.5)), 3.0, mix(app_color(name), th().bg, 0.15));
                    if sh >= 15.0 {
                        text(&p, pos2(sr.left() + 5.0, sr.center().y), Align2::LEFT_CENTER, name, font(10.0), alpha(Color32::WHITE, 0xE0), tw - 10.0);
                    }
                    segs.push((sr, pi));
                    y -= sh;
                }
                text(&p, tr.min + vec2(6.0, 4.0), Align2::LEFT_TOP, &format!("Thread {}", j + 1), font(10.0), th().muted, tw - 10.0);
                text(&p, tr.min + vec2(6.0, 17.0), Align2::LEFT_TOP, &format!("CPU {k}  ·  {v:.0}%"), font(10.0), th().text, tw - 10.0);
            }
        }
        // hover: which process, how much of this CPU; click selects it
        let resp = ui.interact(r, Id::unique("cores"), Sense::click());
        let Some(pos) = resp.hover_pos() else { return };
        let Some(&(sr, pi)) = segs.iter().find(|s| s.0.contains(pos)) else { return };
        let q = &self.procs[pi];
        p.rect_stroke(sr, 3.0, Stroke::new(1.5, Color32::WHITE), StrokeKind::Inside);
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        if resp.clicked() {
            self.sel = Some(Hit::Pid(q.pid));
        }
        let lines: Vec<String> = on_cpu.iter().enumerate().filter_map(|(c, v)| v.iter().find(|e| e.0 == pi).map(|e| format!("CPU {c}: {:.0}%", e.1))).collect();
        let body = format!(
            "PID {}  ·  {:.1}% of the machine\n{}\nWhere it ran is an estimate (Windows records it only for administrators); the totals are measured.",
            q.pid,
            q.cpu,
            lines.join("  ·  ")
        );
        let g = p.layout(body, font(11.0), th().muted, 296.0);
        let sz = vec2(320.0, g.size().y + 44.0);
        let x = if pos.x + 16.0 + sz.x < r.right() { pos.x + 16.0 } else { pos.x - sz.x - 16.0 };
        let y = if pos.y + 16.0 + sz.y < r.bottom() { pos.y + 16.0 } else { pos.y - sz.y - 8.0 };
        let tr = Rect::from_min_size(pos2(x, y), sz);
        p.rect(tr, 10.0, alpha(th().raised, 0xF5), Stroke::new(1.0, th().line), StrokeKind::Inside);
        text(&p, tr.min + vec2(12.0, 12.0), Align2::LEFT_TOP, &q.name, bold(13.0), th().text, 296.0);
        p.galley(tr.min + vec2(12.0, 34.0), g, th().muted);
    }

    fn treemap(&mut self, ui: &mut Ui, r: Rect, now: f64) {
        if self.per_core && self.metric == Field::Cpu && self.space.is_none() {
            self.draw_cores(ui, r);
            return;
        }
        let p = ui.painter().with_clip_rect(r);
        p.rect_filled(r, 10.0, th().tm_bg);
        if (r.size() - self.tm_size).length() > 0.5 {
            self.tm_size = r.size();
            if !self.procs.is_empty() {
                self.relayout(now);
            }
        }
        let moving = now - self.anim_t0 < TILE_ANIM;
        if moving {
            ui.ctx().request_repaint();
        }
        let o = r.min;
        let mut rects: Vec<Rect> = Vec::with_capacity(self.tiles.len());
        for t in &self.tiles {
            let tr = r4(self.shown_rect(&t.key, now).unwrap_or(t.rect), o);
            rects.push(tr);
            let c = self.tile_color(t);
            let value = self.fmt_metric(t.value);
            if t.header {
                // a group frame: dark, its processes sit on top
                p.rect_filled(tr, 3.0, mix(c, th().bg, 0.55));
                if tr.width() >= 46.0 {
                    text(&p, tr.min + vec2(6.0, 3.0), Align2::LEFT_TOP, &format!("{}   {value}", t.title), bold(11.0), if th().light { th().text } else { alpha(Color32::WHITE, 0xEE) }, tr.width() - 10.0);
                }
            } else {
                p.rect_filled(tr, 3.0, mix(c, Color32::WHITE, 0.05));
                if !t.free {
                    p.line_segment([tr.left_top() + vec2(2.0, 0.5), tr.right_top() + vec2(-2.0, 0.5)], Stroke::new(1.0, mix(c, Color32::WHITE, 0.3)));
                }
                if tr.width() >= 46.0 && tr.height() >= 18.0 {
                    text(&p, tr.min + vec2(6.0, 3.0), Align2::LEFT_TOP, &t.title, bold(11.0), if t.free && th().light { th().muted } else { alpha(Color32::WHITE, 0xEE) }, tr.width() - 10.0);
                }
                if tr.width() >= 46.0 && tr.height() >= 34.0 {
                    text(&p, tr.min + vec2(6.0, 18.0), Align2::LEFT_TOP, &value, font(10.0), if t.free && th().light { th().faint } else { alpha(Color32::WHITE, 0xB0) }, tr.width() - 10.0);
                }
            }
        }
        if self.tiles.is_empty() && self.split_fields().is_none() {
            let net_err = self.tot.as_ref().and_then(|t| t.net_err.clone()).filter(|_| matches!(self.metric, Field::NetDown | Field::NetUp));
            let msg = if self.space.is_some() {
                Some("Scanning…".to_string())
            } else if let Some(e) = net_err {
                Some(e)
            } else if self.metric.is_rate() && !self.procs.is_empty() {
                Some("No process is moving data right now".to_string())
            } else {
                (!self.procs.is_empty()).then(|| "Nothing uses this resource right now".to_string())
            };
            if let Some(m) = msg {
                p.text(r.center(), Align2::CENTER_CENTER, m, font(13.0), th().faint);
            }
        }
        let resp = ui.interact(r, Id::unique("treemap"), Sense::click());
        // children were laid out after their group: test them first; a group answers on its header band only
        let hit_at = |pos: Pos2| -> Option<Hit> {
            self.tiles.iter().zip(&rects).rev().find_map(|(t, tr)| {
                let hr = if t.header { Rect::from_min_size(tr.min, vec2(tr.width(), treemap::HEADER)) } else { *tr };
                (t.hit.is_some() && hr.contains(pos)).then(|| t.hit.clone().unwrap())
            })
        };
        let pos = resp.hover_pos();
        let target = pos.and_then(hit_at);
        // a split view has the same process in both halves: outline it in both
        let rects_of = |h: &Hit| -> Vec<Rect> {
            let k = match h {
                Hit::Pid(p) => Key::Pid(*p),
                Hit::Group(n) => Key::Group(n.clone()),
                Hit::Dir(d) => Key::Dir(*d),
            };
            self.tiles.iter().zip(&rects).filter(|(t, _)| t.key == k || matches!(&t.key, Key::Side(_, b) if **b == k)).map(|(_, r)| *r).collect()
        };
        for sr in self.sel.as_ref().map(rects_of).unwrap_or_default() {
            p.rect_stroke(sr, 4.0, Stroke::new(2.5, Color32::WHITE), StrokeKind::Inside);
        }
        for hr in target.as_ref().filter(|h| Some(*h) != self.sel.as_ref()).map(rects_of).unwrap_or_default() {
            p.rect_stroke(hr, 4.0, Stroke::new(1.5, alpha(Color32::WHITE, 0x90)), StrokeKind::Inside);
        }
        if let Some((a, b)) = self.split_fields() {
            let hw = (r.width() - SPLIT_GAP) / 2.0;
            let labels = if a == Field::IoRead { [(I_READ, "Read", ac().disk), (I_WRITE, "Write", ac().disk2)] } else { [(I_DOWN, "Download", ac().net), (I_UP, "Upload", ac().net2)] };
            for (side, (f, (ic, label, c))) in [a, b].into_iter().zip(labels).enumerate() {
                let x = r.left() + side as f32 * (hw + SPLIT_GAP);
                let cap = Rect::from_min_size(pos2(x, r.top()), vec2(hw, SPLIT_CAPTION));
                let total: f64 = self.procs.iter().map(|q| q.get(f)).sum();
                p.text(pos2(cap.left() + 10.0, cap.center().y), Align2::LEFT_CENTER, ic, icons(14.0), c);
                let tr = text(&p, pos2(cap.left() + 32.0, cap.center().y), Align2::LEFT_CENTER, label, bold(13.0), th().text, 120.0);
                text(&p, pos2(tr.right() + 10.0, cap.center().y), Align2::LEFT_CENTER, &fmt_rate(total), font(12.0), th().muted, 140.0);
                let empty = !self.tiles.iter().any(|t| matches!(&t.key, Key::Side(s, _) if *s as usize == side));
                if empty && !self.procs.is_empty() {
                    let body = Rect::from_min_max(pos2(x, cap.bottom()), pos2(x + hw, r.bottom()));
                    let net_err = self.tot.as_ref().and_then(|t| t.net_err.clone()).filter(|_| a == Field::NetDown);
                    p.text(body.center(), Align2::CENTER_CENTER, net_err.unwrap_or_else(|| "Nothing right now".into()), font(12.0), th().faint);
                }
            }
        }
        if target.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if resp.clicked() {
            self.sel = target.clone();
        }
        if resp.double_clicked()
            && let Some(Hit::Dir(id)) = &target
        {
            self.space_enter(*id, now);
            return;
        }
        if resp.double_clicked() && self.zoom.is_none() {
            let name = match &target {
                Some(Hit::Group(n)) => Some(n.clone()),
                Some(Hit::Pid(pid)) => self.by_pid.get(pid).map(|&i| self.procs[i].name.clone()),
                _ => None,
            };
            if let Some(n) = name.filter(|n| self.groups.get(n).is_some_and(|g| g.len() > 1)) {
                self.zoom = Some(n);
                if matches!(self.sel, Some(Hit::Group(_))) {
                    self.sel = None;
                }
                self.relayout(now);
                return;
            }
        }
        self.hover = target.clone();
        if let (Some(h), Some(pos)) = (target, pos) {
            let (title, body) = self.describe(&h);
            let p = ui.painter().with_clip_rect(r);
            let bg = p.layout(body, font(11.0), th().muted, 296.0);
            let sz = vec2(320.0, bg.size().y + 44.0);
            let x = if pos.x + 16.0 + sz.x < r.right() { pos.x + 16.0 } else { (pos.x - sz.x - 16.0).max(r.left()) };
            let y = if pos.y + 16.0 + sz.y < r.bottom() { pos.y + 16.0 } else { (pos.y - sz.y - 8.0).max(r.top()) };
            let tr = Rect::from_min_size(pos2(x, y), sz);
            p.add(egui::epaint::Shadow { offset: [0, 6], blur: 18, spread: 0, color: alpha(Color32::BLACK, 0x90) }.as_shape(tr, 10.0));
            p.rect(tr, 10.0, alpha(th().raised, 0xF5), Stroke::new(1.0, th().line), StrokeKind::Inside);
            text(&p, tr.min + vec2(12.0, 12.0), Align2::LEFT_TOP, &title, bold(13.0), th().text, 296.0);
            p.galley(tr.min + vec2(12.0, 34.0), bg, th().muted);
        }
    }

    fn space_selbar(&mut self, ui: &mut Ui, r: Rect, now: f64) {
        let Some(sv) = &self.space else { return };
        let sel = match self.sel {
            Some(Hit::Dir(d)) => Some(d),
            _ => None,
        };
        let (msg, path, kids) = {
            let g = sv.scan.tree.lock().unwrap();
            match sel {
                Some(d) => {
                    let n = &g.nodes[d as usize];
                    (format!("{}   {}   ·   {}", n.name, fmt_bytes(n.total as f64), g.path(d)), Some(g.path(d)), !n.children.is_empty())
                }
                None => ("Click a folder to select it, double-click to open it. Click a card above to go back to processes.".to_string(), None, false),
            }
        };
        let c = if path.is_some() { th().text } else { th().muted };
        text(ui.painter(), pos2(r.left() + 12.0, r.center().y), Align2::LEFT_CENTER, &msg, font(12.0), c, r.width() - 24.0 - if path.is_some() { 330.0 } else { 0.0 });
        let (Some(path), Some(d)) = (path, sel) else { return };
        let mut au = ui.new_child(UiBuilder::new().max_rect(r.shrink2(vec2(6.0, 0.0))).layout(Layout::right_to_left(Align::Center)));
        if link(&mut au, "Show in Explorer", th().muted, 12.0).clicked() {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("explorer").raw_arg(format!("\"{path}\"")).spawn();
        }
        if link(&mut au, "Copy path", th().muted, 12.0).clicked() {
            ui.ctx().copy_text(path);
            self.toast("Copied path", false, now);
        }
        if kids && link(&mut au, "Open", th().muted, 12.0).clicked() {
            self.space_enter(d, now);
        }
    }

    /// Disk space map: the folders shown, biggest first.
    fn space_table(&mut self, ui: &mut Ui, r: Rect, now: f64) {
        let Some(sv) = &self.space else { return };
        let rows: Vec<(u32, String, u64, u64, usize)> = {
            let g = sv.scan.tree.lock().unwrap();
            let mut v: Vec<_> = g.nodes[sv.at as usize]
                .children
                .iter()
                .map(|&c| {
                    let n = &g.nodes[c as usize];
                    (c, n.name.to_string(), n.total, n.files, n.children.len())
                })
                .collect();
            v.sort_by(|a, b| b.2.cmp(&a.2));
            v
        };
        let parent: u64 = rows.iter().map(|r| r.2).sum::<u64>().max(1);
        let mut tu = ui.new_child(UiBuilder::new().max_rect(r).layout(Layout::top_down(Align::Min)));
        let cols: [(&str, f32, bool); 5] = [("Folder", 300.0, false), ("Size", 100.0, true), ("Share", 80.0, true), ("Files here", 100.0, true), ("Subfolders", 100.0, true)];
        let (hr, _) = tu.allocate_exact_size(vec2(r.width(), 24.0), Sense::hover());
        let mut x = r.left();
        for (label, w, num) in cols {
            let cr = Rect::from_min_size(pos2(x, hr.top()), vec2(w, 22.0));
            let (pos, al) = if num { (pos2(cr.right() - 6.0, cr.center().y), Align2::RIGHT_CENTER) } else { (pos2(cr.left() + 6.0, cr.center().y), Align2::LEFT_CENTER) };
            text(tu.painter(), pos, al, label, bold(11.0), th().muted, w - 12.0);
            x += w;
        }
        tu.painter().line_segment([pos2(r.left(), hr.bottom()), pos2(r.right(), hr.bottom())], Stroke::new(1.0, th().line));
        let sel = match self.sel {
            Some(Hit::Dir(d)) => Some(d),
            _ => None,
        };
        let (mut clicked, mut open) = (None, None);
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(&mut tu, 26.0, rows.len(), |ui, range| {
            for (id, name, total, files, kids) in &rows[range] {
                let (rr, resp) = ui.allocate_exact_size(vec2(r.width(), 26.0), Sense::click());
                if sel == Some(*id) {
                    ui.painter().rect_filled(rr, 6.0, th().row_sel);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rr, 6.0, alpha(th().text, 10));
                }
                if resp.clicked() {
                    clicked = Some(*id);
                }
                if resp.double_clicked() {
                    open = Some(*id);
                }
                let vals = [name.clone(), fmt_bytes(*total as f64), format!("{:.1}%", *total as f64 / parent as f64 * 100.0), fmt_bytes(*files as f64), kids.to_string()];
                let mut x = rr.left();
                for (c, ((_, w, num), v)) in cols.iter().zip(vals).enumerate() {
                    let (pos, al) = if *num { (pos2(x + w - 6.0, rr.center().y), Align2::RIGHT_CENTER) } else { (pos2(x + 6.0, rr.center().y), Align2::LEFT_CENTER) };
                    text(ui.painter(), pos, al, &v, font(12.0), if c == 0 { th().text } else { th().muted }, w - 12.0);
                    x += w;
                }
            }
        });
        if let Some(id) = clicked {
            self.sel = Some(Hit::Dir(id));
        }
        if let Some(id) = open {
            self.space_enter(id, now);
        }
    }

    fn selbar(&mut self, ui: &mut Ui, r: Rect, now: f64) {
        ui.painter().rect_filled(r, 10.0, th().card);
        if self.space.is_some() {
            self.space_selbar(ui, r, now);
            return;
        }
        let qs = self.selected();
        let k = self.cpu_scale();
        let (msg, c) = match (qs.first(), &self.sel) {
            (None, _) => ("Click a tile or a row to select it. Double-click an app to see its processes. Click a card above to size the treemap by it.".to_string(), th().muted),
            (Some(q), Some(Hit::Pid(_))) => (format!("{}   PID {}   ·   {}   ·   CPU {:.1}%", q.name, q.pid, fmt_bytes(q.ws), q.cpu * k), th().text),
            (Some(q), _) => (
                format!("{}   {} processes   ·   {}   ·   CPU {:.1}%", q.name, qs.len(), fmt_bytes(qs.iter().map(|q| q.ws).sum()), qs.iter().map(|q| q.cpu).sum::<f64>() * k),
                th().text,
            ),
        };
        let pids: Vec<u32> = qs.iter().map(|q| q.pid).collect();
        let first = qs.first().map(|q| (q.pid, q.name.clone(), q.cmd.clone()));
        drop(qs);
        let actions_w = if first.is_some() { 470.0 } else { 0.0 };
        text(ui.painter(), pos2(r.left() + 12.0, r.center().y), Align2::LEFT_CENTER, &msg, font(12.0), c, r.width() - 24.0 - actions_w);
        let Some((pid, name, cmd)) = first else { return };
        let mut au = ui.new_child(UiBuilder::new().max_rect(r.shrink2(vec2(6.0, 0.0))).layout(Layout::right_to_left(Align::Center)));
        if link(&mut au, "End task", DANGER, 12.0).clicked() {
            let what = if pids.len() == 1 { format!("{name} (PID {pid})") } else { format!("all {} {name} processes", pids.len()) };
            let me = pids.contains(&std::process::id());
            self.kill_ask = Some(KillAsk { pids: pids.clone(), what, me, tree: false });
        }
        au.menu_button(egui::RichText::new("Priority").font(font(12.0)).color(th().muted), |ui| {
            for (label, class) in sys::PRIORITIES {
                if ui.button(label).clicked() {
                    let ok = pids.iter().filter(|&&p| sys::set_priority(p, class)).count();
                    let denied = pids.len() - ok;
                    if denied > 0 {
                        self.toast(format!("Priority set on {ok}, access denied on {denied} (run as administrator)"), true, now);
                    } else {
                        self.toast(format!("Priority set to {label} on {ok} process{}", if ok != 1 { "es" } else { "" }), false, now);
                    }
                    ui.close();
                }
            }
        });
        if link(&mut au, "Open location", th().muted, 12.0).clicked() {
            self.open_location(pid, now);
        }
        if link(&mut au, "Copy command line", th().muted, 12.0).clicked() {
            if cmd.is_empty() {
                self.toast("Command line not accessible (needs administrator)", true, now);
            } else {
                ui.ctx().copy_text(cmd);
                self.toast("Copied command line", false, now);
            }
        }
        if link(&mut au, "Copy PID", th().muted, 12.0).clicked() {
            ui.ctx().copy_text(pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "));
            self.toast(if pids.len() == 1 { "Copied PID".into() } else { format!("Copied {} PIDs", pids.len()) }, false, now);
        }
    }

    fn table(&mut self, ui: &mut Ui, r: Rect, now: f64) {
        if self.space.is_some() {
            self.space_table(ui, r, now);
            return;
        }
        let mut tu = ui.new_child(UiBuilder::new().max_rect(r).layout(Layout::top_down(Align::Min)));
        tu.horizontal(|ui| {
            let fr = egui::Frame::new().fill(th().card).corner_radius(10.0).inner_margin(egui::Margin::symmetric(10, 8));
            fr.show(ui, |ui| {
                let te = egui::TextEdit::singleline(&mut self.query).hint_text("Filter by name, PID or command line").desired_width(340.0).frame(egui::Frame::NONE).font(font(13.0));
                if ui.add(te).changed() {
                    self.refresh_rows();
                }
            });
            ui.add_space(8.0);
            ui.label(egui::RichText::new(format!("{} shown", self.rows.len())).font(font(12.0)).color(th().faint));
        });
        tu.add_space(6.0);
        let fixed: f32 = COLS.iter().map(|c| c.2).sum();
        let x0 = r.left();
        let col_x = |i: usize| x0 + COLS[..i].iter().map(|c| c.2).sum::<f32>();
        let cmd_w = (r.width() - fixed).max(80.0);
        // header
        let (hr, _) = tu.allocate_exact_size(vec2(r.width(), 24.0), Sense::hover());
        for (i, (col, label, w, num)) in COLS.iter().enumerate() {
            let w = if *w == 0.0 { cmd_w } else { *w };
            let cr = Rect::from_min_size(pos2(col_x(i), hr.top()), vec2(w, 22.0));
            let resp = tu.interact(cr, Id::unique(("head", i)), Sense::click());
            let on = self.sort.0 == *col;
            let mut l = label.to_string();
            if on {
                l += if self.sort.1 { " ↓" } else { " ↑" };
            }
            let (pos, al) = if *num { (pos2(cr.right() - 6.0, cr.center().y), Align2::RIGHT_CENTER) } else { (pos2(cr.left() + 6.0, cr.center().y), Align2::LEFT_CENTER) };
            text(tu.painter(), pos, al, &l, bold(11.0), if on || resp.hovered() { th().text } else { th().muted }, w - 12.0);
            if resp.hovered() {
                tu.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if resp.clicked() {
                self.sort = (*col, if on { !self.sort.1 } else { !matches!(col, Col::Name | Col::Cmd) });
                self.refresh_rows();
            }
        }
        tu.painter().line_segment([pos2(r.left(), hr.bottom()), pos2(r.right(), hr.bottom())], Stroke::new(1.0, th().line));
        let sel: HashSet<u32> = self.selected().iter().map(|q| q.pid).collect();
        let pdh = self.tot.as_ref().is_some_and(|t| t.pdh_ok);
        let k = self.cpu_scale();
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(&mut tu, 26.0, self.rows.len(), |ui, range| {
            for &i in &self.rows[range] {
                let q = &self.procs[i];
                let (rr, resp) = ui.allocate_exact_size(vec2(r.width(), 26.0), Sense::click());
                if sel.contains(&q.pid) {
                    ui.painter().rect_filled(rr, 6.0, th().row_sel);
                } else if resp.hovered() {
                    ui.painter().rect_filled(rr, 6.0, alpha(th().text, 10));
                }
                if resp.clicked() {
                    clicked = Some(q.pid);
                }
                let vals = [
                    q.name.clone(),
                    q.pid.to_string(),
                    format!("{:.1}", q.cpu * k),
                    fmt_bytes(q.ws),
                    fmt_bytes(q.private),
                    if q.gpu_mem > 0.0 { fmt_bytes(q.gpu_mem) } else if pdh { "-".into() } else { "n/a".into() },
                    if q.gpu >= 0.5 { format!("{:.0}%", q.gpu) } else { "-".into() },
                    if q.io_r >= 102400.0 { fmt_rate(q.io_r) } else { "-".into() },
                    if q.io_w >= 102400.0 { fmt_rate(q.io_w) } else { "-".into() },
                    if q.net_d >= 1024.0 { fmt_rate(q.net_d) } else { "-".into() },
                    if q.net_u >= 1024.0 { fmt_rate(q.net_u) } else { "-".into() },
                    q.threads.to_string(),
                    sys::fmt_started(q.create),
                    q.cmd.clone(),
                ];
                for (c, ((_, _, w, num), v)) in COLS.iter().zip(vals).enumerate() {
                    let w = if *w == 0.0 { cmd_w } else { *w };
                    let x = rr.left() + COLS[..c].iter().map(|c| c.2).sum::<f32>();
                    let (pos, al) = if *num { (pos2(x + w - 6.0, rr.center().y), Align2::RIGHT_CENTER) } else { (pos2(x + 6.0, rr.center().y), Align2::LEFT_CENTER) };
                    text(ui.painter(), pos, al, &v, font(12.0), if c == 0 { th().text } else { th().muted }, w - 12.0);
                }
            }
        });
        if let Some(pid) = clicked {
            self.sel = Some(Hit::Pid(pid));
        }
    }

    /// Settings: which cards, what the docked tab shows, theme and colours, timing, start-up.
    fn settings_sheet(&mut self, ui: &mut Ui, r: Rect, now: f64) {
        ui.interact(r, Id::unique("sheet-bg"), Sense::click()); // the map underneath gets no clicks
        let p = ui.painter().clone();
        p.add(egui::epaint::Shadow { offset: [0, 8], blur: 24, spread: 0, color: alpha(Color32::BLACK, if th().light { 0x40 } else { 0xA0 }) }.as_shape(r, 14.0));
        p.rect(r, 14.0, th().card, Stroke::new(1.0, th().sel_line), StrokeKind::Inside);
        let mut su = ui.new_child(UiBuilder::new().max_rect(r.shrink(18.0)).layout(Layout::top_down(Align::Min)));
        let mut changed = false;
        let mut retheme = false;
        su.horizontal(|ui| {
            ui.label(egui::RichText::new("Settings").font(bold(16.0)).color(th().text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, "\u{E8BB}", "Close settings", th().muted).clicked() {
                    self.settings_open = false;
                }
            });
        });
        let section = |ui: &mut Ui, t: &str| {
            ui.add_space(12.0);
            ui.label(egui::RichText::new(t).font(bold(10.0)).color(th().muted));
            ui.add_space(4.0);
        };
        egui::ScrollArea::vertical().auto_shrink(false).show(&mut su, |ui| {
            section(ui, "CARDS");
            ui.horizontal_wrapped(|ui| {
                for (key, label) in settings::CARDS {
                    let mut on = self.cfg.shows(key);
                    if ui.checkbox(&mut on, label).changed() {
                        self.cfg.hidden.retain(|h| h != key);
                        if !on {
                            self.cfg.hidden.push(key.into());
                        }
                        changed = true;
                    }
                }
            });
            section(ui, "DOCKED TAB  ·  bars, up to four");
            ui.horizontal_wrapped(|ui| {
                for (key, label) in settings::TAB_BARS {
                    let mut on = self.cfg.tab.iter().any(|t| t == key);
                    let can = on || self.cfg.tab.len() < 4;
                    if ui.add_enabled(can, egui::Checkbox::new(&mut on, label)).changed() {
                        if on {
                            self.cfg.tab.push(key.into());
                        } else if self.cfg.tab.len() > 1 {
                            self.cfg.tab.retain(|t| t != key);
                        }
                        changed = true;
                    }
                }
            });
            section(ui, "THEME");
            let names: Vec<&str> = theme::THEMES.iter().map(|t| t.name).collect();
            let items: Vec<(&str, bool)> = names.iter().map(|n| (*n, *n == self.cfg.theme)).collect();
            if let Some(i) = pills(ui, &items) {
                self.cfg.theme = names[i].into();
                (changed, retheme) = (true, true);
            }
            section(ui, "COLOURS");
            let names: Vec<&str> = theme::ACCENTS.iter().map(|a| a.name).collect();
            let items: Vec<(&str, bool)> = names.iter().map(|n| (*n, *n == self.cfg.accents)).collect();
            if let Some(i) = pills(ui, &items) {
                self.cfg.accents = names[i].into();
                (changed, retheme) = (true, true);
            }
            section(ui, "BEHAVIOUR");
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Panel opens after").color(th().text));
                if ui.add(egui::Slider::new(&mut self.cfg.hover, 0.2..=2.0).suffix(" s").step_by(0.1)).changed() {
                    changed = true;
                }
            });
            if ui.checkbox(&mut self.table_open, "Show the process list under the map").changed() {
                changed = true;
            }
            let mut st = self.startup;
            if ui.checkbox(&mut st, "Start with Windows").changed() {
                self.toggle_startup(now);
            }
            ui.add_space(16.0);
            if ui.button("Quit ResMon").clicked() {
                self.quit = true;
            }
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Saved as you change them, in %APPDATA%\\ResMon\\settings.txt").font(font(11.0)).color(th().faint));
        });
        if retheme {
            theme::set(&self.cfg.theme, &self.cfg.accents);
            apply_visuals(ui.ctx());
        }
        if changed {
            let _ = self.save_settings();
            self.relayout(now);
        }
    }

    fn kill_dialog(&mut self, ctx: &egui::Context, now: f64) {
        let Some(ask) = &mut self.kill_ask else { return };
        let mut done: Option<bool> = None;
        egui::Modal::new(Id::unique("kill")).frame(egui::Frame::new().fill(th().card).corner_radius(14.0).inner_margin(22.0).stroke(Stroke::new(1.0, th().line))).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(egui::RichText::new("End task?").font(bold(18.0)).color(th().text));
            ui.add_space(10.0);
            let warn = if ask.me { "  This is ResMon itself." } else { "" };
            ui.label(egui::RichText::new(format!("End {}? Unsaved data in it will be lost.{warn}", ask.what)).font(font(13.0)).color(th().muted));
            ui.add_space(10.0);
            ui.checkbox(&mut ask.tree, "Also end all child processes (process tree)");
            ui.add_space(14.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(egui::Button::new(egui::RichText::new("End task").color(Color32::WHITE)).fill(DANGER)).clicked() {
                    done = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    done = Some(false);
                }
            });
        });
        if let Some(go) = done {
            let ask = self.kill_ask.take().unwrap();
            if go {
                self.kill(&ask.pids, ask.tree, now);
            }
        }
    }
}

impl App {
    /// Window bookkeeping before the UI pass (shell.rs calls both every frame).
    fn logic(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if !self.shown && self.hwnd.is_some() {
            // placed natively in physical px, then shown: no flash at the default position
            let b = self.geometry(Mode::Tab);
            self.set_bounds(b, now);
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            self.shown = true;
            // for screenshots and checks: RESMON_OPEN=panel|full opens that face at start
            match std::env::var("RESMON_OPEN").as_deref() {
                Ok("panel") => self.go(Mode::Panel, now),
                Ok("full") => self.go(Mode::Full, now),
                _ => {}
            }
        }
        if let Some(h) = self.hwnd
            && !self.drag
            && (self.mode != Mode::Full || now < self.enforce_until)
            && let Some(wr) = sys::window_rect(h)
        {
            let b = self.bounds;
            if [wr.0 - b.0, wr.1 - b.1, wr.2 - b.2, wr.3 - b.3].iter().any(|d| d.abs() > 1) {
                sys::place(h, b);
            }
        }
        if self.mode == Mode::Full
            && self.trans.is_none()
            && now >= self.enforce_until
            && let Some(wr) = self.hwnd.and_then(sys::window_rect)
        {
            self.bounds = wr; // the user may have dragged the full view around
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.quit {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose); // Alt+F4 docks, Quit quits
            self.go(Mode::Tab, now);
        }
        if self.mode == Mode::Full && self.kill_ask.is_none() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.go(Mode::Tab, now);
        }
        if self.quit {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn ui(&mut self, ui: &mut Ui) {
        let now = ui.ctx().input(|i| i.time);
        self.pull(now);
        let win = ui.max_rect();
        match self.mode {
            Mode::Tab => self.draw_tab(ui, win, now),
            Mode::Panel => self.draw_panel(ui, win, now),
            Mode::Full => self.draw_full(ui, win, now),
        }
        self.finish_trans(ui.ctx(), now);
        self.push_shared();
    }
}

fn setup_style(ctx: &egui::Context) {
    let dir = std::path::PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into())).join("Fonts");
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.clear();
    fonts.families.clear();
    let mut load = |name: &str, files: &[&str]| -> bool {
        for f in files {
            if let Ok(b) = std::fs::read(dir.join(f)) {
                fonts.font_data.insert(name.into(), Arc::new(egui::FontData::from_owned(b)));
                return true;
            }
        }
        false
    };
    load("ui", &["segoeui.ttf"]);
    load("semi", &["seguisb.ttf", "segoeuib.ttf"]);
    load("icons", &["SegoeIcons.ttf", "segmdl2.ttf"]);
    load("sym", &["seguisym.ttf"]);
    let fam = |v: &[&str]| v.iter().map(|s| s.to_string()).filter(|s| fonts.font_data.contains_key(s)).collect::<Vec<_>>();
    let (ui, semi, ic) = (fam(&["ui", "sym"]), fam(&["semi", "ui", "sym"]), fam(&["icons", "ui"]));
    fonts.families.insert(FontFamily::Proportional, ui.clone());
    fonts.families.insert(FontFamily::Monospace, ui);
    fonts.families.insert(FontFamily::Name("semi".into()), semi);
    fonts.families.insert(FontFamily::Name("icons".into()), ic);
    ctx.set_fonts(fonts);
    apply_visuals(ctx);
}

/// egui's own widgets (menus, checkboxes, text fields) in the current theme.
fn apply_visuals(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        let v = &mut s.visuals;
        *v = if th().light { egui::Visuals::light() } else { egui::Visuals::dark() };
        v.panel_fill = Color32::TRANSPARENT;
        v.window_fill = th().card;
        v.window_stroke = Stroke::new(1.0, th().line);
        v.extreme_bg_color = th().card;
        v.selection.bg_fill = if th().light { rgb(0xB9CCEA) } else { rgb(0x2E4A7A) };
        v.override_text_color = None;
        v.widgets.noninteractive.fg_stroke.color = th().muted;
        v.widgets.inactive.fg_stroke.color = th().text;
        v.widgets.inactive.weak_bg_fill = th().pill_on;
        v.widgets.hovered.weak_bg_fill = mix(th().pill_on, th().text, 0.12);
        v.menu_corner_radius = 10.into();
        s.text_styles.insert(egui::TextStyle::Body, font(13.0));
        s.text_styles.insert(egui::TextStyle::Button, font(13.0));
        s.spacing.button_padding = vec2(10.0, 5.0);
        s.interaction.tooltip_delay = 0.4;
    });
}

fn main() {
    shell::run();
    net::stop(); // a real-time trace session would outlive us
}
