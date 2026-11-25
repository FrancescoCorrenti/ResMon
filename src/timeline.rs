//! The last 24 hours: machine totals and the top processes every 10 s, plus the quiet alerts.
//!
//! Kept small on purpose: per sample only the processes that are among the top few of some measure
//! (names interned), about 5-10 MB for a full day. Alerts fire only after a threshold has held for a
//! while, carry the process most responsible, and clear themselves when the condition goes away.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::sys::{Field, Proc, Totals};

pub const EVERY: u64 = 10; // seconds between two samples
const DAY: usize = (24 * 3600 / EVERY) as usize;
const TOP: usize = 5; // processes kept per measure and sample

/// Process measures stored, in this order.
pub const FIELDS: [Field; 9] = [Field::Cpu, Field::Ws, Field::Private, Field::Gpu, Field::GpuMem, Field::IoRead, Field::IoWrite, Field::NetDown, Field::NetUp];

pub fn slot(f: Field) -> usize {
    FIELDS.iter().position(|&x| x == f).unwrap_or(1)
}

pub struct PSnap {
    pub name: u32,
    pub pid: u32,
    pub v: [f32; 9],
}

pub struct Snap {
    pub t: i64, // unix seconds
    /// machine totals: cpu %, ram used, commit, gpu %, vram used, disk read, disk write, net down, net up
    pub tot: [f32; 9],
    pub procs: Vec<PSnap>,
}

#[derive(Default)]
pub struct Timeline {
    pub names: Vec<String>,
    index: HashMap<String, u32>,
    pub snaps: VecDeque<Snap>,
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

impl Timeline {
    pub fn push(&mut self, tot: &Totals, procs: &[Proc]) {
        let mut keep: Vec<usize> = Vec::new();
        for f in FIELDS {
            let mut idx: Vec<usize> = (0..procs.len()).filter(|&i| procs[i].get(f) > 0.0).collect();
            idx.sort_by(|&a, &b| procs[b].get(f).total_cmp(&procs[a].get(f)));
            keep.extend(idx.into_iter().take(TOP));
        }
        keep.sort_unstable();
        keep.dedup();
        let procs = keep
            .into_iter()
            .map(|i| {
                let q = &procs[i];
                let name = match self.index.get(&q.name) {
                    Some(&n) => n,
                    None => {
                        let n = self.names.len() as u32;
                        self.names.push(q.name.clone());
                        self.index.insert(q.name.clone(), n);
                        n
                    }
                };
                PSnap { name, pid: q.pid, v: FIELDS.map(|f| q.get(f) as f32) }
            })
            .collect();
        let g = &tot.gpu;
        let tot = [
            tot.cpu,
            tot.ram_used as f32,
            tot.commit as f32,
            g.util.unwrap_or(0.0),
            g.mem_used as f32,
            tot.disk_r as f32,
            tot.disk_w as f32,
            tot.net_d as f32,
            tot.net_u as f32,
        ];
        self.snaps.push_back(Snap { t: now_unix(), tot, procs });
        while self.snaps.len() > DAY {
            self.snaps.pop_front();
        }
    }

    /// The processes of one sample, as process records for the map and the table.
    pub fn procs_at(&self, i: usize) -> Vec<Proc> {
        let Some(s) = self.snaps.get(i) else { return Vec::new() };
        s.procs
            .iter()
            .map(|p| {
                let v = |f: Field| p.v[slot(f)] as f64;
                Proc {
                    pid: p.pid,
                    name: self.names[p.name as usize].clone(),
                    cpu: v(Field::Cpu),
                    ws: v(Field::Ws),
                    private: v(Field::Private),
                    gpu: v(Field::Gpu),
                    gpu_mem: v(Field::GpuMem),
                    io_r: v(Field::IoRead),
                    io_w: v(Field::IoWrite),
                    net_d: v(Field::NetDown),
                    net_u: v(Field::NetUp),
                    ..Default::default()
                }
            })
            .collect()
    }

    /// The process that used the most of `f` in sample i: (name, value).
    pub fn culprit(&self, i: usize, f: Field) -> Option<(&str, f32)> {
        let s = self.snaps.get(i)?;
        let k = slot(f);
        let p = s.procs.iter().max_by(|a, b| a.v[k].total_cmp(&b.v[k]))?;
        (p.v[k] > 0.0).then(|| (self.names[p.name as usize].as_str(), p.v[k]))
    }
}

// ---------------------------------------------------------------- alerts

#[derive(Clone)]
pub struct Alert {
    pub kind: &'static str,
    pub critical: bool,
    pub text: String,
    pub pid: Option<u32>,
}

/// One rule: a condition that must hold for `hold` seconds before it shows, and go away for 10 s to clear.
struct Rule {
    since: Option<Instant>,
    clear_since: Option<Instant>,
    active: bool,
}

#[derive(Default)]
pub struct Alerts {
    rules: HashMap<&'static str, Rule>,
    muted: HashMap<&'static str, Instant>,
    pub list: Vec<Alert>,
}

impl Alerts {
    /// Mute one kind for 30 minutes.
    pub fn mute(&mut self, kind: &str) {
        if let Some(k) = self.rules.keys().find(|k| **k == kind).copied() {
            self.muted.insert(k, Instant::now());
        }
        self.list.retain(|a| a.kind != kind);
    }

    pub fn update(&mut self, tot: &Totals, procs: &[Proc]) {
        let top = |f: Field| procs.iter().max_by(|a, b| a.get(f).total_cmp(&b.get(f)));
        let gb = |b: f64| b / (1u64 << 30) as f64;
        let ram = tot.ram_used as f64 / tot.ram_total.max(1) as f64 * 100.0;
        let commit = tot.commit as f64 / tot.commit_limit.max(1) as f64 * 100.0;
        let g = &tot.gpu;
        let vram = g.mem_used as f64 / g.mem_total.max(1) as f64 * 100.0;
        let full = tot.drives.iter().find(|d| d.2 > 0 && (d.2 - d.1) as f64 / (d.2 as f64) < 0.05);
        #[allow(clippy::type_complexity)]
    let mut checks: Vec<(&'static str, bool, u64, bool, Box<dyn Fn() -> (String, Option<u32>) + '_>)> = vec![
            (
                "ram",
                ram >= 92.0,
                30,
                ram >= 97.0,
                Box::new(move || match top(Field::Ws) {
                    Some(q) => (format!("Memory at {ram:.0}%: {} uses {:.1} GB", q.name, gb(q.ws)), Some(q.pid)),
                    None => (format!("Memory at {ram:.0}%"), None),
                }),
            ),
            (
                "commit",
                commit >= 92.0,
                30,
                commit >= 97.0,
                Box::new(move || match top(Field::Private) {
                    Some(q) => (format!("Commit at {commit:.0}% of the limit: {} holds {:.1} GB", q.name, gb(q.private)), Some(q.pid)),
                    None => (format!("Commit at {commit:.0}% of the limit"), None),
                }),
            ),
            (
                "vram",
                g.mem_total > 0 && vram >= 95.0,
                30,
                false,
                Box::new(move || match top(Field::GpuMem) {
                    Some(q) => (format!("VRAM at {vram:.0}%: {} holds {:.1} GB", q.name, gb(q.gpu_mem)), Some(q.pid)),
                    None => (format!("VRAM at {vram:.0}%"), None),
                }),
            ),
            (
                "gputemp",
                g.temp.is_some_and(|t| t >= 85),
                60,
                g.temp.is_some_and(|t| t >= 92),
                Box::new(move || {
                    let t = g.temp.unwrap_or(0);
                    match top(Field::Gpu) {
                        Some(q) => (format!("GPU at {t}°C: {} uses {:.0}% of it", q.name, q.gpu), Some(q.pid)),
                        None => (format!("GPU at {t}°C"), None),
                    }
                }),
            ),
            (
                "cpu",
                tot.cpu >= 95.0,
                120,
                false,
                Box::new(move || match top(Field::Cpu) {
                    Some(q) => (format!("CPU at {:.0}% for minutes: {} takes {:.0}% of the machine", tot.cpu, q.name, q.cpu), Some(q.pid)),
                    None => (format!("CPU at {:.0}% for minutes", tot.cpu), None),
                }),
            ),
            (
                "drive",
                full.is_some(),
                0,
                full.is_some_and(|d| (d.2 - d.1) as f64 / (d.2 as f64) < 0.02),
                Box::new(move || {
                    let d = full.unwrap();
                    (format!("Drive {} almost full: {:.1} GB free", d.0, gb((d.2 - d.1) as f64)), None)
                }),
            ),
        ];
        let now = Instant::now();
        self.muted.retain(|_, t| now.duration_since(*t).as_secs() < 1800);
        for (kind, on, hold, critical, describe) in checks.drain(..) {
            let r = self.rules.entry(kind).or_insert(Rule { since: None, clear_since: None, active: false });
            if on {
                r.clear_since = None;
                let since = *r.since.get_or_insert(now);
                if !r.active && now.duration_since(since).as_secs() >= hold {
                    r.active = true;
                }
            } else {
                r.since = None;
                if r.active && now.duration_since(*r.clear_since.get_or_insert(now)).as_secs() >= 10 {
                    r.active = false;
                }
            }
            self.list.retain(|a| a.kind != kind);
            if r.active && !self.muted.contains_key(kind) {
                let (text, pid) = describe();
                self.list.push(Alert { kind, critical, text, pid });
            }
        }
        self.list.sort_by_key(|a| !a.critical);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, name: &str, cpu: f64, ws: f64) -> Proc {
        Proc { pid, name: name.into(), cpu, ws, ..Default::default() }
    }

    #[test]
    fn keeps_top_processes_and_culprits() {
        let mut t = Timeline::default();
        let mut procs: Vec<Proc> = (0..50).map(|i| p(i, &format!("p{i}"), i as f64 * 0.1, 1e6)).collect();
        procs.push(p(999, "hog", 90.0, 8e9));
        t.push(&Totals::default(), &procs);
        assert!(t.snaps[0].procs.len() <= 9 * TOP);
        assert_eq!(t.culprit(0, Field::Cpu).unwrap().0, "hog");
        assert_eq!(t.culprit(0, Field::Ws).unwrap().0, "hog");
        let back = t.procs_at(0);
        assert!(back.iter().any(|q| q.name == "hog" && q.ws == 8e9));
    }

    #[test]
    fn alerts_need_the_condition_to_hold() {
        let mut a = Alerts::default();
        let tot = Totals { ram_used: 99, ram_total: 100, commit_limit: 100, ..Default::default() };
        let procs = [p(7, "hog", 1.0, 5e9)];
        a.update(&tot, &procs);
        assert!(a.list.is_empty(), "a 30 s threshold must not fire at once");
        a.rules.get_mut("ram").unwrap().since = Some(Instant::now() - std::time::Duration::from_secs(31));
        a.update(&tot, &procs);
        assert_eq!(a.list.len(), 1);
        assert!(a.list[0].text.contains("hog") && a.list[0].critical && a.list[0].pid == Some(7));
        a.mute("ram");
        a.update(&tot, &procs);
        assert!(a.list.is_empty(), "muted");
    }
}
