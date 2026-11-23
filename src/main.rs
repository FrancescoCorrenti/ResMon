//! ResMon: a resource-monitor widget docked on the right screen edge (egui).
//!
//! One window morphs between three faces: tab (at rest) -> panel (hover) -> full view (click).
//! A sampler thread reads machine totals every 0.5 s and processes every 1.5 s (only while the
//! full view is open); a watcher thread polls the cursor for the tab <-> panel hover. The UI
//! repaints only when one of them asks, or while something is moving.
#![windows_subsystem = "windows"]

mod raster;
mod shell;
mod sys;
mod treemap;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId, Id, Layout, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, Vec2, ViewportCommand, pos2,
    vec2,
};
use sys::{Field, History, Proc, Totals};
use treemap::{Hit, Key, Sticky, Tile};
use windows::Win32::Foundation::HWND;

const REFLOW: f64 = 8.0; // seconds between two treemap reflows
const TAB: Vec2 = vec2(26.0, 168.0); // docked tab (logical px)
const PANEL: Vec2 = vec2(264.0, 790.0); // hover panel
const FULL: Vec2 = vec2(1440.0, 900.0); // full view (clamped to the work area)
const HOVER_OPEN: f32 = 2.0; // seconds resting on the tab before the panel opens (time to grab and drag it)
const UI_ANIM: f64 = 0.28; // shell transitions
const FADE: f64 = 0.22; // full view content fade
const TILE_ANIM: f64 = 0.55; // treemap tiles glide

// monochrome shell; colour is kept for data only (sparklines, bars, tiles)
const BG: Color32 = Color32::from_rgb(0x0B, 0x0D, 0x11);
const CARD: Color32 = Color32::from_rgb(0x13, 0x16, 0x1C);
const LINE: Color32 = Color32::from_rgb(0x1F, 0x23, 0x2C);
const SEL_BG: Color32 = Color32::from_rgb(0x1A, 0x1F, 0x29);
const SEL_LINE: Color32 = Color32::from_rgb(0x4A, 0x54, 0x68);
const TEXT: Color32 = Color32::from_rgb(0xE6, 0xE8, 0xEE);
const MUTED: Color32 = Color32::from_rgb(0x8B, 0x91, 0xA0);
const FAINT: Color32 = Color32::from_rgb(0x56, 0x60, 0x70);
const PILL_ON: Color32 = Color32::from_rgb(0x2A, 0x30, 0x40);
const C_CPU: Color32 = Color32::from_rgb(0x6C, 0xB6, 0xF5);
const C_RAM: Color32 = Color32::from_rgb(0xA9, 0x93, 0xE8);
const C_COMMIT: Color32 = Color32::from_rgb(0x8A, 0x80, 0xC8);
const C_GPU: Color32 = Color32::from_rgb(0x5C, 0xCB, 0x98);
const C_VRAM: Color32 = Color32::from_rgb(0xE0, 0xBC, 0x62);
const C_DISK: Color32 = Color32::from_rgb(0xE8, 0x95, 0x7A);
const C_DISK2: Color32 = Color32::from_rgb(0xF2, 0xC7, 0xB6);
const C_NET: Color32 = Color32::from_rgb(0x62, 0xC6, 0xD4);
const C_NET2: Color32 = Color32::from_rgb(0xB6, 0xE6, 0xEC);
const DANGER: Color32 = Color32::from_rgb(0xF0, 0x62, 0x72);
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
    // hover watcher
    mode: Mode,
    bounds: (i32, i32, i32, i32),
    busy: bool,
    hover_fire: bool,
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
                (g.want_procs, std::mem::take(&mut g.procs_now))
            };
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
    // Cursor polling with hysteresis: rest on the tab for HOVER_OPEN -> panel; leave the panel for 0.4 s -> tab.
    // Polling (not hover events) because a frameless window that resizes under the cursor misses exits.
    std::thread::spawn(move || {
        if std::env::var_os("RESMON_OPEN").is_some() {
            return; // a face opened for a check stays open
        }
        let mut since: Option<Instant> = None;
        loop {
            std::thread::sleep(Duration::from_millis(80));
            let (mode, (l, t, w, h), busy) = {
                let g = sh.lock().unwrap();
                (g.mode, g.bounds, g.busy || g.hover_fire)
            };
            if busy || mode == Mode::Full {
                since = None;
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
            let need = if mode == Mode::Tab { HOVER_OPEN } else { 0.4 };
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
    Io,
    Threads,
    Create,
    Cmd,
}

const COLS: [(Col, &str, f32, bool); 11] = [
    (Col::Name, "Name", 210.0, false),
    (Col::Pid, "PID", 70.0, true),
    (Col::Cpu, "CPU", 64.0, true),
    (Col::Ws, "RAM", 82.0, true),
    (Col::Private, "Private", 82.0, true),
    (Col::GpuMem, "VRAM", 82.0, true),
    (Col::Gpu, "GPU", 60.0, true),
    (Col::Io, "Disk I/O", 90.0, true),
    (Col::Threads, "Threads", 64.0, true),
    (Col::Create, "Started", 110.0, true),
    (Col::Cmd, "Command line", 0.0, false),
];

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
    last_reflow: f64,
    tm_size: Vec2,
    // table
    sort: (Col, bool),
    query: String,
    rows: Vec<usize>,
    // misc
    toast: Option<(String, bool, f64)>,
    kill_ask: Option<KillAsk>,
    startup: bool,
    users: HashMap<u32, String>,
    quit: bool,
}

impl App {
    fn new(ctx: &egui::Context, hwnd: HWND) -> App {
        setup_style(ctx);
        let anchor = sys::load_anchor().unwrap_or_else(|| {
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
            mode: Mode::Tab,
            bounds: (0, 0, 0, 0),
            busy: true,
            hover_fire: false,
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
            metric: Field::Ws,
            heat: false,
            relative: false,
            per_core: false,
            zoom: None,
            sel: None,
            hover: None,
            tiles: Vec::new(),
            anim: HashMap::new(),
            anim_t0: 0.0,
            sticky: Sticky::new(0.15),
            last_reflow: -1e9,
            tm_size: Vec2::ZERO,
            sort: (Col::Ws, true),
            query: String::new(),
            rows: Vec::new(),
            toast: None,
            kill_ask: None,
            startup: sys::startup_enabled(),
            users: HashMap::new(),
            quit: false,
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
                let v = if mode == Mode::Tab { TAB } else { PANEL };
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
            Mode::Tab => TAB,
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
            None => false,
        };
        if gone {
            self.sel = None;
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
        let whole = self.whole();
        treemap::model(&self.procs, self.metric, self.zoom.as_deref(), self.tm_size.x, self.tm_size.y, &mut self.sticky, whole)
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
            Field::Io => None,
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
            let old: HashMap<&Key, &Tile> = self.tiles.iter().map(|t| (&t.key, t)).collect();
            m = m
                .into_iter()
                .filter_map(|mut t| {
                    let o = old.get(&t.key)?;
                    (t.rect, t.header) = (o.rect, o.header);
                    Some(t)
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
        self.sticky = Sticky::new(0.15);
        let m = self.layout();
        self.apply(m, true, now);
        self.refresh_rows();
    }

    fn cpu_scale(&self) -> f64 {
        if self.per_core { self.tot.as_ref().map_or(1, |t| t.per_core.len()) as f64 } else { 1.0 }
    }

    fn fmt_metric(&self, v: f64) -> String {
        match self.metric {
            Field::Cpu => format!("{:.1}%", v * self.cpu_scale()),
            Field::Gpu => format!("{v:.1}%"),
            Field::Io => fmt_rate(v),
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
            Field::Io => "disk I/O",
        }
    }

    fn tile_color(&self, t: &Tile) -> Color32 {
        if t.free {
            return if t.name == "Other" { rgb(0x1C2029) } else { rgb(0x15181F) };
        }
        if self.heat { heat_color(t.cpu) } else { mix(app_color(&t.name), BG, 0.25) }
    }

    fn selected(&self) -> Vec<&Proc> {
        match &self.sel {
            Some(Hit::Pid(p)) => self.by_pid.get(p).map(|&i| vec![&self.procs[i]]).unwrap_or_default(),
            Some(Hit::Group(n)) => self.groups.get(n).map(|v| v.iter().map(|&i| &self.procs[i]).collect()).unwrap_or_default(),
            None => Vec::new(),
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
                Col::Io => q.io,
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
                    "PID {}  ·  parent {}  ·  {}\nRAM {}  ·  private {}\nCPU {:.1}%  ·  {gm}  ·  {} threads\nDisk I/O {}\n{cmd}",
                    q.pid,
                    q.ppid,
                    self.username(q.pid),
                    fmt_bytes(q.ws),
                    fmt_bytes(q.private),
                    q.cpu * k,
                    q.threads,
                    fmt_rate(q.io)
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
                    "{} processes  ·  PIDs {}\nRAM {}  ·  private {}\nCPU {:.1}%  ·  {} VRAM  ·  {:.0}% GPU\nDisk I/O {}\nDouble-click to show its processes",
                    qs.len(),
                    pids.join(", "),
                    fmt_bytes(sum(|q| q.ws)),
                    fmt_bytes(sum(|q| q.private)),
                    sum(|q| q.cpu) * k,
                    fmt_bytes(sum(|q| q.gpu_mem)),
                    qs.iter().map(|q| q.gpu).fold(0.0, f64::max),
                    fmt_rate(sum(|q| q.io))
                );
                (name.clone(), body)
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

    fn toggle_startup(&mut self, now: f64) {
        if let Err(e) = sys::set_startup(!self.startup) {
            self.toast(format!("Cannot change start with Windows: {e}"), true, now);
        }
        self.startup = sys::startup_enabled();
    }
}

// ------------------------------------------------------------------ drawing

struct Gauge<'a> {
    title: String,
    value: String,
    sub: String,
    cores: Option<&'a [f32]>,
    s1: (&'a VecDeque<f32>, Color32),
    s2: Option<(&'a VecDeque<f32>, Color32)>,
    top: f32,
}

fn card(p: &egui::Painter, r: Rect, selected: bool) {
    p.rect(r, 12.0, if selected { SEL_BG } else { CARD }, Stroke::new(1.0, if selected { SEL_LINE } else { CARD }), StrokeKind::Inside);
}

fn draw_gauge(ui: &Ui, r: Rect, g: &Gauge, small: bool, selected: bool) {
    let p = ui.painter().with_clip_rect(r);
    card(&p, r, selected);
    let inner = Rect::from_min_max(r.min + vec2(12.0, if small { 8.0 } else { 10.0 }), r.max - vec2(12.0, if small { 6.0 } else { 8.0 }));
    let w = inner.width();
    let tc = if selected { TEXT } else { MUTED };
    let mut y = inner.top();
    if small {
        text(&p, pos2(inner.left(), y + 9.0), Align2::LEFT_CENTER, &g.title, bold(10.0), tc, w * 0.5);
        text(&p, pos2(inner.right(), y + 9.0), Align2::RIGHT_CENTER, &g.value, bold(15.0), TEXT, w * 0.6);
        y += 21.0;
    } else {
        text(&p, pos2(inner.left(), y), Align2::LEFT_TOP, &g.title, bold(10.0), tc, w);
        y += 16.0;
        text(&p, pos2(inner.left(), y), Align2::LEFT_TOP, &g.value, bold(20.0), TEXT, w);
        y += 28.0;
    }
    match g.cores {
        Some(cores) if !cores.is_empty() => {
            let n = cores.len() as f32;
            let bw = (w - 3.0 * (n - 1.0)) / n;
            for (i, &v) in cores.iter().enumerate() {
                // values jump to each sample: easing them kept the CPU renderer at ~40 fps (half a core)
                let tr = Rect::from_min_size(pos2(inner.left() + i as f32 * (bw + 3.0), y + 1.0), vec2(bw, 12.0));
                p.rect_filled(tr, 2.0, LINE);
                let fh = (v / 100.0).clamp(0.1, 1.0) * 12.0;
                p.rect_filled(Rect::from_min_max(pos2(tr.left(), tr.bottom() - fh), tr.max), 2.0, g.s1.1);
            }
        }
        _ => {
            text(&p, pos2(inner.left(), y), Align2::LEFT_TOP, &g.sub, font(11.0), MUTED, w);
        }
    }
    y += 17.0;
    let sr = Rect::from_min_max(pos2(inner.left(), y), pos2(inner.right(), inner.bottom()));
    spark(&p, sr, g.s1.0, g.top, g.s1.1, true);
    if let Some((s, c)) = g.s2 {
        spark(&p, sr, s, g.top, c, false);
    }
}

fn draw_drives(ui: &Ui, r: Rect, drives: &[(String, u64, u64)], small: bool) {
    let p = ui.painter().with_clip_rect(r);
    card(&p, r, false);
    let inner = r.shrink2(vec2(12.0, 9.0));
    text(&p, inner.left_top(), Align2::LEFT_TOP, "DRIVES", bold(10.0), MUTED, inner.width());
    let mut y = inner.top() + 22.0;
    let step = if small { 17.0 } else { 18.0 };
    for (mount, used, total) in drives {
        let cy = y + 6.0;
        text(&p, pos2(inner.left(), cy), Align2::LEFT_CENTER, mount, font(11.0), TEXT, 24.0);
        let free = format!("{} free", fmt_bytes((total - used) as f64));
        let fr = text(&p, pos2(inner.right(), cy), Align2::RIGHT_CENTER, &free, font(11.0), MUTED, 90.0);
        let bar = Rect::from_min_max(pos2(inner.left() + 28.0, cy - 2.0), pos2(fr.left() - 8.0, cy + 2.0));
        p.rect_filled(bar, 2.0, LINE);
        let f = if *total > 0 { *used as f32 / *total as f32 } else { 0.0 };
        p.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * f, 4.0)), 2.0, if f > 0.9 { DANGER } else { MUTED });
        y += step;
    }
}

/// A text button drawn by hand (labels only: they already say what they do).
fn link(ui: &mut Ui, label: &str, color: Color32, size: f32) -> egui::Response {
    let g = ui.painter().layout_no_wrap(label.into(), font(size), color);
    let (r, resp) = ui.allocate_exact_size(g.size() + vec2(20.0, 10.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, 6.0, alpha(TEXT, 14));
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    ui.painter().galley(r.center() - g.size() / 2.0, g, color);
    resp
}

fn icon_button(ui: &mut Ui, glyph: &str, tip: &str, color: Color32) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(r, 8.0, alpha(TEXT, 16));
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    ui.painter().text(r.center(), Align2::CENTER_CENTER, glyph, icons(14.0), color);
    resp.on_hover_text(tip)
}

/// Segmented pills; returns the index clicked.
fn pills(ui: &mut Ui, items: &[(&str, bool)]) -> Option<usize> {
    let gs: Vec<_> = items.iter().map(|(l, on)| ui.painter().layout_no_wrap(l.to_string(), font(12.0), if *on { TEXT } else { MUTED })).collect();
    let w: f32 = gs.iter().map(|g| g.size().x + 20.0).sum::<f32>() + 2.0 * (gs.len() - 1) as f32 + 6.0;
    let (r, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
    ui.painter().rect_filled(r, 10.0, CARD);
    let mut x = r.left() + 3.0;
    let mut hit = None;
    for (i, g) in gs.into_iter().enumerate() {
        let pr = Rect::from_min_size(pos2(x, r.top() + 3.0), vec2(g.size().x + 20.0, 24.0));
        let resp = ui.interact(pr, ui.scope_id().with(("pill", items[i].0)), Sense::click());
        let bg = ui.ctx().animate_bool_with_time(resp.id.with("on"), items[i].1, 0.2);
        ui.painter().rect_filled(pr, 8.0, alpha(PILL_ON, (bg * 255.0) as u8));
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        ui.painter().galley(pr.center() - g.size() / 2.0, g, TEXT);
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
            ("cpu", Gauge { title: "CPU".into(), value: format!("{:.0}%", s.cpu), sub: String::new(), cores: Some(&s.per_core), s1: (&h.cpu, C_CPU), s2: None, top: 100.0 }),
            (
                "ram",
                Gauge {
                    title: if !small && self.metric == Field::Private { "MEMORY · PRIVATE".into() } else { "MEMORY".into() },
                    value: format!("{:.1} / {}", gb(s.ram_used), fmt_bytes(s.ram_total as f64)),
                    sub: format!("Commit {} of {}", fmt_bytes(s.commit as f64), fmt_bytes(s.commit_limit as f64)),
                    cores: None,
                    s1: (&h.ram, C_RAM),
                    s2: None,
                    top: 100.0,
                },
            ),
        ];
        if small {
            v.push((
                "commit",
                Gauge {
                    title: "COMMIT".into(),
                    value: format!("{:.1} / {}", gb(s.commit), fmt_bytes(s.commit_limit as f64)),
                    sub: format!("{:.0}% of the commit limit", s.commit as f64 / s.commit_limit.max(1) as f64 * 100.0),
                    cores: None,
                    s1: (&h.commit, C_COMMIT),
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
        v.push(("gpu", Gauge { title: "GPU".into(), value: gv, sub: gs, cores: None, s1: (&h.gpu, C_GPU), s2: None, top: 100.0 }));
        let (vv, vs) = if g.mem_total > 0 { (format!("{:.1} / {}", gb(g.mem_used), fmt_bytes(g.mem_total as f64)), g.name.replace("NVIDIA GeForce ", "")) } else { ("-".into(), String::new()) };
        v.push(("vram", Gauge { title: "VRAM".into(), value: vv, sub: vs, cores: None, s1: (&h.vram, C_VRAM), s2: None, top: 100.0 }));
        let mx = |a: &VecDeque<f32>, b: &VecDeque<f32>, floor: f32| a.iter().chain(b).fold(floor, |m, &x| m.max(x));
        v.push((
            "disk",
            Gauge {
                title: "DISK  read / write".into(),
                value: fmt_rate(s.disk_r),
                sub: format!("write {}", fmt_rate(s.disk_w)),
                cores: None,
                s1: (&h.disk_r, C_DISK),
                s2: Some((&h.disk_w, C_DISK2)),
                top: mx(&h.disk_r, &h.disk_w, 1048576.0),
            },
        ));
        v.push((
            "net",
            Gauge {
                title: "NETWORK  down / up".into(),
                value: fmt_rate(s.net_d),
                sub: format!("up {}", fmt_rate(s.net_u)),
                cores: None,
                s1: (&h.net_d, C_NET),
                s2: Some((&h.net_u, C_NET2)),
                top: mx(&h.net_d, &h.net_u, 102400.0),
            },
        ));
        v
    }

    fn draw_tab(&mut self, ui: &mut Ui, win: Rect, now: f64) {
        let ctx = ui.ctx().clone();
        let r = Rect::from_min_size(pos2(win.right() - TAB.x, win.center().y - TAB.y / 2.0), TAB);
        let p = ui.painter();
        p.rect(r, CornerRadius { nw: 12, sw: 12, ne: 0, se: 0 }, alpha(rgb(0x222631), 0xF2), Stroke::new(1.0, rgb(0x3A4150)), StrokeKind::Inside);
        let vals = self.tot.as_ref().map_or([0.0; 3], |s| [s.cpu, s.ram_used as f32 / s.ram_total.max(1) as f32 * 100.0, s.gpu.util.unwrap_or(0.0)]);
        let bh = TAB.y - 36.0;
        let x0 = r.center().x + 1.5 - (3.0 * 4.0 + 2.0 * 3.0) / 2.0;
        for (i, (v, c)) in vals.iter().zip([C_CPU, C_RAM, C_GPU]).enumerate() {
            // no easing here: the docked tab is always on screen, and an animation every 0.5 s kept it at ~36 fps (3% of a core)
            let v = *v;
            let tr = Rect::from_min_size(pos2(x0 + i as f32 * 7.0, r.center().y - bh / 2.0), vec2(4.0, bh));
            p.rect_filled(tr, 2.0, LINE);
            let fh = (v / 100.0).clamp(0.02, 1.0) * bh;
            p.rect_filled(Rect::from_min_max(pos2(tr.left(), tr.bottom() - fh), tr.max), 2.0, c);
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
                if let Err(e) = sys::save_anchor(self.anchor) {
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
        ui.painter().rect(r, CornerRadius { nw: 16, sw: 16, ne: 0, se: 0 }, alpha(rgb(0x0E1014), 0xF7), Stroke::new(1.0, LINE), StrokeKind::Inside);
        let inner = Rect::from_min_max(r.min + vec2(12.0, 10.0), r.max - vec2(10.0, 4.0));
        let pt = ui.painter().clone();
        text(&pt, inner.left_top(), Align2::LEFT_TOP, "ResMon", bold(13.0), TEXT, 100.0);
        text(&pt, pos2(inner.right(), inner.top() + 2.0), Align2::RIGHT_TOP, "click for details", font(10.0), FAINT, 120.0);
        let gauges = self.gauges(true);
        let mut y = inner.top() + 26.0;
        for (_, g) in &gauges {
            let gr = Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), 82.0));
            draw_gauge(&ui, gr, g, true, false);
            y += 88.0;
        }
        if let Some(s) = &self.tot {
            let dh = 31.0 + 17.0 * s.drives.len() as f32;
            draw_drives(&ui, Rect::from_min_size(pos2(inner.left(), y), vec2(inner.width(), dh)), &s.drives, true);
        }
        let foot = Rect::from_min_max(pos2(inner.left(), inner.bottom() - 34.0), inner.max);
        let mut fu = ui.new_child(UiBuilder::new().max_rect(foot).layout(Layout::left_to_right(Align::Center)));
        if icon_button(&mut fu, "\u{E740}", "Open the full monitor", TEXT).clicked() {
            self.go(Mode::Full, now);
            return;
        }
        fu.with_layout(Layout::right_to_left(Align::Center), |fu| {
            if icon_button(fu, "\u{E8BB}", "Quit ResMon", MUTED).clicked() {
                self.quit = true;
            }
            let tip = format!("Start with Windows: {} (click to toggle)", if self.startup { "on" } else { "off" });
            if icon_button(fu, "\u{E7E8}", &tip, if self.startup { C_GPU } else { MUTED }).clicked() {
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
            Some(t) if t.to == Mode::Tab => TAB,
            Some(t) => t.from,
            None => win.size(),
        };
        let size = from + (win.size() - from) * size_t;
        let card_r = Rect::from_min_size(pos2(win.right() - size.x, win.center().y - size.y / 2.0), size);
        ui.painter().rect(card_r, CornerRadius { nw: 14, sw: 14, ne: 0, se: 0 }, BG, Stroke::new(1.0, LINE), StrokeKind::Inside);
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
        let rest = inner.bottom() - tb.bottom() - 10.0 - 40.0 - 20.0;
        let tm = Rect::from_min_size(pos2(inner.left(), tb.bottom() + 10.0), vec2(inner.width(), (rest * 0.6).floor()));
        self.treemap(&mut ui, tm, now);
        let sb = Rect::from_min_size(pos2(inner.left(), tm.bottom() + 10.0), vec2(inner.width(), 40.0));
        self.selbar(&mut ui, sb, now);
        let table = Rect::from_min_max(pos2(inner.left(), sb.bottom() + 10.0), inner.max);
        self.table(&mut ui, table, now);
        self.kill_dialog(ui.ctx(), now);
        if let Some((msg, err, t0)) = &self.toast {
            let left = 4.0 - (now - t0);
            if left <= 0.0 {
                self.toast = None;
            } else {
                let p = ui.painter();
                let g = p.layout_no_wrap(msg.clone(), font(13.0), TEXT);
                let r = Rect::from_center_size(pos2(card_r.center().x, card_r.bottom() - 40.0), g.size() + vec2(32.0, 20.0));
                p.rect_filled(r, 10.0, if *err { rgb(0x3A1F25) } else { rgb(0x1F2A24) });
                p.galley(r.center() - g.size() / 2.0, g, TEXT);
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
        let t = text(p, pos2(bar.left() + 4.0, bar.center().y), Align2::LEFT_CENTER, "ResMon", bold(13.0), TEXT, 80.0);
        text(p, pos2(t.right() + 8.0, bar.center().y), Align2::LEFT_CENTER, "drag to move · Esc to dock", font(11.0), FAINT, 300.0);
        let mut bu = ui.new_child(UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Center)));
        if icon_button(&mut bu, "\u{E8BB}", "Dock to the edge", MUTED).clicked() || icon_button(&mut bu, "\u{E921}", "Dock to the edge", MUTED).clicked() {
            self.go(Mode::Tab, now);
        }
        let startup = self.startup;
        let mut toggle = false;
        bu.visuals_mut().widgets.inactive.weak_bg_fill = Color32::TRANSPARENT; // the menu reads as an icon, like the two next to it
        bu.visuals_mut().widgets.inactive.bg_stroke = Stroke::NONE;
        bu.menu_button(egui::RichText::new("\u{E712}").font(icons(14.0)).color(MUTED), |ui| {
            if ui.checkbox(&mut startup.clone(), "Start with Windows").clicked() {
                toggle = true;
                ui.close();
            }
            if ui.button("Quit ResMon").clicked() {
                self.quit = true;
            }
        });
        if toggle {
            self.toggle_startup(now);
        }
    }

    fn header(&mut self, ui: &Ui, r: Rect, now: f64) {
        let gauges = self.gauges(false);
        let n = gauges.len() as f32 + 1.0;
        let w = (r.width() - 10.0 * (n - 1.0)) / n;
        let mut pick = None;
        for (i, (key, g)) in gauges.iter().enumerate() {
            let gr = Rect::from_min_size(pos2(r.left() + i as f32 * (w + 10.0), r.top()), vec2(w, r.height()));
            let metric = match *key {
                "cpu" => Some(Field::Cpu),
                "ram" => Some(Field::Ws),
                "gpu" => Some(Field::Gpu),
                "vram" => Some(Field::GpuMem),
                "disk" => Some(Field::Io),
                _ => None,
            };
            let on = metric.is_some_and(|m| m == self.metric || (*key == "ram" && self.metric == Field::Private));
            draw_gauge(ui, gr, g, false, on);
            if let Some(m) = metric {
                let tip = format!("Size the treemap by {}", if *key == "ram" { "memory (click again for private bytes)" } else { g.title.split(' ').next().unwrap_or("") });
                let resp = ui.interact(gr, Id::unique(("card", *key)), Sense::click()).on_hover_text(tip);
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                }
                if resp.clicked() {
                    // MEMORY clicked again flips working set / private bytes
                    pick = Some(match (m, self.metric) {
                        (Field::Ws, Field::Ws) => Field::Private,
                        (Field::Ws, Field::Private) => Field::Ws,
                        _ => m,
                    });
                }
            }
        }
        if let Some(s) = &self.tot {
            let dr = Rect::from_min_size(pos2(r.right() - w, r.top()), vec2(w, r.height()));
            draw_drives(ui, dr, &s.drives, false);
        }
        if let Some(m) = pick {
            self.metric = m;
            self.relayout(now);
        }
    }

    fn toolbar(&mut self, ui: &mut Ui, r: Rect, now: f64) {
        let mut tu = ui.new_child(UiBuilder::new().max_rect(r).layout(Layout::left_to_right(Align::Center)));
        tu.spacing_mut().item_spacing.x = 4.0;
        if link(&mut tu, "All processes", if self.zoom.is_some() { MUTED } else { TEXT }, 14.0).clicked() && self.zoom.is_some() {
            self.zoom = None;
            self.relayout(now);
        }
        if let Some(z) = &self.zoom {
            tu.label(egui::RichText::new("\u{E76C}").font(icons(11.0)).color(FAINT));
            tu.label(egui::RichText::new(z.as_str()).font(bold(14.0)).color(TEXT));
        }
        let n = if self.zoom.is_some() { self.tiles.iter().filter(|t| t.hit.is_some()).count() } else { self.tiles.iter().filter(|t| t.hit.is_some()).map(|t| &t.name).collect::<HashSet<_>>().len() };
        let by = format!("Sized by {}{}", self.metric_label(), if self.whole().is_some() { " · share of total" } else { "" });
        let count = if self.zoom.is_some() { format!("{by}  ·  {n} processes") } else { format!("{by}  ·  {n} apps · {} processes", self.procs.len()) };
        tu.add_space(8.0);
        tu.label(egui::RichText::new(count).font(font(12.0)).color(FAINT));
        tu.with_layout(Layout::right_to_left(Align::Center), |tu| {
            tu.spacing_mut().item_spacing.x = 8.0;
            if let Some(i) = pills(tu, &[("App", !self.heat), ("CPU heat", self.heat)]) {
                self.heat = i == 1;
            }
            if pills(tu, &[("Per core", self.per_core)]).is_some() {
                self.per_core = !self.per_core;
            }
            let can = self.metric != Field::Io; // disk I/O has no total to share
            tu.add_enabled_ui(can, |tu| {
                if pills(tu, &[("Share of total", self.relative)]).is_some() {
                    self.relative = !self.relative;
                    self.relayout(now);
                }
            })
            .response
            .on_hover_text("Size tiles against the machine total, with the free part as its own tile");
        });
    }

    fn treemap(&mut self, ui: &mut Ui, r: Rect, now: f64) {
        let p = ui.painter().with_clip_rect(r);
        p.rect_filled(r, 10.0, rgb(0x12151B));
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
                p.rect_filled(tr, 3.0, mix(c, BG, 0.55));
                if tr.width() >= 46.0 {
                    text(&p, tr.min + vec2(6.0, 3.0), Align2::LEFT_TOP, &format!("{}   {value}", t.title), bold(11.0), alpha(Color32::WHITE, 0xEE), tr.width() - 10.0);
                }
            } else {
                p.rect_filled(tr, 3.0, mix(c, Color32::WHITE, 0.05));
                if !t.free {
                    p.line_segment([tr.left_top() + vec2(2.0, 0.5), tr.right_top() + vec2(-2.0, 0.5)], Stroke::new(1.0, mix(c, Color32::WHITE, 0.3)));
                }
                if tr.width() >= 46.0 && tr.height() >= 18.0 {
                    text(&p, tr.min + vec2(6.0, 3.0), Align2::LEFT_TOP, &t.title, bold(11.0), alpha(Color32::WHITE, 0xEE), tr.width() - 10.0);
                }
                if tr.width() >= 46.0 && tr.height() >= 34.0 {
                    text(&p, tr.min + vec2(6.0, 18.0), Align2::LEFT_TOP, &value, font(10.0), alpha(Color32::WHITE, 0xB0), tr.width() - 10.0);
                }
            }
        }
        if self.tiles.is_empty() && !self.procs.is_empty() {
            p.text(r.center(), Align2::CENTER_CENTER, "Nothing uses this resource right now", font(13.0), FAINT);
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
        let rect_of = |h: &Hit| -> Option<Rect> {
            let k = match h {
                Hit::Pid(p) => Key::Pid(*p),
                Hit::Group(n) => Key::Group(n.clone()),
            };
            self.tiles.iter().zip(&rects).find(|(t, _)| t.key == k).map(|(_, r)| *r)
        };
        if let Some(sr) = self.sel.as_ref().and_then(rect_of) {
            p.rect_stroke(sr, 4.0, Stroke::new(2.5, Color32::WHITE), StrokeKind::Inside);
        }
        if let Some(hr) = target.as_ref().filter(|h| Some(*h) != self.sel.as_ref()).and_then(rect_of) {
            p.rect_stroke(hr, 4.0, Stroke::new(1.5, alpha(Color32::WHITE, 0x90)), StrokeKind::Inside);
        }
        if target.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        if resp.clicked() {
            self.sel = target.clone();
        }
        if resp.double_clicked() && self.zoom.is_none() {
            let name = match &target {
                Some(Hit::Group(n)) => Some(n.clone()),
                Some(Hit::Pid(pid)) => self.by_pid.get(pid).map(|&i| self.procs[i].name.clone()),
                None => None,
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
            let bg = p.layout(body, font(11.0), MUTED, 296.0);
            let sz = vec2(320.0, bg.size().y + 44.0);
            let x = if pos.x + 16.0 + sz.x < r.right() { pos.x + 16.0 } else { (pos.x - sz.x - 16.0).max(r.left()) };
            let y = if pos.y + 16.0 + sz.y < r.bottom() { pos.y + 16.0 } else { (pos.y - sz.y - 8.0).max(r.top()) };
            let tr = Rect::from_min_size(pos2(x, y), sz);
            p.add(egui::epaint::Shadow { offset: [0, 6], blur: 18, spread: 0, color: alpha(Color32::BLACK, 0x90) }.as_shape(tr, 10.0));
            p.rect(tr, 10.0, alpha(rgb(0x1C2029), 0xF5), Stroke::new(1.0, LINE), StrokeKind::Inside);
            text(&p, tr.min + vec2(12.0, 12.0), Align2::LEFT_TOP, &title, bold(13.0), TEXT, 296.0);
            p.galley(tr.min + vec2(12.0, 34.0), bg, MUTED);
        }
    }

    fn selbar(&mut self, ui: &mut Ui, r: Rect, now: f64) {
        ui.painter().rect_filled(r, 10.0, CARD);
        let qs = self.selected();
        let k = self.cpu_scale();
        let (msg, c) = match (qs.first(), &self.sel) {
            (None, _) => ("Click a tile or a row to select it. Double-click an app to see its processes. Click a card above to size the treemap by it.".to_string(), MUTED),
            (Some(q), Some(Hit::Pid(_))) => (format!("{}   PID {}   ·   {}   ·   CPU {:.1}%", q.name, q.pid, fmt_bytes(q.ws), q.cpu * k), TEXT),
            (Some(q), _) => (
                format!("{}   {} processes   ·   {}   ·   CPU {:.1}%", q.name, qs.len(), fmt_bytes(qs.iter().map(|q| q.ws).sum()), qs.iter().map(|q| q.cpu).sum::<f64>() * k),
                TEXT,
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
        au.menu_button(egui::RichText::new("Priority").font(font(12.0)).color(MUTED), |ui| {
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
        if link(&mut au, "Open location", MUTED, 12.0).clicked() {
            self.open_location(pid, now);
        }
        if link(&mut au, "Copy command line", MUTED, 12.0).clicked() {
            if cmd.is_empty() {
                self.toast("Command line not accessible (needs administrator)", true, now);
            } else {
                ui.ctx().copy_text(cmd);
                self.toast("Copied command line", false, now);
            }
        }
        if link(&mut au, "Copy PID", MUTED, 12.0).clicked() {
            ui.ctx().copy_text(pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "));
            self.toast(if pids.len() == 1 { "Copied PID".into() } else { format!("Copied {} PIDs", pids.len()) }, false, now);
        }
    }

    fn table(&mut self, ui: &mut Ui, r: Rect, _now: f64) {
        let mut tu = ui.new_child(UiBuilder::new().max_rect(r).layout(Layout::top_down(Align::Min)));
        tu.horizontal(|ui| {
            let fr = egui::Frame::new().fill(CARD).corner_radius(10.0).inner_margin(egui::Margin::symmetric(10, 8));
            fr.show(ui, |ui| {
                let te = egui::TextEdit::singleline(&mut self.query).hint_text("Filter by name, PID or command line").desired_width(340.0).frame(egui::Frame::NONE).font(font(13.0));
                if ui.add(te).changed() {
                    self.refresh_rows();
                }
            });
            ui.add_space(8.0);
            ui.label(egui::RichText::new(format!("{} shown", self.rows.len())).font(font(12.0)).color(FAINT));
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
            let mut l = if *col == Col::Cpu && self.per_core { "CPU/core".to_string() } else { label.to_string() };
            if on {
                l += if self.sort.1 { " ↓" } else { " ↑" };
            }
            let (pos, al) = if *num { (pos2(cr.right() - 6.0, cr.center().y), Align2::RIGHT_CENTER) } else { (pos2(cr.left() + 6.0, cr.center().y), Align2::LEFT_CENTER) };
            text(tu.painter(), pos, al, &l, bold(11.0), if on || resp.hovered() { TEXT } else { MUTED }, w - 12.0);
            if resp.hovered() {
                tu.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if resp.clicked() {
                self.sort = (*col, if on { !self.sort.1 } else { !matches!(col, Col::Name | Col::Cmd) });
                self.refresh_rows();
            }
        }
        tu.painter().line_segment([pos2(r.left(), hr.bottom()), pos2(r.right(), hr.bottom())], Stroke::new(1.0, LINE));
        let sel: HashSet<u32> = self.selected().iter().map(|q| q.pid).collect();
        let pdh = self.tot.as_ref().is_some_and(|t| t.pdh_ok);
        let k = self.cpu_scale();
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(&mut tu, 26.0, self.rows.len(), |ui, range| {
            for &i in &self.rows[range] {
                let q = &self.procs[i];
                let (rr, resp) = ui.allocate_exact_size(vec2(r.width(), 26.0), Sense::click());
                if sel.contains(&q.pid) {
                    ui.painter().rect_filled(rr, 6.0, rgb(0x243049));
                } else if resp.hovered() {
                    ui.painter().rect_filled(rr, 6.0, alpha(TEXT, 10));
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
                    if q.io >= 102400.0 { fmt_rate(q.io) } else { "-".into() },
                    q.threads.to_string(),
                    sys::fmt_started(q.create),
                    q.cmd.clone(),
                ];
                for (c, ((_, _, w, num), v)) in COLS.iter().zip(vals).enumerate() {
                    let w = if *w == 0.0 { cmd_w } else { *w };
                    let x = rr.left() + COLS[..c].iter().map(|c| c.2).sum::<f32>();
                    let (pos, al) = if *num { (pos2(x + w - 6.0, rr.center().y), Align2::RIGHT_CENTER) } else { (pos2(x + 6.0, rr.center().y), Align2::LEFT_CENTER) };
                    text(ui.painter(), pos, al, &v, font(12.0), if c == 0 { TEXT } else { MUTED }, w - 12.0);
                }
            }
        });
        if let Some(pid) = clicked {
            self.sel = Some(Hit::Pid(pid));
        }
    }

    fn kill_dialog(&mut self, ctx: &egui::Context, now: f64) {
        let Some(ask) = &mut self.kill_ask else { return };
        let mut done: Option<bool> = None;
        egui::Modal::new(Id::unique("kill")).frame(egui::Frame::new().fill(CARD).corner_radius(14.0).inner_margin(22.0).stroke(Stroke::new(1.0, LINE))).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(egui::RichText::new("End task?").font(bold(18.0)).color(TEXT));
            ui.add_space(10.0);
            let warn = if ask.me { "  This is ResMon itself." } else { "" };
            ui.label(egui::RichText::new(format!("End {}? Unsaved data in it will be lost.{warn}", ask.what)).font(font(13.0)).color(MUTED));
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
    ctx.all_styles_mut(|s| {
        let v = &mut s.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = Color32::TRANSPARENT;
        v.window_fill = CARD;
        v.window_stroke = Stroke::new(1.0, LINE);
        v.extreme_bg_color = CARD;
        v.selection.bg_fill = rgb(0x2E4A7A);
        v.override_text_color = None;
        v.widgets.noninteractive.fg_stroke.color = MUTED;
        v.widgets.inactive.fg_stroke.color = TEXT;
        v.widgets.inactive.weak_bg_fill = PILL_ON;
        v.widgets.hovered.weak_bg_fill = rgb(0x323A4D);
        v.menu_corner_radius = 10.into();
        s.text_styles.insert(egui::TextStyle::Body, font(13.0));
        s.text_styles.insert(egui::TextStyle::Button, font(13.0));
        s.spacing.button_padding = vec2(10.0, 5.0);
        s.interaction.tooltip_delay = 0.4;
    });
}

fn main() {
    shell::run();
}
