"""Data side of ResMon: one fast system-wide sample per tick, plus the squarified treemap layout.

Per-process numbers come from a single NtQuerySystemInformation call (psutil's per-process
iteration costs ~1.2 s here because of protected processes). Per-process GPU comes from the
English-name PDH counters, since the OS is localized and NVML reports N/A under WDDM.
"""
import ctypes
import ctypes.wintypes as wt
import time
from collections import deque

import psutil

try:
    import win32pdh
except ImportError:  # pywin32 missing: per-process GPU disabled
    win32pdh = None

try:
    import pynvml
except ImportError:
    pynvml = None

HISTORY = 120  # samples kept for sparklines (60 s at 0.5 s)


# ---------------------------------------------------------------- process table (NT API)

class _UNICODE_STRING(ctypes.Structure):
    _fields_ = [("Length", wt.USHORT), ("MaximumLength", wt.USHORT), ("Buffer", ctypes.c_void_p)]


class _SPI(ctypes.Structure):  # SYSTEM_PROCESS_INFORMATION, x64 layout
    _fields_ = [
        ("NextEntryOffset", wt.ULONG), ("NumberOfThreads", wt.ULONG),
        ("WorkingSetPrivateSize", ctypes.c_longlong), ("HardFaultCount", wt.ULONG),
        ("NumberOfThreadsHighWatermark", wt.ULONG), ("CycleTime", ctypes.c_ulonglong),
        ("CreateTime", ctypes.c_longlong), ("UserTime", ctypes.c_longlong),
        ("KernelTime", ctypes.c_longlong), ("ImageName", _UNICODE_STRING),
        ("BasePriority", ctypes.c_long), ("UniqueProcessId", ctypes.c_void_p),
        ("InheritedFromUniqueProcessId", ctypes.c_void_p), ("HandleCount", wt.ULONG),
        ("SessionId", wt.ULONG), ("UniqueProcessKey", ctypes.c_void_p),
        ("PeakVirtualSize", ctypes.c_size_t), ("VirtualSize", ctypes.c_size_t),
        ("PageFaultCount", wt.ULONG), ("PeakWorkingSetSize", ctypes.c_size_t),
        ("WorkingSetSize", ctypes.c_size_t), ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPagedPoolUsage", ctypes.c_size_t), ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
        ("QuotaNonPagedPoolUsage", ctypes.c_size_t), ("PagefileUsage", ctypes.c_size_t),
        ("PeakPagefileUsage", ctypes.c_size_t), ("PrivatePageCount", ctypes.c_size_t),
        ("ReadOperationCount", ctypes.c_longlong), ("WriteOperationCount", ctypes.c_longlong),
        ("OtherOperationCount", ctypes.c_longlong), ("ReadTransferCount", ctypes.c_longlong),
        ("WriteTransferCount", ctypes.c_longlong), ("OtherTransferCount", ctypes.c_longlong),
    ]


_ntdll = ctypes.WinDLL("ntdll")
_EPOCH_DIFF = 116444736000000000  # FILETIME 1601 -> 1970, in 100 ns


def nt_processes():
    """[(pid, ppid, name, threads, ws, private, cpu_100ns, create_epoch, io_bytes)] for every process.
    io_bytes = read + write transfer bytes: all file I/O, like Task Manager's I/O columns (cached reads too)."""
    size = ctypes.c_ulong(1 << 20)
    while True:
        buf = ctypes.create_string_buffer(size.value)
        st = _ntdll.NtQuerySystemInformation(5, buf, size, ctypes.byref(size))
        if st == 0:
            break
        if st & 0xFFFFFFFF != 0xC0000004:  # anything but STATUS_INFO_LENGTH_MISMATCH
            raise OSError(f"NtQuerySystemInformation failed: {st & 0xFFFFFFFF:#x}")
        size.value += 1 << 16
    base, off, out = ctypes.addressof(buf), 0, []
    while True:
        e = _SPI.from_address(base + off)
        pid = e.UniqueProcessId or 0
        if pid:
            name = ctypes.wstring_at(e.ImageName.Buffer, e.ImageName.Length // 2) if e.ImageName.Buffer else "System"
            out.append((pid, e.InheritedFromUniqueProcessId or 0, name, e.NumberOfThreads,
                        e.WorkingSetSize, e.PagefileUsage, e.UserTime + e.KernelTime,
                        (e.CreateTime - _EPOCH_DIFF) / 1e7 if e.CreateTime else 0.0,
                        e.ReadTransferCount + e.WriteTransferCount))
        if not e.NextEntryOffset:
            return out
        off += e.NextEntryOffset


class _PERF_INFO(ctypes.Structure):
    _fields_ = [("cb", wt.DWORD), ("CommitTotal", ctypes.c_size_t), ("CommitLimit", ctypes.c_size_t),
                ("CommitPeak", ctypes.c_size_t), ("PhysicalTotal", ctypes.c_size_t),
                ("PhysicalAvailable", ctypes.c_size_t), ("SystemCache", ctypes.c_size_t),
                ("KernelTotal", ctypes.c_size_t), ("KernelPaged", ctypes.c_size_t),
                ("KernelNonpaged", ctypes.c_size_t), ("PageSize", ctypes.c_size_t),
                ("HandleCount", wt.DWORD), ("ProcessCount", wt.DWORD), ("ThreadCount", wt.DWORD)]


def commit_bytes():
    pi = _PERF_INFO(cb=ctypes.sizeof(_PERF_INFO))
    ctypes.windll.psapi.GetPerformanceInfo(ctypes.byref(pi), pi.cb)
    return pi.CommitTotal * pi.PageSize, pi.CommitLimit * pi.PageSize


# ---------------------------------------------------------------- GPU

class _PdhGpu:
    """Per-process GPU utilisation (%) and dedicated memory (bytes), keyed by pid."""

    def __init__(self):
        self.q = win32pdh.OpenQuery()
        self.util = win32pdh.AddEnglishCounter(self.q, r"\GPU Engine(*)\Utilization Percentage")
        self.mem = win32pdh.AddEnglishCounter(self.q, r"\GPU Process Memory(*)\Dedicated Usage")
        win32pdh.CollectQueryData(self.q)

    @staticmethod
    def _pid(inst):
        return int(inst.split("_", 2)[1]) if inst.startswith("pid_") else None

    def sample(self):
        win32pdh.CollectQueryData(self.q)
        util, mem = {}, {}
        try:
            per_engine = {}  # (pid, engtype) -> summed %, then max over engine types (Task Manager rule)
            for inst, v in win32pdh.GetFormattedCounterArray(self.util, win32pdh.PDH_FMT_DOUBLE).items():
                pid = self._pid(inst)
                if pid is not None:
                    key = (pid, inst.rsplit("engtype_", 1)[-1])
                    per_engine[key] = per_engine.get(key, 0.0) + v
            for (pid, _), v in per_engine.items():
                util[pid] = min(100.0, max(util.get(pid, 0.0), v))
        except Exception:  # counter not ready on the very first tick
            pass
        try:
            for inst, v in win32pdh.GetFormattedCounterArray(self.mem, win32pdh.PDH_FMT_LARGE).items():
                pid = self._pid(inst)
                if pid is not None:
                    mem[pid] = mem.get(pid, 0) + v
        except Exception:
            pass
        return util, mem


class _Nvml:
    def __init__(self):
        pynvml.nvmlInit()
        self.h = pynvml.nvmlDeviceGetHandleByIndex(0)
        self.name = pynvml.nvmlDeviceGetName(self.h)
        if isinstance(self.name, bytes):
            self.name = self.name.decode()

    def sample(self):
        g = {"name": self.name}
        for key, fn in (("util", lambda: pynvml.nvmlDeviceGetUtilizationRates(self.h).gpu),
                        ("mem", lambda: pynvml.nvmlDeviceGetMemoryInfo(self.h)),
                        ("temp", lambda: pynvml.nvmlDeviceGetTemperature(self.h, 0)),
                        ("power", lambda: pynvml.nvmlDeviceGetPowerUsage(self.h) / 1000.0)):
            try:
                g[key] = fn()
            except Exception:
                g[key] = None
        if g["mem"] is not None:
            g["mem_used"], g["mem_total"] = g["mem"].used, g["mem"].total
        del g["mem"]
        return g


# ---------------------------------------------------------------- sampler

class Sampler:
    def __init__(self):
        self.ncpu = psutil.cpu_count() or 1
        self.mem_total = psutil.virtual_memory().total
        self.hist = {k: deque(maxlen=HISTORY)
                     for k in ("cpu", "ram", "commit", "gpu", "vram", "disk_r", "disk_w", "net_d", "net_u")}
        self.gpu_err = None
        try:
            self.nvml = _Nvml() if pynvml else None
        except Exception as e:
            self.nvml, self.gpu_err = None, f"NVML: {e}"
        try:
            self.pdh = _PdhGpu() if win32pdh else None
        except Exception as e:
            self.pdh, self.gpu_err = None, f"PDH: {e}"
        self._cmd = {}  # (pid, create) -> cmdline string
        self._prev_cpu, self._prev_t, self._tot_t = {}, None, None
        self._disk = psutil.disk_io_counters()
        self._net = psutil.net_io_counters()
        self._ticks, self._drives = 0, []
        psutil.cpu_percent(percpu=True)

    @staticmethod
    def drives():
        """[(mount, used, total)] for local volumes with a file system (empty card readers are skipped)."""
        out = []
        for p in psutil.disk_partitions():
            if not p.fstype or "cdrom" in p.opts or "remote" in p.opts:
                continue
            try:
                u = psutil.disk_usage(p.mountpoint)
            except OSError:
                continue
            out.append((p.mountpoint.rstrip("\\"), u.used, u.total))
        return out

    def cmdline(self, pid, create):
        key = (pid, create)
        if key not in self._cmd:
            try:
                self._cmd[key] = " ".join(psutil.Process(pid).cmdline())
            except Exception:
                self._cmd[key] = ""
        return self._cmd[key]

    def sample_totals(self):
        """Cheap machine totals (~2 ms): run often, feeds gauges and sparklines."""
        now = time.perf_counter()
        dt = (now - self._tot_t) if self._tot_t else None
        self._tot_t = now
        per_core = psutil.cpu_percent(percpu=True)
        cpu = sum(per_core) / len(per_core)
        vm = psutil.virtual_memory()
        commit, commit_limit = commit_bytes()
        gpu = self.nvml.sample() if self.nvml else {}
        disk, net = psutil.disk_io_counters(), psutil.net_io_counters()
        span = dt or 1.0
        rates = {"disk_r": (disk.read_bytes - self._disk.read_bytes) / span,
                 "disk_w": (disk.write_bytes - self._disk.write_bytes) / span,
                 "net_d": (net.bytes_recv - self._net.bytes_recv) / span,
                 "net_u": (net.bytes_sent - self._net.bytes_sent) / span}
        if dt is None:
            rates = dict.fromkeys(rates, 0.0)
        self._disk, self._net = disk, net
        h = self.hist
        h["cpu"].append(cpu)
        h["ram"].append(vm.used / vm.total * 100)
        h["commit"].append(commit / commit_limit * 100)
        h["gpu"].append(float(gpu.get("util") or 0))
        if gpu.get("mem_total"):
            h["vram"].append(gpu["mem_used"] / gpu["mem_total"] * 100)
        for k, v in rates.items():
            h[k].append(v)
        if self._ticks % 20 == 0:  # drive space barely moves: every 10 s
            self._drives = self.drives()
        self._ticks += 1
        return {"cpu": cpu, "per_core": per_core, "ram_used": vm.used, "ram_total": vm.total,
                "drives": self._drives,
                "commit": commit, "commit_limit": commit_limit, "gpu": gpu, "rates": rates,
                "pdh_ok": self.pdh is not None, "gpu_err": self.gpu_err}

    def sample_procs(self):
        """Per-process table (~15 ms): run every 1.5 s, only while the full view is open."""
        now = time.perf_counter()
        dt = (now - self._prev_t) if self._prev_t else None
        self._prev_t = now
        gutil, gmem = self.pdh.sample() if self.pdh else ({}, {})
        procs, cur_cpu, live = [], {}, set()
        for pid, ppid, name, thr, ws, priv, t100, create, io in nt_processes():
            key = (pid, create)
            live.add(key)
            cur_cpu[key] = (t100, io)
            prev = self._prev_cpu.get(key)
            pcpu = pio = 0.0
            if dt and prev is not None:
                pcpu = max(0.0, (t100 - prev[0]) / 1e7 / dt * 100.0 / self.ncpu)  # % of whole machine
                pio = max(0.0, (io - prev[1]) / dt)  # bytes/s
            procs.append({"pid": pid, "ppid": ppid, "name": name, "threads": thr, "ws": ws, "priv": priv,
                          "cpu": pcpu, "io": pio, "create": create, "gpu_mem": gmem.get(pid, 0),
                          "gpu": gutil.get(pid, 0.0), "cmd": self.cmdline(pid, create)})
        self._prev_cpu = cur_cpu
        for key in [k for k in self._cmd if k not in live]:
            del self._cmd[key]
        return procs


# ---------------------------------------------------------------- treemap layout

def squarify(items, x, y, w, h):
    """Bruls squarified layout. items: [(value, key)] with value > 0, sorted descending.
    Returns [(key, x, y, w, h)] covering the rectangle exactly."""
    out = []
    total = sum(v for v, _ in items)
    if total <= 0 or w <= 0 or h <= 0:
        return out
    scale = w * h / total
    areas = [(v * scale, k) for v, k in items]

    def worst(s, mx, mn, side):
        return max(side * side * mx / (s * s), s * s / (side * side * mn))

    i = 0
    while i < len(areas):
        side = min(w, h)
        row = [areas[i]]
        s = mx = mn = areas[i][0]
        i += 1
        while i < len(areas):
            a = areas[i][0]
            if worst(s + a, max(mx, a), min(mn, a), side) > worst(s, mx, mn, side):
                break
            row.append(areas[i])
            s, mx, mn = s + a, max(mx, a), min(mn, a)
            i += 1
        if i == len(areas):  # last row takes all remaining space (absorbs float drift)
            s = sum(a for a, _ in row) or 1
        if w >= h:
            cw = s / h if i < len(areas) else w
            yy = y
            for a, k in row:
                hh = a / s * h
                out.append((k, x, yy, cw, hh))
                yy += hh
            x, w = x + cw, w - cw
        else:
            rh = s / w if i < len(areas) else h
            xx = x
            for a, k in row:
                ww = a / s * w
                out.append((k, xx, y, ww, rh))
                xx += ww
            y, h = y + rh, h - rh
    return out


class Sticky:
    """Layout values with hysteresis: a key keeps its old value until it moves by more than `tol`
    (relative), so tiles do not twitch on small RAM/CPU wobbles."""

    def __init__(self, tol):
        self.tol, self.values = tol, {}

    def update(self, current):
        old = self.values
        self.values = {k: (old[k] if k in old and abs(v - old[k]) <= self.tol * max(old[k], 1e-9) else v)
                       for k, v in current.items()}
        return self.values


HEADER = 18  # px of the app-name band on a group tile


def whole_parts(live, field, whole):
    """Relative-to-total mode: whole = (total, used or None, free label), in the field's units.
    Returns [(value, label)]: 'Other' (used but not by the shown processes: kernel, drivers, other apps
    when zoomed) and the free part. Shared pages can make the processes sum past `used`: no 'Other' then."""
    if not whole:
        return []
    total, used, free_label = whole
    s = sum(q[field] for q in live)
    used = s if used is None else max(used, s)
    return [(v, k) for k, v in (("Other", used - s), (free_label, total - used)) if v > 0]


def treemap_model(procs, field, zoom, W, H, sticky, whole=None):
    """Pure layout (runs in the sampler thread). Returns tiles:
    {key, hit, x, y, w, h, title, value, name, cpu, header, free} where key is stable per app ('g', name),
    per process ('p', pid) or per partition ('x', label); hit is what a click selects (None: nothing)."""
    live = [q for q in procs if q[field] > 0 and (zoom is None or q["name"] == zoom)]
    parts = whole_parts(live, field, whole)
    vals = {q["pid"]: float(q[field]) for q in live}
    part_val = {k: v for v, k in parts}
    vals.update({k: float(v) for k, v in part_val.items()})
    sv = sticky.update(vals)
    tiles = []
    extra = [(sv[k], ("x", k)) for k in part_val]  # appended after the processes: the free part fills the corner

    def part(key, x, y, w, h):
        tiles.append({"key": key, "hit": None, "x": x + 1, "y": y + 1, "w": w - 2, "h": h - 2,
                      "title": key[1], "value": part_val[key[1]], "name": key[1], "cpu": 0.0,
                      "header": False, "free": True})

    def leaf(q, x, y, w, h, title, value):
        if w >= 7 and h >= 7:  # specks are left to the group frame: fewer controls to animate
            tiles.append({"key": ("p", q["pid"]), "hit": ("pid", q["pid"]), "x": x + 1, "y": y + 1,
                          "w": w - 2, "h": h - 2, "title": title, "value": value, "name": q["name"],
                          "cpu": q["cpu"], "header": False})

    def order(qs):  # by layout value, ties by pid: same input -> same layout
        return sorted(qs, key=lambda q: (-sv[q["pid"]], q["pid"]))

    if zoom:
        for q, x, y, w, h in squarify([(sv[q["pid"]], q) for q in order(live)] + extra, 0, 0, W, H):
            if isinstance(q, tuple):
                part(q, x, y, w, h)
            else:
                leaf(q, x, y, w, h, f"PID {q['pid']}", q[field])
        return tiles
    groups = {}
    for q in live:
        groups.setdefault(q["name"], []).append(q)
    totals = sorted(((sum(sv[q["pid"]] for q in qs), name) for name, qs in groups.items()),
                    key=lambda t: (-t[0], t[1]))
    for name, x, y, w, h in squarify(totals + extra, 0, 0, W, H):
        if isinstance(name, tuple):
            part(name, x, y, w, h)
            continue
        qs = groups[name]
        real = sum(q[field] for q in qs)
        cpu = sum(q["cpu"] for q in qs)
        title = f"{name}  ×{len(qs)}" if len(qs) > 1 else name
        if len(qs) == 1:
            leaf(qs[0], x, y, w, h, title, real)
            continue
        collapsed = w < 30 or h < 40
        tiles.append({"key": ("g", name), "hit": ("group", name), "x": x + 1, "y": y + 1, "w": w - 2,
                      "h": h - 2, "title": title, "value": real, "name": name, "cpu": cpu,
                      "header": not collapsed})
        if not collapsed:
            for q, cx, cy, cw, ch in squarify([(sv[q["pid"]], q) for q in order(qs)],
                                              x + 2, y + HEADER + 1, w - 4, h - HEADER - 3):
                leaf(q, cx, cy, cw, ch, f"PID {q['pid']}", q[field])
    return tiles


def smooth(series, alpha=0.45):
    """Light EMA for display, so sparklines read as a curve instead of a staircase."""
    out, acc = [], None
    for v in series:
        acc = v if acc is None else acc + alpha * (v - acc)
        out.append(acc)
    return out


if __name__ == "__main__":
    # self-check: layout covers the box with no overlap, areas proportional to values
    vals = sorted([50, 30, 30, 10, 6, 4, 2, 1, 1, 0.5] * 7, reverse=True)
    rects = squarify([(v, i) for i, v in enumerate(vals)], 0, 0, 600, 400)
    assert len(rects) == len(vals)
    assert abs(sum(r[3] * r[4] for r in rects) - 600 * 400) < 1e-3
    for k, rx, ry, rw, rh in rects:
        assert rx >= -1e-6 and ry >= -1e-6 and rx + rw <= 600 + 1e-6 and ry + rh <= 400 + 1e-6
        assert abs(rw * rh / (600 * 400) - vals[k] / sum(vals)) < 1e-6
    # NT process table agrees with psutil on our own process
    me = next(p for p in nt_processes() if p[0] == psutil.Process().pid)
    assert me[2].lower().startswith("python"), me
    assert abs(me[4] - psutil.Process().memory_info().rss) < 50e6
    io = psutil.Process().io_counters()
    assert abs(me[8] - (io.read_bytes + io.write_bytes)) < 1e6, (me[8], io)  # struct offsets of the I/O counters
    # relative-to-total: processes + Other + Free tile the whole machine
    qs = [{"pid": 1, "name": "a", "ws": 30.0, "cpu": 0.0}, {"pid": 2, "name": "b", "ws": 10.0, "cpu": 0.0}]
    assert whole_parts(qs, "ws", (100, 60, "Free")) == [(20, "Other"), (40, "Free")]
    assert whole_parts(qs, "ws", (100, 35, "Free")) == [(60, "Free")]  # shared pages: no negative Other
    tl = treemap_model(qs, "ws", None, 400, 300, Sticky(0.15), (100, 60, "Free"))
    assert {t["key"] for t in tl} == {("p", 1), ("p", 2), ("x", "Other"), ("x", "Free")}
    free = next(t for t in tl if t["key"] == ("x", "Free"))
    assert abs((free["w"] + 2) * (free["h"] + 2) / (400 * 300) - 0.4) < 1e-6
    # hysteresis: a 5% wobble keeps the layout value, a 30% jump moves it
    st = Sticky(0.15)
    st.update({"a": 100.0, "b": 50.0})
    st.update({"a": 105.0, "b": 65.0})
    assert st.values == {"a": 100.0, "b": 65.0}, st.values
    st.update({"a": 100.0})
    assert "b" not in st.values
    s = Sampler()
    time.sleep(1)
    s.sample_procs()
    t = time.perf_counter()
    tot = s.sample_totals()
    t1 = time.perf_counter()
    procs = s.sample_procs()
    t2 = time.perf_counter()
    tiles = treemap_model(procs, "ws", None, 1400, 400, Sticky(0.15))
    t3 = time.perf_counter()
    print(f"totals {(t1 - t) * 1000:.1f} ms, procs {(t2 - t1) * 1000:.1f} ms ({len(procs)}), "
          f"treemap model {(t3 - t2) * 1000:.1f} ms ({len(tiles)} tiles), gpu={tot['gpu']}")
    top = sorted(procs, key=lambda p: -p["gpu_mem"])[:3]
    print("top VRAM:", [(p["name"], p["pid"], p["gpu_mem"] >> 20, round(p["gpu"], 1)) for p in top])
    print("ok")
