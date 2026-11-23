"""In-app self test: RESMON_TEST=full. Run with pythonw so no console window opens.

Only touches processes it spawns itself. Writes PNGs and selftest.log to RESMON_OUT (default docs/selftest/).
Does not move the real mouse: hover-to-panel is driven by calling the same transition the cursor poll calls.
"""
import asyncio
import io
import os
import subprocess
import sys
import time
from types import SimpleNamespace

import psutil
from PIL import Image, ImageGrab

import app as appmod

OUT = os.environ.get("RESMON_OUT", os.path.join(os.path.dirname(__file__), "docs", "selftest"))
DUMMY = ("import subprocess, sys, time; "
         "subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(600)']); time.sleep(600)")


def log(msg):
    with open(os.path.join(OUT, "selftest.log"), "a", encoding="utf-8") as f:
        f.write(msg + "\n")


async def ensure_full(app):
    if app.mode != "full":  # someone clicked the dock button on the test window: reopen
        log("(test window was docked by a click; reopening)")
        await app.to_full()
        await asyncio.sleep(2)


async def grab_page(app):
    return Image.open(io.BytesIO(await app.page.take_screenshot(pixel_ratio=1.0))).convert("RGB")


async def shot(app, name):
    await ensure_full(app)
    app.page.update()
    await asyncio.sleep(0.8)
    (await grab_page(app)).save(os.path.join(OUT, name))
    log(f"screenshot {name}")


def edge_box(width, height):
    x, cy = appmod.load_anchor()  # physical px, on the monitor the tab is docked to
    L, T, R, B, s = appmod.monitor_at(x, cy)
    return (int(R - width * s), int(cy - height * s / 2), int(R), int(cy + height * s / 2))


def edge_shot(name, width=330, height=760):
    """Real screen capture of the right edge of the work area, to see the tab/panel in place."""
    ImageGrab.grab(bbox=edge_box(width, height)).save(os.path.join(OUT, name))
    b = appmod.window_bounds()
    log(f"edge capture {name}; window bounds " + ("none" if b is None else "(%.0f, %.0f, %.0f x %.0f)" % b))


def sheet(frames, name, cols, width):
    """Contact sheet of a frame sequence, left to right, top to bottom."""
    fr = [f.resize((width, round(f.height * width / f.width))) for f in frames]
    rows = (len(fr) + cols - 1) // cols
    out = Image.new("RGB", (cols * (width + 8), rows * (fr[0].height + 8)), "#000000")
    for i, f in enumerate(fr):
        out.paste(f, ((i % cols) * (width + 8), (i // cols) * (fr[0].height + 8)))
    out.save(os.path.join(OUT, name))
    log(f"frame sheet {name} ({len(fr)} frames)")


def os_clipboard():
    import win32clipboard
    win32clipboard.OpenClipboard()
    try:
        return win32clipboard.GetClipboardData(win32clipboard.CF_UNICODETEXT)
    finally:
        win32clipboard.CloseClipboard()


async def measure_cpu(app, seconds, label):
    """CPU of the python process + the flet client, idle (no interaction) for `seconds`."""
    me = psutil.Process()
    procs = [me] + me.children(recursive=True)
    t0 = {p.pid: sum(p.cpu_times()[:2]) for p in procs}
    s0, a0 = len(app.stats["stall_ms"]), len(app.stats["apply_ms"])
    start = time.perf_counter()
    await asyncio.sleep(seconds)
    wall = time.perf_counter() - start
    total = 0.0
    parts = []
    for p in procs:
        try:
            d = sum(p.cpu_times()[:2]) - t0[p.pid]
        except psutil.NoSuchProcess:
            continue
        total += d
        if d > 0.01:
            parts.append(f"{p.name()}={d / wall * 100:.1f}%core")
    st = sorted(app.stats["stall_ms"][s0:]) or [0]
    ap = app.stats["apply_ms"][a0:] or [0]
    log(f"[{label}] own CPU over {wall:.0f}s: {total / wall * 100 / psutil.cpu_count():.2f}% of the machine "
        f"({', '.join(parts)}); event-loop lag p50 {st[len(st) // 2]:.0f} ms, p99 {st[int(len(st) * .99)]:.0f} ms, "
        f"max {st[-1]:.0f} ms; process-tick apply max {max(ap):.0f} ms")


async def run(app):
    os.makedirs(OUT, exist_ok=True)
    open(os.path.join(OUT, "selftest.log"), "w").close()
    app.auto_hover = False  # the real cursor is not ours to move; transitions are called directly
    if os.environ["RESMON_TEST"] == "perf":  # phases for an external RAM/GPU sampler; no screenshots on purpose
        off = os.environ.get("RESMON_PERF_OFF", "").split(",")  # bisect: switch parts of the full view off
        if "treemap" in off:
            app.render_treemap = lambda *a, **k: None
        if "table" in off:
            app.render_table = lambda *a, **k: None
        for g in list(app.G_full.values()) + list(app.G_panel.values()):
            if "spark" in off:
                g._curve = lambda *a: []
            if "cores" in off:
                g.set_cores = lambda pc: None
        log(f"perf, off: {off}")
        for phase, go, secs in (("tab", None, 10), ("full", app.to_full, 20), ("tab-after", app.to_tab, 15)):
            if go:
                await go()
            log(f"phase {phase} at {time.time():.0f}")
            await asyncio.sleep(secs)
        appmod.hard_exit()
    app.page.enable_screenshots = True
    if os.environ["RESMON_TEST"] == "dock":  # every way back to the tab, through the real window events
        def docked(label):
            b = appmod.window_bounds()
            L, T, R, B, s = appmod.monitor_at(*app.anchor)
            ok = app.mode == "tab" and b is not None and abs(b[0] + b[2] - R) < 3 and b[2] <= appmod.TAB_WIN_W * s + 2
            log(f"{label}: mode={app.mode} bounds=" + ("none" if b is None else "(%.0f, %.0f, %.0f x %.0f)" % b)
                + f" -> {'OK' if ok else 'FAIL'}")
            return ok

        click = SimpleNamespace(name="click", data=None)  # what the - and x buttons pass
        esc = SimpleNamespace(key="Escape")
        try:
            await asyncio.sleep(3)
            docked("start")
            steps = [("minus/x button", lambda: app.to_tab(click)), ("Esc key", lambda: app.on_key(esc))]
            for label, act in steps:
                await app.to_full()
                await asyncio.sleep(1.5)
                await act()
                await asyncio.sleep(1.0)
                docked(f"full -> {label}")
            for label in ("Windows minimise", "Windows close"):
                await app.to_full()
                await asyncio.sleep(1.5)
                if label == "Windows minimise":
                    app.page.window.minimized = True
                    app.page.update()
                else:
                    await app.page.window.close()
                await asyncio.sleep(2.0)
                docked(f"full -> {label} (OS event)")
            await app.to_panel()
            await asyncio.sleep(1)
            edge_shot("dock_panel.png")
            await app.to_tab()  # what the cursor poll calls when the mouse leaves
            await asyncio.sleep(1)
            docked("panel -> mouse leaves")
            await app.to_panel()
            await asyncio.sleep(1)
            await app.to_full()  # click on the panel
            await asyncio.sleep(1.5)
            await shot(app, "dock_full.png")
            await app.to_tab(click)
            await asyncio.sleep(1)
            docked("panel -> full -> x")
            edge_shot("dock_final_tab.png", 200, 400)
        except Exception:
            import traceback
            log("ERROR " + traceback.format_exc())
        appmod.hard_exit()
    if os.environ["RESMON_TEST"] == "profile":  # where does the full view's CPU go?
        try:
            await app.to_full()
            await asyncio.sleep(4)
            log(f"mode {app.mode}, procs {len(app.procs)}, tiles {len(app.model)}, tm {app.tm_w}x{app.tm_h}")
            await shot(app, "profile.png")
            await measure_cpu(app, 20, "full, everything live")
            app.render_treemap = lambda m: None
            await measure_cpu(app, 20, "full, treemap frozen")
            for g in app.G_full.values():
                g.set_cores = lambda pc: None
            await measure_cpu(app, 20, "full, treemap + core bars frozen")
            for g in app.G_full.values():
                g._curve = lambda *a: []
            await measure_cpu(app, 20, "full, treemap + core bars + sparklines frozen")
        except Exception:
            import traceback
            log("ERROR " + traceback.format_exc())
        appmod.hard_exit()
    dummy = subprocess.Popen([sys.executable, "-c", DUMMY], creationflags=subprocess.CREATE_NO_WINDOW)
    try:
        log(f"anchor {app.anchor}, monitor (work area + scale) {appmod.monitor_at(*app.anchor)}")
        await asyncio.sleep(3)
        edge_shot("tab.png", 200, 400)
        frames = []
        for _ in range(6):  # tab bars between samples
            frames.append(ImageGrab.grab(bbox=edge_box(60, 220)))
            await asyncio.sleep(0.15)
        sheet(frames, "tab_frames.png", 6, 60)
        await measure_cpu(app, 40, "tab")  # also fills the 60 s history for the panel

        await app.to_panel()
        await asyncio.sleep(2)
        await measure_cpu(app, 20, "panel")
        edge_shot("panel.png")
        frames = []
        for _ in range(8):  # sparklines scrolling, bars easing
            frames.append(ImageGrab.grab(bbox=edge_box(270, 300)))
            await asyncio.sleep(0.25)
        sheet(frames, "panel_frames.png", 4, 270)

        t = time.perf_counter()
        await app.to_full()
        log(f"-> full transition {(time.perf_counter() - t) * 1000:.0f} ms")
        await asyncio.sleep(4)
        log(f"dummy pid {dummy.pid}; procs {len(app.procs)}; treemap {app.tm_w:.0f}x{app.tm_h:.0f}, "
            f"{len(app.model)} tiles")
        await shot(app, "full.png")

        big = max((h for h in app.hits if h[4] == "pid"), key=lambda h: h[2] * h[3])
        app.on_hover(SimpleNamespace(local_position=SimpleNamespace(x=big[0] + big[2] * .5, y=big[1] + big[3] * .4)))
        app.select(app.hover)
        await shot(app, "hover.png")
        app.on_exit(None)

        # tiles gliding: switch RAM -> CPU and film the transition
        await ensure_full(app)
        frames = []
        app.set_metric("cpu")
        app.page.update()
        t = time.perf_counter()
        while time.perf_counter() - t < 0.9:
            frames.append(await grab_page(app))
        sheet(frames[:8], "glide_frames.png", 4, 520)
        log(f"glide: {len(frames)} frames in 0.9 s")
        await asyncio.sleep(1)
        app.set_metric("ws")

        # --- actions on the dummy only
        await asyncio.sleep(2)
        app.select(("pid", dummy.pid))
        assert app.selected_procs(), "dummy not in snapshot"
        await app.copy_pid()
        await asyncio.sleep(0.5)
        cb = os_clipboard()
        log(f"copy PID -> clipboard {cb!r}: {'OK' if cb == str(dummy.pid) else 'FAIL'}")
        await app.copy_cmd()
        await asyncio.sleep(0.5)
        cb = os_clipboard()
        log(f"copy cmd -> clipboard: {'OK' if 'time.sleep(600)' in cb else 'FAIL'}")
        app.ask_kill()
        await shot(app, "kill_dialog.png")
        app.page.pop_dialog()
        child = [c.pid for c in psutil.Process(dummy.pid).children()]
        killed, denied = app.kill([dummy.pid], tree=True)
        await asyncio.sleep(1)
        alive = [p for p in [dummy.pid] + child if psutil.pid_exists(p)]
        log(f"kill tree {dummy.pid}+{child}: killed={killed} denied={denied} still alive={alive}: "
            f"{'OK' if not alive and killed == 1 + len(child) else 'FAIL'}")
        if not appmod.startup_enabled():
            app.toggle_startup()
            made = appmod.startup_enabled()
            app.toggle_startup()
            log(f"start with Windows round trip: {'OK' if made and not appmod.startup_enabled() else 'FAIL'}")

        await asyncio.sleep(4)  # let the toast expire
        await ensure_full(app)
        await asyncio.sleep(2)
        await measure_cpu(app, 30, "full")
        t = time.perf_counter()
        await app.to_tab()
        log(f"-> tab transition {(time.perf_counter() - t) * 1000:.0f} ms")
        await asyncio.sleep(2)
        edge_shot("back_to_tab.png", 200, 400)
    except Exception:
        import traceback
        log("ERROR " + traceback.format_exc())
    finally:
        try:
            for c in psutil.Process(dummy.pid).children(recursive=True):
                c.kill()
            psutil.Process(dummy.pid).kill()
        except psutil.NoSuchProcess:
            pass
        log("done")
        appmod.hard_exit()
