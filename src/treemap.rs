//! Squarified treemap layout (Bruls et al.), hysteresis on layout values, and the tile model.

use std::collections::HashMap;

use crate::sys::{Field, Proc};

pub const HEADER: f32 = 18.0; // px of the app-name band on a group tile

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Key {
    Group(String),
    Pid(u32),
    Part(&'static str), // "Other" / "Free" / "Idle" / "Files" / "Not scanned yet"
    Dir(u32),           // a folder of the disk space map
    Side(u8, Box<Key>), // a tile of the left (0) or right (1) half of a split view
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Hit {
    Pid(u32),
    Group(String),
    Dir(u32),
}

#[derive(Clone, Debug)]
pub struct Tile {
    pub key: Key,
    pub hit: Option<Hit>,
    pub rect: [f32; 4], // x, y, w, h
    pub title: String,
    pub value: f64,
    pub name: String,
    pub cpu: f64,
    pub header: bool,
    pub free: bool,
}

/// items: (value > 0, key) sorted descending. Returns (key, [x, y, w, h]) covering the box exactly.
pub fn squarify<K: Clone>(items: &[(f64, K)], mut x: f64, mut y: f64, mut w: f64, mut h: f64) -> Vec<(K, [f64; 4])> {
    let mut out = Vec::with_capacity(items.len());
    let total: f64 = items.iter().map(|i| i.0).sum();
    if total <= 0.0 || w <= 0.0 || h <= 0.0 {
        return out;
    }
    let scale = w * h / total;
    let areas: Vec<f64> = items.iter().map(|i| i.0 * scale).collect();
    let worst = |s: f64, mx: f64, mn: f64, side: f64| (side * side * mx / (s * s)).max(s * s / (side * side * mn));
    let mut i = 0;
    while i < areas.len() {
        let side = w.min(h);
        let start = i;
        let (mut s, mut mx, mut mn) = (areas[i], areas[i], areas[i]);
        i += 1;
        while i < areas.len() {
            let a = areas[i];
            if worst(s + a, mx.max(a), mn.min(a), side) > worst(s, mx, mn, side) {
                break;
            }
            (s, mx, mn) = (s + a, mx.max(a), mn.min(a));
            i += 1;
        }
        let last = i == areas.len(); // the last row takes all remaining space (absorbs float drift)
        let row = start..i;
        if w >= h {
            let cw = if last { w } else { s / h };
            let mut yy = y;
            for k in row {
                let hh = areas[k] / s * h;
                out.push((items[k].1.clone(), [x, yy, cw, hh]));
                yy += hh;
            }
            (x, w) = (x + cw, w - cw);
        } else {
            let rh = if last { h } else { s / w };
            let mut xx = x;
            for k in row {
                let ww = areas[k] / s * w;
                out.push((items[k].1.clone(), [xx, y, ww, rh]));
                xx += ww;
            }
            (y, h) = (y + rh, h - rh);
        }
    }
    out
}

/// Layout values with hysteresis: a key keeps its old value until it moves by more than `tol`
/// (relative), so tiles do not twitch on small RAM/CPU wobbles.
pub struct Sticky {
    tol: f64,
    pub values: HashMap<Key, f64>,
}

impl Sticky {
    pub fn new(tol: f64) -> Sticky {
        Sticky { tol, values: HashMap::new() }
    }

    pub fn update(&mut self, current: HashMap<Key, f64>) {
        let old = std::mem::take(&mut self.values);
        self.values = current
            .into_iter()
            .map(|(k, v)| {
                let keep = old.get(&k).filter(|&&o| (v - o).abs() <= self.tol * o.max(1e-9)).copied();
                (k, keep.unwrap_or(v))
            })
            .collect();
    }
}

/// Share-of-total mode: whole = (total, used, free label) in the field's units. Returns [(value, label)]:
/// "Other" (used, but not by the processes shown) and the free part. Shared pages can make the processes
/// sum past `used`: no "Other" then.
pub fn whole_parts(sum: f64, whole: Option<(f64, f64, &'static str)>) -> Vec<(f64, &'static str)> {
    let Some((total, used, free)) = whole else { return Vec::new() };
    let used = used.max(sum);
    [(used - sum, "Other"), (total - used, free)].into_iter().filter(|p| p.0 > 0.0).collect()
}

/// Pure layout of the treemap for W x H px. Group tiles come before their children (draw order).
pub fn model(procs: &[Proc], field: Field, zoom: Option<&str>, w: f32, h: f32, sticky: &mut Sticky, whole: Option<(f64, f64, &'static str)>) -> Vec<Tile> {
    let live: Vec<&Proc> = procs.iter().filter(|q| q.get(field) > 0.0 && zoom.is_none_or(|z| q.name == z)).collect();
    let parts = whole_parts(live.iter().map(|q| q.get(field)).sum(), whole);
    let mut vals: HashMap<Key, f64> = live.iter().map(|q| (Key::Pid(q.pid), q.get(field))).collect();
    vals.extend(parts.iter().map(|&(v, k)| (Key::Part(k), v)));
    sticky.update(vals);
    let sv = |k: &Key| sticky.values[k];
    let (w, h) = (w as f64, h as f64);
    let mut tiles = Vec::new();

    #[derive(Clone)]
    enum Item<'a> {
        One(&'a Proc),
        Group(String),
        Part(&'static str, f64),
    }
    let extra = parts.iter().map(|&(v, k)| (sv(&Key::Part(k)), Item::Part(k, v)));
    let part_tile = |k: &'static str, v: f64, r: [f64; 4]| Tile {
        key: Key::Part(k),
        hit: None,
        rect: shrink(r, 1.0),
        title: k.into(),
        value: v,
        name: k.into(),
        cpu: 0.0,
        header: false,
        free: true,
    };
    let leaf = |q: &Proc, r: [f64; 4], title: String, tiles: &mut Vec<Tile>| {
        if r[2] >= 7.0 && r[3] >= 7.0 {
            // specks are left to the group frame
            tiles.push(Tile { key: Key::Pid(q.pid), hit: Some(Hit::Pid(q.pid)), rect: shrink(r, 1.0), title, value: q.get(field), name: q.name.clone(), cpu: q.cpu, header: false, free: false });
        }
    };
    let order = |qs: &mut Vec<&Proc>| qs.sort_by(|a, b| sv(&Key::Pid(b.pid)).total_cmp(&sv(&Key::Pid(a.pid))).then(a.pid.cmp(&b.pid)));

    if zoom.is_some() {
        let mut qs = live.clone();
        order(&mut qs);
        let items: Vec<(f64, Item)> = qs.iter().map(|q| (sv(&Key::Pid(q.pid)), Item::One(q))).chain(extra).collect();
        for (it, r) in squarify(&items, 0.0, 0.0, w, h) {
            match it {
                Item::One(q) => leaf(q, r, format!("PID {}", q.pid), &mut tiles),
                Item::Part(k, v) => tiles.push(part_tile(k, v, r)),
                Item::Group(_) => {}
            }
        }
        return tiles;
    }
    let mut groups: HashMap<&str, Vec<&Proc>> = HashMap::new();
    for q in &live {
        groups.entry(q.name.as_str()).or_default().push(q);
    }
    let mut totals: Vec<(f64, &str)> = groups.iter().map(|(n, qs)| (qs.iter().map(|q| sv(&Key::Pid(q.pid))).sum(), *n)).collect();
    totals.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
    let items: Vec<(f64, Item)> = totals.iter().map(|&(v, n)| (v, Item::Group(n.to_string()))).chain(extra).collect();
    for (it, r) in squarify(&items, 0.0, 0.0, w, h) {
        let name = match it {
            Item::Part(k, v) => {
                tiles.push(part_tile(k, v, r));
                continue;
            }
            Item::Group(n) => n,
            Item::One(_) => continue,
        };
        let mut qs = groups[name.as_str()].clone();
        let real: f64 = qs.iter().map(|q| q.get(field)).sum();
        if qs.len() == 1 {
            leaf(qs[0], r, name, &mut tiles);
            continue;
        }
        let collapsed = r[2] < 30.0 || r[3] < 40.0;
        tiles.push(Tile {
            key: Key::Group(name.clone()),
            hit: Some(Hit::Group(name.clone())),
            rect: shrink(r, 1.0),
            title: format!("{name}  ×{}", qs.len()),
            value: real,
            name: name.clone(),
            cpu: qs.iter().map(|q| q.cpu).sum(),
            header: !collapsed,
            free: false,
        });
        if !collapsed {
            order(&mut qs);
            let inner: Vec<(f64, &Proc)> = qs.iter().map(|q| (sv(&Key::Pid(q.pid)), *q)).collect();
            let hd = HEADER as f64;
            for (q, cr) in squarify(&inner, r[0] + 2.0, r[1] + hd + 1.0, r[2] - 4.0, r[3] - hd - 3.0) {
                leaf(q, cr, format!("PID {}", q.pid), &mut tiles);
            }
        }
    }
    tiles
}

/// A tile to lay out, with optional children drawn inside it (the disk space map: folders and their
/// subfolders).
pub struct Item {
    pub key: Key,
    pub hit: Option<Hit>,
    pub title: String,
    pub value: f64,
    pub free: bool,
    pub delta: f64, // disk map: bytes gained (or lost) since the walk before; carried in Tile::cpu
    pub children: Vec<Item>,
}

/// Two-level layout: every item gets its share of the box; an item with children becomes a frame with a
/// header band and its children inside, when it is big enough. Same hysteresis as the process map.
pub fn nested(items: Vec<Item>, w: f32, h: f32, sticky: &mut Sticky) -> Vec<Tile> {
    let mut vals = HashMap::new();
    for it in &items {
        vals.insert(it.key.clone(), it.value);
        for c in &it.children {
            vals.insert(c.key.clone(), c.value);
        }
    }
    sticky.update(vals);
    let sv = |k: &Key| sticky.values[k];
    let order = |v: &mut Vec<&Item>| v.sort_by(|a, b| sv(&b.key).total_cmp(&sv(&a.key)).then(a.title.cmp(&b.title)));
    let mut top: Vec<&Item> = items.iter().filter(|i| i.value > 0.0).collect();
    order(&mut top);
    // free / not-scanned parts last, so they fill the corner
    top.sort_by_key(|i| i.free);
    let tile = |it: &Item, r: [f64; 4], header: bool| Tile {
        key: it.key.clone(),
        hit: it.hit.clone(),
        rect: shrink(r, 1.0),
        title: it.title.clone(),
        value: it.value,
        name: it.title.clone(),
        cpu: it.delta,
        header,
        free: it.free,
    };
    let mut tiles = Vec::new();
    let list: Vec<(f64, &Item)> = top.iter().map(|i| (sv(&i.key), *i)).collect();
    for (it, r) in squarify(&list, 0.0, 0.0, w as f64, h as f64) {
        let mut kids: Vec<&Item> = it.children.iter().filter(|c| c.value > 0.0).collect();
        let framed = kids.len() > 1 && r[2] >= 30.0 && r[3] >= 40.0;
        tiles.push(tile(it, r, framed));
        if framed {
            order(&mut kids);
            let inner: Vec<(f64, &Item)> = kids.iter().map(|c| (sv(&c.key), *c)).collect();
            let hd = HEADER as f64;
            for (c, cr) in squarify(&inner, r[0] + 2.0, r[1] + hd + 1.0, r[2] - 4.0, r[3] - hd - 3.0) {
                if cr[2] >= 7.0 && cr[3] >= 7.0 {
                    tiles.push(tile(c, cr, false));
                }
            }
        }
    }
    tiles
}

fn shrink(r: [f64; 4], m: f64) -> [f32; 4] {
    [(r[0] + m) as f32, (r[1] + m) as f32, (r[2] - 2.0 * m) as f32, (r[3] - 2.0 * m) as f32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_tiles_the_box() {
        let mut vals: Vec<f64> = [50.0, 30.0, 30.0, 10.0, 6.0, 4.0, 2.0, 1.0, 1.0, 0.5].repeat(7);
        vals.sort_by(|a, b| b.total_cmp(a));
        let items: Vec<(f64, usize)> = vals.iter().copied().zip(0..).collect();
        let rects = squarify(&items, 0.0, 0.0, 600.0, 400.0);
        assert_eq!(rects.len(), vals.len());
        let total: f64 = vals.iter().sum();
        assert!((rects.iter().map(|r| r.1[2] * r.1[3]).sum::<f64>() - 240000.0).abs() < 1e-3);
        for (k, r) in &rects {
            assert!(r[0] >= -1e-6 && r[1] >= -1e-6 && r[0] + r[2] <= 600.0 + 1e-6 && r[1] + r[3] <= 400.0 + 1e-6);
            assert!((r[2] * r[3] / 240000.0 - vals[*k] / total).abs() < 1e-6);
        }
    }

    fn p(pid: u32, name: &str, ws: f64) -> Proc {
        Proc { pid, name: name.into(), ws, ..Default::default() }
    }

    #[test]
    fn share_of_total() {
        assert_eq!(whole_parts(40.0, Some((100.0, 60.0, "Free"))), vec![(20.0, "Other"), (40.0, "Free")]);
        assert_eq!(whole_parts(40.0, Some((100.0, 35.0, "Free"))), vec![(60.0, "Free")]); // shared pages: no negative Other
        let qs = [p(1, "a", 30.0), p(2, "b", 10.0)];
        let tl = model(&qs, Field::Ws, None, 400.0, 300.0, &mut Sticky::new(0.15), Some((100.0, 60.0, "Free")));
        let keys: Vec<&Key> = tl.iter().map(|t| &t.key).collect();
        assert_eq!(keys.len(), 4);
        assert!(keys.contains(&&Key::Part("Other")) && keys.contains(&&Key::Pid(1)));
        let free = tl.iter().find(|t| t.key == Key::Part("Free")).unwrap();
        assert!(((free.rect[2] + 2.0) * (free.rect[3] + 2.0) / 120000.0 - 0.4).abs() < 1e-4);
    }

    #[test]
    fn groups_and_zoom() {
        let qs = [p(1, "a", 30.0), p(2, "a", 20.0), p(3, "b", 10.0)];
        let tl = model(&qs, Field::Ws, None, 400.0, 300.0, &mut Sticky::new(0.15), None);
        let g = tl.iter().find(|t| t.key == Key::Group("a".into())).unwrap();
        assert!(g.header && g.value == 50.0 && g.title.contains("×2"));
        let gi = tl.iter().position(|t| t.key == Key::Group("a".into())).unwrap();
        let ci = tl.iter().position(|t| t.key == Key::Pid(1)).unwrap();
        assert!(gi < ci, "group drawn under its children");
        let z = model(&qs, Field::Ws, Some("a"), 400.0, 300.0, &mut Sticky::new(0.15), None);
        assert_eq!(z.len(), 2);
    }

    #[test]
    fn hysteresis() {
        let mut st = Sticky::new(0.15);
        let m = |a: f64, b: f64| HashMap::from([(Key::Pid(1), a), (Key::Pid(2), b)]);
        st.update(m(100.0, 50.0));
        st.update(m(105.0, 65.0));
        assert_eq!(st.values[&Key::Pid(1)], 100.0);
        assert_eq!(st.values[&Key::Pid(2)], 65.0);
        st.update(HashMap::from([(Key::Pid(1), 100.0)]));
        assert!(!st.values.contains_key(&Key::Pid(2)));
    }
}
