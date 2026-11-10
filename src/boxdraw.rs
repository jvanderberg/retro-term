//! Geometric rendering of box-drawing (U+2500–U+257F) and block-element
//! (U+2580–U+259F) characters, so lines and ANSI-art blocks join seamlessly
//! between cells regardless of the font.

use egui::{pos2, Color32, Painter, Rect, Stroke};

/// Arm styles in the order up, right, down, left. 0 none, 1 light, 2 heavy, 3 double.
const BOX: [[u8; 4]; 128] = {
    const fn p(s: &[u8; 4]) -> [u8; 4] {
        [s[0] - b'0', s[1] - b'0', s[2] - b'0', s[3] - b'0']
    }
    [
        p(b"0101"),
        p(b"0202"),
        p(b"1010"),
        p(b"2020"), // ─ ━ │ ┃
        p(b"0101"),
        p(b"0202"),
        p(b"1010"),
        p(b"2020"), // ┄ ┅ ┆ ┇
        p(b"0101"),
        p(b"0202"),
        p(b"1010"),
        p(b"2020"), // ┈ ┉ ┊ ┋
        p(b"0110"),
        p(b"0210"),
        p(b"0120"),
        p(b"0220"), // ┌ ┍ ┎ ┏
        p(b"0011"),
        p(b"0012"),
        p(b"0021"),
        p(b"0022"), // ┐ ┑ ┒ ┓
        p(b"1100"),
        p(b"1200"),
        p(b"2100"),
        p(b"2200"), // └ ┕ ┖ ┗
        p(b"1001"),
        p(b"1002"),
        p(b"2001"),
        p(b"2002"), // ┘ ┙ ┚ ┛
        p(b"1110"),
        p(b"1210"),
        p(b"2110"),
        p(b"1120"), // ├ ┝ ┞ ┟
        p(b"2120"),
        p(b"2210"),
        p(b"1220"),
        p(b"2220"), // ┠ ┡ ┢ ┣
        p(b"1011"),
        p(b"1012"),
        p(b"2011"),
        p(b"1021"), // ┤ ┥ ┦ ┧
        p(b"2021"),
        p(b"2012"),
        p(b"1022"),
        p(b"2022"), // ┨ ┩ ┪ ┫
        p(b"0111"),
        p(b"0112"),
        p(b"0211"),
        p(b"0212"), // ┬ ┭ ┮ ┯
        p(b"0121"),
        p(b"0122"),
        p(b"0221"),
        p(b"0222"), // ┰ ┱ ┲ ┳
        p(b"1101"),
        p(b"1102"),
        p(b"1201"),
        p(b"1202"), // ┴ ┵ ┶ ┷
        p(b"2101"),
        p(b"2102"),
        p(b"2201"),
        p(b"2202"), // ┸ ┹ ┺ ┻
        p(b"1111"),
        p(b"1112"),
        p(b"1211"),
        p(b"1212"), // ┼ ┽ ┾ ┿
        p(b"2111"),
        p(b"1121"),
        p(b"2121"),
        p(b"2112"), // ╀ ╁ ╂ ╃
        p(b"2211"),
        p(b"1122"),
        p(b"1221"),
        p(b"2212"), // ╄ ╅ ╆ ╇
        p(b"1222"),
        p(b"2122"),
        p(b"2221"),
        p(b"2222"), // ╈ ╉ ╊ ╋
        p(b"0101"),
        p(b"0202"),
        p(b"1010"),
        p(b"2020"), // ╌ ╍ ╎ ╏
        p(b"0303"),
        p(b"3030"),
        p(b"0310"),
        p(b"0130"), // ═ ║ ╒ ╓
        p(b"0330"),
        p(b"0013"),
        p(b"0031"),
        p(b"0033"), // ╔ ╕ ╖ ╗
        p(b"1300"),
        p(b"3100"),
        p(b"3300"),
        p(b"1003"), // ╘ ╙ ╚ ╛
        p(b"3001"),
        p(b"3003"),
        p(b"1310"),
        p(b"3130"), // ╜ ╝ ╞ ╟
        p(b"3330"),
        p(b"1013"),
        p(b"3031"),
        p(b"3033"), // ╠ ╡ ╢ ╣
        p(b"0313"),
        p(b"0131"),
        p(b"0333"),
        p(b"1303"), // ╤ ╥ ╦ ╧
        p(b"3101"),
        p(b"3303"),
        p(b"1313"),
        p(b"3131"), // ╨ ╩ ╪ ╫
        p(b"3333"),
        p(b"0110"),
        p(b"0011"),
        p(b"1001"), // ╬ ╭ ╮ ╯
        p(b"1100"),
        p(b"0000"),
        p(b"0000"),
        p(b"0000"), // ╰ ╱ ╲ ╳
        p(b"0001"),
        p(b"1000"),
        p(b"0100"),
        p(b"0010"), // ╴ ╵ ╶ ╷
        p(b"0002"),
        p(b"2000"),
        p(b"0200"),
        p(b"0020"), // ╸ ╹ ╺ ╻
        p(b"0201"),
        p(b"1020"),
        p(b"0102"),
        p(b"2010"), // ╼ ╽ ╾ ╿
    ]
};

/// Whether `c` is drawn by this module.
pub fn is_special(c: char) -> bool {
    matches!(c as u32, 0x2500..=0x259F)
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Draw `c` into `cell`. Returns false if `c` isn't a special character.
pub fn draw(painter: &Painter, cell: Rect, c: char, fg: Color32, bg: Color32, ppp: f32) -> bool {
    let code = c as u32;
    match code {
        0x2500..=0x257F => {
            draw_box(painter, cell, (code - 0x2500) as usize, fg, ppp);
            true
        }
        0x2580..=0x259F => {
            draw_block(painter, cell, code, fg, bg, ppp);
            true
        }
        _ => false,
    }
}

/// Snap a coordinate to the physical pixel grid.
fn snap(v: f32, ppp: f32) -> f32 {
    (v * ppp).round() / ppp
}

fn fill(painter: &Painter, x0: f32, y0: f32, x1: f32, y1: f32, color: Color32, ppp: f32) {
    let r = Rect::from_min_max(pos2(snap(x0, ppp), snap(y0, ppp)), pos2(snap(x1, ppp), snap(y1, ppp)));
    if r.width() > 0.0 && r.height() > 0.0 {
        painter.rect_filled(r, 0.0, color);
    }
}

fn draw_box(painter: &Painter, cell: Rect, idx: usize, fg: Color32, ppp: f32) {
    let px = 1.0 / ppp;
    // Light line thickness: about 1/8 of the cell width, at least one pixel.
    let t = snap((cell.width() / 8.0).max(px), ppp).max(px);
    let heavy = (t * 2.0).max(t + px);
    let gap = t; // half distance between the two strokes of a double line
    let cx = snap(cell.center().x - t / 2.0, ppp) + t / 2.0;
    let cy = snap(cell.center().y - t / 2.0, ppp) + t / 2.0;

    if let 0x71..=0x73 = idx {
        let stroke = Stroke::new(t, fg);
        if idx != 0x72 {
            painter.line_segment([cell.left_bottom(), cell.right_top()], stroke);
        }
        if idx != 0x71 {
            painter.line_segment([cell.left_top(), cell.right_bottom()], stroke);
        }
        return;
    }

    let arms = BOX[idx];
    let width_of = |s: u8| match s {
        1 => t,
        2 => heavy,
        3 => t,
        _ => 0.0,
    };
    // Direction vectors: up, right, down, left.
    for (dir, &style) in arms.iter().enumerate() {
        if style == 0 {
            continue;
        }
        let perp = [arms[(dir + 1) % 4], arms[(dir + 3) % 4]];
        let perp_double = perp.iter().filter(|&&s| s == 3).count();
        let perp_half = perp.iter().map(|&s| width_of(s)).fold(0.0f32, f32::max) / 2.0;
        // Each arm is drawn as one or two strokes, each described by its
        // offset across the arm and where it starts along the arm (measured
        // from the centre toward the cell edge).
        let mut strokes: [(f32, f32, f32); 2] = [(0.0, 0.0, 0.0); 2];
        let n;
        if style == 3 {
            n = 2;
            for (k, side) in [-1.0f32, 1.0].into_iter().enumerate() {
                // Which perpendicular arm lies on this stroke's side?
                // For vertical arms "side -1" is left; for horizontal arms it is up.
                let same = match (dir, side < 0.0) {
                    (0 | 2, true) => arms[3],
                    (0 | 2, false) => arms[1],
                    (_, true) => arms[0],
                    (_, false) => arms[2],
                };
                let start = if perp_double > 0 {
                    if same != 0 {
                        gap - t / 2.0
                    } else {
                        -gap - t / 2.0
                    }
                } else if perp_half > 0.0 {
                    -perp_half
                } else {
                    0.0
                };
                strokes[k] = (side * gap, start, t);
            }
        } else {
            n = 1;
            let start = match perp_double {
                2 => gap - t / 2.0,
                1 => -gap - t / 2.0,
                _ => -perp_half,
            };
            strokes[0] = (0.0, start, width_of(style));
        }
        for &(offset, start, w) in &strokes[..n] {
            match dir {
                0 => fill(painter, cx + offset - w / 2.0, cell.top(), cx + offset + w / 2.0, cy - start, fg, ppp),
                2 => fill(painter, cx + offset - w / 2.0, cy + start, cx + offset + w / 2.0, cell.bottom(), fg, ppp),
                1 => fill(painter, cx + start, cy + offset - w / 2.0, cell.right(), cy + offset + w / 2.0, fg, ppp),
                _ => fill(painter, cell.left(), cy + offset - w / 2.0, cx - start, cy + offset + w / 2.0, fg, ppp),
            }
        }
    }
}

fn draw_block(painter: &Painter, cell: Rect, code: u32, fg: Color32, bg: Color32, ppp: f32) {
    let (x0, y0, x1, y1) = (cell.left(), cell.top(), cell.right(), cell.bottom());
    let w = cell.width();
    let h = cell.height();
    let f = |a: f32, b: f32, c: f32, d: f32| fill(painter, a, b, c, d, fg, ppp);
    let (mx, my) = (x0 + w / 2.0, y0 + h / 2.0);
    match code {
        0x2580 => f(x0, y0, x1, my),
        0x2581..=0x2588 => {
            let eighths = (code - 0x2580) as f32;
            f(x0, y1 - h * eighths / 8.0, x1, y1);
        }
        0x2589..=0x258F => {
            let eighths = (0x2590 - code) as f32;
            f(x0, y0, x0 + w * eighths / 8.0, y1);
        }
        0x2590 => f(mx, y0, x1, y1),
        0x2591..=0x2593 => {
            let t = (code - 0x2590) as f32 * 0.25;
            fill(painter, x0, y0, x1, y1, lerp(bg, fg, t), ppp);
        }
        0x2594 => f(x0, y0, x1, y0 + h / 8.0),
        0x2595 => f(x1 - w / 8.0, y0, x1, y1),
        _ => {
            // Quadrants: bit 0 UL, 1 UR, 2 LL, 3 LR.
            let q: u8 = match code {
                0x2596 => 0b0100,
                0x2597 => 0b1000,
                0x2598 => 0b0001,
                0x2599 => 0b1101,
                0x259A => 0b1001,
                0x259B => 0b0111,
                0x259C => 0b1011,
                0x259D => 0b0010,
                0x259E => 0b0110,
                _ => 0b1110,
            };
            if q & 1 != 0 {
                f(x0, y0, mx, my);
            }
            if q & 2 != 0 {
                f(mx, y0, x1, my);
            }
            if q & 4 != 0 {
                f(x0, my, mx, y1);
            }
            if q & 8 != 0 {
                f(mx, my, x1, y1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_spot_checks() {
        assert_eq!(BOX['┌' as usize - 0x2500], [0, 1, 1, 0]);
        assert_eq!(BOX['╬' as usize - 0x2500], [3, 3, 3, 3]);
        assert_eq!(BOX['╒' as usize - 0x2500], [0, 3, 1, 0]);
        assert_eq!(BOX['╜' as usize - 0x2500], [3, 0, 0, 1]);
        assert_eq!(BOX['╿' as usize - 0x2500], [2, 0, 1, 0]);
        assert_eq!(BOX['┾' as usize - 0x2500], [1, 2, 1, 1]);
        assert_eq!(BOX['╆' as usize - 0x2500], [1, 2, 2, 1]);
    }
}
