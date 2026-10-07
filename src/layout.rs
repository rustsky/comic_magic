//! Pack detected panels onto Kindle pages.
//!
//! Readability rule: a panel must be shown at least `readable_scale` times its
//! printed size. With the screen's ppi known, that gives a minimum number of
//! screen pixels per source pixel. Panels are kept at that size (or as big as
//! the screen allows) and stacked until the page is full, then each page is
//! scaled up uniformly to use the leftover space.

use crate::detect::Rect;

#[derive(Clone, Debug)]
pub struct SourcePage {
    pub height: u32,
    pub panels: Vec<Rect>,
}

#[derive(Clone, Debug)]
pub struct LayoutOptions {
    pub screen_w: u32,
    pub screen_h: u32,
    pub ppi: u32,
    pub margin: u32,
    pub gap: u32,
    /// Minimum on-screen size relative to print (1.0 = same size as the paper comic).
    pub readable_scale: f32,
    /// Physical height of the printed source page in inches.
    pub print_height_in: f32,
    pub max_upscale: f32,
    pub rotate: bool,
    pub page_break: bool,
    pub rtl: bool,
}

#[derive(Clone, Debug)]
pub struct Placement {
    pub page: usize,
    pub src: Rect,
    pub x: u32,
    pub y: u32,
    /// Final on-canvas size (after rotation).
    pub w: u32,
    pub h: u32,
    pub rotated: bool,
}

#[derive(Clone, Debug, Default)]
pub struct OutPage {
    pub items: Vec<Placement>,
}

struct Unit {
    page: usize,
    panels: Vec<Rect>,
    scale: f32,
    rotated: bool,
}

impl Unit {
    fn size(&self, gap: f32) -> (f32, f32) {
        if self.rotated {
            let p = self.panels[0];
            return (p.h as f32 * self.scale, p.w as f32 * self.scale);
        }
        let w: f32 = self.panels.iter().map(|p| p.w as f32 * self.scale).sum::<f32>()
            + gap * (self.panels.len() - 1) as f32;
        let h = self.panels.iter().map(|p| p.h as f32 * self.scale).fold(0.0, f32::max);
        (w, h)
    }
}

pub fn layout(pages: &[SourcePage], o: &LayoutOptions) -> Vec<OutPage> {
    let uw = o.screen_w.saturating_sub(2 * o.margin).max(1) as f32;
    let uh = o.screen_h.saturating_sub(2 * o.margin).max(1) as f32;
    let gap = o.gap as f32;

    let mut units = Vec::new();
    for (pi, sp) in pages.iter().enumerate() {
        // Screen pixels per source pixel needed for readability.
        let target = o.readable_scale * o.print_height_in * o.ppi as f32 / sp.height as f32;
        for row in group_rows(&sp.panels) {
            if row.len() > 1 {
                let sum_w: f32 = row.iter().map(|p| p.w as f32).sum();
                let max_h = row.iter().map(|p| p.h as f32).fold(0.0, f32::max);
                let s = target
                    .min((uw - gap * (row.len() - 1) as f32) / sum_w)
                    .min(uh / max_h);
                // Keep panels side by side when that costs at most 10% of the target size.
                if s >= target * 0.9 {
                    units.push(Unit { page: pi, panels: row, scale: s, rotated: false });
                    continue;
                }
            }
            for p in row {
                let (w, h) = (p.w as f32, p.h as f32);
                let s = target.min(uw / w).min(uh / h);
                if o.rotate && s < target * 0.95 && w > h {
                    let sr = target.min(uw / h).min(uh / w);
                    if sr > s * 1.2 {
                        units.push(Unit { page: pi, panels: vec![p], scale: sr, rotated: true });
                        continue;
                    }
                }
                units.push(Unit { page: pi, panels: vec![p], scale: s, rotated: false });
            }
        }
    }

    let mut out = Vec::new();
    let mut cur: Vec<Unit> = Vec::new();
    let mut cur_h = 0.0;
    for u in units {
        let (_, h) = u.size(gap);
        if let Some(last) = cur.last() {
            let brk = u.rotated
                || last.rotated
                || (o.page_break && last.page != u.page)
                || cur_h + gap + h > uh;
            if brk {
                out.push(place(std::mem::take(&mut cur), o, uw, uh));
                cur_h = 0.0;
            }
        }
        cur_h = if cur.is_empty() { h } else { cur_h + gap + h };
        cur.push(u);
    }
    if !cur.is_empty() {
        out.push(place(cur, o, uw, uh));
    }
    out
}

/// Group consecutive panels that share a horizontal band (same row on the page).
fn group_rows(panels: &[Rect]) -> Vec<Vec<Rect>> {
    let mut rows: Vec<Vec<Rect>> = Vec::new();
    for &p in panels {
        if let Some(row) = rows.last_mut() {
            let top = row.iter().map(|r| r.y).min().unwrap();
            let bot = row.iter().map(|r| r.bottom()).max().unwrap();
            let overlap = bot.min(p.bottom()).saturating_sub(top.max(p.y));
            if overlap as f32 >= 0.6 * p.h.min(bot - top) as f32 {
                row.push(p);
                continue;
            }
        }
        rows.push(vec![p]);
    }
    rows
}

fn place(units: Vec<Unit>, o: &LayoutOptions, uw: f32, uh: f32) -> OutPage {
    let gap = o.gap as f32;
    let sizes: Vec<(f32, f32)> = units.iter().map(|u| u.size(gap)).collect();
    let n = units.len() as f32;
    let max_w = sizes.iter().map(|s| s.0).fold(0.0, f32::max);
    let sum_h: f32 = sizes.iter().map(|s| s.1).sum();
    // Gaps between panels inside a row don't scale, so solve for width conservatively.
    let k = (uw / max_w)
        .min((uh - gap * (n - 1.0)) / sum_h)
        .min(o.max_upscale)
        .max(0.01);

    let total_h = sum_h * k + gap * (n - 1.0);
    let mut y = o.margin as f32 + ((uh - total_h) / 2.0).max(0.0);
    let mut items = Vec::new();
    for (u, &(_, uh_unit)) in units.iter().zip(&sizes) {
        let s = u.scale * k;
        let row_h = uh_unit * k;
        let dims: Vec<(f32, f32)> = u
            .panels
            .iter()
            .map(|p| {
                if u.rotated {
                    (p.h as f32 * s, p.w as f32 * s)
                } else {
                    (p.w as f32 * s, p.h as f32 * s)
                }
            })
            .collect();
        let row_w: f32 = dims.iter().map(|d| d.0).sum::<f32>() + gap * (dims.len() - 1) as f32;
        let mut x = o.margin as f32 + ((uw - row_w) / 2.0).max(0.0);
        let mut order: Vec<usize> = (0..u.panels.len()).collect();
        if o.rtl {
            order.reverse();
        }
        for i in order {
            let (w, h) = dims[i];
            let (w, h) = (w.round().max(1.0) as u32, h.round().max(1.0) as u32);
            items.push(Placement {
                page: u.page,
                src: u.panels[i],
                x: x.round() as u32,
                y: (y + (row_h - h as f32) / 2.0).round() as u32,
                w: w.min(o.screen_w),
                h: h.min(o.screen_h),
                rotated: u.rotated,
            });
            x += dims[i].0 + gap;
        }
        y += row_h + gap;
    }
    // Keep source reading order in the item list regardless of on-screen position.
    if o.rtl {
        items.sort_by_key(|p| (p.y, std::cmp::Reverse(p.x)));
    }
    OutPage { items }
}
