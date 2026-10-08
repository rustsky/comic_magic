//! Panel detection.
//!
//! 1. Binarise the page against the paper colour (median of the page edge).
//! 2. Flood-fill paper inward from the page edge. Paper reached this way, plus any
//!    thin elongated paper strips, is the gutter network; everything else is a
//!    "solid" panel blob with its interior filled.
//! 3. Erode the solid mask slightly so thin bridges break, and take connected
//!    components as panel candidates.
//! 4. A candidate can still hold several panels joined by a speech balloon that
//!    crosses a gutter. Because panel interiors are filled, a line through a panel
//!    is almost fully covered while a gutter line is covered only where a balloon
//!    crosses it, so each candidate is split recursively along low-coverage lines.
//! 5. Panels are put in reading order with an XY-cut over the boxes: rows top to
//!    bottom, then left to right (right to left for manga).
//!
//! Each panel keeps its blob as a [`Shape`] so non-rectangular panels can be cut
//! out exactly instead of as a bounding box.

use image::{DynamicImage, GrayImage, imageops::FilterType};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }
    pub fn bottom(&self) -> u32 {
        self.y + self.h
    }
    fn right(&self) -> u32 {
        self.x + self.w
    }
    fn overlap_area(&self, o: &Rect) -> u64 {
        let ix = self.right().min(o.right()).saturating_sub(self.x.max(o.x));
        let iy = self.bottom().min(o.bottom()).saturating_sub(self.y.max(o.y));
        ix as u64 * iy as u64
    }
}

#[derive(Clone, Debug)]
pub struct DetectOptions {
    /// Luma difference from the background that counts as ink.
    pub threshold: u8,
    /// A line through a panel blob counts as a gutter when at most this fraction of it is covered.
    pub split_coverage: f32,
    /// Panels smaller than this fraction of the page area are dropped (page numbers, specks).
    pub min_panel_area: f32,
    /// Padding added around each panel, as a fraction of the shorter page side.
    pub padding: f32,
    /// Keep the outline of non-rectangular panels so they can be cut out exactly.
    pub exact_shapes: bool,
    pub rtl: bool,
}

/// Longest side used for analysis; detection runs on a downscaled copy.
const DETECT_MAX: u32 = 1200;
/// Erosion radius (analysis pixels) used to break thin bridges between panels.
const ERODE: usize = 3;
const MAX_DEPTH: u32 = 12;

/// A panel together with its exact outline when it isn't a plain rectangle.
#[derive(Clone, Debug)]
pub struct Panel {
    pub rect: Rect,
    pub shape: Option<Shape>,
}

/// Panel outline: a bit mask at analysis resolution placed in page coordinates.
#[derive(Clone, Debug)]
pub struct Shape {
    /// Page coordinates of the mask's top-left corner.
    ox: f32,
    oy: f32,
    /// Page pixels per mask pixel.
    sx: f32,
    sy: f32,
    w: usize,
    h: usize,
    bits: Vec<u64>,
}

impl Shape {
    fn from_mask(m: &Mask, ox: f32, oy: f32, sx: f32, sy: f32) -> Shape {
        let mut bits = vec![0u64; (m.w * m.h).div_ceil(64)];
        for (i, _) in m.data.iter().enumerate().filter(|(_, b)| **b) {
            bits[i / 64] |= 1 << (i % 64);
        }
        Shape { ox, oy, sx, sy, w: m.w, h: m.h, bits }
    }

    fn get(&self, x: isize, y: isize) -> bool {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return false;
        }
        let i = y as usize * self.w + x as usize;
        self.bits[i / 64] >> (i % 64) & 1 == 1
    }

    /// How much of the page pixel at (`px`, `py`) belongs to the panel, 0..=1.
    /// Bilinear over the mask, then steepened so edges stay crisp but anti-aliased.
    pub fn alpha(&self, px: f32, py: f32) -> f32 {
        let mx = (px - self.ox) / self.sx - 0.5;
        let my = (py - self.oy) / self.sy - 0.5;
        let (x0, y0) = (mx.floor(), my.floor());
        let (tx, ty) = (mx - x0, my - y0);
        let (x0, y0) = (x0 as isize, y0 as isize);
        let v = |x, y| self.get(x, y) as u8 as f32;
        let top = v(x0, y0) * (1.0 - tx) + v(x0 + 1, y0) * tx;
        let bot = v(x0, y0 + 1) * (1.0 - tx) + v(x0 + 1, y0 + 1) * tx;
        let a = top * (1.0 - ty) + bot * ty;
        ((a - 0.5) * 2.0 + 0.5).clamp(0.0, 1.0)
    }

    /// Mask pixels on the outline, as page-coordinate rectangles.
    fn outline(&self) -> impl Iterator<Item = (f32, f32)> + '_ {
        (0..self.h as isize).flat_map(move |y| {
            (0..self.w as isize).filter_map(move |x| {
                let edge = self.get(x, y)
                    && !(self.get(x - 1, y) && self.get(x + 1, y) && self.get(x, y - 1) && self.get(x, y + 1));
                edge.then(|| (self.ox + x as f32 * self.sx, self.oy + y as f32 * self.sy))
            })
        })
    }
}

/// A shape that fills this much of its bounding box is treated as a rectangle.
const RECT_FILL: f32 = 0.97;

#[derive(Clone)]
struct Mask {
    w: usize,
    h: usize,
    data: Vec<bool>,
}

impl Mask {
    fn new(w: usize, h: usize) -> Self {
        Mask { w, h, data: vec![false; w * h] }
    }

    fn at(&self, x: usize, y: usize) -> bool {
        self.data[y * self.w + x]
    }

    fn row_profile(&self, r: Rect) -> Vec<usize> {
        (r.y..r.bottom())
            .map(|y| {
                let row = &self.data[y as usize * self.w..];
                row[r.x as usize..r.right() as usize].iter().filter(|&&b| b).count()
            })
            .collect()
    }

    fn col_profile(&self, r: Rect) -> Vec<usize> {
        let mut cols = vec![0usize; r.w as usize];
        for y in r.y..r.bottom() {
            for (i, c) in cols.iter_mut().enumerate() {
                if self.at(r.x as usize + i, y as usize) {
                    *c += 1;
                }
            }
        }
        cols
    }

    /// Replace the mask with its convex hull. Panels are almost always convex, and the
    /// hull also closes white balloons that break through the panel border into the
    /// gutter, which would otherwise read as holes. L-shaped panels get their notch filled.
    fn convex_fill(&self) -> Mask {
        // Pixel-corner points of each row's extent are enough to build the hull.
        let mut pts: Vec<(i64, i64)> = Vec::new();
        for y in 0..self.h {
            let line = &self.data[y * self.w..(y + 1) * self.w];
            if let (Some(a), Some(b)) = (line.iter().position(|&v| v), line.iter().rposition(|&v| v)) {
                let (y, a, b) = (y as i64, a as i64, b as i64 + 1);
                pts.extend([(a, y), (b, y), (a, y + 1), (b, y + 1)]);
            }
        }
        let hull = convex_hull(pts);
        let mut out = Mask::new(self.w, self.h);
        if hull.len() < 3 {
            return self.clone();
        }
        // Scanline fill at pixel centres.
        for y in 0..self.h {
            let cy = y as f64 + 0.5;
            let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
            for i in 0..hull.len() {
                let (x0, y0) = (hull[i].0 as f64, hull[i].1 as f64);
                let (x1, y1) = (hull[(i + 1) % hull.len()].0 as f64, hull[(i + 1) % hull.len()].1 as f64);
                if (y0 <= cy && cy <= y1) || (y1 <= cy && cy <= y0) {
                    let x = if y1 == y0 { x0.min(x1) } else { x0 + (cy - y0) * (x1 - x0) / (y1 - y0) };
                    let x_hi = if y1 == y0 { x0.max(x1) } else { x };
                    lo = lo.min(x);
                    hi = hi.max(x_hi);
                }
            }
            if lo < hi {
                let a = (lo - 0.5).ceil().max(0.0) as usize;
                let b = ((hi - 0.5).floor() as i64).min(self.w as i64 - 1);
                if b >= a as i64 {
                    out.data[y * self.w + a..=y * self.w + b as usize].fill(true);
                }
            }
        }
        out
    }

    /// Square-kernel dilation (`erode = false`) or erosion (`erode = true`) of radius `r`.
    /// Separable, using running counts. Outside the mask counts as set when eroding.
    fn morph(&self, r: usize, erode: bool) -> Mask {
        let pass = |src: &Mask, horizontal: bool| -> Mask {
            let (len, lines) = if horizontal { (src.w, src.h) } else { (src.h, src.w) };
            let idx = |line: usize, i: usize| if horizontal { line * src.w + i } else { i * src.w + line };
            let mut out = Mask::new(src.w, src.h);
            let mut prefix = vec![0usize; len + 1];
            for line in 0..lines {
                for i in 0..len {
                    prefix[i + 1] = prefix[i] + src.data[idx(line, i)] as usize;
                }
                for i in 0..len {
                    let (a, b) = (i.saturating_sub(r), (i + r + 1).min(len));
                    let n = prefix[b] - prefix[a];
                    out.data[idx(line, i)] = if erode { n == b - a } else { n > 0 };
                }
            }
            out
        };
        pass(&pass(self, true), false)
    }
}

pub fn detect_panels(img: &DynamicImage, o: &DetectOptions) -> Vec<Panel> {
    let (ow, oh) = (img.width(), img.height());
    let full = Panel { rect: Rect { x: 0, y: 0, w: ow, h: oh }, shape: None };
    if ow < 32 || oh < 32 {
        return vec![full];
    }

    let mut gray = img.to_luma8();
    let longest = ow.max(oh);
    if longest > DETECT_MAX {
        let s = DETECT_MAX as f32 / longest as f32;
        let nw = ((ow as f32 * s).round() as u32).max(1);
        let nh = ((oh as f32 * s).round() as u32).max(1);
        gray = image::imageops::resize(&gray, nw, nh, FilterType::Triangle);
    }
    let (dw, dh) = gray.dimensions();
    let (w, h) = (dw as usize, dh as usize);
    let fx = ow as f32 / dw as f32;
    let fy = oh as f32 / dh as f32;

    let bg = background_luma(&gray);
    let thr = o.threshold as i16;
    let ink = Mask {
        w,
        h,
        data: gray.pixels().map(|p| (p[0] as i16 - bg as i16).abs() > thr).collect(),
    }
    .morph(1, false);

    let solid = solid_mask(&ink);
    let core = solid.morph(ERODE, true);
    let (labels, boxes) = label(&core);

    let mut panels = Vec::new();
    let page_area = (w * h) as f32;
    for (k, b) in boxes.iter().enumerate() {
        // Grow back what the erosion removed.
        let x0 = (b.x as usize).saturating_sub(ERODE);
        let y0 = (b.y as usize).saturating_sub(ERODE);
        let x1 = (b.right() as usize + ERODE).min(w);
        let y1 = (b.bottom() as usize + ERODE).min(h);
        if ((x1 - x0) * (y1 - y0)) as f32 <= page_area * 0.01 {
            continue;
        }
        // This component only, restored to full size and clipped to the solid mask.
        let mut own = Mask::new(x1 - x0, y1 - y0);
        for y in y0..y1 {
            for x in x0..x1 {
                own.data[(y - y0) * own.w + (x - x0)] = labels[y * w + x] == k as u32 + 1;
            }
        }
        let mut own = own.morph(ERODE, false);
        for y in y0..y1 {
            for x in x0..x1 {
                own.data[(y - y0) * own.w + (x - x0)] &= solid.at(x, y);
            }
        }
        let mut parts = Vec::new();
        let local = Rect { x: 0, y: 0, w: own.w as u32, h: own.h as u32 };
        let min_seg = ((w as f32 * 0.08) as usize, (h as f32 * 0.06) as usize);
        split(&own, local, o.split_coverage, min_seg, 0, &mut parts);
        for r in parts {
            // The piece's own pixels, grown by one pixel so the panel border line survives.
            let mut m = Mask::new(r.w as usize, r.h as usize);
            for y in 0..m.h {
                for x in 0..m.w {
                    m.data[y * m.w + x] = own.at(r.x as usize + x, r.y as usize + y);
                }
            }
            let m = m.morph(1, false).convex_fill();
            panels.push((Rect { x: r.x + x0 as u32, y: r.y + y0 as u32, ..r }, m));
        }
    }

    let min_area = (page_area * o.min_panel_area) as u64;
    panels.retain(|(r, _)| r.area() >= min_area && r.w as f32 >= 0.03 * w as f32 && r.h as f32 >= 0.03 * h as f32);
    // Drop boxes that sit mostly inside a bigger one (insets, stray balloons).
    let all: Vec<Rect> = panels.iter().map(|(r, _)| *r).collect();
    panels.retain(|(a, _)| !all.iter().any(|b| b.area() > a.area() && a.overlap_area(b) * 10 >= a.area() * 8));
    let panels = reading_order(panels, |(r, _)| *r, o.rtl);

    let pad = (ow.min(oh) as f32 * o.padding).round() as i64;
    let panels: Vec<Panel> = panels
        .iter()
        .map(|(r, m)| {
            let x0 = ((r.x as f32 * fx).floor() as i64 - pad).max(0);
            let y0 = ((r.y as f32 * fy).floor() as i64 - pad).max(0);
            let x1 = ((r.right() as f32 * fx).ceil() as i64 + pad).min(ow as i64);
            let y1 = ((r.bottom() as f32 * fy).ceil() as i64 + pad).min(oh as i64);
            let rect = Rect { x: x0 as u32, y: y0 as u32, w: (x1 - x0) as u32, h: (y1 - y0) as u32 };
            let fill = m.data.iter().filter(|&&b| b).count() as f32 / m.data.len().max(1) as f32;
            let shape = (o.exact_shapes && fill < RECT_FILL)
                .then(|| Shape::from_mask(m, r.x as f32 * fx, r.y as f32 * fy, fx, fy));
            Panel { rect, shape }
        })
        .collect();

    if panels.is_empty() { vec![full] } else { panels }
}

/// Andrew's monotone chain; returns hull vertices in order.
fn convex_hull(mut pts: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    pts.sort_unstable();
    pts.dedup();
    if pts.len() < 3 {
        return pts;
    }
    let cross = |o: (i64, i64), a: (i64, i64), b: (i64, i64)| (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0);
    let mut hull: Vec<(i64, i64)> = Vec::with_capacity(pts.len() * 2);
    for pass in 0..2 {
        let start = hull.len();
        let iter: Box<dyn Iterator<Item = &(i64, i64)>> =
            if pass == 0 { Box::new(pts.iter()) } else { Box::new(pts.iter().rev()) };
        for &p in iter {
            while hull.len() >= start + 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0 {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    hull
}

/// Median luma of a thin band around the page edge: the paper / gutter colour.
fn background_luma(g: &GrayImage) -> u8 {
    let (w, h) = g.dimensions();
    let band = ((w.min(h) as f32 * 0.01) as u32).clamp(2, w.min(h) / 2);
    let mut hist = [0u64; 256];
    for (x, y, p) in g.enumerate_pixels() {
        if x < band || y < band || x >= w - band || y >= h - band {
            hist[p[0] as usize] += 1;
        }
    }
    let total: u64 = hist.iter().sum();
    let mut acc = 0;
    for (v, &n) in hist.iter().enumerate() {
        acc += n;
        if acc * 2 >= total {
            return v as u8;
        }
    }
    255
}

/// Everything that is not gutter. Gutter = paper reachable from the page edge, plus
/// enclosed paper regions shaped like a gutter (thin and long), which happen when
/// balloons or captions close off both ends of a gutter.
fn solid_mask(ink: &Mask) -> Mask {
    let (w, h) = (ink.w, ink.h);
    let mut paper = Mask { w, h, data: ink.data.iter().map(|&b| !b).collect() };
    let (labels, boxes) = label(&paper);
    let mut area = vec![0usize; boxes.len()];
    for &l in &labels {
        if l > 0 {
            area[l as usize - 1] += 1;
        }
    }
    let mut gutter = vec![false; boxes.len()];
    for (i, b) in boxes.iter().enumerate() {
        let touches_edge = b.x == 0 || b.y == 0 || b.right() as usize == w || b.bottom() as usize == h;
        let long = b.w.max(b.h) as f32;
        let thickness = area[i] as f32 / long;
        let thin_long = long >= 0.15 * w.min(h) as f32 && thickness <= 0.012 * w.min(h) as f32;
        gutter[i] = touches_edge || thin_long;
    }
    for (p, &l) in paper.data.iter_mut().zip(&labels) {
        *p = !(l > 0 && gutter[l as usize - 1]);
    }
    paper
}

/// 4-connected components. Returns per-pixel labels (0 = unset, k+1 = component k) and bounding boxes.
fn label(m: &Mask) -> (Vec<u32>, Vec<Rect>) {
    let (w, h) = (m.w, m.h);
    let mut labels = vec![0u32; w * h];
    let mut boxes = Vec::new();
    let mut stack = Vec::new();
    for start in 0..w * h {
        if !m.data[start] || labels[start] != 0 {
            continue;
        }
        let id = boxes.len() as u32 + 1;
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        labels[start] = id;
        stack.push(start);
        while let Some(i) = stack.pop() {
            let (x, y) = (i % w, i / w);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            let mut visit = |j: usize| {
                if m.data[j] && labels[j] == 0 {
                    labels[j] = id;
                    stack.push(j);
                }
            };
            if x > 0 {
                visit(i - 1);
            }
            if x + 1 < w {
                visit(i + 1);
            }
            if y > 0 {
                visit(i - w);
            }
            if y + 1 < h {
                visit(i + w);
            }
        }
        boxes.push(Rect { x: x0 as u32, y: y0 as u32, w: (x1 - x0 + 1) as u32, h: (y1 - y0 + 1) as u32 });
    }
    (labels, boxes)
}

/// Recursively cut a blob along lines covered at most `coverage` of their length.
/// `min_seg` = (min width, min height) of a piece; cuts that would create slivers are rejected.
fn split(m: &Mask, r: Rect, coverage: f32, min_seg: (usize, usize), depth: u32, out: &mut Vec<Rect>) {
    let Some(r) = trim(m, r) else { return };
    if depth < MAX_DEPTH {
        let rows = segments(&m.row_profile(r), (coverage * r.w as f32) as usize, min_seg.1);
        if rows.len() > 1 {
            for (a, b) in rows {
                let sub = Rect { x: r.x, y: r.y + a as u32, w: r.w, h: (b - a) as u32 };
                split(m, sub, coverage, min_seg, depth + 1, out);
            }
            return;
        }
        let cols = segments(&m.col_profile(r), (coverage * r.h as f32) as usize, min_seg.0);
        if cols.len() > 1 {
            for (a, b) in cols {
                let sub = Rect { x: r.x + a as u32, y: r.y, w: (b - a) as u32, h: r.h };
                split(m, sub, coverage, min_seg, depth + 1, out);
            }
            return;
        }
    }
    out.push(r);
}

/// Shrink a region to the lines that are more than 2% covered.
fn trim(m: &Mask, r: Rect) -> Option<Rect> {
    if r.w == 0 || r.h == 0 {
        return None;
    }
    let rows = m.row_profile(r);
    let rl = r.w as usize / 50;
    let top = rows.iter().position(|&n| n > rl)?;
    let bot = rows.iter().rposition(|&n| n > rl)? + 1;
    let r = Rect { x: r.x, y: r.y + top as u32, w: r.w, h: (bot - top) as u32 };
    let cols = m.col_profile(r);
    let cl = r.h as usize / 50;
    let left = cols.iter().position(|&n| n > cl)?;
    let right = cols.iter().rposition(|&n| n > cl)? + 1;
    Some(Rect { x: r.x + left as u32, y: r.y, w: (right - left) as u32, h: r.h })
}

/// Runs `[start, end)` of lines above `limit`, separated by at least 2 lines at or below it.
/// Returns a single run if any piece would be shorter than `min_len`.
fn segments(profile: &[usize], limit: usize, min_len: usize) -> Vec<(usize, usize)> {
    const MIN_GAP: usize = 2;
    let mut segs = Vec::new();
    let mut start: Option<usize> = None;
    let mut last = 0;
    for (i, &n) in profile.iter().enumerate() {
        if n <= limit {
            continue;
        }
        match start {
            Some(s) if i - last - 1 >= MIN_GAP => {
                segs.push((s, last + 1));
                start = Some(i);
            }
            None => start = Some(i),
            _ => {}
        }
        last = i;
    }
    if let Some(s) = start {
        segs.push((s, last + 1));
    }
    if segs.len() > 1 && segs.iter().any(|(a, b)| b - a < min_len) {
        return vec![(segs[0].0, segs[segs.len() - 1].1)];
    }
    segs
}

/// XY-cut over boxes: split into horizontal bands where no box straddles the cut
/// (allowing 15% overlap for slanted panels), else into columns, recursively.
fn reading_order<T: Clone>(boxes: Vec<T>, rect: fn(&T) -> Rect, rtl: bool) -> Vec<T> {
    if boxes.len() <= 1 {
        return boxes;
    }
    for horizontal in [true, false] {
        let span = |t: &T| {
            let b = rect(t);
            if horizontal { (b.y, b.bottom()) } else { (b.x, b.right()) }
        };
        let mut sorted = boxes.clone();
        sorted.sort_by_key(|b| {
            let (lo, hi) = span(b);
            lo + hi
        });
        let mut groups: Vec<Vec<T>> = Vec::new();
        let mut end = 0;
        for b in sorted {
            let (lo, hi) = span(&b);
            if let Some(g) = groups.last_mut() {
                let smallest = g.iter().map(|x| span(x).1 - span(x).0).min().unwrap();
                let slack = 0.15 * (hi - lo).min(smallest) as f32;
                if (lo as f32) < end as f32 - slack {
                    g.push(b);
                    end = end.max(hi);
                    continue;
                }
            }
            groups.push(vec![b]);
            end = hi;
        }
        if groups.len() > 1 {
            if !horizontal && rtl {
                groups.reverse();
            }
            return groups.into_iter().flat_map(|g| reading_order(g, rect, rtl)).collect();
        }
    }
    let mut boxes = boxes;
    boxes.sort_by_key(|t| (rect(t).y, rect(t).x));
    boxes
}

/// Draw detected panels on a copy of the page: exact outlines and numbered badges in reading order.
pub fn debug_image(img: &DynamicImage, panels: &[Panel]) -> image::RgbImage {
    let mut out = img.to_rgb8();
    let (w, h) = out.dimensions();
    let t = (w.min(h) / 250).max(2);
    let colors = [[230, 57, 70], [29, 120, 214], [46, 160, 67], [240, 140, 0], [142, 68, 173]];
    let fill = |out: &mut image::RgbImage, x0: f32, y0: f32, x1: f32, y1: f32, c: [u8; 3]| {
        let (x0, y0) = (x0.max(0.0) as u32, y0.max(0.0) as u32);
        for y in y0..(y1.max(0.0) as u32).min(h) {
            for x in x0..(x1.max(0.0) as u32).min(w) {
                out.put_pixel(x, y, image::Rgb(c));
            }
        }
    };
    for (i, p) in panels.iter().enumerate() {
        let c = colors[i % colors.len()];
        let r = p.rect;
        let t = t as f32;
        match &p.shape {
            Some(s) => {
                let half = (t - s.sx.max(s.sy)).max(0.0) / 2.0;
                for (x, y) in s.outline() {
                    fill(&mut out, x - half, y - half, x + s.sx + half, y + s.sy + half, c);
                }
            }
            None => {
                let (x0, y0, x1, y1) = (r.x as f32, r.y as f32, (r.x + r.w) as f32, r.bottom() as f32);
                fill(&mut out, x0, y0, x1, y0 + t, c);
                fill(&mut out, x0, y1 - t, x1, y1, c);
                fill(&mut out, x0, y0, x0 + t, y1, c);
                fill(&mut out, x1 - t, y0, x1, y1, c);
            }
        }
        // Numbered badge in the top-left corner.
        let radius = (w.min(h) as f32 / 38.0).max(12.0);
        let (cx, cy) = (r.x as f32 + radius * 1.4, r.y as f32 + radius * 1.4);
        for y in (cy - radius - 3.0) as u32..((cy + radius + 3.0) as u32).min(h) {
            for x in (cx - radius - 3.0).max(0.0) as u32..((cx + radius + 3.0) as u32).min(w) {
                let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
                if d <= radius {
                    out.put_pixel(x, y, image::Rgb(c));
                } else if d <= radius + 3.0 {
                    out.put_pixel(x, y, image::Rgb([255, 255, 255]));
                }
            }
        }
        draw_number(&mut out, i + 1, cx, cy, (radius / 4.5).max(2.0));
    }
    out
}

/// Draw `n` centred at (`cx`, `cy`) in white with a 3x5 pixel font scaled by `k`.
fn draw_number(img: &mut image::RgbImage, n: usize, cx: f32, cy: f32, k: f32) {
    const DIGITS: [[u8; 5]; 10] = [
        [7, 5, 5, 5, 7], [2, 6, 2, 2, 7], [7, 1, 7, 4, 7], [7, 1, 7, 1, 7], [5, 5, 7, 1, 1],
        [7, 4, 7, 1, 7], [7, 4, 7, 5, 7], [7, 1, 1, 1, 1], [7, 5, 7, 5, 7], [7, 5, 7, 1, 7],
    ];
    let text = n.to_string();
    let total_w = (text.len() as f32 * 4.0 - 1.0) * k;
    let (x0, y0) = (cx - total_w / 2.0, cy - 2.5 * k);
    let (w, h) = img.dimensions();
    for (di, ch) in text.bytes().enumerate() {
        let glyph = DIGITS[(ch - b'0') as usize];
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..3 {
                if bits >> (2 - col) & 1 == 0 {
                    continue;
                }
                let px = x0 + (di as f32 * 4.0 + col as f32) * k;
                let py = y0 + row as f32 * k;
                for y in py.max(0.0) as u32..((py + k) as u32).min(h) {
                    for x in px.max(0.0) as u32..((px + k) as u32).min(w) {
                        img.put_pixel(x, y, image::Rgb([255, 255, 255]));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Luma, GrayImage};

    fn page_with(panels: &[(u32, u32, u32, u32)]) -> DynamicImage {
        let mut g = GrayImage::from_pixel(1000, 1500, Luma([255]));
        for &(x, y, w, h) in panels {
            for yy in y..y + h {
                for xx in x..x + w {
                    // Border plus mid-grey art.
                    let border = xx < x + 4 || xx >= x + w - 4 || yy < y + 4 || yy >= y + h - 4;
                    g.put_pixel(xx, yy, Luma([if border { 0 } else { 150 }]));
                }
            }
        }
        DynamicImage::ImageLuma8(g)
    }

    fn opts(rtl: bool) -> DetectOptions {
        DetectOptions {
            threshold: 48,
            split_coverage: 0.35,
            min_panel_area: 0.01,
            padding: 0.0,
            exact_shapes: true,
            rtl,
        }
    }

    #[test]
    fn convex_fill_closes_open_holes_and_keeps_slant() {
        // A slanted panel (wider at the bottom) with a balloon-shaped gap open at the top.
        let rows = [
            "0011100000",
            "0011100110",
            "0111100111",
            "0111111111",
            "1111111111",
        ];
        let m = Mask {
            w: 10,
            h: 5,
            data: rows.iter().flat_map(|r| r.bytes().map(|b| b == b'1')).collect(),
        };
        let f = m.convex_fill();
        assert!(f.at(6, 1) && f.at(5, 0), "open gap inside the outline is filled");
        assert!(!f.at(0, 0) && !f.at(0, 1), "area outside the slanted edge stays out");
    }

    #[test]
    fn grid_in_reading_order() {
        // Row 1: two panels. Row 2: one wide panel. Row 3: tall left + two stacked right.
        let img = page_with(&[
            (50, 50, 430, 400),
            (520, 50, 430, 400),
            (50, 500, 900, 400),
            (50, 950, 430, 500),
            (520, 950, 430, 230),
            (520, 1220, 430, 230),
        ]);
        let expect = |p: Vec<Panel>, want: &[(u32, u32)]| {
            assert_eq!(p.len(), want.len(), "{p:?}");
            assert!(p.iter().all(|p| p.shape.is_none()), "rectangles need no shape mask");
            for (r, &(x, y)) in p.iter().map(|p| p.rect).zip(want) {
                assert!(r.x.abs_diff(x) <= 4 && r.y.abs_diff(y) <= 4, "{r:?} vs {:?}", (x, y));
            }
        };
        expect(
            detect_panels(&img, &opts(false)),
            &[(50, 50), (520, 50), (50, 500), (50, 950), (520, 950), (520, 1220)],
        );
        expect(
            detect_panels(&img, &opts(true)),
            &[(520, 50), (50, 50), (50, 500), (520, 950), (520, 1220), (50, 950)],
        );
    }
}
