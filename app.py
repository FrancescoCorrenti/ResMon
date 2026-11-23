"""ResMon: a resource-monitor widget docked on the right screen edge (Flet).

One window morphs between three faces: tab (at rest) -> panel (hover) -> full view (click).
Two loops feed it: machine totals every 0.5 s (gauges, sparklines), processes every 1.5 s
(treemap, table; only while the full view is open). Values move with client-side implicit
animations, so Python only sends the targets.
"""
import asyncio
import ctypes
import ctypes.wintypes as wt
import datetime as dt
import json
import os
import subprocess
import sys
import time
import zlib

import flet as ft
import flet.canvas as cv
import psutil

from core import HEADER, HISTORY, Sampler, Sticky, smooth, treemap_model

FAST, SLOW = 0.5, 1.5           # seconds: totals / processes
REFLOW = 8.0                    # seconds: minimum time between two treemap reflows
TAB_W, TAB_H = 26, 168          # docked tab (logical px)
TAB_WIN_W = 160                 # window width while docked: above Windows' minimum
HOVER_OPEN = 2.0                # seconds resting on the tab before the panel opens (time to grab and drag it)
PANEL_W, PANEL_H = 264, 660     # hover panel
FULL_W, FULL_H = 1440, 900      # full view (clamped to the work area)
EASE = ft.AnimationCurve.EASE_OUT_CUBIC
UI_ANIM = ft.Animation(280, EASE)        # shell transitions
VALUE_ANIM = ft.Animation(450, EASE)     # bars: a bit shorter than FAST, so they settle between samples
TILE_ANIM = ft.Animation(550, EASE)      # treemap tiles glide

BG, CARD, LINE = "#0E1014", "#161920", "#232733"
TEXT, MUTED, FAINT = "#E8EAF0", "#8A90A0", "#5A6070"
ACCENT = {"cpu": "#5AB8FF", "ram": "#B48CFF", "commit": "#8C7BD6", "gpu": "#4ADE9A", "vram": "#F5C451",
          "disk": "#FF8A65", "disk2": "#FFC2A8", "net": "#4DD6E6", "net2": "#B4F0F7"}
DANGER = "#FF5C6C"
PALETTE = ["#3D7FD9", "#8C5BD6", "#D6508F", "#D99A2B", "#2FA57A", "#2AA6BF",
           "#D9573D", "#7DAA2F", "#5B63D6", "#2F9C94", "#C2732E", "#A24FC9"]
METRICS = {"ws": ("RAM", "ws"), "priv": ("Private", "priv"), "cpu": ("CPU", "cpu"),
           "gpu_mem": ("VRAM", "gpu_mem"), "gpu": ("GPU", "gpu")}  # key -> (pill label, process field)

# ---------------------------------------------------------------- Windows glue (physical px, per monitor)
# Monitors can have different scales (e.g. a 150% laptop next to a 100% screen), so every position is in
# physical px and the window is placed natively: Flet's logical bounds are ambiguous across monitors.
try:
    ctypes.windll.shcore.SetProcessDpiAwareness(2)  # per-monitor aware: Win32 calls are in physical px
except OSError:
    pass
_u32, _gdi = ctypes.windll.user32, ctypes.windll.gdi32
_u32.MonitorFromPoint.restype = wt.HMONITOR
_u32.MonitorFromPoint.argtypes = [wt.POINT, wt.DWORD]
_gdi.CreateRectRgn.restype = wt.HRGN
_u32.SetWindowRgn.argtypes = [wt.HWND, wt.HRGN, wt.BOOL]
SETTINGS = os.path.join(os.environ.get("APPDATA", ""), "ResMon", "settings.json")


class _MonitorInfo(ctypes.Structure):
    _fields_ = [("cbSize", wt.DWORD), ("rcMonitor", wt.RECT), ("rcWork", wt.RECT), ("dwFlags", wt.DWORD)]


def monitor_at(x, y):
    """Work area (taskbar excluded) and scale of the monitor nearest to (x, y): (left, top, right, bottom, scale)."""
    h = _u32.MonitorFromPoint(wt.POINT(round(x), round(y)), 2)  # MONITOR_DEFAULTTONEAREST
    mi = _MonitorInfo(cbSize=ctypes.sizeof(_MonitorInfo))
    _u32.GetMonitorInfoW(h, ctypes.byref(mi))
    dx, dy = ctypes.c_uint(96), ctypes.c_uint(96)
    ctypes.windll.shcore.GetDpiForMonitor(h, 0, ctypes.byref(dx), ctypes.byref(dy))  # MDT_EFFECTIVE_DPI
    r = mi.rcWork
    return r.left, r.top, r.right, r.bottom, dx.value / 96


def load_anchor():
    """Where the tab was last docked (a point on the right edge of a monitor), or the primary monitor's
    right-centre if none was saved or that monitor is gone."""
    try:
        with open(SETTINGS, encoding="utf-8") as f:
            x, y = json.load(f)["anchor"]
        if _u32.MonitorFromPoint(wt.POINT(round(x), round(y)), 0):  # MONITOR_DEFAULTTONULL
            return x, y
    except (OSError, ValueError, KeyError, TypeError):
        pass
    L, T, R, B, _ = monitor_at(0, 0)  # (0, 0) is always on the primary monitor
    return R - 1, (T + B) / 2


def save_anchor(anchor):
    os.makedirs(os.path.dirname(SETTINGS), exist_ok=True)
    with open(SETTINGS, "w", encoding="utf-8") as f:
        json.dump({"anchor": [round(v) for v in anchor]}, f)


def cursor_pos():
    p = wt.POINT()
    _u32.GetCursorPos(ctypes.byref(p))
    return p.x, p.y


def _hwnd():
    return _u32.FindWindowW("FLUTTER_RUNNER_WIN32_WINDOW", "ResMon")


def window_bounds():
    """Real bounds of our window, physical px, or None."""
    r = wt.RECT()
    h = _hwnd()
    if not h or not _u32.GetWindowRect(h, ctypes.byref(r)):
        return None
    return r.left, r.top, r.right - r.left, r.bottom - r.top


def place(b, clip=None):
    """Move/resize the window natively (physical px). clip = (x, y, w, h) in window coordinates limits
    drawing and clicks to that rect; None gives the whole window back."""
    h = _hwnd()
    if not h:
        return
    _u32.SetWindowPos(h, 0, *(round(v) for v in b), 0x0014)  # NOZORDER|NOACTIVATE
    # no SetWindowRgn for clip: a window region breaks Flutter's transparent window (it paints white)


def move_to(x, y):
    h = _hwnd()
    if h:
        _u32.SetWindowPos(h, 0, round(x), round(y), 0, 0, 0x0001 | 0x0004 | 0x0010)  # NOSIZE|NOZORDER|NOACTIVATE


def strip_frame():
    """Windows keeps a minimum width on the Flutter window even when frameless; without the caption and
    thick-frame styles it goes down to ~118 logical px. The tab region clips the rest away."""
    h = _hwnd()
    if not h:
        return
    style = _u32.GetWindowLongW(h, -16)  # GWL_STYLE
    if style & (0x00C00000 | 0x00040000):
        _u32.SetWindowLongW(h, -16, style & ~(0x00C00000 | 0x00040000))
        _u32.SetWindowPos(h, 0, 0, 0, 0, 0, 0x0001 | 0x0002 | 0x0004 | 0x0010 | 0x0020)  # FRAMECHANGED only


def hard_exit():
    """Exit and take the Flet client window with us (os._exit alone leaves flet.exe running)."""
    for c in psutil.Process().children(recursive=True):
        try:
            c.kill()
        except psutil.Error:
            pass
    os._exit(0)


STARTUP_LNK = os.path.join(os.environ.get("APPDATA", ""), r"Microsoft\Windows\Start Menu\Programs\Startup\ResMon.lnk")


def startup_enabled():
    return os.path.exists(STARTUP_LNK)


def set_startup(on):
    if not on:
        if os.path.exists(STARTUP_LNK):
            os.remove(STARTUP_LNK)
        return
    import win32com.client
    here = os.path.dirname(os.path.abspath(__file__))
    pyw = os.path.join(os.path.dirname(sys.executable), "pythonw.exe")
    sc = win32com.client.Dispatch("WScript.Shell").CreateShortcut(STARTUP_LNK)
    sc.TargetPath = pyw if os.path.exists(pyw) else sys.executable
    started = os.path.join(here, "start.pyw")  # base interpreter + venv packages: start the same way again
    sc.Arguments = f'"{started if sys.prefix == sys.base_prefix and os.path.exists(started) else os.path.join(here, "app.py")}"'
    sc.WorkingDirectory = here
    sc.Description = "ResMon resource monitor"
    sc.save()


# ---------------------------------------------------------------- formatting / colour

def fmt_bytes(b):
    b = float(b or 0)
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if b < 1024 or unit == "TB":
            return f"{b:.0f} {unit}" if unit in ("B", "KB") or b >= 100 else f"{b:.1f} {unit}"
        b /= 1024


def fmt_rate(b):
    return fmt_bytes(b) + "/s"


def fmt_metric(key, v):
    return f"{v:.1f}%" if key in ("cpu", "gpu") else fmt_bytes(v)


def mix(c1, c2, t):
    a = [int(c1[i:i + 2], 16) for i in (1, 3, 5)]
    b = [int(c2[i:i + 2], 16) for i in (1, 3, 5)]
    return "#" + "".join(f"{round(x + (y - x) * t):02X}" for x, y in zip(a, b))


def app_color(name):
    return PALETTE[zlib.crc32(name.lower().encode()) % len(PALETTE)]


def heat_color(cpu):  # 0..~25% of the machine -> cold slate to hot orange
    return mix("#2A3445", "#FF6A3D", min(1.0, cpu / 25.0) ** 0.6)


def quantize(c, steps=8):
    """Heat colours change in a few steps only, so tiles are not repainted on every CPU wobble."""
    return c if c is None else round(c * steps / 25.0) * 25.0 / steps


def isolate(control, height=None):
    """Wrap in a one-item ListView: Flutter gives list items their own repaint boundary, so the 60 fps
    of an animation inside stops repainting the rest of the window (measured: 160% -> ? of a core)."""
    return ft.ListView([control], height=height, expand=height is None, spacing=0, padding=0)


# ---------------------------------------------------------------- gauges

class Gauge:
    """Label, value, sub line (or animated per-core bars) and a smoothed sparkline."""

    def __init__(self, title, color, color2=None, cores=0, small=False):
        self.color, self.color2, self.w = color, color2, 170
        self.sh = 28 if small else 34
        self.value = ft.Text("-", size=15 if small else 20, weight=ft.FontWeight.W_600, color=TEXT, no_wrap=True)
        self.sub = ft.Text("", size=11, color=MUTED, no_wrap=True, overflow=ft.TextOverflow.ELLIPSIS)
        self.spark = cv.Canvas(height=self.sh, on_resize=self._resize, resize_interval=200)
        self.fills = []
        if cores:
            tracks = []
            for _ in range(cores):
                # a full-height bar slid down inside a clipped track: a translation only repaints, while a
                # height animation re-lays out the window every frame (measured 140% of a core for 12 bars).
                # (animate_scale cannot do it: Flutter's AnimatedScale is uniform-only, scale_y is dropped)
                f = ft.Container(bgcolor=color, border_radius=2, height=12, offset=ft.Offset(0, 0.9),
                                 animate_offset=VALUE_ANIM)
                self.fills.append(f)
                tracks.append(ft.Container(f, height=12, expand=True, bgcolor=LINE, border_radius=2,
                                           clip_behavior=ft.ClipBehavior.HARD_EDGE))
            third = ft.Row(tracks, spacing=3, height=12)
        else:
            third = self.sub
        dot = ft.Container(width=7, height=7, bgcolor=color, border_radius=4)
        if small:  # label and value on one line
            head = ft.Row([dot, ft.Text(title, size=10, color=MUTED, weight=ft.FontWeight.W_700, expand=True),
                           self.value], spacing=6, vertical_alignment=ft.CrossAxisAlignment.CENTER)
            rows = [head, third, self.spark]
        else:
            head = ft.Row([dot, ft.Text(title, size=10, color=MUTED, weight=ft.FontWeight.W_700)], spacing=6)
            rows = [head, self.value, third, self.spark]
        self.view = ft.Container(ft.Column(rows, spacing=3, horizontal_alignment=ft.CrossAxisAlignment.STRETCH),
                                 padding=ft.Padding.only(left=12, right=12, top=8 if small else 10,
                                                         bottom=6 if small else 8),
                                 bgcolor=CARD, border_radius=12, expand=1)

    def _resize(self, e):
        self.w = e.width

    def _curve(self, series, top, color, fill):
        """Smoothed series drawn as quadratic segments through the midpoints (no corners)."""
        n, h, w = len(series), self.sh, self.w
        if n < 3:
            return []
        step = w / (HISTORY - 1)  # right-aligned: a young history grows from the right edge
        pts = [(w - (n - 1 - i) * step, h - 2 - min(1.0, v / top) * (h - 4)) for i, v in enumerate(smooth(series))]
        line = []
        for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
            line.append(cv.Path.QuadraticTo(x0, y0, (x0 + x1) / 2, (y0 + y1) / 2))
        line.append(cv.Path.LineTo(*pts[-1]))
        out = []
        if fill:
            el = [cv.Path.MoveTo(pts[0][0], h), cv.Path.LineTo(*pts[0])] + line + [cv.Path.LineTo(w, h), cv.Path.Close()]
            out.append(cv.Path(el, paint=ft.Paint(style=ft.PaintingStyle.FILL, gradient=ft.PaintLinearGradient(
                (0, 0), (0, h), ["#55" + color[1:], "#05" + color[1:]]))))
        out.append(cv.Path([cv.Path.MoveTo(*pts[0])] + line, paint=ft.Paint(
            style=ft.PaintingStyle.STROKE, stroke_width=1.6, color=color, stroke_join=ft.StrokeJoin.ROUND)))
        return out

    def set(self, value, sub, series, top, series2=None):
        self.value.value, self.sub.value = value, sub
        shapes = self._curve(list(series), top, self.color, True)
        if series2 is not None:
            shapes += self._curve(list(series2), top, self.color2, False)
        self.spark.shapes = shapes

    def set_cores(self, per_core):
        for f, v in zip(self.fills, per_core):
            f.offset = ft.Offset(0, round(1 - max(0.1, v / 100), 2))


# ---------------------------------------------------------------- treemap tile (persistent, animated)

class Tile:
    def __init__(self, t):
        self.title = ft.Text("", size=11, weight=ft.FontWeight.W_600, color="#EEFFFFFF", no_wrap=True,
                             overflow=ft.TextOverflow.ELLIPSIS, max_lines=1)
        self.value = ft.Text("", size=10, color="#B0FFFFFF", no_wrap=True, overflow=ft.TextOverflow.ELLIPSIS,
                             max_lines=1)
        self.view = ft.Container(ft.Column([self.title, self.value], spacing=1, tight=True),
                                 padding=ft.Padding.only(left=6, right=4, top=3), border_radius=3,
                                 left=t["x"], top=t["y"], width=t["w"], height=t["h"],
                                 animate_position=TILE_ANIM, animate=TILE_ANIM)
        self.rect, self.color, self.texts = None, None, None

    def set(self, t, color, value_text):
        """Apply a tile model; returns True if anything visible changed (only then is it sent)."""
        v = self.view
        changed = False
        r = (t["x"], t["y"], t["w"], t["h"])
        if self.rect is None or max(abs(a - b) for a, b in zip(r, self.rect)) > 0.5:
            v.left, v.top, v.width, v.height = r
            self.rect = r
            changed = True
        texts = (t["title"], value_text, t["header"], r[2] >= 46 and r[3] >= 18, r[2] >= 46 and r[3] >= 34)
        if texts != self.texts:
            self.texts = texts
            changed = True
            self._texts(t, value_text)
        if (color, t["header"]) != self.color:
            changed = True
            self.color = (color, t["header"])
            if t["header"]:  # a group frame: dark, its processes sit on top
                v.gradient, v.bgcolor = None, mix(color, BG, 0.55)
            else:
                v.gradient = ft.LinearGradient(begin=ft.Alignment.TOP_LEFT, end=ft.Alignment.BOTTOM_RIGHT,
                                               colors=[mix(color, "#FFFFFF", 0.18), mix(color, BG, 0.3)])
        return changed

    def _texts(self, t, value_text):
        w, h = self.rect[2], self.rect[3]
        if t["header"]:
            self.title.value = f"{t['title']}   {value_text}"
            self.title.visible, self.value.visible = True, False
        else:
            self.title.value, self.value.value = t["title"], value_text
            self.title.visible = w >= 46 and h >= 18
            self.value.visible = w >= 46 and h >= 34


# ---------------------------------------------------------------- the app

class ResMon:
    def __init__(self, page: ft.Page):
        self.page = page
        self.sampler = Sampler()
        self.tot = None
        self.procs, self.by_pid, self.groups = [], {}, {}
        self.metric, self.color_mode = "ws", "app"
        self.zoom = None              # app name when drilled into one app
        self.sel = None               # ("pid", pid) | ("group", name)
        self.hover = None
        self.model = []               # tiles of the last layout
        self.hits = []                # [(x, y, w, h, kind, key)] hit-test order (topmost first)
        self.rects = {}               # tile key -> (x, y, w, h)
        self.tile_ctl = {}            # tile key -> Tile
        self.sticky = Sticky(0.15)
        self.last_reflow = 0.0
        self.tm_w, self.tm_h = 0, 0
        self.sort_key, self.sort_desc = "ws", True
        self.query = ""
        self.mode = "tab"             # tab | panel | full
        self.busy = False             # a shell transition is running
        self.auto_hover = True        # cursor polling drives tab <-> panel (self test turns it off)
        self.user_cache = {}
        self.last_pos = (0, 0)
        self.stats = {"apply_ms": [], "stall_ms": []}
        self.build()

    # ------------------------------------------------------------ layout
    def pill_group(self, options, attr, on_pick):
        """Segmented pills bound to self.<attr>; on_pick(key) must set it."""
        pills = []

        def paint():
            for p in pills:
                on = p.data == getattr(self, attr)
                p.bgcolor = "#2A3040" if on else None
                p.content.color = TEXT if on else MUTED

        for key, label in options:
            pills.append(ft.Container(ft.Text(label, size=12, weight=ft.FontWeight.W_500), data=key,
                                      on_click=lambda e: on_pick(e.control.data), animate=UI_ANIM,
                                      padding=ft.Padding.symmetric(horizontal=10, vertical=4), border_radius=8))
        paint()
        self.pill_paint[attr] = paint
        return ft.Container(ft.Row(pills, spacing=2, tight=True), bgcolor=CARD, border_radius=10, padding=3)

    def build(self):
        p = self.page
        p.title = "ResMon"
        p.theme_mode = ft.ThemeMode.DARK
        p.padding = p.spacing = 0
        p.bgcolor = ft.Colors.TRANSPARENT
        w = p.window
        w.bgcolor = ft.Colors.TRANSPARENT
        w.frameless = w.title_bar_hidden = True
        w.skip_task_bar = w.always_on_top = True
        w.resizable = False
        w.min_width, w.min_height = 1, 1
        w.shadow = False
        w.prevent_close = True
        w.on_event = self.on_window_event
        p.on_keyboard_event = self.on_key
        self.anchor = load_anchor()
        self.tab_clip = None  # tab rect inside the docked window (see dock_tab); self.clip is the clipboard
        self.busy_drag, self.tab_win_w = False, 0
        self.bounds = self.geometry("tab")
        # first placement goes through Flet, which multiplies by the system scale when it shows the window;
        # dividing by the same scale puts it right where it belongs, then it is placed natively
        L, T, W, H = self.bounds
        ww = TAB_WIN_W * monitor_at(*self.anchor)[4]  # the docked window, not just the tab (see dock_tab)
        s = _u32.GetDpiForSystem() / 96
        w.left, w.top, w.width, w.height = (round(v / s) for v in (L + W - ww, T, ww, H))

        ncores = psutil.cpu_count() or 1

        def gauges(small):
            G = {"cpu": Gauge("CPU", ACCENT["cpu"], cores=ncores, small=small),
                 "ram": Gauge("MEMORY", ACCENT["ram"], small=small)}
            if small:
                G["commit"] = Gauge("COMMIT", ACCENT["commit"], small=small)
            G.update(gpu=Gauge("GPU", ACCENT["gpu"], small=small), vram=Gauge("VRAM", ACCENT["vram"], small=small),
                     disk=Gauge("DISK  read / write", ACCENT["disk"], ACCENT["disk2"], small=small),
                     net=Gauge("NETWORK  down / up", ACCENT["net"], ACCENT["net2"], small=small))
            return G

        self.G_full, self.G_panel = gauges(False), gauges(True)
        self.header = isolate(ft.Container(ft.Row([g.view for g in self.G_full.values()], spacing=10,
                                                  vertical_alignment=ft.CrossAxisAlignment.STRETCH), height=112), 112)

        # treemap toolbar
        self.crumb = ft.Row(spacing=4, tight=True)
        self.count = ft.Text("", size=12, color=FAINT)
        self.pill_paint = {}
        metric_pills = self.pill_group([(k, v[0]) for k, v in METRICS.items()], "metric", self.set_metric)
        color_pills = self.pill_group([("app", "App"), ("heat", "CPU heat")], "color_mode", self.set_color)
        toolbar = ft.Row([self.crumb, self.count, ft.Container(expand=True),
                          ft.Text("Size", size=12, color=FAINT), metric_pills,
                          ft.Container(width=8), ft.Text("Colour", size=12, color=FAINT), color_pills],
                         spacing=8, vertical_alignment=ft.CrossAxisAlignment.CENTER)

        # treemap: persistent tile controls + two animated outlines + tooltip, under one gesture detector
        self.tile_stack = ft.Stack([], expand=True)

        def outline(width, color, ms):
            return ft.Container(border=ft.Border.all(width, color), border_radius=4, visible=False,
                                left=0, top=0, width=10, height=10,
                                animate_position=ft.Animation(ms, EASE), animate=ft.Animation(ms, EASE))

        self.hl_sel = outline(2.5, "#FFFFFF", 300)
        self.hl_hover = outline(1.5, "#90FFFFFF", 140)
        self.tip_title = ft.Text("", size=13, weight=ft.FontWeight.W_600, color=TEXT)
        self.tip_body = ft.Text("", size=11, color=MUTED)
        self.tip = ft.Container(ft.Column([self.tip_title, self.tip_body], spacing=4, tight=True),
                                width=320, padding=12, bgcolor="#F51C2029", border_radius=10,
                                border=ft.Border.all(1, LINE), visible=False, left=0, top=0,
                                shadow=ft.BoxShadow(blur_radius=18, color="#90000000"))
        self.empty = ft.Container(ft.Text("Nothing uses this resource right now", color=FAINT),
                                  alignment=ft.Alignment.CENTER, expand=True, visible=False)
        stack = ft.Stack([self.tile_stack, self.hl_sel, self.hl_hover, self.empty, self.tip], expand=True)
        self.tm = ft.GestureDetector(stack, hover_interval=40, on_hover=self.on_hover, on_exit=self.on_exit,
                                     on_tap_down=self.on_tap, on_double_tap=self.on_double_tap,
                                     mouse_cursor=ft.MouseCursor.CLICK)
        self.tm_holder = ft.Container(self.tm, height=300)
        tm_box = ft.Container(isolate(self.tm_holder), expand=3, border_radius=10, bgcolor="#12151B",
                              clip_behavior=ft.ClipBehavior.HARD_EDGE, on_size_change=self.on_tm_resize,
                              size_change_interval=150)

        # selection bar
        self.sel_dot = ft.Container(width=10, height=10, border_radius=5, animate=UI_ANIM)
        self.sel_text = ft.Text("", size=12, color=MUTED, no_wrap=True, overflow=ft.TextOverflow.ELLIPSIS,
                                expand=True)

        def act(icon, label, fn, color=MUTED):
            return ft.TextButton(content=ft.Text(label, size=12), icon=icon, icon_color=color, on_click=fn,
                                 style=ft.ButtonStyle(color=color, padding=ft.Padding.symmetric(horizontal=10)))

        self.prio = ft.PopupMenuButton(
            content=ft.Row([ft.Icon(ft.Icons.SPEED, size=16, color=MUTED), ft.Text("Priority", size=12, color=MUTED)],
                           spacing=6, tight=True),
            items=[ft.PopupMenuItem(content=ft.Text(t), data=v, on_click=self.set_priority) for t, v in (
                ("Idle", psutil.IDLE_PRIORITY_CLASS), ("Below normal", psutil.BELOW_NORMAL_PRIORITY_CLASS),
                ("Normal", psutil.NORMAL_PRIORITY_CLASS))], tooltip="Set priority")
        self.actions = ft.Row([act(ft.Icons.TAG, "Copy PID", self.copy_pid),
                               act(ft.Icons.TERMINAL, "Copy command line", self.copy_cmd),
                               act(ft.Icons.FOLDER_OPEN_OUTLINED, "Open location", self.open_location),
                               ft.Container(self.prio, padding=ft.Padding.symmetric(horizontal=8)),
                               act(ft.Icons.CLOSE, "End task", self.ask_kill, DANGER)], spacing=0,
                              opacity=0, animate_opacity=UI_ANIM, disabled=True)
        self.selbar = ft.Container(ft.Row([self.sel_dot, self.sel_text, self.actions], spacing=10,
                                          vertical_alignment=ft.CrossAxisAlignment.CENTER),
                                   bgcolor=CARD, border_radius=10, padding=ft.Padding.symmetric(horizontal=12),
                                   height=40)

        # table: a fixed pool of rows, values mutated in place
        self.cols = [("name", "Name", 210, False), ("pid", "PID", 70, True), ("cpu", "CPU", 64, True),
                     ("ws", "RAM", 82, True), ("priv", "Private", 82, True), ("gpu_mem", "VRAM", 82, True),
                     ("gpu", "GPU", 60, True), ("threads", "Threads", 64, True), ("create", "Started", 110, True),
                     ("cmd", "Command line", None, False)]
        self.search = ft.TextField(hint_text="Filter by name, PID or command line", dense=True, height=36,
                                   text_size=13, prefix_icon=ft.Icons.SEARCH, border=ft.NoInputBorder(),
                                   content_padding=ft.Padding.symmetric(horizontal=10, vertical=8),
                                   on_change=self.on_search)
        self.table_info = ft.Text("", size=12, color=FAINT)
        self.head_cells = []
        for key, label, cw, num in self.cols:
            t = ft.Text(label, size=11, color=MUTED, weight=ft.FontWeight.W_600,
                        text_align=ft.TextAlign.RIGHT if num else ft.TextAlign.LEFT)
            self.head_cells.append(ft.Container(t, width=cw, expand=cw is None, data=key, on_click=self.on_sort,
                                                padding=ft.Padding.symmetric(horizontal=6)))
        self.rows = []
        for _ in range(120):
            cells = [ft.Text("", size=12, color=MUTED, no_wrap=True, overflow=ft.TextOverflow.ELLIPSIS,
                             text_align=ft.TextAlign.RIGHT if num else ft.TextAlign.LEFT) for *_, num in self.cols]
            cells[0].color = TEXT
            row = ft.Container(ft.Row([ft.Container(c, width=cw, expand=cw is None,
                                                    padding=ft.Padding.symmetric(horizontal=6))
                                       for c, (_, _, cw, _) in zip(cells, self.cols)], spacing=0),
                               height=26, border_radius=6, on_click=self.on_row, visible=False,
                               animate=ft.Animation(200, EASE))
            row.cells, row.vals = cells, None
            self.rows.append(row)
        self.table = ft.ListView(self.rows, spacing=0, expand=True)
        table_box = ft.Container(ft.Column([
            ft.Row([ft.Container(self.search, width=360, bgcolor=CARD, border_radius=10), self.table_info],
                   spacing=12),
            ft.Container(ft.Row(self.head_cells, spacing=0), border=ft.Border.only(bottom=ft.BorderSide(1, LINE)),
                         padding=ft.Padding.only(bottom=6)),
            self.table], spacing=8, expand=True), expand=2)
        self.body = ft.Column([toolbar, tm_box, self.selbar, table_box], spacing=10, expand=True)

        # ---- full view: card with its own title bar
        self.startup_item = ft.PopupMenuItem(content=ft.Text("Start with Windows"), on_click=self.toggle_startup)
        menu = ft.PopupMenuButton(icon=ft.Icons.MORE_HORIZ, icon_color=MUTED, icon_size=18, tooltip="Menu", items=[
            self.startup_item, ft.PopupMenuItem(content=ft.Text("Quit ResMon"), on_click=self.quit)])
        topbar = ft.Row([
            ft.WindowDragArea(ft.Container(ft.Row([
                ft.Container(width=10, height=10, border_radius=5, bgcolor=ACCENT["cpu"]),
                ft.Text("ResMon", size=13, weight=ft.FontWeight.W_700, color=TEXT),
                ft.Text("drag to move · Esc to dock", size=11, color=FAINT)], spacing=8),
                height=30, padding=ft.Padding.only(left=4)), expand=True),
            menu,
            ft.IconButton(ft.Icons.REMOVE, icon_color=MUTED, icon_size=18, tooltip="Dock to the edge",
                          on_click=self.to_tab),
            ft.IconButton(ft.Icons.CLOSE, icon_color=MUTED, icon_size=18, tooltip="Dock to the edge",
                          on_click=self.to_tab)], spacing=0, height=32)
        self.full_inner = ft.Column([topbar, self.header, self.body], spacing=10, expand=True, visible=False,
                                    opacity=0, animate_opacity=ft.Animation(220, EASE))
        # the card animates its size inside a transparent full-size window: Flutter does the motion at frame
        # rate, Python only sends start and end (moving the OS window step by step was ~80 ms a step)
        self.card = ft.Container(self.full_inner, bgcolor=BG, border=ft.Border.all(1, LINE),
                                 width=TAB_W, height=TAB_H,
                                 border_radius=ft.BorderRadius.only(top_left=14, bottom_left=14),
                                 padding=ft.Padding.only(left=14, right=14, top=6, bottom=14), animate=UI_ANIM)

        # ---- docked tab: three live bars (CPU, RAM, GPU)
        self.tab_bars = []
        tracks = []
        for key in ("cpu", "ram", "gpu"):
            fill = ft.Container(width=4, height=TAB_H - 36, bgcolor=ACCENT[key], border_radius=2,
                                offset=ft.Offset(0, 0.98), animate_offset=VALUE_ANIM)  # slid, see Gauge
            self.tab_bars.append(fill)
            tracks.append(ft.Container(fill, width=4, height=TAB_H - 36, bgcolor=LINE, border_radius=2,
                                       clip_behavior=ft.ClipBehavior.HARD_EDGE))
        self.tab_view = ft.GestureDetector(
            ft.Container(ft.Row(tracks, spacing=3, alignment=ft.MainAxisAlignment.CENTER,
                                vertical_alignment=ft.CrossAxisAlignment.CENTER),
                         width=TAB_W, height=TAB_H, bgcolor="#F2222631",
                         border=ft.Border.all(1, "#3A4150"), padding=ft.Padding.only(left=3),
                         border_radius=ft.BorderRadius.only(top_left=12, bottom_left=12)),
            on_tap=self.to_full, on_secondary_tap=self.to_panel, on_pan_start=self.start_drag,
            mouse_cursor=ft.MouseCursor.MOVE)

        # ---- hover panel: slides in from the right edge
        def foot(icon, tip, fn, color=MUTED):
            return ft.IconButton(icon, icon_color=color, icon_size=18, tooltip=tip, on_click=fn)

        self.startup_btn = foot(ft.Icons.POWER_SETTINGS_NEW, "", self.toggle_startup)
        self.paint_startup()
        panel_col = ft.Column(
            [ft.Row([ft.Text("ResMon", size=13, weight=ft.FontWeight.W_700, color=TEXT, expand=True),
                     ft.Text("click for details", size=10, color=FAINT)])]
            + [g.view for g in self.G_panel.values()]
            + [ft.Row([foot(ft.Icons.OPEN_IN_FULL, "Open full monitor", self.to_full, ACCENT["cpu"]),
                       ft.Container(expand=True), self.startup_btn,
                       foot(ft.Icons.LOGOUT, "Quit ResMon", self.quit)], spacing=0)],
            spacing=6, expand=True)
        self.panel_inner = ft.Container(
            ft.GestureDetector(panel_col, on_tap=self.to_full, mouse_cursor=ft.MouseCursor.CLICK),
            width=PANEL_W, height=PANEL_H, bgcolor="#F70E1014", border=ft.Border.all(1, LINE),
            border_radius=ft.BorderRadius.only(top_left=16, bottom_left=16),
            padding=ft.Padding.only(left=12, right=10, top=10, bottom=4),
            offset=ft.Offset(1, 0), animate_offset=UI_ANIM)
        self.panel_view = ft.Container(self.panel_inner, width=PANEL_W, height=PANEL_H,
                                       clip_behavior=ft.ClipBehavior.HARD_EDGE)

        # one root, always anchored at the right-centre: swapping faces never flashes at the left
        self.root = ft.Container(self.tab_view, expand=True, alignment=ft.Alignment.CENTER_RIGHT)
        p.add(self.root)
        self.render_crumb()
        self.paint_sort()
        self.render_selbar()

    # ------------------------------------------------------------ gauges (fast loop)
    def render_totals(self):
        s = self.tot
        if self.mode == "tab":
            g = s["gpu"]
            for bar, v in zip(self.tab_bars, (s["cpu"], s["ram_used"] / s["ram_total"] * 100, g.get("util") or 0)):
                bar.offset = ft.Offset(0, round(1 - max(0.02, v / 100), 3))
            return [self.tab_view]
        G = self.G_panel if self.mode == "panel" else self.G_full
        self.update_gauges(G)
        return [g.view for g in G.values()]

    def update_gauges(self, G):
        s, h = self.tot, self.sampler.hist
        G["cpu"].set(f"{s['cpu']:.0f}%", "", h["cpu"], 100)
        G["cpu"].set_cores(s["per_core"])
        G["ram"].set(f"{s['ram_used'] / 2**30:.1f} / {fmt_bytes(s['ram_total'])}",
                     f"Commit {fmt_bytes(s['commit'])} of {fmt_bytes(s['commit_limit'])}", h["ram"], 100)
        if "commit" in G:
            G["commit"].set(f"{s['commit'] / 2**30:.1f} / {fmt_bytes(s['commit_limit'])}",
                            f"{s['commit'] / s['commit_limit'] * 100:.0f}% of the commit limit", h["commit"], 100)
        g = s["gpu"]
        if g.get("util") is not None:
            extra = " · ".join(x for x in (f"{g['temp']}°C" if g.get("temp") is not None else "",
                                            f"{g['power']:.0f} W" if g.get("power") is not None else "") if x)
            G["gpu"].set(f"{g['util']}%", extra or g.get("name", ""), h["gpu"], 100)
        else:
            G["gpu"].set("n/a", s.get("gpu_err") or "No NVIDIA GPU", h["gpu"], 100)
        if g.get("mem_total"):
            G["vram"].set(f"{g['mem_used'] / 2**30:.1f} / {fmt_bytes(g['mem_total'])}",
                          g.get("name", "").replace("NVIDIA GeForce ", ""), h["vram"], 100)
        r = s["rates"]
        G["disk"].set(fmt_rate(r["disk_r"]), f"write {fmt_rate(r['disk_w'])}", h["disk_r"],
                      max(1 << 20, max(h["disk_r"]), max(h["disk_w"])), h["disk_w"])
        G["net"].set(fmt_rate(r["net_d"]), f"up {fmt_rate(r['net_u'])}", h["net_d"],
                     max(100 << 10, max(h["net_d"]), max(h["net_u"])), h["net_u"])

    # ------------------------------------------------------------ processes (slow loop)
    def set_procs(self, procs):
        self.procs = procs
        self.by_pid = {q["pid"]: q for q in procs}
        groups = {}
        for q in procs:
            groups.setdefault(q["name"], []).append(q)
        self.groups = groups
        if self.zoom and self.zoom not in groups:
            self.zoom = None
            self.render_crumb()
        if self.sel and ((self.sel[0] == "pid" and self.sel[1] not in self.by_pid)
                         or (self.sel[0] == "group" and self.sel[1] not in groups)):
            self.sel = None

    def layout(self):
        """Treemap model for the current state (pure; ~1 ms, safe to run in a thread)."""
        if not self.tm_w:
            return []
        return treemap_model(self.procs, METRICS[self.metric][1], self.zoom, self.tm_w, self.tm_h, self.sticky)

    def tile_color(self, t):
        if self.color_mode == "heat":
            return heat_color(quantize(t["cpu"]))
        return mix(app_color(t["name"]), BG, 0.25)

    def render_treemap(self, model, reflow=True):
        """Diff the new layout against the live tiles; returns the controls that need sending.
        reflow=False keeps every tile where it is (labels and colours only): reflows are rate-limited,
        because each one animates every tile and that is what costs."""
        if not reflow:
            old = {t["key"]: t for t in self.model}
            model = [dict(t, x=o["x"], y=o["y"], w=o["w"], h=o["h"], header=o["header"])
                     for t in model if (o := old.get(t["key"])) is not None]
        else:
            self.last_reflow = time.monotonic()
        self.model = model
        keys = [t["key"] for t in model]
        gone = set(self.tile_ctl) - set(keys)
        for k in gone:
            del self.tile_ctl[k]
        order_changed = bool(gone)
        dirty = []
        for t in model:
            tile = self.tile_ctl.get(t["key"])
            if tile is None:
                tile = self.tile_ctl[t["key"]] = Tile(t)
                order_changed = True
            if tile.set(t, self.tile_color(t), fmt_metric(self.metric, t["value"])):
                dirty.append(tile.view)
        if order_changed:  # groups must stay under their children: keep the stack in model order
            self.tile_stack.controls = [self.tile_ctl[k].view for k in keys]
            dirty = [self.tile_stack]
        self.rects = {t["key"]: (t["x"], t["y"], t["w"], t["h"]) for t in model}
        hits = []
        for t in reversed(model):  # children were laid out after their group: test them first
            h = HEADER if t["header"] else t["h"]
            hits.append((t["x"], t["y"], t["w"], h) + t["hit"])
        self.hits = hits
        self.empty.visible = not model and bool(self.procs)
        n = len(self.groups.get(self.zoom, [])) if self.zoom else len({t["name"] for t in model})
        self.count.value = f"{n} processes" if self.zoom else f"{n} apps · {len(self.procs)} processes"
        self.paint_outlines()
        return dirty + [self.hl_sel, self.hl_hover, self.empty, self.count]

    def geometry_changed(self, model):
        old = {t["key"]: (t["x"], t["y"], t["w"], t["h"]) for t in self.model}
        if set(old) != {t["key"] for t in model}:
            return True
        return any(max(abs(a - b) for a, b in zip(old[t["key"]], (t["x"], t["y"], t["w"], t["h"]))) > 1
                   for t in model)

    def rect_of(self, target):
        kind, key = target
        return self.rects.get(("p", key) if kind == "pid" else ("g", key))

    def place(self, box, target):
        r = self.rect_of(target) if target else None
        box.visible = r is not None
        if r:
            box.left, box.top, box.width, box.height = r

    def paint_outlines(self):
        self.place(self.hl_sel, self.sel)
        self.place(self.hl_hover, self.hover if self.hover != self.sel else None)

    def hit(self, x, y):
        for hx, hy, hw, hh, kind, key in self.hits:
            if hx <= x <= hx + hw and hy <= y <= hy + hh:
                return kind, key
        return None

    def on_tm_resize(self, e):
        self.tm_w, self.tm_h = e.width, e.height
        self.tm_holder.height = e.height
        if self.procs and self.mode == "full":
            self.render_treemap(self.layout())

    def relayout(self):
        """User changed metric/colour/zoom: new layout now, tiles glide to it."""
        self.sticky = Sticky(0.15)
        self.last_reflow = 0.0
        self.render_treemap(self.layout())
        self.render_table()

    # ------------------------------------------------------------ hover / tooltip
    def username(self, pid):
        if pid not in self.user_cache:
            try:
                self.user_cache[pid] = psutil.Process(pid).username()
            except Exception:
                self.user_cache[pid] = "(access denied)"
        return self.user_cache[pid]

    def describe(self, target):
        kind, key = target
        pdh = self.tot["pdh_ok"] if self.tot else False
        if kind == "pid":
            q = self.by_pid.get(key)
            if not q:
                return "", ""
            gm = f"{fmt_bytes(q['gpu_mem'])} VRAM · {q['gpu']:.0f}% GPU" if pdh else "GPU n/a"
            body = (f"PID {q['pid']}  ·  parent {q['ppid']}  ·  {self.username(q['pid'])}\n"
                    f"RAM {fmt_bytes(q['ws'])}  ·  private {fmt_bytes(q['priv'])}\n"
                    f"CPU {q['cpu']:.1f}%  ·  {gm}  ·  {q['threads']} threads\n"
                    f"{(q['cmd'] or '(command line not accessible)')[:300]}")
            return q["name"], body
        qs = self.groups.get(key, [])
        pids = ", ".join(str(q["pid"]) for q in qs[:12]) + (" …" if len(qs) > 12 else "")
        body = (f"{len(qs)} processes  ·  PIDs {pids}\n"
                f"RAM {fmt_bytes(sum(q['ws'] for q in qs))}  ·  private {fmt_bytes(sum(q['priv'] for q in qs))}\n"
                f"CPU {sum(q['cpu'] for q in qs):.1f}%  ·  {fmt_bytes(sum(q['gpu_mem'] for q in qs))} VRAM  ·  "
                f"{max((q['gpu'] for q in qs), default=0):.0f}% GPU\n"
                f"Double-click to show its processes")
        return key, body

    def on_hover(self, e):
        ft.context.disable_auto_update()
        x, y = e.local_position.x, e.local_position.y
        self.last_pos = (x, y)
        target = self.hit(x, y)
        dirty = [self.tip]
        if target:
            if target != self.hover or not self.tip.visible:
                self.tip_title.value, self.tip_body.value = self.describe(target)
            self.tip.left = x + 16 if x + 16 + 320 < self.tm_w else max(0, x - 336)
            self.tip.top = y + 16 if y + 150 < self.tm_h else max(0, y - 150)
        self.tip.visible = target is not None
        if target != self.hover:
            self.hover = target
            self.paint_outlines()
            dirty += [self.hl_hover]
        self.page.update(*dirty)

    def on_exit(self, e):
        ft.context.disable_auto_update()
        self.hover, self.tip.visible = None, False
        self.paint_outlines()
        self.page.update(self.tip, self.hl_hover)

    def on_tap(self, e):
        self.last_pos = (e.local_position.x, e.local_position.y)
        self.select(self.hit(*self.last_pos))

    def on_double_tap(self, e):
        target = self.hit(*self.last_pos)
        if not self.zoom and target:
            name = target[1] if target[0] == "group" else self.by_pid.get(target[1], {}).get("name")
            if name and len(self.groups.get(name, [])) > 1:
                self.set_zoom(name)

    def set_zoom(self, name):
        self.zoom, self.hover, self.tip.visible = name, None, False
        if self.sel and self.sel[0] == "group":
            self.sel = None
        self.render_crumb()
        self.relayout()
        self.render_selbar()

    def render_crumb(self):
        root = ft.TextButton(content=ft.Text("All processes", size=14, weight=ft.FontWeight.W_600,
                                             color=MUTED if self.zoom else TEXT),
                             on_click=lambda e: self.set_zoom(None), style=ft.ButtonStyle(padding=4))
        items = [root]
        if self.zoom:
            items += [ft.Icon(ft.Icons.CHEVRON_RIGHT, size=16, color=FAINT),
                      ft.Text(self.zoom, size=14, weight=ft.FontWeight.W_600, color=TEXT)]
        self.crumb.controls = items

    def set_metric(self, key):
        self.metric = key
        self.pill_paint["metric"]()
        self.relayout()

    def set_color(self, key):
        self.color_mode = key
        self.pill_paint["color_mode"]()
        self.render_treemap(self.model)

    # ------------------------------------------------------------ selection + actions
    def select(self, target):
        self.sel = target
        self.paint_outlines()
        self.render_selbar()
        self.render_table()

    def selected_procs(self):
        if not self.sel:
            return []
        if self.sel[0] == "pid":
            return [self.by_pid[self.sel[1]]] if self.sel[1] in self.by_pid else []
        return list(self.groups.get(self.sel[1], []))

    def render_selbar(self):
        qs = self.selected_procs()
        self.actions.opacity, self.actions.disabled = (1, False) if qs else (0, True)
        if not qs:
            self.sel_dot.bgcolor = FAINT
            self.sel_text.value = "Click a tile or a row to select it. Double-click an app to see its processes."
            self.sel_text.color = MUTED
            return
        name = qs[0]["name"]
        self.sel_dot.bgcolor = app_color(name)
        self.sel_text.color = TEXT
        if self.sel[0] == "pid":
            q = qs[0]
            self.sel_text.value = f"{name}   PID {q['pid']}   ·   {fmt_bytes(q['ws'])}   ·   CPU {q['cpu']:.1f}%"
        else:
            self.sel_text.value = (f"{name}   {len(qs)} processes   ·   {fmt_bytes(sum(q['ws'] for q in qs))}"
                                   f"   ·   CPU {sum(q['cpu'] for q in qs):.1f}%")

    def toast(self, msg, error=False):
        self.page.show_dialog(ft.SnackBar(ft.Text(msg, color=TEXT), bgcolor="#3A1F25" if error else "#1F2A24",
                                          duration=ft.Duration(seconds=4)))

    async def clip(self, text, what):
        await ft.Clipboard().set(text)
        self.toast(f"Copied {what}")

    async def copy_pid(self, e=None):
        qs = self.selected_procs()
        if qs:
            await self.clip(", ".join(str(q["pid"]) for q in qs), "PID" if len(qs) == 1 else f"{len(qs)} PIDs")

    async def copy_cmd(self, e=None):
        qs = self.selected_procs()
        if qs:
            cmd = qs[0]["cmd"] or self.sampler.cmdline(qs[0]["pid"], qs[0]["create"])
            if not cmd:
                self.toast("Command line not accessible (needs administrator)", True)
                return
            await self.clip(cmd, "command line")

    def open_location(self, e=None):
        qs = self.selected_procs()
        if not qs:
            return
        try:
            subprocess.Popen(["explorer", "/select,", psutil.Process(qs[0]["pid"]).exe()])
        except psutil.AccessDenied:
            self.toast("Access denied: run ResMon as administrator to locate this file", True)
        except Exception as ex:
            self.toast(f"Cannot open location: {ex}", True)

    def set_priority(self, e):
        done, denied = 0, 0
        for q in self.selected_procs():
            try:
                psutil.Process(q["pid"]).nice(e.control.data)
                done += 1
            except psutil.AccessDenied:
                denied += 1
            except psutil.NoSuchProcess:
                pass
        if denied:
            self.toast(f"Priority set on {done}, access denied on {denied} (run as administrator)", True)
        else:
            self.toast(f"Priority set to {e.control.content.value} on {done} process{'es' if done != 1 else ''}")

    def ask_kill(self, e=None):
        qs = self.selected_procs()
        if not qs:
            return
        tree = ft.Checkbox(label="Also end all child processes (process tree)", value=False)
        what = f"{qs[0]['name']} (PID {qs[0]['pid']})" if len(qs) == 1 else f"all {len(qs)} {qs[0]['name']} processes"
        warn = "  This is ResMon itself." if any(q["pid"] == os.getpid() for q in qs) else ""

        def confirm(ev):
            self.page.pop_dialog()
            self.kill([q["pid"] for q in qs], tree.value)

        self.page.show_dialog(ft.AlertDialog(
            modal=True, bgcolor=CARD, title=ft.Text("End task?", size=18),
            content=ft.Column([ft.Text(f"End {what}? Unsaved data in it will be lost.{warn}", color=MUTED), tree],
                              tight=True, spacing=14, width=420),
            actions=[ft.TextButton("Cancel", on_click=lambda ev: self.page.pop_dialog()),
                     ft.FilledButton("End task", on_click=confirm, bgcolor=DANGER, color="#FFFFFF")]))

    def kill(self, pids, tree):
        """Kill pids (and their descendants if tree). Returns (killed, denied)."""
        targets = {}
        for pid in pids:
            try:
                p = psutil.Process(pid)
                targets[pid] = p
                if tree:
                    for c in p.children(recursive=True):
                        targets[c.pid] = c
            except psutil.NoSuchProcess:
                pass
        killed, denied = 0, 0
        for p in targets.values():
            try:
                p.kill()
                killed += 1
            except psutil.NoSuchProcess:
                pass
            except psutil.AccessDenied:
                denied += 1
        if denied:
            self.toast(f"Ended {killed}, access denied on {denied}: run ResMon as administrator for system processes", True)
        else:
            self.toast(f"Ended {killed} process{'es' if killed != 1 else ''}")
        return killed, denied

    # ------------------------------------------------------------ table
    def on_search(self, e):
        self.query = e.control.value.strip().lower()
        self.render_table()

    def on_sort(self, e):
        key = e.control.data
        self.sort_desc = not self.sort_desc if key == self.sort_key else key not in ("name", "cmd")
        self.sort_key = key
        self.paint_sort()
        self.render_table()

    def paint_sort(self):
        for c, (key, label, _, _) in zip(self.head_cells, self.cols):
            c.content.value = label + ((" ↓" if self.sort_desc else " ↑") if key == self.sort_key else "")
            c.content.color = TEXT if key == self.sort_key else MUTED

    def on_row(self, e):
        self.select(("pid", e.control.data))

    def render_table(self):
        qs = self.groups.get(self.zoom, []) if self.zoom else self.procs
        if self.query:
            ql = self.query
            qs = [q for q in qs if ql in q["name"].lower() or ql == str(q["pid"]) or ql in q["cmd"].lower()]
        k = self.sort_key
        qs = sorted(qs, key=(lambda q: q[k].lower()) if k in ("name", "cmd") else (lambda q: q[k]),
                    reverse=self.sort_desc)
        sel_pids = {q["pid"] for q in self.selected_procs()}
        self.table_info.value = f"{min(len(qs), len(self.rows))} of {len(qs)} shown"
        pdh = self.tot["pdh_ok"] if self.tot else False
        dirty = [self.table_info]
        for row, q in zip(self.rows, qs + [None] * len(self.rows)):
            if q is None:
                if row.visible:
                    row.visible = False
                    dirty.append(row)
                continue
            bg = "#243049" if q["pid"] in sel_pids else None
            vals = (q["name"], str(q["pid"]), f"{q['cpu']:.1f}", fmt_bytes(q["ws"]), fmt_bytes(q["priv"]),
                    fmt_bytes(q["gpu_mem"]) if q["gpu_mem"] else ("-" if pdh else "n/a"),
                    f"{q['gpu']:.0f}%" if q["gpu"] >= 0.5 else "-", str(q["threads"]),
                    dt.datetime.fromtimestamp(q["create"]).strftime("%d %b %H:%M") if q["create"] else "",
                    q["cmd"])
            if vals != row.vals or bg != row.bgcolor or not row.visible:  # untouched rows are not sent
                row.visible, row.data, row.bgcolor, row.vals = True, q["pid"], bg, vals
                for cell, v in zip(row.cells, vals):
                    cell.value = v
                dirty.append(row)
        return dirty

    # ------------------------------------------------------------ shell: tab <-> panel <-> full (one window)
    def geometry(self, mode):
        """Bounds in physical px on the anchor's monitor, flush with its right edge; tab and panel are
        centred on the anchor's height, the full view on the monitor."""
        L, T, R, B, s = monitor_at(*self.anchor)
        if mode == "full":
            w, h = min(FULL_W * s, R - L - 12 * s), min(FULL_H * s, B - T - 24 * s)
            return (R - w, (T + B - h) / 2, w, h)
        w, h = (TAB_W, TAB_H) if mode == "tab" else (PANEL_W, PANEL_H)
        w, h = w * s, min(h * s, B - T)
        return (R - w, min(max(self.anchor[1] - h / 2, T), B - h), w, h)

    def dock_tab(self):
        """Windows will not shrink the window below a minimum width (it depends on the scale): keep the window
        flush with the edge at whatever width it has and clip it to the tab, so the excess neither shows nor
        catches clicks. Cheap when nothing changed (called every fast tick)."""
        strip_frame()
        wb = window_bounds()
        if not wb:
            return
        L, T, W, H = self.bounds
        # never ask for less than Windows allows: a refused width grows rightwards, over the next monitor,
        # whose different scale makes Flutter resize the window back and forth
        ww = max(round(TAB_WIN_W * monitor_at(*self.anchor)[4]), self.tab_win_w)
        target, clip = (L + W - ww, T, ww, H), (ww - W, 0, W, H)
        if clip != self.tab_clip or any(abs(a - b) > 1 for a, b in zip(wb, target)):
            place(target, clip)
            self.tab_clip = clip
            wb = window_bounds()
            if wb and wb[2] > ww + 1:  # Windows insists on more: remember it, re-flush on the next tick
                self.tab_win_w, self.tab_clip = wb[2], None

    def set_bounds(self, b):
        """Native placement on the anchor's monitor, then the face change."""
        self.bounds, self.tab_clip, self.tab_win_w = b, None, 0
        if self.mode == "tab":
            self.page.update()
            self.dock_tab()
        else:
            place(b)
            self.page.update()
        # moving to a monitor with another scale makes Flutter resize the window to its DPI suggestion:
        # place it again once that has happened
        self.page.run_task(self._settle, b)

    async def _settle(self, b):
        await asyncio.sleep(0.15)
        if self.bounds == b and not self.busy_drag:
            wb = window_bounds()
            if wb and self.mode != "tab" and any(abs(x - y) > 1 for x, y in zip(wb, b)):
                place(b)
            elif self.mode == "tab":
                self.tab_clip = None
                self.dock_tab()

    def start_drag(self, e):
        if self.mode == "tab" and not self.busy:
            self.page.run_task(self.drag)

    async def drag(self):
        """The tab follows the cursor while the button is held, then docks on the right edge of the monitor
        it was dropped on, at that height. The spot is saved for the next start."""
        self.busy = self.busy_drag = True
        try:
            while _u32.GetAsyncKeyState(0x01) & 0x8000:  # VK_LBUTTON
                x, y = cursor_pos()
                wb = window_bounds()
                if wb and self.tab_clip:  # keep the grabbed tab centred under the cursor
                    move_to(x - self.tab_clip[0] - self.tab_clip[2] / 2, y - wb[3] / 2)
                await asyncio.sleep(0.016)
            x, y = cursor_pos()
            R = monitor_at(x, y)[2]
            self.anchor = (R - 1, y)
            try:
                save_anchor(self.anchor)
            except OSError as ex:
                self.toast(f"Cannot save the tab position: {ex}", True)
            self.set_bounds(self.geometry("tab"))
        finally:
            self.busy = self.busy_drag = False

    async def to_panel(self, e=None):
        if self.busy or self.mode != "tab":
            return
        self.busy = True
        try:
            self.mode = "panel"
            self.panel_inner.offset = ft.Offset(1, 0)  # parked off the right edge, then slides in
            if self.tot:
                self.render_totals()
            self.root.content = self.panel_view
            self.set_bounds(self.geometry("panel"))
            await asyncio.sleep(0.03)
            self.panel_inner.offset = ft.Offset(0, 0)
            self.page.update()
            await asyncio.sleep(UI_ANIM.duration / 1000)
        finally:
            self.busy = False

    async def to_full(self, e=None):
        if self.busy or self.mode == "full":
            return
        self.busy = True
        try:
            start, end = self.bounds, self.geometry("full")
            s = monitor_at(*self.anchor)[4]  # bounds are physical px, the card is laid out in logical px
            self.mode = "full"
            self.full_inner.visible, self.full_inner.opacity = False, 0
            self.card.width, self.card.height = start[2] / s, start[3] / s  # card starts as big as the tab/panel...
            self.root.content = self.card
            self.set_bounds(end)
            await asyncio.sleep(0.03)
            self.card.width, self.card.height = end[2] / s, end[3] / s  # ...and grows out of the right edge
            self.page.update()
            procs = await asyncio.to_thread(self.sampler.sample_procs)  # sampled during the animation
            self.set_procs(procs)
            await asyncio.sleep(UI_ANIM.duration / 1000)
            self.full_inner.visible = True
            if self.tot:
                self.render_totals()
            if self.tm_w:  # same size as last time: no size event will come, lay out now
                self.render_treemap(self.layout())
            self.render_table()
            self.render_selbar()
            self.page.update()  # the treemap is laid out on its first size event
            await asyncio.sleep(0.03)
            self.full_inner.opacity = 1
            self.page.update()
        finally:
            self.busy = False

    async def to_tab(self, e=None):
        if self.busy or self.mode == "tab":
            return
        self.busy = True
        try:
            if self.mode == "panel":
                self.panel_inner.offset = ft.Offset(1, 0)
                self.page.update()
                await asyncio.sleep(UI_ANIM.duration / 1000)
            else:
                self.full_inner.opacity = 0
                self.page.update()
                await asyncio.sleep(0.22)
                self.full_inner.visible, self.tip.visible = False, False
                self.card.width, self.card.height = TAB_W, TAB_H  # shrink back into the edge
                self.page.update()
                await asyncio.sleep(UI_ANIM.duration / 1000)
            self.mode = "tab"
            if self.tot:
                self.render_totals()
            self.root.content = self.tab_view  # same right-centre anchor as the shrunken card
            self.set_bounds(self.geometry("tab"))
        finally:
            self.busy = False

    async def watch_hover(self):
        """Cursor polling with hysteresis: enter the tab for 150 ms -> panel; leave the panel for 400 ms -> tab.
        Polling (not hover events) because a frameless window that resizes under the cursor misses exits."""
        since = None
        while True:
            await asyncio.sleep(0.08)
            if self.busy or self.mode == "full" or not self.auto_hover:
                since = None
                continue
            x, y = cursor_pos()
            L, T, W, H = self.bounds
            m = 0 if self.mode == "tab" else 14
            inside = L - m <= x <= L + W + m and T - m <= y <= T + H + m
            if inside != (self.mode == "tab"):
                since = None
                continue
            since = since or time.monotonic()
            # the tab waits before opening the panel, so it can be grabbed and dragged
            if time.monotonic() - since >= (HOVER_OPEN if self.mode == "tab" else 0.4):
                since = None
                await (self.to_panel() if self.mode == "tab" else self.to_tab())

    async def on_window_event(self, e):
        if e.type in (ft.WindowEventType.CLOSE, ft.WindowEventType.MINIMIZE):
            if e.type == ft.WindowEventType.MINIMIZE:
                self.page.window.minimized = False
            await self.to_tab(e)

    async def on_key(self, e):
        if e.key == "Escape" and self.mode == "full":
            await self.to_tab(e)

    def paint_startup(self):
        on = startup_enabled()
        self.startup_btn.icon_color = ACCENT["gpu"] if on else MUTED
        self.startup_btn.tooltip = f"Start with Windows: {'on' if on else 'off'} (click to toggle)"
        self.startup_item.checked = on

    def toggle_startup(self, e=None):
        try:
            set_startup(not startup_enabled())
        except Exception as ex:
            self.toast(f"Cannot change the startup shortcut: {ex}", True)
        self.paint_startup()

    async def quit(self, e=None):
        self.page.window.prevent_close = False
        await self.page.window.destroy()
        hard_exit()

    # ------------------------------------------------------------ loops
    async def fast_loop(self):
        """Totals every 0.5 s; only the visible gauges are touched, in one batched update."""
        while True:
            t0 = time.perf_counter()
            self.tot = await asyncio.to_thread(self.sampler.sample_totals)
            if not self.busy:
                if self.mode == "tab":
                    self.dock_tab()
                self.page.update(*self.render_totals())
            await asyncio.sleep(max(0.05, FAST - (time.perf_counter() - t0)))

    async def slow_loop(self):
        """Processes every 1.5 s, only in full view. Sampling and layout run in the thread; the main
        loop only applies the diff."""
        while True:
            t0 = time.perf_counter()
            if self.mode == "full" and not self.busy:
                field, zoom, W, H = METRICS[self.metric][1], self.zoom, self.tm_w, self.tm_h

                def work():  # sampling + layout off the event loop
                    procs = self.sampler.sample_procs()
                    names = {q["name"] for q in procs}
                    z = zoom if zoom in names else None
                    return procs, (treemap_model(procs, field, z, W, H, self.sticky) if W else [])

                procs, m = await asyncio.to_thread(work)
                if self.mode == "full" and not self.busy:
                    a = time.perf_counter()
                    self.set_procs(procs)
                    if (field, zoom) != (METRICS[self.metric][1], self.zoom):  # user changed it meanwhile
                        m = self.layout()
                    # hysteresis in time: tiles reflow at most every REFLOW s, labels update every tick
                    reflow = time.monotonic() - self.last_reflow >= REFLOW and self.geometry_changed(m)
                    dirty = self.render_treemap(m, reflow) + self.render_table()
                    self.render_selbar()
                    self.page.update(*dirty, self.selbar, self.crumb)
                    self.stats["apply_ms"].append((time.perf_counter() - a) * 1000)
            await asyncio.sleep(max(0.05, SLOW - (time.perf_counter() - t0)))

    async def stall_probe(self):
        """Measures how late the event loop wakes up: this is the lag a hover would feel."""
        while True:
            t = time.perf_counter()
            await asyncio.sleep(0.05)
            self.stats["stall_ms"].append((time.perf_counter() - t - 0.05) * 1000)
            if len(self.stats["stall_ms"]) > 4000:
                del self.stats["stall_ms"][:2000]


async def main(page: ft.Page):
    app = ResMon(page)
    page.update()
    # Flet applies its own (logical) bounds only once the window is visible, with a scale that depends on
    # the monitor: show it transparent, let that settle, place it natively, then fade it in
    page.window.opacity = 0
    page.window.visible = True
    page.update()
    await asyncio.sleep(0.5)
    app.set_bounds(app.geometry("tab"))
    await asyncio.sleep(0.2)
    app.dock_tab()
    page.window.opacity = 1
    page.update()
    page.run_task(app.fast_loop)
    page.run_task(app.slow_loop)
    page.run_task(app.watch_hover)
    page.run_task(app.stall_probe)
    if os.environ.get("RESMON_TEST"):
        import selftest
        page.run_task(selftest.run, app)


if __name__ == "__main__":
    sys.modules.setdefault("app", sys.modules[__name__])  # selftest does `import app`: reuse this module
    ft.run(main, view=ft.AppView.FLET_APP_HIDDEN)
