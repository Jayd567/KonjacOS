//! Sketch: a small drawing app -- a paper canvas, a palette, three brush
//! sizes and an eraser. It exists as much for the pointer as anything:
//! the pen cursor over the paper.

extern crate alloc;

use alloc::vec::Vec;

use super::apps::{App, ContextItem, MouseEvent, Reply, TEXT};
use super::assets;
use super::icon_ids as icon;
use super::surface::{rgb, Painter, Rect};
use crate::cursor::Shape as Cursor;

const BAR: i32 = 50;
const INSET: i32 = 12;
const PAPER: u32 = rgb(250, 250, 246);
const COLORS: [u32; 8] = [
    rgb(30, 32, 38),
    rgb(229, 57, 53),
    rgb(251, 140, 0),
    rgb(253, 210, 50),
    rgb(67, 160, 71),
    rgb(0, 150, 136),
    rgb(30, 136, 229),
    rgb(142, 36, 170),
];
/// Brush radii.
const SIZES: [i32; 3] = [2, 4, 9];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Color(usize),
    Size(usize),
    Eraser,
    Clear,
}

pub struct Sketch {
    /// The paper, `cw x ch`.
    px: Vec<u32>,
    cw: i32,
    ch: i32,
    color: usize,
    size: usize,
    eraser: bool,
    /// The last point of the stroke in progress (paper coordinates).
    stroke: Option<(i32, i32)>,
    hover: Option<Tool>,
}

impl Sketch {
    pub fn new() -> Self {
        Sketch { px: Vec::new(), cw: 0, ch: 0, color: 0, size: 1, eraser: false, stroke: None, hover: None }
    }

    fn paper(w: i32, h: i32) -> Rect {
        Rect::new(INSET, BAR, (w - 2 * INSET).max(1), (h - BAR - INSET).max(1))
    }

    fn tool_rect(t: Tool, w: i32) -> Rect {
        match t {
            Tool::Color(i) => Rect::new(INSET + 4 + i as i32 * 32, 12, 26, 26),
            Tool::Size(i) => Rect::new(INSET + 4 + 8 * 32 + 14 + i as i32 * 34, 9, 32, 32),
            Tool::Eraser => Rect::new(w - INSET - 84, 9, 38, 32),
            Tool::Clear => Rect::new(w - INSET - 42, 9, 38, 32),
        }
    }

    fn tool_at(x: i32, y: i32, w: i32) -> Option<Tool> {
        let all = (0..COLORS.len()).map(Tool::Color).chain((0..SIZES.len()).map(Tool::Size)).chain([Tool::Eraser, Tool::Clear]);
        all.into_iter().find(|&t| Self::tool_rect(t, w).contains(x, y))
    }

    fn clear(&mut self) {
        self.px.fill(PAPER);
    }

    /// Paints a round dab of the current brush at paper point `(cx, cy)`.
    fn dab(&mut self, cx: i32, cy: i32) {
        let r = SIZES[self.size] + if self.eraser { 6 } else { 0 };
        let color = if self.eraser { PAPER } else { COLORS[self.color] };
        for dy in -r..=r {
            let y = cy + dy;
            if y < 0 || y >= self.ch {
                continue;
            }
            for dx in -r..=r {
                let x = cx + dx;
                if x >= 0 && x < self.cw && dx * dx + dy * dy <= r * r + r {
                    self.px[(y * self.cw + x) as usize] = color;
                }
            }
        }
    }

    /// Draws the stroke on to `(x, y)` and returns the paper area changed.
    fn line_to(&mut self, x: i32, y: i32) -> Rect {
        let (x0, y0) = self.stroke.unwrap_or((x, y));
        let steps = (x - x0).abs().max((y - y0).abs()).max(1);
        for s in 0..=steps {
            self.dab(x0 + (x - x0) * s / steps, y0 + (y - y0) * s / steps);
        }
        self.stroke = Some((x, y));
        let r = SIZES[self.size] + 7;
        Rect::new(x0.min(x) - r, y0.min(y) - r, (x - x0).abs() + 2 * r + 1, (y - y0).abs() + 2 * r + 1)
    }
}

impl App for Sketch {
    fn client_size(&self) -> (i32, i32) {
        (660, 470)
    }

    fn min_size(&self) -> (i32, i32) {
        (500, 240)
    }

    fn resized(&mut self, w: i32, h: i32) {
        let r = Self::paper(w, h);
        let mut px = alloc::vec![PAPER; (r.w * r.h) as usize];
        for y in 0..r.h.min(self.ch) {
            for x in 0..r.w.min(self.cw) {
                px[(y * r.w + x) as usize] = self.px[(y * self.cw + x) as usize];
            }
        }
        self.px = px;
        self.cw = r.w;
        self.ch = r.h;
    }

    fn opaque_rect(&self, w: i32, h: i32) -> Option<Rect> {
        Some(Self::paper(w, h))
    }

    fn paint(&mut self, p: &mut Painter, w: i32, h: i32) {
        for (i, &c) in COLORS.iter().enumerate() {
            let r = Self::tool_rect(Tool::Color(i), w);
            if i == self.color && !self.eraser {
                p.fill_squircle(r.expand(3), 11.0, TEXT, 255);
            } else if self.hover == Some(Tool::Color(i)) {
                p.fill_squircle(r.expand(3), 11.0, TEXT, 90);
            }
            p.fill_squircle(r, 9.0, c, 255);
        }
        for (i, &s) in SIZES.iter().enumerate() {
            let r = Self::tool_rect(Tool::Size(i), w);
            if i == self.size {
                p.fill_squircle(r, 9.0, TEXT, 46);
            } else if self.hover == Some(Tool::Size(i)) {
                p.fill_squircle(r, 9.0, TEXT, 24);
            }
            let d = 2 * s + 1;
            p.fill_squircle(Rect::new(r.x + (r.w - d) / 2, r.y + (r.h - d) / 2, d, d), s as f32, TEXT, 255);
        }
        for (t, id, on) in [(Tool::Eraser, icon::ERASER_20, self.eraser), (Tool::Clear, icon::DELETE_20, false)] {
            let r = Self::tool_rect(t, w);
            if on {
                p.fill_squircle(r, 9.0, TEXT, 46);
            } else if self.hover == Some(t) {
                p.fill_squircle(r, 9.0, TEXT, 24);
            }
            let (iw, ih, m) = assets::icon(id);
            p.draw_mask(r.x + (r.w - iw) / 2, r.y + (r.h - ih) / 2, iw, ih, m, TEXT, 255);
        }

        let r = Self::paper(w, h);
        if self.cw == r.w && self.ch == r.h {
            p.blit(r.x, r.y, r.w, r.h, &self.px, r.w);
        }
    }

    fn mouse(&mut self, ev: MouseEvent, x: i32, y: i32, w: i32, h: i32) -> Reply {
        let paper = Self::paper(w, h);
        let (px, py) = (x - paper.x, y - paper.y);
        match ev {
            MouseEvent::Move => {
                let hover = Self::tool_at(x, y, w);
                let changed = hover != self.hover;
                self.hover = hover;
                Reply { damage: changed.then(|| Rect::new(0, 0, w, BAR)), ..Reply::default() }
            }
            MouseEvent::Down | MouseEvent::DoubleClick => {
                if paper.contains(x, y) {
                    self.stroke = None;
                    let r = self.line_to(px, py);
                    return Reply { damage: Some(r.offset(paper.x, paper.y).intersect(&paper)), ..Reply::default() };
                }
                match Self::tool_at(x, y, w) {
                    Some(Tool::Color(i)) => {
                        self.color = i;
                        self.eraser = false;
                    }
                    Some(Tool::Size(i)) => self.size = i,
                    Some(Tool::Eraser) => self.eraser = !self.eraser,
                    Some(Tool::Clear) => {
                        self.clear();
                        return Reply::repaint(true);
                    }
                    None => return Reply::default(),
                }
                Reply { damage: Some(Rect::new(0, 0, w, BAR)), ..Reply::default() }
            }
            MouseEvent::Drag if self.stroke.is_some() => {
                let r = self.line_to(px, py);
                Reply { damage: Some(r.offset(paper.x, paper.y).intersect(&paper)), ..Reply::default() }
            }
            MouseEvent::Drag => Reply::default(),
            MouseEvent::Up => {
                self.stroke = None;
                Reply::default()
            }
        }
    }

    fn cursor(&self, x: i32, y: i32, w: i32, h: i32) -> Cursor {
        if Self::paper(w, h).contains(x, y) {
            Cursor::Pen
        } else if Self::tool_at(x, y, w).is_some() {
            Cursor::Hand
        } else {
            Cursor::Arrow
        }
    }

    fn context_menu(&mut self, _x: i32, _y: i32, _w: i32, _h: i32) -> Vec<ContextItem> {
        alloc::vec![(if self.eraser { "Use Pen" } else { "Use Eraser" }, Some(1)), ("", None), ("Clear Canvas", Some(0))]
    }

    fn context_cmd(&mut self, id: u32) -> Reply {
        match id {
            0 => self.clear(),
            _ => self.eraser = !self.eraser,
        }
        Reply::repaint(true)
    }
}
