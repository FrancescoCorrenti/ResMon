//! Data side: one NtQuerySystemInformation call for every process, per-core CPU, memory, PDH counters
//! (disk, network, per-process GPU by English names so localized Windows works), NVML for the GPU
//! totals, plus the small Win32 helpers the shell needs (monitors, cursor, window placement).

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::time::Instant;

use windows::Wdk::System::SystemInformation::{NtQuerySystemInformation, SYSTEM_INFORMATION_CLASS};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL};
use windows::Win32::Security::{GetTokenInformation, LookupAccountSidW, TokenUser, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER};
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW};
use windows::Win32::System::Performance::*;
use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER};
use windows::core::{HSTRING, PCWSTR, PWSTR};

pub const HISTORY: usize = 120; // samples kept for sparklines (60 s at 0.5 s)

// ------------------------------------------------------------------ process table (NT API)

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[repr(C)]
#[allow(dead_code)]
struct Spi {
    // SYSTEM_PROCESS_INFORMATION, x64 layout (the documented struct hides most fields)
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name: UnicodeString,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_page_count: usize,
    read_operation_count: i64,
    write_operation_count: i64,
    other_operation_count: i64,
    read_transfer_count: i64,
    write_transfer_count: i64,
    other_transfer_count: i64,
}

const EPOCH_DIFF: i64 = 116_444_736_000_000_000; // FILETIME 1601 -> 1970, in 100 ns

pub struct RawProc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub threads: u32,
    pub ws: u64,
    pub private: u64,
    pub cpu_100ns: i64,
    pub create: i64, // unix seconds, 0 if unknown
    pub io: u64,     // read + write transfer bytes: all file I/O, like Task Manager's I/O columns
}

/// Query a system information class into `words` u64s (u64 for alignment), growing the buffer while the
/// kernel asks for more. Returns the bytes actually written, rounded down to whole u64s.
fn nt_query(class: i32, words: usize) -> Vec<u64> {
    let mut buf: Vec<u64> = vec![0; words];
    loop {
        let mut need = 0u32;
        let st = unsafe {
            NtQuerySystemInformation(SYSTEM_INFORMATION_CLASS(class), buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32, &mut need)
        };
        if st.0 == 0 {
            buf.truncate((need as usize / 8).min(buf.len()));
            return buf;
        }
        // anything but STATUS_INFO_LENGTH_MISMATCH asking for a bigger buffer: give up
        if st.0 as u32 != 0xC000_0004 || (need as usize) <= buf.len() * 8 || buf.len() > 1 << 24 {
            return Vec::new();
        }
        buf.resize((need as usize + (1 << 16)) / 8 + 1, 0); // the table grows between two calls: some slack
    }
}

pub fn nt_processes() -> Vec<RawProc> {
    let buf = nt_query(5, 1 << 17);
    let mut out = Vec::with_capacity(512);
    if buf.is_empty() {
        return out;
    }
    let base = buf.as_ptr() as *const u8;
    let mut off = 0usize;
    loop {
        let e = unsafe { &*(base.add(off) as *const Spi) };
        let pid = e.unique_process_id as u32;
        if pid != 0 {
            let name = if e.image_name.buffer.is_null() {
                "System".to_string()
            } else {
                let s = unsafe { std::slice::from_raw_parts(e.image_name.buffer, e.image_name.length as usize / 2) };
                String::from_utf16_lossy(s)
            };
            out.push(RawProc {
                pid,
                ppid: e.inherited_from_unique_process_id as u32,
                name,
                threads: e.number_of_threads,
                ws: e.working_set_size as u64,
                private: e.pagefile_usage as u64,
                cpu_100ns: e.user_time + e.kernel_time,
                create: if e.create_time > 0 { (e.create_time - EPOCH_DIFF) / 10_000_000 } else { 0 },
                io: (e.read_transfer_count + e.write_transfer_count) as u64,
            });
        }
        if e.next_entry_offset == 0 {
            return out;
        }
        off += e.next_entry_offset as usize;
    }
}

/// Busy and total 100 ns ticks per logical processor.
fn core_times() -> Vec<(i64, i64)> {
    // SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: idle, kernel (includes idle), user, dpc, interrupt, count
    // the length must be exactly one 48-byte record per processor of the current group
    let n = unsafe { windows::Win32::System::Threading::GetActiveProcessorCount(0) } as usize;
    let buf = nt_query(8, n.max(1) * 6);
    buf.chunks_exact(6)
        .map(|c| {
            let (idle, kernel, user) = (c[0] as i64, c[1] as i64, c[2] as i64);
            (kernel + user - idle, kernel + user)
        })
        .collect()
}

fn commit_bytes() -> (u64, u64) {
    let mut pi = PERFORMANCE_INFORMATION { cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32, ..Default::default() };
    let _ = unsafe { GetPerformanceInfo(&mut pi, pi.cb) };
    let page = pi.PageSize as u64;
    (pi.CommitTotal as u64 * page, pi.CommitLimit as u64 * page)
}

fn memory() -> (u64, u64) {
    let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    let _ = unsafe { GlobalMemoryStatusEx(&mut m) };
    (m.ullTotalPhys - m.ullAvailPhys, m.ullTotalPhys)
}

fn open(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> Option<HANDLE> {
    unsafe { OpenProcess(access, false, pid).ok() }
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Command line via ProcessCommandLineInformation (Windows 8.1+): limited query rights are enough.
pub fn cmdline(pid: u32) -> String {
    let Some(h) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else { return String::new() };
    let h = Handle(h);
    let mut buf: Vec<u64> = vec![0; 512];
    loop {
        let mut need = 0u32;
        let st = unsafe {
            NtQueryInformationProcess(h.0, PROCESSINFOCLASS(60), buf.as_mut_ptr() as *mut c_void, (buf.len() * 8) as u32, &mut need)
        };
        if st.0 == 0 {
            let us = unsafe { &*(buf.as_ptr() as *const UnicodeString) };
            if us.buffer.is_null() {
                return String::new();
            }
            let s = unsafe { std::slice::from_raw_parts(us.buffer, us.length as usize / 2) };
            return String::from_utf16_lossy(s);
        }
        if need as usize <= buf.len() * 8 || buf.len() > 1 << 16 {
            return String::new();
        }
        buf.resize(need as usize / 8 + 1, 0);
    }
}

pub fn exe_path(pid: u32) -> Option<String> {
    let h = Handle(open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?);
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    unsafe { QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut n).ok()? };
    Some(String::from_utf16_lossy(&buf[..n as usize]))
}

pub fn username(pid: u32) -> String {
    let get = || -> Option<String> {
        let h = Handle(open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?);
        let mut tok = HANDLE::default();
        unsafe { windows::Win32::System::Threading::OpenProcessToken(h.0, TOKEN_QUERY, &mut tok).ok()? };
        let tok = Handle(tok);
        let mut buf: Vec<u64> = vec![0; 64];
        let mut n = 0u32;
        unsafe { GetTokenInformation(tok.0, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), 512, &mut n).ok()? };
        let tu = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        let (mut name, mut dom) = ([0u16; 256], [0u16; 256]);
        let (mut nn, mut dn) = (256u32, 256u32);
        let mut use_ = SID_NAME_USE::default();
        unsafe {
            LookupAccountSidW(PCWSTR::null(), tu.User.Sid, Some(PWSTR(name.as_mut_ptr())), &mut nn, Some(PWSTR(dom.as_mut_ptr())), &mut dn, &mut use_)
                .ok()?
        };
        Some(format!("{}\\{}", String::from_utf16_lossy(&dom[..dn as usize]), String::from_utf16_lossy(&name[..nn as usize])))
    };
    get().unwrap_or_else(|| "(access denied)".into())
}

/// Ok(true) killed, Ok(false) gone already, Err(()) access denied.
pub fn kill(pid: u32) -> Result<bool, ()> {
    let Some(h) = open(pid, PROCESS_TERMINATE) else {
        return if open(pid, PROCESS_QUERY_LIMITED_INFORMATION).map(|h| Handle(h)).is_some() { Err(()) } else { Ok(false) };
    };
    let h = Handle(h);
    unsafe { TerminateProcess(h.0, 1) }.map(|_| true).map_err(|_| ())
}

pub fn set_priority(pid: u32, class: PROCESS_CREATION_FLAGS) -> bool {
    open(pid, PROCESS_SET_INFORMATION).map(Handle).is_some_and(|h| unsafe { SetPriorityClass(h.0, class) }.is_ok())
}

pub const PRIORITIES: [(&str, PROCESS_CREATION_FLAGS); 3] =
    [("Idle", IDLE_PRIORITY_CLASS), ("Below normal", BELOW_NORMAL_PRIORITY_CLASS), ("Normal", NORMAL_PRIORITY_CLASS)];

/// [(mount, used, total)] for local volumes that are ready (empty card readers and CD drives are skipped).
pub fn drives() -> Vec<(String, u64, u64)> {
    let mut buf = [0u16; 512];
    let n = unsafe { GetLogicalDriveStringsW(Some(&mut buf)) } as usize;
    let mut out = Vec::new();
    for root in buf[..n.min(buf.len())].split(|&c| c == 0).filter(|s| !s.is_empty()) {
        let path = HSTRING::from_wide(root);
        let kind = unsafe { GetDriveTypeW(&path) };
        if kind != 2 && kind != 3 {
            continue; // DRIVE_REMOVABLE, DRIVE_FIXED only
        }
        let (mut free, mut total) = (0u64, 0u64);
        if unsafe { GetDiskFreeSpaceExW(&path, None, Some(&mut total), Some(&mut free)) }.is_ok() && total > 0 {
            out.push((String::from_utf16_lossy(root).trim_end_matches('\\').to_string(), total - free, total));
        }
    }
    out
}

// ------------------------------------------------------------------ PDH

struct Pdh {
    q: PDH_HQUERY,
    counters: Vec<PDH_HCOUNTER>,
}

impl Pdh {
    fn new(paths: &[&str]) -> Option<Pdh> {
        let mut q = PDH_HQUERY::default();
        if unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut q) } != 0 {
            return None;
        }
        let mut counters = Vec::new();
        for p in paths {
            let mut c = PDH_HCOUNTER::default();
            if unsafe { PdhAddEnglishCounterW(q, &HSTRING::from(*p), 0, &mut c) } != 0 {
                return None;
            }
            counters.push(c);
        }
        unsafe { PdhCollectQueryData(q) };
        Some(Pdh { q, counters })
    }

    fn collect(&self) {
        unsafe { PdhCollectQueryData(self.q) };
    }

    /// (instance name, value) for every instance of counter i.
    fn array(&self, i: usize, large: bool) -> Vec<(String, f64)> {
        let fmt = if large { PDH_FMT_LARGE } else { PDH_FMT_DOUBLE };
        let (mut size, mut count) = (0u32, 0u32);
        unsafe { PdhGetFormattedCounterArrayW(self.counters[i], fmt, &mut size, &mut count, None) };
        if size == 0 {
            return Vec::new();
        }
        let mut buf: Vec<u64> = vec![0; size as usize / 8 + 1];
        let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
        if unsafe { PdhGetFormattedCounterArrayW(self.counters[i], fmt, &mut size, &mut count, Some(items)) } != 0 {
            return Vec::new();
        }
        (0..count as usize)
            .filter_map(|k| {
                let it = unsafe { &*items.add(k) };
                if it.FmtValue.CStatus > 1 {
                    return None; // PDH_CSTATUS_VALID_DATA = 0, NEW_DATA = 1
                }
                let name = unsafe { it.szName.to_string() }.unwrap_or_default();
                let v = unsafe { if large { it.FmtValue.Anonymous.largeValue as f64 } else { it.FmtValue.Anonymous.doubleValue } };
                Some((name, v))
            })
            .collect()
    }

    fn sum(&self, i: usize) -> f64 {
        self.array(i, false).iter().map(|(_, v)| v).sum()
    }
}

fn pdh_pid(inst: &str) -> Option<u32> {
    inst.strip_prefix("pid_")?.split('_').next()?.parse().ok()
}

// ------------------------------------------------------------------ GPU totals (NVML)

#[derive(Clone, Default)]
pub struct Gpu {
    pub name: String,
    pub util: Option<f32>,
    pub temp: Option<u32>,
    pub power: Option<f32>,
    pub mem_used: u64,
    pub mem_total: u64,
}

struct Nvml {
    nvml: nvml_wrapper::Nvml,
    name: String,
}

impl Nvml {
    fn new() -> Result<Nvml, String> {
        let nvml = nvml_wrapper::Nvml::init().map_err(|e| format!("NVML: {e}"))?;
        let name = nvml.device_by_index(0).and_then(|d| d.name()).map_err(|e| format!("NVML: {e}"))?;
        Ok(Nvml { nvml, name })
    }

    fn sample(&self) -> Gpu {
        let mut g = Gpu { name: self.name.clone(), ..Default::default() };
        if let Ok(d) = self.nvml.device_by_index(0) {
            g.util = d.utilization_rates().ok().map(|u| u.gpu as f32);
            g.temp = d.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu).ok();
            g.power = d.power_usage().ok().map(|mw| mw as f32 / 1000.0);
            if let Ok(m) = d.memory_info() {
                (g.mem_used, g.mem_total) = (m.used, m.total);
            }
        }
        g
    }
}

// ------------------------------------------------------------------ sampler

#[derive(Clone, Default)]
pub struct Totals {
    pub cpu: f32,
    pub per_core: Vec<f32>,
    pub ram_used: u64,
    pub ram_total: u64,
    pub commit: u64,
    pub commit_limit: u64,
    pub gpu: Gpu,
    pub disk_r: f64,
    pub disk_w: f64,
    pub net_d: f64,
    pub net_u: f64,
    pub drives: Vec<(String, u64, u64)>,
    pub pdh_ok: bool,
    pub gpu_err: Option<String>,
}

#[derive(Clone, Default)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub threads: u32,
    pub ws: f64,
    pub private: f64,
    pub cpu: f64, // % of the whole machine
    pub io: f64,  // bytes/s
    pub create: i64,
    pub gpu_mem: f64,
    pub gpu: f64,
    pub cmd: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Ws,
    Private,
    Cpu,
    GpuMem,
    Gpu,
    Io,
}

impl Proc {
    pub fn get(&self, f: Field) -> f64 {
        match f {
            Field::Ws => self.ws,
            Field::Private => self.private,
            Field::Cpu => self.cpu,
            Field::GpuMem => self.gpu_mem,
            Field::Gpu => self.gpu,
            Field::Io => self.io,
        }
    }
}

#[derive(Default, Clone)]
pub struct History {
    pub cpu: VecDeque<f32>,
    pub ram: VecDeque<f32>,
    pub commit: VecDeque<f32>,
    pub gpu: VecDeque<f32>,
    pub vram: VecDeque<f32>,
    pub disk_r: VecDeque<f32>,
    pub disk_w: VecDeque<f32>,
    pub net_d: VecDeque<f32>,
    pub net_u: VecDeque<f32>,
}

fn push(d: &mut VecDeque<f32>, v: f32) {
    if d.len() == HISTORY {
        d.pop_front();
    }
    d.push_back(v);
}

pub struct Sampler {
    pub ncpu: usize,
    nvml: Option<Nvml>,
    gpu_err: Option<String>,
    io: Option<Pdh>,  // disk + network rates, every tick
    gpu: Option<Pdh>, // per-process GPU, only with the process table
    cores: Vec<(i64, i64)>,
    prev: HashMap<(u32, i64), (i64, u64)>,
    prev_t: Option<Instant>,
    cmd: HashMap<(u32, i64), String>,
    ticks: u64,
    drives: Vec<(String, u64, u64)>,
}

impl Sampler {
    pub fn new() -> Sampler {
        let (nvml, gpu_err) = match Nvml::new() {
            Ok(n) => (Some(n), None),
            Err(e) => (None, Some(e)),
        };
        let cores = core_times();
        Sampler {
            ncpu: cores.len().max(1),
            nvml,
            gpu_err,
            io: Pdh::new(&[
                r"\PhysicalDisk(_Total)\Disk Read Bytes/sec",
                r"\PhysicalDisk(_Total)\Disk Write Bytes/sec",
                r"\Network Interface(*)\Bytes Received/sec",
                r"\Network Interface(*)\Bytes Sent/sec",
            ]),
            gpu: Pdh::new(&[r"\GPU Engine(*)\Utilization Percentage", r"\GPU Process Memory(*)\Dedicated Usage"]),
            cores,
            prev: HashMap::new(),
            prev_t: None,
            cmd: HashMap::new(),
            ticks: 0,
            drives: Vec::new(),
        }
    }

    /// Cheap machine totals (a few ms): feeds gauges and sparklines.
    pub fn sample_totals(&mut self, h: &mut History) -> Totals {
        let cores = core_times();
        let per_core: Vec<f32> = cores
            .iter()
            .zip(&self.cores)
            .map(|(&(b1, t1), &(b0, t0))| if t1 > t0 { ((b1 - b0) as f32 / (t1 - t0) as f32 * 100.0).clamp(0.0, 100.0) } else { 0.0 })
            .collect();
        self.cores = cores;
        let cpu = per_core.iter().sum::<f32>() / per_core.len().max(1) as f32;
        let (ram_used, ram_total) = memory();
        let (commit, commit_limit) = commit_bytes();
        let gpu = self.nvml.as_ref().map(|n| n.sample()).unwrap_or_default();
        let (mut disk_r, mut disk_w, mut net_d, mut net_u) = (0.0, 0.0, 0.0, 0.0);
        if let Some(p) = &self.io {
            p.collect();
            (disk_r, disk_w, net_d, net_u) = (p.sum(0), p.sum(1), p.sum(2), p.sum(3));
        }
        push(&mut h.cpu, cpu);
        push(&mut h.ram, ram_used as f32 / ram_total.max(1) as f32 * 100.0);
        push(&mut h.commit, commit as f32 / commit_limit.max(1) as f32 * 100.0);
        push(&mut h.gpu, gpu.util.unwrap_or(0.0));
        if gpu.mem_total > 0 {
            push(&mut h.vram, gpu.mem_used as f32 / gpu.mem_total as f32 * 100.0);
        }
        push(&mut h.disk_r, disk_r as f32);
        push(&mut h.disk_w, disk_w as f32);
        push(&mut h.net_d, net_d as f32);
        push(&mut h.net_u, net_u as f32);
        if self.ticks % 20 == 0 {
            self.drives = drives(); // drive space barely moves: every 10 s
        }
        self.ticks += 1;
        Totals {
            cpu,
            per_core,
            ram_used,
            ram_total,
            commit,
            commit_limit,
            gpu,
            disk_r,
            disk_w,
            net_d,
            net_u,
            drives: self.drives.clone(),
            pdh_ok: self.gpu.is_some(),
            gpu_err: self.gpu_err.clone(),
        }
    }

    /// Per-process table: CPU and I/O as rates since the previous call.
    pub fn sample_procs(&mut self) -> Vec<Proc> {
        let now = Instant::now();
        let dt = self.prev_t.map(|t| (now - t).as_secs_f64());
        self.prev_t = Some(now);
        let (mut gutil, mut gmem) = (HashMap::<u32, f64>::new(), HashMap::<u32, f64>::new());
        if let Some(p) = &self.gpu {
            p.collect();
            let mut per_engine: HashMap<(u32, String), f64> = HashMap::new(); // summed per engine type, then max (Task Manager rule)
            for (inst, v) in p.array(0, false) {
                if let Some(pid) = pdh_pid(&inst) {
                    let eng = inst.rsplit("engtype_").next().unwrap_or("").to_string();
                    *per_engine.entry((pid, eng)).or_default() += v;
                }
            }
            for ((pid, _), v) in per_engine {
                let e = gutil.entry(pid).or_default();
                *e = e.max(v).min(100.0);
            }
            for (inst, v) in p.array(1, true) {
                if let Some(pid) = pdh_pid(&inst) {
                    *gmem.entry(pid).or_default() += v;
                }
            }
        }
        let mut cur = HashMap::new();
        let mut live = HashSet::new();
        let ncpu = self.ncpu as f64;
        let procs: Vec<Proc> = nt_processes()
            .into_iter()
            .map(|r| {
                let key = (r.pid, r.create);
                live.insert(key);
                cur.insert(key, (r.cpu_100ns, r.io));
                let (mut cpu, mut io) = (0.0, 0.0);
                if let (Some(dt), Some(&(c0, i0))) = (dt, self.prev.get(&key)) {
                    cpu = ((r.cpu_100ns - c0) as f64 / 1e7 / dt * 100.0 / ncpu).max(0.0);
                    io = (r.io.saturating_sub(i0)) as f64 / dt;
                }
                let cmd = self.cmd.entry(key).or_insert_with(|| cmdline(r.pid)).clone();
                Proc {
                    pid: r.pid,
                    ppid: r.ppid,
                    threads: r.threads,
                    ws: r.ws as f64,
                    private: r.private as f64,
                    cpu,
                    io,
                    create: r.create,
                    gpu_mem: gmem.get(&r.pid).copied().unwrap_or(0.0),
                    gpu: gutil.get(&r.pid).copied().unwrap_or(0.0),
                    cmd,
                    name: r.name,
                }
            })
            .collect();
        self.prev = cur;
        self.cmd.retain(|k, _| live.contains(k));
        procs
    }
}

// ------------------------------------------------------------------ shell helpers (physical px)

/// Work area (taskbar excluded) and scale of the monitor nearest to (x, y).
pub fn monitor_at(x: i32, y: i32) -> (i32, i32, i32, i32, f32) {
    unsafe {
        let h = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(h, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let r = mi.rcWork;
        (r.left, r.top, r.right, r.bottom, dx as f32 / 96.0)
    }
}

pub fn monitor_exists(x: i32, y: i32) -> bool {
    !unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL) }.is_invalid()
}

pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT::default();
    let _ = unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

pub fn left_button_down() -> bool {
    (unsafe { GetAsyncKeyState(0x01) } as u16 & 0x8000) != 0
}

pub fn window_rect(h: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut r = RECT::default();
    unsafe { GetWindowRect(h, &mut r).ok()? };
    Some((r.left, r.top, r.right - r.left, r.bottom - r.top))
}

pub fn place(h: HWND, (x, y, w, hh): (i32, i32, i32, i32)) {
    let _ = unsafe { SetWindowPos(h, None, x, y, w, hh, SWP_NOZORDER | SWP_NOACTIVATE) };
}

pub fn move_to(h: HWND, x: i32, y: i32) {
    let _ = unsafe { SetWindowPos(h, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE) };
}

/// Unix seconds -> local "23 Nov 14:05" (empty when unknown).
pub fn fmt_started(unix: i64) -> String {
    use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    if unix <= 0 {
        return String::new();
    }
    let ft = (unix * 10_000_000 + EPOCH_DIFF) as u64;
    let ft = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    let (mut utc, mut loc) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err() || SystemTimeToTzSpecificLocalTime(None, &utc, &mut loc).is_err() {
            return String::new();
        }
    }
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{:02} {} {:02}:{:02}", loc.wDay, M[(loc.wMonth as usize).clamp(1, 12) - 1], loc.wHour, loc.wMinute)
}

// ------------------------------------------------------------------ settings + start with Windows

fn settings_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("APPDATA").unwrap_or_default()).join("ResMon").join("settings.json")
}

/// Where the tab was last docked (same file and format as the Python version: {"anchor": [x, y]}).
pub fn load_anchor() -> Option<(i32, i32)> {
    let s = std::fs::read_to_string(settings_path()).ok()?;
    let inner = s.split('[').nth(1)?.split(']').next()?;
    let mut it = inner.split(',').map(|v| v.trim().parse::<f64>().ok());
    let (x, y) = (it.next()??.round() as i32, it.next()??.round() as i32);
    monitor_exists(x, y).then_some((x, y))
}

pub fn save_anchor((x, y): (i32, i32)) -> std::io::Result<()> {
    let p = settings_path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::write(p, format!("{{\"anchor\": [{x}, {y}]}}"))
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

fn run_key(write: bool) -> Option<HKEY> {
    let mut k = HKEY::default();
    let sam = if write { KEY_SET_VALUE } else { KEY_QUERY_VALUE };
    (unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(RUN_KEY), None, sam, &mut k) }.0 == 0).then_some(k)
}

pub fn startup_enabled() -> bool {
    let Some(k) = run_key(false) else { return false };
    let ok = unsafe { RegQueryValueExW(k, &HSTRING::from("ResMon"), None, None, None, None) }.0 == 0;
    let _ = unsafe { RegCloseKey(k) };
    ok
}

pub fn set_startup(on: bool) -> Result<(), String> {
    let k = run_key(true).ok_or("cannot open the Run key")?;
    let r = if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let v: Vec<u8> = format!("\"{}\"", exe.display()).encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect();
        unsafe { RegSetValueExW(k, &HSTRING::from("ResMon"), None, REG_SZ, Some(&v)) }
    } else {
        unsafe { RegDeleteValueW(k, &HSTRING::from("ResMon")) }
    };
    let _ = unsafe { RegCloseKey(k) };
    if r.0 == 0 || (!on && r.0 == 2) { Ok(()) } else { Err(format!("registry error {}", r.0)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_table_sees_us() {
        let me = std::process::id();
        let ps = nt_processes();
        let p = ps.iter().find(|p| p.pid == me).expect("own pid");
        assert!(p.name.to_lowercase().starts_with("resmon"), "{}", p.name);
        assert!(p.ws > 1 << 20 && p.threads >= 1);
        assert!(p.create > 1_600_000_000);
        assert!(ps.len() > 20);
        assert!(cmdline(me).to_lowercase().contains("resmon"));
    }

    #[test]
    fn totals_are_sane() {
        let mut s = Sampler::new();
        let mut h = History::default();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let t = s.sample_totals(&mut h);
        assert_eq!(t.per_core.len(), s.ncpu);
        assert!((0.0..=100.0).contains(&t.cpu));
        assert!(t.ram_used > 0 && t.ram_used < t.ram_total);
        assert!(t.commit > 0 && t.commit <= t.commit_limit);
        assert!(!t.drives.is_empty());
        assert!(t.drives.iter().any(|d| d.0 == "C:"));
        let ps = s.sample_procs();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let ps2 = s.sample_procs();
        assert!(ps.len() > 20 && ps2.len() > 20);
        assert!(ps2.iter().map(|p| p.cpu).sum::<f64>() <= 101.0);
        assert!(pdh_pid("pid_1234_luid_0x0_phys_0_eng_0_engtype_3D") == Some(1234));
    }
}
