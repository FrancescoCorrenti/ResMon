//! CPU renderer for egui: rasterizes the tessellated meshes into a premultiplied BGRA buffer that
//! UpdateLayeredWindow can show with per-pixel alpha. No GPU driver is loaded at all: on this machine
//! the NVIDIA OpenGL driver alone cost ~140 MB and wgpu ~440 MB, for three bars and a treemap.
//!
//! Scanline rasterizer: for every pixel row the triangle's span is computed once, attributes
//! (premultiplied colour, uv) are planar so they step linearly along it. Pixel centres on a shared
//! edge belong to exactly one triangle (half-open spans), so quads have no double-blended seams.

use std::collections::HashMap;

use egui::epaint::{ClippedPrimitive, Color32, ImageData, Primitive, TextureId, Vertex, textures::TexturesDelta};

struct Tex {
    w: usize,
    h: usize,
    px: Vec<Color32>,
}

/// Pixel rectangle [x0, y0, x1, y1), x1/y1 exclusive.
pub type IRect = [i32; 4];

#[derive(Default)]
pub struct Raster {
    tex: HashMap<TextureId, Tex>,
    tex_changed: bool,
    prev: Vec<(IRect, u64)>, // area and hash of every primitive of the last frame
    prev_size: (usize, usize),
}

fn union(a: Option<IRect>, b: IRect) -> Option<IRect> {
    if b[0] >= b[2] || b[1] >= b[3] {
        return a;
    }
    Some(a.map_or(b, |a| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]))
}

fn clip_of(p: &ClippedPrimitive, w: usize, h: usize, ppp: f32) -> IRect {
    let c = p.clip_rect;
    [
        (c.min.x * ppp).round().max(0.0) as i32,
        (c.min.y * ppp).round().max(0.0) as i32,
        (c.max.x * ppp).round().min(w as f32) as i32,
        (c.max.y * ppp).round().min(h as f32) as i32,
    ]
}

/// Where a primitive can touch pixels (mesh bounds within its clip) and a hash of everything it draws.
fn footprint(p: &ClippedPrimitive, w: usize, h: usize, ppp: f32) -> (IRect, u64) {
    let Primitive::Mesh(m) = &p.primitive else { return ([0; 4], 0) };
    let mut hs: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |x: u32| hs = (hs ^ x as u64).wrapping_mul(0x0000_0100_0000_01b3);
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for v in &m.vertices {
        (x0, y0, x1, y1) = (x0.min(v.pos.x), y0.min(v.pos.y), x1.max(v.pos.x), y1.max(v.pos.y));
        for x in [v.pos.x.to_bits(), v.pos.y.to_bits(), v.uv.x.to_bits(), v.uv.y.to_bits(), u32::from_le_bytes(v.color.to_array())] {
            mix(x);
        }
    }
    for &i in &m.indices {
        mix(i);
    }
    let c = clip_of(p, w, h, ppp);
    for x in c {
        mix(x as u32);
    }
    mix(match m.texture_id {
        TextureId::Managed(i) => i as u32,
        TextureId::User(i) => !(i as u32),
    });
    let b = [(x0 * ppp).floor() as i32 - 1, (y0 * ppp).floor() as i32 - 1, (x1 * ppp).ceil() as i32 + 1, (y1 * ppp).ceil() as i32 + 1];
    ([b[0].max(c[0]), b[1].max(c[1]), b[2].min(c[2]), b[3].min(c[3])], hs)
}

impl Raster {
    pub fn set_textures(&mut self, delta: &TexturesDelta) {
        self.tex_changed |= !delta.set.is_empty();
        for (id, deltas) in &delta.set {
            for d in deltas {
                let ImageData::Color(img) = &d.image;
                let [w, h] = img.size;
                match d.pos {
                    None => {
                        self.tex.insert(*id, Tex { w, h, px: img.pixels.clone() });
                    }
                    Some([x0, y0]) => {
                        let Some(t) = self.tex.get_mut(id) else { continue };
                        for y in 0..h {
                            let dst = (y0 + y) * t.w + x0;
                            t.px[dst..dst + w].copy_from_slice(&img.pixels[y * w..(y + 1) * w]);
                        }
                    }
                }
            }
        }
    }

    pub fn free_textures(&mut self, delta: &TexturesDelta) {
        for id in &delta.free {
            self.tex.remove(id);
        }
    }

    /// Draw into `buf` (w x h, 0xAARRGGBB premultiplied, rows top-down), which still holds the previous
    /// frame: only the area where primitives changed is cleared and redrawn. Returns that area (None:
    /// nothing changed, nothing to upload). A gauge tick then redraws one card, not two million pixels.
    pub fn render(&mut self, prims: &[ClippedPrimitive], buf: &mut [u32], w: usize, h: usize, ppp: f32) -> Option<IRect> {
        let cur: Vec<(IRect, u64)> = prims.iter().map(|p| footprint(p, w, h, ppp)).collect();
        let full = [0, 0, w as i32, h as i32];
        let damage = if self.prev_size != (w, h) || self.tex_changed {
            Some(full)
        } else {
            let mut d = None;
            for i in 0..cur.len().max(self.prev.len()) {
                match (cur.get(i), self.prev.get(i)) {
                    (Some(a), Some(b)) if a == b => {}
                    (a, b) => {
                        for x in [a, b].into_iter().flatten() {
                            d = union(d, x.0);
                        }
                    }
                }
            }
            d
        };
        (self.prev, self.prev_size, self.tex_changed) = (cur, (w, h), false);
        let d = damage?;
        for y in d[1]..d[3] {
            buf[y as usize * w + d[0] as usize..y as usize * w + d[2] as usize].fill(0);
        }
        for p in prims {
            let Primitive::Mesh(m) = &p.primitive else { continue };
            let c = clip_of(p, w, h, ppp);
            let clip = [c[0].max(d[0]), c[1].max(d[1]), c[2].min(d[2]), c[3].min(d[3])];
            if clip[0] >= clip[2] || clip[1] >= clip[3] {
                continue;
            }
            let Some(tex) = self.tex.get(&m.texture_id) else { continue };
            for t in m.indices.chunks_exact(3) {
                let v = [&m.vertices[t[0] as usize], &m.vertices[t[1] as usize], &m.vertices[t[2] as usize]];
                tri(buf, w, clip, v, ppp, tex);
            }
        }
        Some(d)
    }
}

#[inline]
fn blend(d: &mut u32, s: [f32; 4]) {
    let sa = s[3];
    if sa <= 0.0 && s[0] <= 0.0 && s[1] <= 0.0 && s[2] <= 0.0 {
        return;
    }
    let k = 1.0 - sa / 255.0;
    let (da, dr, dg, db) = ((*d >> 24) as f32, ((*d >> 16) & 255) as f32, ((*d >> 8) & 255) as f32, (*d & 255) as f32);
    let c = |s: f32, d: f32| (s + d * k + 0.5).min(255.0) as u32;
    *d = (c(sa, da) << 24) | (c(s[0], dr) << 16) | (c(s[1], dg) << 8) | c(s[2], db);
}

fn tri(buf: &mut [u32], w: usize, clip: [i32; 4], v: [&Vertex; 3], ppp: f32, tex: &Tex) {
    let p: [[f32; 2]; 3] = std::array::from_fn(|i| [v[i].pos.x * ppp, v[i].pos.y * ppp]);
    let area = (p[1][0] - p[0][0]) * (p[2][1] - p[0][1]) - (p[2][0] - p[0][0]) * (p[1][1] - p[0][1]);
    if area.abs() < 1e-6 {
        return;
    }
    // attributes: premultiplied rgba (0..255) and uv in texels
    let at: [[f32; 6]; 3] = std::array::from_fn(|i| {
        let c = v[i].color;
        [c.r() as f32, c.g() as f32, c.b() as f32, c.a() as f32, v[i].uv.x * tex.w as f32 - 0.5, v[i].uv.y * tex.h as f32 - 0.5]
    });
    // planar gradients: A(x, y) = A0 + dx * (x - x0) + dy * (y - y0)
    let (e1, e2) = ([p[1][0] - p[0][0], p[1][1] - p[0][1]], [p[2][0] - p[0][0], p[2][1] - p[0][1]]);
    let mut ddx = [0f32; 6];
    let mut ddy = [0f32; 6];
    for k in 0..6 {
        let (a1, a2) = (at[1][k] - at[0][k], at[2][k] - at[0][k]);
        ddx[k] = (a1 * e2[1] - a2 * e1[1]) / area;
        ddy[k] = (a2 * e1[0] - a1 * e2[0]) / area;
    }
    let flat_color = ddx[..4].iter().chain(&ddy[..4]).all(|g| g.abs() < 1e-4);
    let flat_uv = ddx[4..].iter().chain(&ddy[4..]).all(|g| g.abs() < 1e-4);
    let sample = |u: f32, vv: f32| -> [f32; 4] {
        let x = (u.round() as i32).clamp(0, tex.w as i32 - 1) as usize;
        let y = (vv.round() as i32).clamp(0, tex.h as i32 - 1) as usize;
        let t = tex.px[y * tex.w + x];
        [t.r() as f32, t.g() as f32, t.b() as f32, t.a() as f32]
    };
    let modulate = |c: [f32; 4], t: [f32; 4]| -> [f32; 4] { std::array::from_fn(|i| c[i] * t[i] / 255.0) };
    let flat = (flat_color && flat_uv).then(|| modulate([at[0][0], at[0][1], at[0][2], at[0][3]], sample(at[0][4], at[0][5])));

    // rows whose pixel centre lies in [ymin, ymax)
    let mut s = p;
    s.sort_by(|a, b| a[1].total_cmp(&b[1]));
    let y0 = ((s[0][1] - 0.5).ceil() as i32).max(clip[1]);
    let y1 = ((s[2][1] - 0.5).ceil() as i32).min(clip[3]);
    let xat = |a: [f32; 2], b: [f32; 2], y: f32| if (b[1] - a[1]).abs() < 1e-12 { a[0] } else { a[0] + (b[0] - a[0]) * (y - a[1]) / (b[1] - a[1]) };
    for y in y0..y1 {
        let yc = y as f32 + 0.5;
        let xl = xat(s[0], s[2], yc);
        let xs = if yc < s[1][1] { xat(s[0], s[1], yc) } else { xat(s[1], s[2], yc) };
        let (a, b) = if xl < xs { (xl, xs) } else { (xs, xl) };
        let x0 = ((a - 0.5).ceil() as i32).max(clip[0]);
        let x1 = ((b - 0.5).ceil() as i32).min(clip[2]);
        if x0 >= x1 {
            continue;
        }
        let row = &mut buf[y as usize * w..(y as usize + 1) * w];
        if let Some(c) = flat {
            if c[3] >= 254.5 {
                let px = 0xFF00_0000 | ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32;
                row[x0 as usize..x1 as usize].fill(px);
            } else {
                for d in &mut row[x0 as usize..x1 as usize] {
                    blend(d, c);
                }
            }
            continue;
        }
        let (dx0, dy0) = (x0 as f32 + 0.5 - p[0][0], yc - p[0][1]);
        let mut a: [f32; 6] = std::array::from_fn(|k| at[0][k] + ddx[k] * dx0 + ddy[k] * dy0);
        for d in &mut row[x0 as usize..x1 as usize] {
            let c = [a[0].max(0.0), a[1].max(0.0), a[2].max(0.0), a[3].max(0.0)];
            let c = if flat_uv { modulate(c, sample(at[0][4], at[0][5])) } else { modulate(c, sample(a[4], a[5])) };
            blend(d, c);
            for k in 0..6 {
                a[k] += ddx[k];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::{Mesh, Pos2, Rect, WHITE_UV};

    fn white() -> Raster {
        let mut r = Raster::default();
        r.tex.insert(TextureId::default(), Tex { w: 1, h: 1, px: vec![Color32::WHITE] });
        r
    }

    #[test]
    fn quad_has_no_seam_and_exact_coverage() {
        let mut r = white();
        let mut m = Mesh::default();
        // half-transparent rect from (2,3) to (12,9): any pixel covered twice would come out darker
        m.add_colored_rect(Rect::from_min_max(Pos2::new(2.0, 3.0), Pos2::new(12.0, 9.0)), Color32::from_rgba_premultiplied(0, 0, 100, 100));
        for v in &mut m.vertices {
            v.uv = WHITE_UV;
        }
        let prim = ClippedPrimitive { clip_rect: Rect::from_min_max(Pos2::ZERO, Pos2::new(20.0, 20.0)), primitive: Primitive::Mesh(m) };
        let mut buf = vec![0u32; 400];
        r.render(&[prim], &mut buf, 20, 20, 1.0);
        let covered: Vec<u32> = buf.iter().copied().filter(|&p| p != 0).collect();
        assert_eq!(covered.len(), 60); // 10 x 6 pixels
        assert!(covered.iter().all(|&p| p == covered[0]), "seam: {:x?}", covered.iter().collect::<std::collections::HashSet<_>>());
        assert_eq!(covered[0], 0x6400_0064);
    }

    /// Paint with egui itself, tessellate, rasterize: returns the buffer.
    fn paint(w: usize, h: usize, f: impl FnMut(&mut egui::Ui)) -> Vec<u32> {
        let ctx = egui::Context::default();
        let mut f = f;
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(w as f32, h as f32))), ..Default::default() };
        let out = ctx.run_ui(input, |ui| f(ui));
        let prims = ctx.tessellate(out.shapes, 1.0);
        let mut r = Raster::default();
        r.set_textures(&out.textures_delta);
        let mut buf = vec![0u32; w * h];
        r.render(&prims, &mut buf, w, h, 1.0);
        let mut d = out.textures_delta;
        d.clear();
        buf
    }

    #[test]
    fn rounded_tall_rect_is_filled() {
        let buf = paint(40, 200, |ui| {
            ui.painter().rect_filled(Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(14.0, 180.0)), 2.0, Color32::RED);
        });
        let mid = buf[100 * 40 + 12];
        assert_eq!(mid, 0xFFFF_0000, "middle of the bar: {mid:#x}");
        let col: usize = (0..200).filter(|&y| buf[y * 40 + 12] >> 24 > 128).count();
        assert!((158..=161).contains(&col), "covered rows {col}");
    }

    #[test]
    fn clip_rect_is_respected() {
        let mut r = white();
        let mut m = Mesh::default();
        m.add_colored_rect(Rect::from_min_max(Pos2::ZERO, Pos2::new(20.0, 20.0)), Color32::RED);
        for v in &mut m.vertices {
            v.uv = WHITE_UV;
        }
        let prim = ClippedPrimitive { clip_rect: Rect::from_min_max(Pos2::new(5.0, 5.0), Pos2::new(10.0, 8.0)), primitive: Primitive::Mesh(m) };
        let mut buf = vec![0u32; 400];
        r.render(&[prim], &mut buf, 20, 20, 1.0);
        assert_eq!(buf.iter().filter(|&&p| p == 0xFFFF_0000).count(), 15);
    }
}
