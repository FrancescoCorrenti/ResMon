# ResMon

**See who is eating your machine.** A resource monitor for Windows that lives on the edge of your screen
instead of in a window.

![Windows 11](https://img.shields.io/badge/Windows-11-0078D4?logo=windows)
![Rust](https://img.shields.io/badge/Rust-egui-B7410E?logo=rust)
![Memory](https://img.shields.io/badge/RAM-~35%20MB-2ea44f)
![License: MIT](https://img.shields.io/badge/license-MIT-blue)

<p align="center">
  <img src="docs/hero.png" width="960" alt="ResMon: the full view with every process as a tile sized by memory, the hover panel and the tab docked on the screen edge">
</p>

Task Manager gives you a list of numbers. ResMon draws **every process as a tile** sized by what it uses,
grouped by app, so the culprit is the biggest thing on screen. Most of the time it is just a slim tab on
the right edge of the screen; it opens when you reach for it. One 6 MB executable; on a 6-core desktop it
takes about 35 MB of memory and under 1% of one CPU core (a bigger screen or many more threads cost a
little more).

<sub>Screenshots taken in demo mode: real measurements, app names swapped for common ones.</sub>

## Three states, one widget

| | How you get there | What you see |
|---|---|---|
| **Tab** | always there | A slim tab on the screen edge, always on top, no taskbar button. Live bars for the measures you pick (CPU, memory, GPU, VRAM, network, disk). Drag it to any height or monitor. |
| **Panel** | rest the mouse on the tab | Slides out of the edge: CPU per core, memory, commit, GPU load and memory (plus temperature, power and fan on NVIDIA cards), network, disk, free space per drive, each with a sparkline. It fits the screen height. |
| **Full view** | click the tab or the panel | A treemap of all processes and a searchable table. `Esc` docks it back. |

## What it shows

<table>
<tr>
<td width="50%"><img src="docs/cores.png" alt="Per-core view"></td>
<td width="50%"><img src="docs/menu.png" alt="Right-click menu on a process"></td>
</tr>
<tr>
<td><b>Per core.</b> Every physical core with its threads (however many it has: one, two, P- and E-cores mixed), each filled by its load and split into the processes that ran there.</td>
<td><b>Right-click anything.</b> Copy name, PID, command line or path, open its folder, search the web for it, change priority, pause it, end it or its whole tree.</td>
</tr>
</table>

- **Size the map by anything:** click a card. Memory (working set or private), CPU, GPU load, GPU memory.
- **Disk and network, split in two:** who reads | who writes, who downloads | who uploads, per process.
- **What fills a drive:** click a drive to see it folder by folder. The scan runs in the background at low
  priority, is cached per volume, and **Refresh** rescans only what changed. **Changes** shows what grew since
  last time. Cloud placeholders (OneDrive online-only files) count as the nothing they take on disk.
- **ML storage:** one card for the Hugging Face, torch, Ollama, LM Studio, pip and conda caches (at their
  default places or where `HF_HOME`, `TORCH_HOME`, `OLLAMA_MODELS`, `PIP_CACHE_DIR`, `CONDA_PKGS_DIRS`...
  point), plus model and dataset folders at a drive root. Shown only if the machine has any.
- **24 h history:** a strip over the map with the top processes every 10 s. Click a moment to see the map
  as it was.
- **Quiet alerts:** RAM, commit, GPU memory, CPU, GPU temperature (against that GPU's own slowdown point)
  and slowdowns, a nearly full drive (in percent and in gigabytes, so a big disk does not cry wolf). Only
  when the condition lasts, with the process to blame. A dot on the tab and a line in the full view, never a
  pop-up.
- **Families:** group processes by the app that launched them (every `python.exe` of one training run
  under its terminal) instead of by name.
- **Share of total:** tiles sized against the whole machine, with what no process accounts for and what is
  free as their own tiles.
- **Settings:** dark, black or light theme, accent colours, which cards to show, which bars on the tab,
  hover delay. Everything you choose is remembered.

## Install

Windows 11 (Windows 10 should work but is not tested). Build it with [Rust](https://rustup.rs) (stable,
MSVC toolchain):

```
git clone https://github.com/FrancescoCorrenti/ResMon.git
cd ResMon
cargo build --release
```

Run `target\release\resmon.exe`. It is self-contained: copy it anywhere. **Start with Windows** and **Quit**
are in the panel and in the menu of the full view.

Optional, once, from an elevated PowerShell:

```
powershell -ExecutionPolicy Bypass -File tools\install-admin.ps1
```

It registers a scheduled task that starts ResMon as administrator without a UAC prompt (needed to end or
reprioritise system processes and read their command lines), points a Desktop shortcut and the logon start
at it, and adds you to *Performance Log Users*, so per-process network also works when ResMon runs without
elevation (from your next sign-in). `-Exe` picks another copy of ResMon, `-User` another account.

## How it works

- **No GPU, on purpose.** A GPU renderer loads the graphics driver into the process: on an NVIDIA machine
  OpenGL alone added about 140 MB and wgpu about 440 MB, for a few bars and a treemap. ResMon draws
  [egui](https://github.com/emilk/egui)'s triangles with its own small CPU rasterizer (`src/raster.rs`) into
  a layered window, which gives real per-pixel transparency: clicks pass through the rounded corners.
- **Only what changed is redrawn.** Each frame is compared with the previous one primitive by primitive,
  and frames happen only when something changed.
- **Processes:** one `NtQuerySystemInformation` call per tick for all of them, instead of opening each one.
- **Per-process network:** a real-time ETW session on the Winsock provider, running only while the full view
  is open (`src/net.rs`). It needs ResMon elevated or your account in *Performance Log Users*; otherwise the
  network split says so. The totals count physical adapters only, so WSL, Docker and VPN adapters do not
  count the same bytes twice.
- **Per core:** where a thread ran is known only to a kernel trace. ResMon starts from each thread's ideal
  processor and balances the table so that each process sums to its measured CPU time and each CPU to its
  measured load. Loads are measured; which process ran where is an estimate.
- **GPU, any vendor:** load and memory come from the PDH GPU counters (what Task Manager shows), read by
  their English names so localized Windows works too, for the main adapter found through DXGI: the one
  with the most dedicated memory, or the integrated GPU when it is the only one (its memory is then shared
  system memory, and the card says so). On NVIDIA cards NVML adds temperature, power, fan, clocks and the
  reasons the GPU is slowing down. Per-process GPU load counts each adapter separately.
- **Safe actions:** end, pause and priority check that the PID still belongs to the process you clicked,
  refuse processes Windows cannot lose (`csrss`, `lsass`, `svchost`, `dwm`...), and are off while you look
  at the past.

## Limits

- Without administrator rights some system processes cannot be ended or reprioritised, and their command
  line cannot be read. ResMon says so when it happens.
- Windows only. Tested on Windows 11 with a 6-core Intel CPU and an NVIDIA GPU; AMD and Intel GPUs go
  through the vendor-neutral path, which is tested on the same machine with NVML switched off
  (`RESMON_NO_NVML=1`).
- The per-process network counts were calibrated on Windows 11; other builds may count differently.
- On machines with more than 64 logical CPUs, per-process CPU is right but the per-core view and the CPU
  total show the first 64.
- Hard-linked files (Windows component store) are counted once per link in the drive map.

## Development

- `cargo test` checks the treemap layout, the rasterizer, the system readers, the network trace (loopback)
  and the drive scanner.
- `RESMON_OPEN=panel|full` opens that state at start. `RESMON_SCRIPT="2:click:110,130; 4:shot"` plays
  clicks and writes frames to `%TEMP%`. `RESMON_DEMO=1` renames processes to common apps, for screenshots.
  `RESMON_NO_NVML=1` uses the vendor-neutral GPU path on an NVIDIA machine. `RESMON_LOG=1` writes per-frame
  timings to `%TEMP%\resmon.log`.
- `tools/compose_hero.py` builds the README images from those frames; `tools\perf.ps1` measures ResMon's own
  CPU and memory.

The first version was written in Python with Flet: it is the history of `main` up to `4232e53`.

## License

MIT
