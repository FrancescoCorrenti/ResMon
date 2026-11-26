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
    pub io_r: u64,   // read / write transfer bytes: all file I/O, like Task Manager's I/O columns
    pub io_w: u64,
    pub thr: Vec<(u32, i64)>, // (thread id, kernel + user 100 ns), only when asked for
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

/// The SYSTEM_THREAD_INFORMATION records that follow a process record (x64: 0x50 bytes each; kernel time
/// at 0, user time at 8, thread id in CLIENT_ID at 48).
fn thread_times(proc_rec: *const u8, n: u32) -> Vec<(u32, i64)> {
    (0..n as usize)
        .map(|i| unsafe {
            let t = proc_rec.add(0x100 + i * 0x50);
            let k = (t as *const i64).read_unaligned();
            let u = (t.add(8) as *const i64).read_unaligned();
            let tid = (t.add(48) as *const usize).read_unaligned() as u32;
            (tid, k + u)
        })
        .collect()
}

/// Ideal processor of a thread (the logical CPU the scheduler prefers for it), group 0 only.
fn ideal_cpu(tid: u32) -> Option<u16> {
    use windows::Win32::System::Kernel::PROCESSOR_NUMBER;
    let h = Handle(unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, tid) }.ok()?);
    let mut pn = PROCESSOR_NUMBER::default();
    unsafe { GetThreadIdealProcessorEx(h.0, &mut pn) }.ok()?;
    (pn.Group == 0).then_some(pn.Number as u16)
}

pub fn nt_processes(threads: bool) -> Vec<RawProc> {
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
                io_r: e.read_transfer_count as u64,
                io_w: e.write_transfer_count as u64,
                thr: if threads { thread_times(unsafe { base.add(off) }, e.number_of_threads) } else { Vec::new() },
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

/// Physical cores of processor group 0: for each, its logical processors (threads, same order as the
/// per-core loads) and its efficiency class (hybrid CPUs: higher = performance core).
pub fn core_topology() -> Vec<(Vec<usize>, u8)> {
    use windows::Win32::System::SystemInformation::{GetLogicalProcessorInformationEx, RelationProcessorCore};
    let mut len = 0u32;
    let _ = unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len) };
    let mut buf: Vec<u64> = vec![0; len as usize / 8 + 1];
    if unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, Some(buf.as_mut_ptr() as *mut _), &mut len) }.is_err() {
        return Vec::new();
    }
    let b = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, len as usize) };
    let mut out = Vec::new();
    let mut off = 0;
    // record: relationship u32, size u32, then PROCESSOR_RELATIONSHIP: flags u8, efficiency class u8,
    // reserved [u8; 20], group count u16 (offset 30), GROUP_AFFINITY { mask u64 (32), group u16 (40) }
    while off + 42 <= b.len() {
        let size = u32::from_le_bytes(b[off + 4..off + 8].try_into().unwrap()) as usize;
        let class = b[off + 9];
        let mask = u64::from_le_bytes(b[off + 32..off + 40].try_into().unwrap());
        let group = u16::from_le_bytes(b[off + 40..off + 42].try_into().unwrap());
        if group == 0 {
            out.push(((0..64).filter(|i| mask >> i & 1 == 1).collect(), class));
        }
        if size == 0 {
            break;
        }
        off += size;
    }
    out
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

/// RESMON_DEMO: for screenshots that must not show the owner's machine. Real measurements, but every
/// process that is not a stock Windows one is renamed to a common app, command lines and users are made up.
pub fn demo() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var_os("RESMON_DEMO").is_some())
}

const DEMO_KEEP: [&str; 40] = [
    "System", "Registry", "Idle", "System Idle Process", "Memory Compression", "Secure System", "svchost.exe", "explorer.exe",
    "dwm.exe", "csrss.exe", "lsass.exe", "services.exe", "wininit.exe", "winlogon.exe", "smss.exe", "fontdrvhost.exe",
    "sihost.exe", "taskhostw.exe", "ctfmon.exe", "StartMenuExperienceHost.exe", "TextInputHost.exe", "ShellExperienceHost.exe",
    "SearchHost.exe", "RuntimeBroker.exe", "conhost.exe", "audiodg.exe", "spoolsv.exe", "WmiPrvSE.exe", "dllhost.exe",
    "MsMpEng.exe", "SecurityHealthService.exe", "NVDisplay.Container.exe", "nvcontainer.exe", "msedge.exe",
    "msedgewebview2.exe", "vmmem", "vmmemWSL", "WindowsTerminal.exe", "OpenConsole.exe", "ResMon.exe",
];
const DEMO_APPS: [&str; 16] = [
    "chrome.exe", "Code.exe", "Spotify.exe", "Slack.exe", "steam.exe", "blender.exe", "python.exe", "Figma.exe",
    "Teams.exe", "Notion.exe", "obs64.exe", "node.exe", "firefox.exe", "Photoshop.exe", "Docker Desktop.exe", "zoom.exe",
];

fn demo_mask(p: &mut Proc) {
    if !DEMO_KEEP.iter().any(|k| k.eq_ignore_ascii_case(&p.name)) {
        let h = p.name.bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b.to_ascii_lowercase() as u32);
        p.name = DEMO_APPS[h as usize % DEMO_APPS.len()].to_string();
        p.group = p.name.clone();
    }
    p.cmd = format!("\"C:\\Program Files\\{}\" --type=renderer", p.name);
}

pub fn username(pid: u32) -> String {
    if demo() {
        return "DESKTOP\\you".into();
    }
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

/// Bytes (received, sent) so far on the physical network adapters only. Virtual ones (Hyper-V and WSL
/// switches, Docker, most VPN and TAP drivers) carry the same bytes again and would count them twice.
pub fn net_octets() -> Option<(u64, u64)> {
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
    let mut t: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    if unsafe { GetIfTable2(&mut t) }.is_err() || t.is_null() {
        return None;
    }
    let rows = unsafe { std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize) };
    let (mut rx, mut tx) = (0u64, 0u64);
    for r in rows {
        let f = r.InterfaceAndOperStatusFlags._bitfield;
        let (hardware, filter) = (f & 1 != 0, f & 2 != 0); // HardwareInterface, FilterInterface
        if hardware && !filter {
            rx += r.InOctets;
            tx += r.OutOctets;
        }
    }
    unsafe { FreeMibTable(t as *const _) };
    Some((rx, tx))
}

/// False if another ResMon already runs in this session (a named mutex it holds until it exits).
pub fn first_instance() -> bool {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    match unsafe { CreateMutexW(None, true, &HSTRING::from("Local\\ResMon-single-instance")) } {
        Ok(_) => (unsafe { GetLastError() }) != ERROR_ALREADY_EXISTS, // the handle stays open for the process lifetime
        Err(_) => true,
    }
}

/// No "There is no disk in the drive" system box when a card reader slot is polled for free space.
pub fn quiet_drive_errors() {
    use windows::Win32::System::Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SetErrorMode};
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS) };
}

/// The process `pid` only if it is still the one born at `born` (its creation time, 100 ns since 1601):
/// between a click and the action the pid may have been reused by another process.
fn open_same(pid: u32, born: i64, access: PROCESS_ACCESS_RIGHTS) -> Option<Handle> {
    let h = Handle(open(pid, access | PROCESS_QUERY_LIMITED_INFORMATION)?);
    let (mut c, mut e, mut k, mut u) = Default::default();
    unsafe { GetProcessTimes(h.0, &mut c, &mut e, &mut k, &mut u).ok()? };
    let created = ((c.dwHighDateTime as i64) << 32) | c.dwLowDateTime as i64;
    (created == born).then_some(h)
}

/// Ok(true) killed, Ok(false) gone already (or the pid now belongs to another process), Err(()) access denied.
pub fn kill(pid: u32, born: i64) -> Result<bool, ()> {
    let Some(h) = open_same(pid, born, PROCESS_TERMINATE) else {
        return if open(pid, PROCESS_QUERY_LIMITED_INFORMATION).map(Handle).is_some_and(|_| open(pid, PROCESS_TERMINATE).is_none()) {
            Err(())
        } else {
            Ok(false)
        };
    };
    unsafe { TerminateProcess(h.0, 1) }.map(|_| true).map_err(|_| ())
}

pub fn set_priority(pid: u32, born: i64, class: PROCESS_CREATION_FLAGS) -> bool {
    open_same(pid, born, PROCESS_SET_INFORMATION).is_some_and(|h| unsafe { SetPriorityClass(h.0, class) }.is_ok())
}

pub const PRIORITIES: [(&str, PROCESS_CREATION_FLAGS); 5] = [
    ("Idle", IDLE_PRIORITY_CLASS),
    ("Below normal", BELOW_NORMAL_PRIORITY_CLASS),
    ("Normal", NORMAL_PRIORITY_CLASS),
    ("Above normal", ABOVE_NORMAL_PRIORITY_CLASS),
    ("High", HIGH_PRIORITY_CLASS),
];

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtSuspendProcess(h: HANDLE) -> i32;
    fn NtResumeProcess(h: HANDLE) -> i32;
}

/// Freeze (or thaw) every thread of a process: a training run paused while the machine is needed, then
/// resumed where it was. True if it worked.
pub fn suspend(pid: u32, born: i64, on: bool) -> bool {
    let Some(h) = open_same(pid, born, PROCESS_SUSPEND_RESUME) else { return false };
    (unsafe { if on { NtSuspendProcess(h.0) } else { NtResumeProcess(h.0) } }) == 0
}

/// Ask the shell to do `verb` on `target` ("open" a folder or URL, "properties" of a file or folder).
pub fn shell(verb: &str, target: &str) -> bool {
    use windows::Win32::UI::Shell::{SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW, ShellExecuteExW};
    let (v, t) = (HSTRING::from(verb), HSTRING::from(target));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST, // needed for "properties"
        lpVerb: PCWSTR(v.as_ptr()),
        lpFile: PCWSTR(t.as_ptr()),
        nShow: 1, // SW_SHOWNORMAL
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.is_ok()
}

/// A terminal in `dir`: Windows Terminal if there is one, otherwise PowerShell.
pub fn terminal(dir: &str) {
    use std::os::windows::process::CommandExt;
    let wt = std::process::Command::new("wt.exe").args(["-d", dir]).spawn();
    if wt.is_err() {
        let _ = std::process::Command::new("powershell.exe").args(["-NoExit", "-Command", "Set-Location -LiteralPath $args[0]", dir]).creation_flags(0x10).spawn();
    }
}

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
            out.push((String::from_utf16_lossy(root).trim_end_matches('\\').to_string(), total.saturating_sub(free), total));
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
            // a counter this Windows lacks (an old build, no network adapter...) reads as empty, the rest still works
            if unsafe { PdhAddEnglishCounterW(q, &HSTRING::from(*p), 0, &mut c) } != 0 {
                c = PDH_HCOUNTER::default();
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
        if self.counters[i].is_invalid() {
            return Vec::new();
        }
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
    pub power_limit: Option<f32>,     // W, the cap the driver enforces
    pub clock: Option<(u32, u32)>,    // graphics MHz now, max
    pub mem_clock: Option<u32>,       // MHz
    pub fan: Option<u32>,             // %
    pub pcie: Option<(u32, u32, u32)>, // gen, width now, max width
    pub throttle: Vec<&'static str>,  // why the clocks are held down (idle and app settings left out)
    pub shared: bool,                 // integrated GPU: its memory is shared system memory, not VRAM
    pub temp_limit: Option<u32>,      // °C where this GPU starts slowing itself down (NVML)
}

/// "NVIDIA GeForce RTX 4070", "AMD Radeon(TM) Graphics", "Intel(R) UHD Graphics 620" without the
/// trademark noise and the vendor prefix, for a short subtitle.
pub fn short_gpu_name(n: &str) -> String {
    let s = n.replace("(TM)", "").replace("(R)", "");
    let s = ["NVIDIA GeForce ", "NVIDIA ", "AMD Radeon ", "AMD ", "Intel "].iter().find_map(|p| s.strip_prefix(p)).unwrap_or(&s);
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A display adapter as DXGI sees it: any vendor, integrated or discrete.
#[derive(Clone)]
struct Adapter {
    name: String,
    luid: String, // as in PDH instance names: "luid_0x00000000_0x0000D1F2"
    dedicated: u64,
    shared: u64,
}

/// Hardware adapters (the software renderer left out), the main one first: the most dedicated memory, i.e.
/// the discrete GPU on a laptop with two, or the integrated one when it is the only GPU.
fn adapters() -> Vec<Adapter> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1};
    let Ok(f) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return Vec::new() };
    let mut out = Vec::new();
    for i in 0.. {
        let Ok(a) = (unsafe { f.EnumAdapters1(i) }) else { break };
        let Ok(d) = (unsafe { a.GetDesc1() }) else { continue };
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || d.VendorId == 0x1414 {
            continue; // Microsoft Basic Render Driver
        }
        let n = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
        out.push(Adapter {
            name: String::from_utf16_lossy(&d.Description[..n]).trim().to_string(),
            luid: format!("luid_0x{:08X}_0x{:08X}", d.AdapterLuid.HighPart as u32, d.AdapterLuid.LowPart),
            dedicated: d.DedicatedVideoMemory as u64,
            shared: d.SharedSystemMemory as u64,
        });
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.dedicated));
    out
}

/// GPU load and memory for any vendor from the PDH GPU counters (what Task Manager shows), for machines
/// without NVML (AMD, Intel, integrated graphics).
struct PdhGpu {
    q: Pdh,
    main: Adapter,
    uma: bool, // integrated: no dedicated memory to speak of, it works in shared system memory
}

impl PdhGpu {
    fn new(main: Adapter) -> Option<PdhGpu> {
        let q = Pdh::new(&[r"\GPU Engine(*)\Utilization Percentage", r"\GPU Adapter Memory(*)\Dedicated Usage", r"\GPU Adapter Memory(*)\Shared Usage"])?;
        let uma = main.dedicated < 512 << 20;
        Some(PdhGpu { q, main, uma })
    }

    fn sample(&self) -> Gpu {
        self.q.collect();
        let mut per_engine: HashMap<String, f64> = HashMap::new(); // summed over processes per engine type, then max
        for (inst, v) in self.q.array(0, false) {
            if inst.contains(&self.main.luid) {
                let eng = inst.rsplit("engtype_").next().unwrap_or("").to_string();
                *per_engine.entry(eng).or_default() += v;
            }
        }
        let util = per_engine.values().fold(0.0f64, |m, &v| m.max(v)).min(100.0) as f32;
        let mem = |i: usize| self.q.array(i, true).into_iter().filter(|(n, _)| n.contains(&self.main.luid)).map(|(_, v)| v as u64).sum::<u64>();
        let (mem_used, mem_total) = if self.uma { (mem(2), self.main.shared) } else { (mem(1), self.main.dedicated) };
        Gpu { name: self.main.name.clone(), util: Some(util), mem_used, mem_total, shared: self.uma, ..Default::default() }
    }
}

struct Nvml {
    nvml: nvml_wrapper::Nvml,
    name: String,
    index: u32,
}

impl Nvml {
    /// NVML for the main adapter, if it is an NVIDIA one: temperature, power, fan, clocks, slowdown reasons.
    fn new(main: Option<&Adapter>) -> Result<Nvml, String> {
        let nvml = nvml_wrapper::Nvml::init().map_err(|e| format!("NVML: {e}"))?;
        let n = nvml.device_count().map_err(|e| format!("NVML: {e}"))?;
        // the NVML device that is the main adapter (names match: "NVIDIA GeForce RTX ..."); else the first
        let pick = (0..n)
            .filter_map(|i| Some((i, nvml.device_by_index(i).ok()?.name().ok()?)))
            .find(|(_, name)| main.is_none_or(|a| a.name == *name))
            .ok_or("NVML: no device for the main GPU")?;
        Ok(Nvml { nvml, name: pick.1, index: pick.0 })
    }

    fn sample(&self) -> Gpu {
        let mut g = Gpu { name: self.name.clone(), ..Default::default() };
        if let Ok(d) = self.nvml.device_by_index(self.index) {
            g.util = d.utilization_rates().ok().map(|u| u.gpu as f32);
            g.temp = d.temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu).ok();
            g.temp_limit = d.temperature_threshold(nvml_wrapper::enum_wrappers::device::TemperatureThreshold::Slowdown).ok();
            g.power = d.power_usage().ok().map(|mw| mw as f32 / 1000.0);
            if let Ok(m) = d.memory_info() {
                (g.mem_used, g.mem_total) = (m.used, m.total);
            }
            use nvml_wrapper::bitmasks::device::ThrottleReasons as T;
            use nvml_wrapper::enum_wrappers::device::Clock;
            g.power_limit = d.enforced_power_limit().ok().map(|mw| mw as f32 / 1000.0);
            g.clock = d.clock_info(Clock::Graphics).ok().zip(d.max_clock_info(Clock::Graphics).ok());
            g.mem_clock = d.clock_info(Clock::Memory).ok();
            g.fan = d.fan_speed(0).ok();
            g.pcie = match (d.current_pcie_link_gen(), d.current_pcie_link_width(), d.max_pcie_link_width()) {
                (Ok(a), Ok(b), Ok(c)) => Some((a, b, c)),
                _ => None,
            };
            if let Ok(r) = d.current_throttle_reasons() {
                for (flag, why) in [
                    (T::SW_POWER_CAP, "power cap"),
                    (T::SW_THERMAL_SLOWDOWN, "thermal"),
                    (T::HW_THERMAL_SLOWDOWN, "thermal (hardware)"),
                    (T::HW_SLOWDOWN, "hardware slowdown"),
                    (T::HW_POWER_BRAKE_SLOWDOWN, "power brake"),
                ] {
                    if r.contains(flag) {
                        g.throttle.push(why);
                    }
                }
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
    pub drive_io: Vec<(String, f64, f64)>, // per volume ("C:"): read, write bytes/s
    pub pdh_ok: bool,
    pub gpu_err: Option<String>,
    pub net_err: Option<String>, // why per-process network is missing
}

#[derive(Clone, Default)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    pub group: String, // what the map groups it under: its name, or its family (the process that launched the job)
    pub threads: u32,
    pub ws: f64,
    pub private: f64,
    pub cpu: f64, // % of the whole machine
    pub io_r: f64, // bytes/s
    pub io_w: f64,
    pub net_d: f64, // bytes/s, from the Winsock trace (net.rs)
    pub net_u: f64,
    pub cpus: Vec<(u16, f32)>, // per-core view: (logical CPU, % of it), estimated from the threads' ideal processors
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
    IoRead,
    IoWrite,
    NetDown,
    NetUp,
}

impl Field {
    pub fn is_rate(self) -> bool {
        matches!(self, Field::IoRead | Field::IoWrite | Field::NetDown | Field::NetUp)
    }
}

impl Proc {
    pub fn get(&self, f: Field) -> f64 {
        match f {
            Field::Ws => self.ws,
            Field::Private => self.private,
            Field::Cpu => self.cpu,
            Field::GpuMem => self.gpu_mem,
            Field::Gpu => self.gpu,
            Field::IoRead => self.io_r,
            Field::IoWrite => self.io_w,
            Field::NetDown => self.net_d,
            Field::NetUp => self.net_u,
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
    pdh_gpu: Option<PdhGpu>, // any vendor, when the main GPU has no NVML
    gpu_uma: bool,           // the main GPU is integrated (shared memory only)
    net_prev: Option<(u64, u64, Instant)>, // physical adapters' byte counters at the last tick
    gpu_err: Option<String>,
    io: Option<Pdh>,  // disk + network rates, every tick
    gpu: Option<Pdh>, // per-process GPU, only with the process table
    net_err: Option<String>,
    net_on: bool,
    pub split: bool, // per-core view open: also sample threads
    prev_thr: HashMap<u32, (u32, i64)>,
    cores: Vec<(i64, i64)>,
    prev: HashMap<(u32, i64), [u64; 5]>, // cpu 100 ns, io read, io write, net sent, net received
    prev_t: Option<Instant>,
    cmd: HashMap<(u32, i64), String>,
    ticks: u64,
    drives: Vec<(String, u64, u64)>,
}

impl Sampler {
    pub fn new() -> Sampler {
        let main = adapters().into_iter().next();
        let gpu_uma = main.as_ref().is_some_and(|a| a.dedicated < 512 << 20);
        // RESMON_NO_NVML: run the vendor-neutral path on an NVIDIA machine, to check it
        let nvml = if std::env::var_os("RESMON_NO_NVML").is_some() { None } else { Nvml::new(main.as_ref()).ok() };
        let pdh_gpu = if nvml.is_none() { main.clone().and_then(PdhGpu::new) } else { None };
        let gpu_err = (nvml.is_none() && pdh_gpu.is_none()).then(|| if main.is_none() { "No GPU found" } else { "GPU counters not available" }.to_string());
        let cores = core_times();
        Sampler {
            // every processor group: process CPU times cover all of them. ponytail: per-core bars and the total
            // still read group 0 only (class 8); machines with >64 threads need the per-group query for those
            ncpu: (unsafe { GetActiveProcessorCount(0xFFFF) } as usize).max(cores.len()).max(1),
            nvml,
            pdh_gpu,
            gpu_err,
            net_err: None,
            net_on: false,
            split: false,
            prev_thr: HashMap::new(),
            io: Pdh::new(&[
                r"\PhysicalDisk(_Total)\Disk Read Bytes/sec",
                r"\PhysicalDisk(_Total)\Disk Write Bytes/sec",
                r"\Network Interface(*)\Bytes Received/sec",
                r"\Network Interface(*)\Bytes Sent/sec",
                r"\LogicalDisk(*)\Disk Read Bytes/sec",
                r"\LogicalDisk(*)\Disk Write Bytes/sec",
            ]),
            gpu: Pdh::new(&[r"\GPU Engine(*)\Utilization Percentage", r"\GPU Process Memory(*)\Dedicated Usage", r"\GPU Process Memory(*)\Shared Usage"]),
            gpu_uma,
            net_prev: None,
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
        let gpu = match (&self.nvml, &self.pdh_gpu) {
            (Some(n), _) => n.sample(),
            (None, Some(p)) => p.sample(),
            _ => Gpu::default(),
        };
        let (mut disk_r, mut disk_w, mut net_d, mut net_u) = (0.0, 0.0, 0.0, 0.0);
        let mut drive_io = Vec::new();
        if let Some(p) = &self.io {
            p.collect();
            (disk_r, disk_w, net_d, net_u) = (p.sum(0), p.sum(1), p.sum(2), p.sum(3));
        }
        // physical adapters only (PDH's Network Interface(*) also lists the virtual ones, double counting)
        if let Some((rx, tx)) = net_octets() {
            let now = Instant::now();
            if let Some((rx0, tx0, t0)) = self.net_prev {
                let dt = (now - t0).as_secs_f64().max(1e-3);
                (net_d, net_u) = (rx.saturating_sub(rx0) as f64 / dt, tx.saturating_sub(tx0) as f64 / dt);
            } else {
                (net_d, net_u) = (0.0, 0.0);
            }
            self.net_prev = Some((rx, tx, now));
        }
        if let Some(p) = &self.io {
            let w: HashMap<String, f64> = p.array(5, false).into_iter().collect();
            drive_io = p.array(4, false).into_iter().filter(|(n, _)| n.ends_with(':')).map(|(n, r)| { let wv = w.get(&n).copied().unwrap_or(0.0); (n, r, wv) }).collect();
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
            drive_io,
            pdh_ok: self.gpu.is_some(),
            gpu_err: self.gpu_err.clone(),
            net_err: self.net_err.clone(),
        }
    }

    /// The network trace runs only while someone looks at processes (the full view): docked it would cost
    /// CPU and buffers for numbers nobody sees.
    pub fn set_net(&mut self, on: bool) {
        if on == self.net_on {
            return;
        }
        self.net_on = on;
        if on {
            self.net_err = crate::net::start().err();
        } else {
            crate::net::stop();
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
                    // per adapter and engine type: the same engine type on two GPUs must not add up
                    let adapter = inst.find("luid_").map(|s| &inst[s..]).and_then(|s| s.split("_eng_").next()).unwrap_or("");
                    let eng = inst.rsplit("engtype_").next().unwrap_or("");
                    *per_engine.entry((pid, format!("{adapter}/{eng}"))).or_default() += v;
                }
            }
            for ((pid, _), v) in per_engine {
                let e = gutil.entry(pid).or_default();
                *e = e.max(v).min(100.0);
            }
            // an integrated GPU has no dedicated memory: what processes hold there is shared memory
            for i in if self.gpu_uma { vec![1, 2] } else { vec![1] } {
                for (inst, v) in p.array(i, true) {
                    if let Some(pid) = pdh_pid(&inst) {
                        *gmem.entry(pid).or_default() += v;
                    }
                }
            }
        }
        let mut cur = HashMap::new();
        let mut live = HashSet::new();
        let ncpu = self.ncpu as f64;
        let net = crate::net::totals().unwrap_or_default();
        let prev_thr = std::mem::take(&mut self.prev_thr);
        let mut cur_thr = HashMap::new();
        let mut procs: Vec<Proc> = nt_processes(self.split)
            .into_iter()
            .map(|r| {
                let key = (r.pid, r.create);
                live.insert(key);
                let (ns, nr) = net.get(&r.pid).copied().unwrap_or_default();
                let now = [r.cpu_100ns.max(0) as u64, r.io_r, r.io_w, ns, nr];
                cur.insert(key, now);
                let mut rate = [0.0f64; 5];
                if let (Some(dt), Some(before)) = (dt, self.prev.get(&key)) {
                    for i in 0..5 {
                        rate[i] = now[i].saturating_sub(before[i]) as f64 / dt;
                    }
                }
                let mut cpus: Vec<(u16, f32)> = Vec::new();
                for &(tid, t) in &r.thr {
                    cur_thr.insert(tid, (r.pid, t));
                    if let (Some(dt), Some(&(pid0, t0))) = (dt, prev_thr.get(&tid))
                        && pid0 == r.pid
                        && t > t0
                        && let Some(c) = ideal_cpu(tid)
                    {
                        let pct = ((t - t0) as f64 / 1e7 / dt * 100.0) as f32;
                        match cpus.iter_mut().find(|e| e.0 == c) {
                            Some(e) => e.1 += pct,
                            None => cpus.push((c, pct)),
                        }
                    }
                }
                let cmd = self.cmd.entry(key).or_insert_with(|| cmdline(r.pid)).clone();
                Proc {
                    pid: r.pid,
                    ppid: r.ppid,
                    threads: r.threads,
                    ws: r.ws as f64,
                    private: r.private as f64,
                    cpu: rate[0] / 1e7 * 100.0 / ncpu,
                    io_r: rate[1],
                    io_w: rate[2],
                    net_u: rate[3],
                    net_d: rate[4],
                    cpus,
                    create: r.create,
                    gpu_mem: gmem.get(&r.pid).copied().unwrap_or(0.0),
                    gpu: gutil.get(&r.pid).copied().unwrap_or(0.0),
                    cmd,
                    group: r.name.clone(),
                    name: r.name,
                }
            })
            .collect();
        self.prev = cur;
        self.prev_thr = cur_thr;
        self.cmd.retain(|k, _| live.contains(k));
        let pids: HashSet<u32> = live.iter().map(|k| k.0).collect();
        crate::net::retain(|p| pids.contains(&p));
        if demo() {
            procs.iter_mut().for_each(demo_mask);
        }
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

/// The primary mouse button: GetAsyncKeyState reads physical buttons, and left-handed users swap them.
pub fn left_button_down() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON};
    let vk = if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0 { 0x02 } else { 0x01 };
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
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

use std::os::windows::process::CommandExt;

pub fn set_startup(on: bool) -> Result<(), String> {
    let k = run_key(true).ok_or("cannot open the Run key")?;
    let r = if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        // with the "ResMon" task (tools/install-admin.ps1) the logon start goes through it: elevated, no prompt
        let task = std::process::Command::new("schtasks").args(["/query", "/tn", "ResMon"]).creation_flags(0x0800_0000).output().is_ok_and(|o| o.status.success());
        let cmd = if task { r"schtasks.exe /run /tn ResMon".to_string() } else { format!("\"{}\"", exe.display()) };
        let v: Vec<u8> = cmd.encode_utf16().chain([0]).flat_map(|c| c.to_le_bytes()).collect();
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
        let ps = nt_processes(true);
        assert!(ps.iter().find(|p| p.pid == me).unwrap().thr.len() >= 1);
        assert_eq!(std::mem::size_of::<Spi>(), 0x100);
        let p = ps.iter().find(|p| p.pid == me).expect("own pid");
        assert!(p.name.to_lowercase().starts_with("resmon"), "{}", p.name);
        assert!(p.ws > 1 << 20 && p.threads >= 1);
        assert!(p.create > 1_600_000_000);
        assert!(ps.len() > 20);
        assert!(cmdline(me).to_lowercase().contains("resmon"));
    }

    #[test]
    fn every_thread_belongs_to_one_core() {
        let cores = core_topology();
        let mut all: Vec<usize> = cores.iter().flat_map(|c| c.0.clone()).collect();
        all.sort();
        let n = unsafe { windows::Win32::System::Threading::GetActiveProcessorCount(0) } as usize;
        assert_eq!(all, (0..n).collect::<Vec<_>>(), "{cores:?}");
    }

    #[test]
    fn totals_are_sane() {
        let mut s = Sampler::new();
        let mut h = History::default();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let t = s.sample_totals(&mut h);
        assert!(!t.per_core.is_empty() && t.per_core.len() <= s.ncpu);
        assert!((0.0..=100.0).contains(&t.cpu));
        assert!(t.ram_used > 0 && t.ram_used < t.ram_total);
        assert!(t.commit > 0 && t.commit <= t.commit_limit);
        assert!(!t.drives.is_empty());
        let sys_drive = std::env::var("SystemDrive").unwrap_or("C:".into()); // not always C:
        assert!(t.drives.iter().any(|d| d.0.eq_ignore_ascii_case(&sys_drive)));
        let ps = s.sample_procs();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let ps2 = s.sample_procs();
        assert!(ps.len() > 20 && ps2.len() > 20);
        assert!(ps2.iter().map(|p| p.cpu).sum::<f64>() <= 101.0);
        assert!(pdh_pid("pid_1234_luid_0x0_phys_0_eng_0_engtype_3D") == Some(1234));
    }

    #[test]
    fn gpu_names_and_vendor_neutral_path() {
        assert_eq!(short_gpu_name("NVIDIA GeForce RTX 3060"), "RTX 3060");
        assert_eq!(short_gpu_name("AMD Radeon(TM) Graphics"), "Graphics");
        assert_eq!(short_gpu_name("Intel(R) UHD Graphics 620"), "UHD Graphics 620");
        // whatever GPU this machine has, the PDH path must see the same memory size as DXGI, and, on an
        // NVIDIA card, about the VRAM NVML reports
        let Some(main) = adapters().into_iter().next() else { return };
        let Some(p) = PdhGpu::new(main.clone()) else { return };
        std::thread::sleep(std::time::Duration::from_millis(300));
        let g = p.sample();
        assert!(g.util.is_some_and(|u| (0.0..=100.0).contains(&u)));
        assert!(g.mem_total > 0 && g.mem_used <= g.mem_total);
        if let Ok(n) = Nvml::new(Some(&main)) {
            let nv = n.sample();
            let (a, b) = (g.mem_used as f64, nv.mem_used as f64);
            eprintln!("{}: PDH {:.2} GB of {:.2}, NVML {:.2} GB, load {:?}", main.name, a / 1e9, g.mem_total as f64 / 1e9, b / 1e9, g.util);
            assert!((a - b).abs() < 0.25 * b.max(1e9), "PDH {a} vs NVML {b}");
        }
    }
}
