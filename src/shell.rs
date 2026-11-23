//! The window: winit for messages and input (through egui-winit), our CPU rasterizer for pixels, and
//! UpdateLayeredWindow to show them with per-pixel alpha. Fully transparent pixels let clicks through.

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{ViewportCommand, ViewportId};
use windows::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{GWL_EXSTYLE, GetWindowLongW, SetWindowLongW, ULW_ALPHA, UPDATELAYEREDWINDOWINFO, UpdateLayeredWindowIndirect, WS_EX_LAYERED};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::windows::WindowAttributesExtWindows;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId, WindowLevel};

use crate::App;
use crate::raster::Raster;

const MIN_FRAME: Duration = Duration::from_micros(16_600); // animations: at most ~60 fps

/// A DIB section the size of the window, the source of UpdateLayeredWindow.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    bits: *mut u32,
    w: usize,
    h: usize,
}

impl Dib {
    fn new(w: usize, h: usize) -> Dib {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();
            SelectObject(dc, bmp.into());
            Dib { dc, bmp, bits: bits as *mut u32, w, h }
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.bmp.into());
        }
    }
}

struct Runner {
    proxy: EventLoopProxy<Instant>,
    ctx: egui::Context,
    window: Option<Arc<Window>>,
    state: Option<egui_winit::State>,
    app: Option<App>,
    raster: Raster,
    dib: Option<Dib>,
    hwnd: HWND,
    next: Option<Instant>,
    last: Instant,
    log: bool,  // RESMON_LOG: per-frame timings in %TEMP%/resmon.log
    dump: bool, // RESMON_DUMP: every frame as raw BGRA in %TEMP%
    script: Vec<(Instant, Step)>,
    shots: u32,
}

#[derive(Clone, Copy)]
enum Step {
    Move(f32, f32),
    Button(f32, f32, bool),
    Shot,
}

/// RESMON_SCRIPT="2:click:1580,232; 3.5:dbl:900,500; 4:move:10,10; 5:shot" (seconds from start, window px).
/// A shot writes the frame to %TEMP%\resmon_shotN_WxH.bgra.
fn parse_script(s: &str) -> Vec<(Instant, Step)> {
    let t0 = Instant::now();
    let mut out = Vec::new();
    for item in s.split(';').map(str::trim).filter(|i| !i.is_empty()) {
        let mut parts = item.split(':');
        let (Some(t), Some(kind)) = (parts.next().and_then(|t| t.parse::<f64>().ok()), parts.next()) else { continue };
        let at = t0 + Duration::from_secs_f64(t);
        let xy: Vec<f32> = parts.next().unwrap_or("").split(',').filter_map(|v| v.trim().parse().ok()).collect();
        let (x, y) = (xy.first().copied().unwrap_or(0.0), xy.get(1).copied().unwrap_or(0.0));
        let click = |at: Instant, out: &mut Vec<(Instant, Step)>| {
            out.push((at, Step::Button(x, y, true)));
            out.push((at + Duration::from_millis(40), Step::Button(x, y, false)));
        };
        match kind {
            "click" => {
                out.push((at, Step::Move(x, y))); // hover first, like a real pointer
                click(at + Duration::from_millis(100), &mut out);
            }
            "dbl" => {
                click(at, &mut out);
                click(at + Duration::from_millis(120), &mut out);
            }
            "move" => out.push((at, Step::Move(x, y))),
            "shot" => out.push((at, Step::Shot)),
            _ => {}
        }
    }
    out.sort_by_key(|e| e.0);
    out
}

impl Runner {
    fn want(&mut self, at: Instant) {
        self.next = Some(self.next.map_or(at, |n| n.min(at)));
    }

    fn frame(&mut self, el: &ActiveEventLoop) {
        let (Some(window), Some(state), Some(app)) = (self.window.clone(), self.state.as_mut(), self.app.as_mut()) else { return };
        self.last = Instant::now();
        // scripted input (RESMON_SCRIPT): synthetic pointer events, for checks that must not move the real mouse
        let mut shot = false;
        while let Some((at, step)) = self.script.first().cloned()
            && at <= self.last
        {
            self.script.remove(0);
            let ppp = window.scale_factor() as f32;
            let input = state.egui_input_mut();
            let pos = |x: f32, y: f32| egui::pos2(x / ppp, y / ppp);
            match step {
                Step::Move(x, y) => input.events.push(egui::Event::PointerMoved(pos(x, y))),
                Step::Button(x, y, pressed) => {
                    input.events.push(egui::Event::PointerMoved(pos(x, y)));
                    input.events.push(egui::Event::PointerButton { pos: pos(x, y), button: egui::PointerButton::Primary, pressed, modifiers: Default::default() });
                }
                Step::Shot => shot = true,
            }
        }
        if let Some((at, _)) = self.script.first() {
            let at = *at;
            self.next = Some(self.next.map_or(at, |n| n.min(at)));
        }
        let input = state.take_egui_input(&window);
        let out = self.ctx.run_ui(input, |ui| {
            app.logic(ui.ctx());
            app.ui(ui);
        });
        state.handle_platform_output(&window, out.platform_output);
        if let Some(vo) = out.viewport_output.get(&ViewportId::ROOT) {
            for c in &vo.commands {
                match c {
                    ViewportCommand::Close => el.exit(),
                    ViewportCommand::StartDrag => {
                        let _ = window.drag_window();
                    }
                    _ => {}
                }
            }
            if vo.repaint_delay < Duration::from_secs(3600) {
                let at = Instant::now() + vo.repaint_delay;
                self.next = Some(self.next.map_or(at, |n| n.min(at)));
            }
        }
        // pixels, drawn straight into the DIB that UpdateLayeredWindow reads
        let t_pass = Instant::now();
        let size = window.inner_size();
        let (w, h) = (size.width.max(1) as usize, size.height.max(1) as usize);
        if self.dib.as_ref().is_none_or(|d| d.w != w || d.h != h) {
            self.dib = Some(Dib::new(w, h));
        }
        let dib = self.dib.as_ref().unwrap();
        if dib.bits.is_null() {
            return;
        }
        let pixels = unsafe { std::slice::from_raw_parts_mut(dib.bits, w * h) };
        self.raster.set_textures(&out.textures_delta);
        let prims = self.ctx.tessellate(out.shapes, out.pixels_per_point);
        let t_tess = Instant::now();
        let damage = self.raster.render(&prims, pixels, w, h, out.pixels_per_point);
        self.raster.free_textures(&out.textures_delta);
        let mut d = out.textures_delta;
        d.clear();
        let t_raster = Instant::now();
        if self.dump || shot {
            let bytes: Vec<u8> = pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
            let name = if shot {
                self.shots += 1;
                format!("resmon_shot{}_{w}x{h}.bgra", self.shots)
            } else {
                format!("resmon_{w}x{h}.bgra")
            };
            let _ = std::fs::write(std::env::temp_dir().join(name), bytes);
        }
        let ulw = unsafe {
            // winit rewrites the extended style when it shows or moves the window, dropping WS_EX_LAYERED
            let ex = GetWindowLongW(self.hwnd, GWL_EXSTYLE);
            if ex & WS_EX_LAYERED.0 as i32 == 0 {
                SetWindowLongW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as i32);
            }
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            let size = SIZE { cx: w as i32, cy: h as i32 };
            match damage {
                None => true, // nothing changed on screen
                Some(d) => {
                    let dirty = RECT { left: d[0], top: d[1], right: d[2], bottom: d[3] };
                    let src = POINT::default();
                    let info = UPDATELAYEREDWINDOWINFO {
                        cbSize: std::mem::size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
                        psize: &size,
                        hdcSrc: dib.dc,
                        pptSrc: &src,
                        pblend: &blend,
                        dwFlags: ULW_ALPHA,
                        prcDirty: &dirty,
                        ..Default::default()
                    };
                    UpdateLayeredWindowIndirect(self.hwnd, &info).as_bool()
                }
            }
        };
        if self.log {
            use std::io::Write;
            let ms = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1000.0;
            let line = format!(
                "{w}x{h} pass {:.2} tess {:.2} raster {:.2} ulw {:.2} ms ok={} damage={:?} prims={} causes={:?}
",
                ms(self.last, t_pass),
                ms(t_pass, t_tess),
                ms(t_tess, t_raster),
                ms(t_raster, Instant::now()),
                ulw,
                damage,
                prims.len(),
                self.ctx.repaint_causes()
            );
            let _ = std::fs::OpenOptions::new().create(true).append(true).open(std::env::temp_dir().join("resmon.log")).map(|mut f| f.write_all(line.as_bytes()));
        }
        if app.shown && !window.is_visible().unwrap_or(true) {
            window.set_visible(true);
        }
    }
}

impl ApplicationHandler<Instant> for Runner {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("ResMon")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(winit::dpi::LogicalSize::new(crate::TAB.x, crate::TAB.y))
            .with_skip_taskbar(true)
            .with_undecorated_shadow(false);
        let window = Arc::new(el.create_window(attrs).expect("create window"));
        if let Ok(h) = window.window_handle()
            && let RawWindowHandle::Win32(w) = h.as_raw()
        {
            self.hwnd = HWND(w.hwnd.get() as *mut _);
        }
        unsafe {
            let ex = GetWindowLongW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as i32);
        }
        self.state = Some(egui_winit::State::new(self.ctx.clone(), ViewportId::ROOT, el, Some(window.scale_factor() as f32), None, None));
        self.app = Some(App::new(&self.ctx, self.hwnd));
        self.window = Some(window);
        let proxy = self.proxy.clone();
        self.ctx.set_request_repaint_callback(move |info| {
            let _ = proxy.send_event(Instant::now() + info.delay);
        });
        self.want(Instant::now());
    }

    fn user_event(&mut self, _: &ActiveEventLoop, at: Instant) {
        self.want(at);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let (Some(window), Some(state)) = (self.window.clone(), self.state.as_mut()) else { return };
        if let WindowEvent::RedrawRequested = event {
            self.want(Instant::now());
            return;
        }
        let r = state.on_window_event(&window, &event);
        if r.repaint || matches!(event, WindowEvent::CloseRequested) {
            self.want(Instant::now());
        }
        let _ = el;
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if let Some(n) = self.next
            && n <= now
        {
            let earliest = self.last + MIN_FRAME;
            if now >= earliest {
                self.next = None;
                self.frame(el);
            } else {
                self.next = Some(earliest);
            }
        }
        el.set_control_flow(match self.next {
            Some(n) => ControlFlow::WaitUntil(n),
            None => ControlFlow::Wait,
        });
    }
}

pub fn run() {
    let el = EventLoop::<Instant>::with_user_event().build().expect("event loop");
    let mut r = Runner {
        proxy: el.create_proxy(),
        ctx: egui::Context::default(),
        window: None,
        state: None,
        app: None,
        raster: Raster::default(),
        dib: None,
        hwnd: HWND::default(),
        next: None,
        last: Instant::now() - MIN_FRAME,
        log: std::env::var_os("RESMON_LOG").is_some(),
        dump: std::env::var_os("RESMON_DUMP").is_some(),
        script: parse_script(&std::env::var("RESMON_SCRIPT").unwrap_or_default()),
        shots: 0,
    };
    let _ = el.run_app(&mut r);
}
