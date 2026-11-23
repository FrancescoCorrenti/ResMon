//! Disk space by folder, scanned in the background.
//!
//! Breadth first, at background I/O priority, so the scan is calm and the whole drive fills in evenly:
//! every folder's total is updated as soon as each of its subfolders is read, and the treemap redraws
//! from those running totals. Only folders are kept (files are summed into their folder), and symbolic
//! links / junctions are not followed, so nothing is counted twice.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
use windows::core::HSTRING;

pub struct Node {
    pub name: Box<str>,
    pub parent: u32,
    pub files: u64, // bytes of the files directly in this folder
    pub total: u64, // files + everything below (running while scanning)
    pub children: Vec<u32>,
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub done: bool,
    pub folders: usize,
    pub denied: usize, // folders that could not be read
    pub started: Instant,
    pub took: Option<f64>,
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
        if !p.ends_with('\\') && parts.len() == 1 {
            p.push('\\');
        }
        p
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

impl Scan {
    /// Start scanning `root` ("C:"); `tick` is called about twice a second while it runs.
    pub fn start(root: &str, tick: impl Fn() + Send + 'static) -> Scan {
        let tree = Arc::new(Mutex::new(Tree {
            nodes: vec![Node { name: root.into(), parent: 0, files: 0, total: 0, children: Vec::new() }],
            done: false,
            folders: 1,
            denied: 0,
            started: Instant::now(),
            took: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (t, s) = (tree.clone(), stop.clone());
        std::thread::spawn(move || {
            let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) }; // low I/O priority too
            let mut queue = VecDeque::from([0u32]);
            let mut last_tick = Instant::now();
            while let Some(id) = queue.pop_front() {
                if s.load(Ordering::Relaxed) {
                    return;
                }
                let path = t.lock().unwrap().path(id);
                let (files, dirs, ok) = list(&path);
                let mut g = t.lock().unwrap();
                if !ok {
                    g.denied += 1;
                }
                for name in dirs {
                    let nid = g.nodes.len() as u32;
                    g.nodes.push(Node { name: name.into(), parent: id, files: 0, total: 0, children: Vec::new() });
                    g.nodes[id as usize].children.push(nid);
                    queue.push_back(nid);
                }
                g.folders = g.nodes.len();
                g.nodes[id as usize].files = files;
                let mut up = id; // running totals all the way to the root
                loop {
                    g.nodes[up as usize].total += files;
                    if up == 0 {
                        break;
                    }
                    up = g.nodes[up as usize].parent;
                }
                drop(g);
                if last_tick.elapsed().as_millis() > 500 {
                    last_tick = Instant::now();
                    tick();
                }
            }
            let mut g = t.lock().unwrap();
            g.done = true;
            g.took = Some(g.started.elapsed().as_secs_f64());
            drop(g);
            tick();
        });
        Scan { root: root.to_string(), tree, stop }
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

#[cfg(test)]
mod tests {
    #[test]
    fn scans_a_folder_tree() {
        let base = std::env::temp_dir().join(format!("resmon_space_{}", std::process::id()));
        std::fs::create_dir_all(base.join("a").join("b")).unwrap();
        std::fs::create_dir_all(base.join("c")).unwrap();
        std::fs::write(base.join("top.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(base.join("a").join("b").join("deep.bin"), vec![0u8; 5000]).unwrap();
        std::fs::write(base.join("c").join("x.bin"), vec![0u8; 300]).unwrap();
        let root = base.to_string_lossy().to_string();
        let s = super::Scan::start(&root, || {});
        let t0 = std::time::Instant::now();
        while !s.tree.lock().unwrap().done && t0.elapsed().as_secs() < 10 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let g = s.tree.lock().unwrap();
        assert!(g.done);
        assert_eq!(g.nodes[0].total, 6300);
        assert_eq!(g.nodes[0].files, 1000);
        let a = g.nodes[0].children.iter().find(|&&c| &*g.nodes[c as usize].name == "a").copied().unwrap();
        assert_eq!(g.nodes[a as usize].total, 5000);
        assert!(g.path(g.nodes[a as usize].children[0]).ends_with("\\a\\b"));
        drop(g);
        std::fs::remove_dir_all(base).unwrap();
    }
}
