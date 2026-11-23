//! Per-process network bytes, without administrator rights.
//!
//! Windows has no per-process network counter, and the kernel network provider needs elevation. The
//! Winsock (AFD) provider can be traced by members of "Performance Log Users" (the default for an admin
//! account), so we keep a real-time ETW session on it. Loopback traffic is included (it goes through
//! Winsock too); Task Manager leaves it out.
//!
//! Which events carry the bytes was measured, not documented (Windows 11 26300):
//! - sent: the send requests (location 0). Each send is logged twice per request (sequence 3047 and
//!   3056) and twice per completion, so 3047 is skipped. Blocking and overlapped sends move all they ask.
//! - received: the receive completions (location 1, status 0), logged in the process with the bytes
//!   delivered. A 10 MB curl download counted 10 507 419 bytes (file + headers). Known gap: when data is
//!   already queued as the app asks for it (reader slower than sender), the copy can go unlogged; a
//!   loopback test that writes faster than it reads counted 169 KB of 300 KB. The data indications
//!   (3003/3004) would see those, but they re-report bytes still queued, so they overcount.

use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Foundation::ERROR_ALREADY_EXISTS;
use windows::Win32::System::Diagnostics::Etw::*;
use windows::core::{GUID, PWSTR};

const AFD: GUID = GUID::from_u128(0xe53c6823_7bb8_44bb_90dc_3f86090d48a6); // Microsoft-Windows-Winsock-AFD
const SESSION: &str = "ResMon-net";
const SEND: [u16; 5] = [1003, 1005, 1007, 1011, 1013]; // send, sendto, sendto, sendmsg, sendmsg
const RECV: [u16; 5] = [1004, 1006, 1009, 1012, 1015]; // recv, recvfrom, recvfrom, recvmsg, recvmsg

/// pid -> cumulative (sent, received) bytes since the session started.
static TOTALS: Mutex<Option<HashMap<u32, (u64, u64)>>> = Mutex::new(None);

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    let r = unsafe { &*rec };
    if r.EventHeader.ProviderId != AFD || r.UserDataLength < 44 {
        return;
    }
    let id = r.EventHeader.EventDescriptor.Id;
    let send = SEND.contains(&id);
    if !send && !RECV.contains(&id) {
        return;
    }
    // payload (packed): location u32, sequence u32, process ptr, endpoint ptr, buffer count u32,
    // buffer ptr, length u32 (offset 36), status u32 (offset 40)
    let d = unsafe { std::slice::from_raw_parts(r.UserData as *const u8, r.UserDataLength as usize) };
    let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let (loc, seq) = (u32_at(0), u32_at(4));
    let counted = if send { loc == 0 && !(id == 1003 && seq == 3047) } else { loc == 1 && u32_at(40) == 0 };
    if !counted {
        return;
    }
    if let Ok(mut g) = TOTALS.lock()
        && let Some(m) = g.as_mut()
    {
        let e = m.entry(r.EventHeader.ProcessId).or_default();
        if send { e.0 += u32_at(36) as u64 } else { e.1 += u32_at(36) as u64 }
    }
}

fn props() -> Vec<u64> {
    // EVENT_TRACE_PROPERTIES followed by room for the session name (u64 buffer for alignment)
    let size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + 2 * (SESSION.len() + 1);
    let mut buf = vec![0u64; size / 8 + 1];
    let p = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
    unsafe {
        (*p).Wnode.BufferSize = size as u32;
        (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*p).Wnode.ClientContext = 1; // QPC timestamps
        (*p).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        (*p).FlushTimer = 1;
        (*p).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    }
    buf
}

/// Start the session and its consumer thread. Err tells why per-process network is unavailable.
pub fn start() -> Result<(), String> {
    let name = windows::core::HSTRING::from(SESSION);
    let mut h = CONTROLTRACE_HANDLE::default();
    let mut buf = props();
    let mut st = unsafe { StartTraceW(&mut h, &name, buf.as_mut_ptr() as *mut _) };
    if st == ERROR_ALREADY_EXISTS {
        stop(); // left over by a ResMon that was killed: replace it
        buf = props();
        st = unsafe { StartTraceW(&mut h, &name, buf.as_mut_ptr() as *mut _) };
    }
    if st.0 != 0 {
        return Err(format!("Per-process network needs your user in \"Performance Log Users\" (trace error {})", st.0));
    }
    // only the Winsock-side calls (keyword 0x4, level 4): the transport and fast-path events are the bulk
    let st = unsafe { EnableTraceEx2(h, &AFD, EVENT_CONTROL_CODE_ENABLE_PROVIDER.0, 4, 0x4, 0, 0, None) };
    if st.0 != 0 {
        stop();
        return Err(format!("The Winsock trace provider refused access (error {})", st.0));
    }
    *TOTALS.lock().unwrap() = Some(HashMap::new());
    std::thread::spawn(move || {
        let mut wname: Vec<u16> = SESSION.encode_utf16().chain([0]).collect();
        let mut lf = EVENT_TRACE_LOGFILEW { LoggerName: PWSTR(wname.as_mut_ptr()), ..Default::default() };
        lf.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        lf.Anonymous2.EventRecordCallback = Some(on_event);
        let th = unsafe { OpenTraceW(&mut lf) };
        if th.Value != u64::MAX {
            let _ = unsafe { ProcessTrace(&[th], None, None) }; // blocks until the session stops
            let _ = unsafe { CloseTrace(th) };
        }
    });
    Ok(())
}

/// Stop the session (a real-time session outlives its process otherwise).
pub fn stop() {
    let mut b = props();
    let _ = unsafe { ControlTraceW(CONTROLTRACE_HANDLE::default(), &windows::core::HSTRING::from(SESSION), b.as_mut_ptr() as *mut _, EVENT_TRACE_CONTROL_STOP) };
}

/// Snapshot of the cumulative (sent, received) bytes per PID; None if the session is not running.
pub fn totals() -> Option<HashMap<u32, (u64, u64)>> {
    TOTALS.lock().ok()?.clone()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// 300 KB over loopback TCP from this process: sends exact; receives undercount when the reader
    /// lags (see the module notes), so only a floor is checked.
    #[test]
    fn sees_our_own_traffic() {
        if let Err(e) = super::start() {
            eprintln!("skipped: {e}");
            return;
        }
        std::thread::sleep(Duration::from_millis(1500));
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let a = l.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            use std::io::Read;
            let (mut s, _) = l.accept().unwrap();
            let mut v = Vec::new();
            s.read_to_end(&mut v).unwrap();
            v.len()
        });
        {
            use std::io::Write;
            let mut c = std::net::TcpStream::connect(a).unwrap();
            c.write_all(&vec![7u8; 300_000]).unwrap();
        }
        assert_eq!(t.join().unwrap(), 300_000);
        std::thread::sleep(Duration::from_millis(3000)); // real-time buffers flush every second
        let m = super::totals().unwrap();
        super::stop();
        let (s, r) = m.get(&std::process::id()).copied().unwrap_or_default();
        assert!((300_000..301_000).contains(&s) && (150_000..301_000).contains(&r), "sent {s} received {r}");
    }

    /// Real traffic (needs internet): a 10 MB download through curl must count 10 MB received.
    #[test]
    #[ignore]
    fn curl_download() {
        super::start().unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let mut c = std::process::Command::new("curl.exe").args(["-s", "-o", "NUL", "https://proof.ovh.net/files/10Mb.dat"]).spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        std::thread::sleep(Duration::from_millis(3000));
        let m = super::totals().unwrap();
        super::stop();
        let r = m.get(&pid).map_or(0, |e| e.1);
        assert!((10_485_760..10_600_000).contains(&r), "received {r}");
    }
}
