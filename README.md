# ResMon

A resource monitor for Windows that lives on the edge of your screen instead of in a window.

<p align="center">
  <img src="docs/hero.png" width="900" alt="ResMon: docked tab, hover panel and full process treemap">
</p>

Task Manager tells you numbers. ResMon shows you **who is eating your machine**: every process is a
tile, sized by what it uses, grouped by app. It stays out of the way as a thin tab on the right edge
and opens only when you reach for it. It is a single 5 MB executable that uses about 35 MB of private
memory and well under 1% of one CPU core.

## Three states, one widget

| State | How you get there | What you see |
|---|---|---|
| **Tab** | always there | A slim tab docked on the right edge, always on top, no taskbar button. Three live bars: CPU, RAM, GPU. Press and hold to drag it to another height or monitor. |
| **Panel** | rest on the tab for 2 s, or right-click it | Slides in from the edge: CPU per core, memory, commit, GPU (load, temperature, power), VRAM, disk and network, each with a 60 s sparkline, plus free space per drive. Folds back when the mouse leaves. |
| **Full view** | click the tab or the panel | A squarified treemap of all processes plus a sortable, searchable table. `Esc`, `-` or `x` docks it back to the tab. |

## What you can do in the full view

- **Click a card** (CPU, MEMORY, GPU, VRAM) to size the treemap by it. Click MEMORY again to switch
  between working set and private bytes.
- **DISK and NETWORK** split the map in two: who reads | who writes (all file I/O, per process), who
  downloads | who uploads.
- **DRIVES:** click a drive to see what fills it, folder by folder. The scan runs in the background at low
  I/O priority; folders appear and grow as it goes, the unscanned part shrinks. Double-click a folder to
  open it, the path above takes you back.
- **Share of total:** tiles are sized against the whole machine (RAM, commit, CPU, GPU load, VRAM, drive),
  with **Other** (used, but not by the processes shown) and **Free** / **Idle** as their own tiles.
- **Per core** (with CPU): every physical core with its threads (logical processors), each filled by its
  load and split into the processes that ran there. Loads and per-process totals are measured; where each
  process ran is an estimate (see below).
- **Colour** by app or by CPU heat.
- **Hover** a tile for PID, parent, user, RAM, CPU, VRAM, GPU, disk I/O and threads. **Double-click** an app
  to see its processes.
- **Act on the selection:** copy PID, copy command line, open file location, set priority, end task
  (optionally the whole process tree).

## Install

Windows 10/11. Build it with [Rust](https://rustup.rs) (stable, MSVC toolchain):

```
git clone https://github.com/FrancescoCorrenti/ResMon.git
cd ResMon
cargo build --release
```

Then run `target\release\resmon.exe` (no console window). Copy it anywhere you like: it is self-contained.
To start it at logon, use the power button in the panel or **Start with Windows** in the `...` menu of the
full view (it adds ResMon to your user's `Run` registry key). **Quit** is in the same places.

## How it works

- **No GPU, on purpose.** A GPU renderer would load the graphics driver into the process: on an NVIDIA
  machine the OpenGL driver alone added ~140 MB and wgpu ~440 MB, for three bars and a treemap. ResMon draws
  [egui](https://github.com/emilk/egui)'s triangles with its own small CPU rasterizer (`src/raster.rs`) and
  shows them through a layered window (`UpdateLayeredWindow`), which gives real per-pixel transparency:
  the rounded corners and the empty space around the card let clicks through.
- **Only what changed is redrawn.** Every frame is compared with the previous one primitive by primitive;
  only the changed area is rasterized and uploaded. A gauge tick redraws one strip of cards, not the window.
  And frames only happen when something changed: a new sample, the mouse, an animation.
- **Processes:** one `NtQuerySystemInformation` call per tick for every process (about 15 ms), instead of
  opening processes one by one.
- **Per-process network, no administrator:** Windows keeps no per-process network counter, and the kernel
  network trace needs elevation. The Winsock trace provider does not (for members of "Performance Log
  Users", which an administrator account is by default): ResMon runs a real-time ETW session on it while
  the full view is open and adds up each process's sends and receives (`src/net.rs`; a test checks a
  loopback transfer, an ignored one a 10 MB download). Loopback traffic is included.
- **Per core:** Windows records which CPU ran a thread only in a kernel trace (administrator). ResMon starts
  from each busy thread's ideal processor and balances the table so that every process sums to its measured
  CPU time and every thread of the CPU to its measured load.
- **Per-process GPU:** Windows PDH counters `GPU Engine` and `GPU Process Memory`, read by their English names
  so it also works on localized Windows. NVML adds GPU temperature and power on NVIDIA cards; without one
  ResMon still runs, just without those two values.
- **Calm, not busy:** totals are sampled every 0.5 s, processes every 1.5 s and only while the full view is
  open. Treemap tiles glide to their new place, reflowing at most every 8 s, with hysteresis so small
  changes do not shuffle the layout.

## Limits

- Without administrator rights, some system processes cannot be ended or reprioritised and their command
  line cannot be read. ResMon tells you when that happens.
- Windows only.

## Development

- `cargo test` checks the treemap layout (coverage, proportions, share of total, hysteresis), the rasterizer
  (no double-blended seams on shared edges, clipping, egui's rounded shapes) and the system readers
  against the test process itself.
- `RESMON_OPEN=panel` or `RESMON_OPEN=full` opens that state at start and keeps it open, for screenshots.
  `RESMON_LOG=1` writes per-frame timings to `%TEMP%\resmon.log`; `RESMON_DUMP=1` writes each frame as raw
  BGRA to `%TEMP%`.
- `pwsh -File tools\perf.ps1` measures ResMon's own CPU and memory, docked and in the full view.
- `tools/make_hero.py` builds the README image from three screenshots.

The first version was written in Python with Flet; it is in the history up to commit `7efe0ad`.

## License

MIT
