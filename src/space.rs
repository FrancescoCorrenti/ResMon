//! Disk space by folder: scanned in the background, cached on disk, refreshed in place.
//!
//! One walk does both the first scan and every refresh: breadth first, at background I/O priority, each
//! folder is listed and reconciled with what the tree already holds (same ids for folders still there,
//! new ones added, gone ones dropped, size differences carried up to the root). So the map changes only
//! where the disk did, and a first scan fills in evenly. Only folders are kept (files are summed into
//! their folder); symbolic links and junctions are not followed, so nothing is counted twice.
//!
//! The tree is saved when a walk ends (`%LOCALAPPDATA%\ResMon\space_C.bin`) and loaded next time: a drive
//! opens instantly and is walked again only when asked (Refresh). Without administrator rights there is no
//! change journal to read, so a refresh still lists every folder; it just changes nothing it need not.

use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
use windows::core::HSTRING;

pub struct Node {
    pub name: Box<str>,
    pub parent: u32,
    pub files: u64, // bytes of the files directly in this folder
    pub total: u64, // files + everything below (running while walking)
    pub children: Vec<u32>,
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub running: bool,
    pub checked: usize,           // folders listed by the current walk
    pub denied: usize,            // folders that could not be read
    pub scanned_at: Option<u64>,  // unix seconds of the last complete walk
    pub took: Option<f64>,        // seconds the last walk took
    pub version: u64,             // bumps on every change: the map relayouts when it moves
    started: Instant,
}

impl Tree {
    pub fn path(&self, mut id: u32) -> String {
        let mut parts = Vec::new();
        loop {
            let n = &self.nodes[id as usize];
            parts.push(&*n.name);
            if id == 0 {
                break;
            }
            id = n.parent;
        }
        parts.reverse();
        let mut p = parts.join("\\");
        if parts.len() == 1 && !p.ends_with('\\') {
            p.push('\\');
        }
        p
    }


    fn add_up(&mut self, mut id: u32, delta: i64) {
        loop {
            let t = &mut self.nodes[id as usize].total;
            *t = (*t as i64 + delta).max(0) as u64;
            if id == 0 {
                break;
            }
            id = self.nodes[id as usize].parent;
        }
    }
}

pub struct Scan {
    pub root: String, // "C:"
    pub tree: Arc<Mutex<Tree>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Scan {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn cache_path(root: &str) -> PathBuf {
    let tag: String = root.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join("ResMon").join(format!("space_{tag}.bin"))
}

impl Scan {
    /// Open `root` ("C:"): from the cache if there is one, otherwise with a first walk. `tick` is called
    /// about twice a second while a walk runs.
    pub fn open(root: &str, tick: impl Fn() + Send + 'static) -> Scan {
        let cached = load(root);
        let fresh = cached.is_none();
        let tree = cached.unwrap_or_else(|| Tree {
            nodes: vec![Node { name: root.into(), parent: 0, files: 0, total: 0, children: Vec::new() }],
            running: false,
            checked: 0,
            denied: 0,
            scanned_at: None,
            took: None,
            version: 0,
            started: Instant::now(),
        });
        let s = Scan { root: root.to_string(), tree: Arc::new(Mutex::new(tree)), stop: Arc::new(AtomicBool::new(false)) };
        if fresh {
            s.refresh(tick);
        }
        s
    }

    /// Walk the drive again, changing only what changed. Does nothing if a walk is running.
    pub fn refresh(&self, tick: impl Fn() + Send + 'static) {
        {
            let mut g = self.tree.lock().unwrap();
            if g.running {
                return;
            }
            (g.running, g.checked, g.denied, g.started) = (true, 0, 0, Instant::now());
        }
        let (t, stop, root) = (self.tree.clone(), self.stop.clone(), self.root.clone());
        std::thread::spawn(move || {
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) }; // low I/O priority too
            let mut queue = VecDeque::from([0u32]);
            let mut last_tick = Instant::now();
            while let Some(id) = queue.pop_front() {
                if stop.load(Ordering::Relaxed) {
                    t.lock().unwrap().running = false;
                    return;
                }
                let path = t.lock().unwrap().path(id);
                let (files, dirs, ok) = list(&path);
                let mut g = t.lock().unwrap();
                g.checked += 1;
                if !ok {
                    g.denied += 1; // keep what we knew
                    queue.extend(g.nodes[id as usize].children.iter().copied());
                    continue;
                }
                let mut changed = false;
                // files directly here
                let old = g.nodes[id as usize].files;
                if old != files {
                    g.nodes[id as usize].files = files;
                    g.add_up(id, files as i64 - old as i64);
                    changed = true;
                }
                // subfolders: keep the ones still there, add new ones, drop the gone ones
                let known: HashMap<Box<str>, u32> = g.nodes[id as usize].children.iter().map(|&c| (g.nodes[c as usize].name.clone(), c)).collect();
                let mut keep = Vec::with_capacity(dirs.len());
                for name in dirs {
                    match known.get(name.as_str()) {
                        Some(&c) => keep.push(c),
                        None => {
                            let nid = g.nodes.len() as u32;
                            g.nodes.push(Node { name: name.into(), parent: id, files: 0, total: 0, children: Vec::new() });
                            keep.push(nid);
                            changed = true;
                        }
                    }
                }
                for (_, &c) in known.iter().filter(|(_, c)| !keep.contains(c)) {
                    let gone = g.nodes[c as usize].total;
                    g.add_up(id, -(gone as i64));
                    changed = true;
                }
                queue.extend(keep.iter().copied());
                g.nodes[id as usize].children = keep;
                if changed {
                    g.version += 1;
                }
                drop(g);
                if last_tick.elapsed().as_millis() > 500 {
                    last_tick = Instant::now();
                    tick();
                }
            }
            let mut g = t.lock().unwrap();
            g.running = false;
            g.took = Some(g.started.elapsed().as_secs_f64());
            g.scanned_at = Some(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()));
            g.version += 1;
            let _ = save(&root, &g);
            drop(g);
            tick();
        });
    }
}

/// Bytes of the files in `dir`, names of its real subfolders, and whether it could be read.
fn list(dir: &str) -> (u64, Vec<String>, bool) {
    let pattern = HSTRING::from(format!("{}\\*", dir.trim_end_matches('\\')));
    let mut fd = WIN32_FIND_DATAW::default();
    let h = unsafe { FindFirstFileExW(&pattern, FindExInfoBasic, &mut fd as *mut _ as *mut _, FindExSearchNameMatch, None, FIND_FIRST_EX_LARGE_FETCH) };
    let Ok(h) = h else { return (0, Vec::new(), false) };
    let (mut files, mut dirs) = (0u64, Vec::new());
    loop {
        let len = fd.cFileName.iter().position(|&c| c == 0).unwrap_or(fd.cFileName.len());
        let name = String::from_utf16_lossy(&fd.cFileName[..len]);
        let attr = fd.dwFileAttributes;
        if attr & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            if name != "." && name != ".." && attr & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
                dirs.push(name);
            }
        } else {
            files += ((fd.nFileSizeHigh as u64) << 32) | fd.nFileSizeLow as u64;
        }
        if unsafe { FindNextFileW(h, &mut fd) }.is_err() {
            break;
        }
    }
    let _ = unsafe { FindClose(h) };
    (files, dirs, true)
}

// ---------------------------------------------------------------- cache: "RMS1", scanned_at u64, took f64,
// count u32, then per reachable folder (parents first): parent u32, files u64, name len u16, name UTF-8

fn save(root: &str, g: &Tree) -> std::io::Result<()> {
    let p = cache_path(root);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let mut order = vec![0u32]; // reachable nodes, breadth first, renumbered densely
    let mut i = 0;
    while i < order.len() {
        let id = order[i];
        order.extend(&g.nodes[id as usize].children);
        i += 1;
    }
    let new_id: HashMap<u32, u32> = order.iter().enumerate().map(|(k, &id)| (id, k as u32)).collect();
    let tmp = p.with_extension("tmp");
    let mut w = BufWriter::new(std::fs::File::create(&tmp)?);
    w.write_all(b"RMS1")?;
    w.write_all(&g.scanned_at.unwrap_or(0).to_le_bytes())?;
    w.write_all(&g.took.unwrap_or(0.0).to_le_bytes())?;
    w.write_all(&(order.len() as u32).to_le_bytes())?;
    for &id in &order {
        let n = &g.nodes[id as usize];
        w.write_all(&new_id[&n.parent].to_le_bytes())?;
        w.write_all(&n.files.to_le_bytes())?;
        let name = n.name.as_bytes();
        w.write_all(&(name.len() as u16).to_le_bytes())?;
        w.write_all(name)?;
    }
    w.flush()?;
    drop(w);
    std::fs::rename(tmp, p)
}

fn load(root: &str) -> Option<Tree> {
    let mut r = BufReader::new(std::fs::File::open(cache_path(root)).ok()?);
    let mut b8 = [0u8; 8];
    let mut b4 = [0u8; 4];
    let mut b2 = [0u8; 2];
    r.read_exact(&mut b4).ok()?;
    if &b4 != b"RMS1" {
        return None;
    }
    r.read_exact(&mut b8).ok()?;
    let scanned_at = u64::from_le_bytes(b8);
    r.read_exact(&mut b8).ok()?;
    let took = f64::from_le_bytes(b8);
    r.read_exact(&mut b4).ok()?;
    let count = u32::from_le_bytes(b4) as usize;
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        r.read_exact(&mut b4).ok()?;
        let parent = u32::from_le_bytes(b4);
        r.read_exact(&mut b8).ok()?;
        let files = u64::from_le_bytes(b8);
        r.read_exact(&mut b2).ok()?;
        let mut name = vec![0u8; u16::from_le_bytes(b2) as usize];
        r.read_exact(&mut name).ok()?;
        nodes.push(Node { name: String::from_utf8(name).ok()?.into(), parent, files, total: files, children: Vec::new() });
    }
    // children lists, then totals bottom-up (parents always come before their children)
    for id in (1..nodes.len()).rev() {
        let (p, t) = (nodes[id].parent as usize, nodes[id].total);
        nodes[p].total += t;
        nodes[p].children.push(id as u32);
    }
    for n in &mut nodes {
        n.children.reverse();
    }
    Some(Tree { nodes, running: false, checked: 0, denied: 0, scanned_at: Some(scanned_at), took: Some(took), version: 1, started: Instant::now() })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    fn wait(s: &super::Scan) {
        let t0 = std::time::Instant::now();
        while s.tree.lock().unwrap().running && t0.elapsed().as_secs() < 10 {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn scans_caches_and_refreshes_in_place() {
        let base = std::env::temp_dir().join(format!("resmon_space_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a").join("b")).unwrap();
        std::fs::create_dir_all(base.join("c")).unwrap();
        std::fs::write(base.join("top.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(base.join("a").join("b").join("deep.bin"), vec![0u8; 5000]).unwrap();
        std::fs::write(base.join("c").join("x.bin"), vec![0u8; 300]).unwrap();
        let root = base.to_string_lossy().to_string();
        let _ = std::fs::remove_file(super::cache_path(&root));
        // first walk
        let s = super::Scan::open(&root, || {});
        wait(&s);
        let a = {
            let g = s.tree.lock().unwrap();
            assert_eq!(g.nodes[0].total, 6300);
            assert_eq!(g.nodes[0].files, 1000);
            let a = g.nodes[0].children.iter().find(|&&c| &*g.nodes[c as usize].name == "a").copied().unwrap();
            assert_eq!(g.nodes[a as usize].total, 5000);
            assert!(g.path(g.nodes[a as usize].children[0]).ends_with("\\a\\b"));
            a
        };
        drop(s);
        // from the cache: same numbers, no walk
        let s = super::Scan::open(&root, || {});
        assert!(!s.tree.lock().unwrap().running);
        assert_eq!(s.tree.lock().unwrap().nodes[0].total, 6300);
        // the disk changes: a grows, c goes, d appears; a refresh keeps a's id and fixes the totals
        std::fs::write(base.join("a").join("more.bin"), vec![0u8; 700]).unwrap();
        std::fs::remove_dir_all(base.join("c")).unwrap();
        std::fs::create_dir_all(base.join("d")).unwrap();
        std::fs::write(base.join("d").join("y.bin"), vec![0u8; 40]).unwrap();
        let a_cached = {
            let g = s.tree.lock().unwrap();
            g.nodes[0].children.iter().find(|&&c| &*g.nodes[c as usize].name == "a").copied()
        };
        s.refresh(|| {});
        wait(&s);
        let g = s.tree.lock().unwrap();
        assert_eq!(g.nodes[0].total, 1000 + 5700 + 40);
        let names: Vec<&str> = g.nodes[0].children.iter().map(|&c| &*g.nodes[c as usize].name).collect();
        assert!(names.contains(&"d") && !names.contains(&"c"), "{names:?}");
        assert_eq!(g.nodes[0].children.iter().find(|&&c| &*g.nodes[c as usize].name == "a").copied(), a_cached);
        let _ = a;
        drop(g);
        drop(s);
        let _ = std::fs::remove_file(super::cache_path(&root));
        std::fs::remove_dir_all(base).unwrap();
    }
}
