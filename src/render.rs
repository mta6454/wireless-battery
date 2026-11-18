//! Draws the per-device tray icon (percentage digits plus a level bar) as RGBA pixels, using a
//! built-in 3x5 pixel font so no font or graphics library is needed. Used on Windows, where the
//! tray can only show an image. Port of RenderIcon/IconFor in main.cpp.

use crate::state::LOW_BATTERY;

pub const SIZE: usize = 32;

type Rgb = [u8; 3];

// 3x5 glyphs, one row per entry, bit 2 = leftmost pixel.
const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];
const QUESTION: [u8; 5] = [0b111, 0b001, 0b011, 0b000, 0b010];

fn bar_color(pct: u8, charging: bool) -> Rgb {
    if charging {
        [0, 170, 255]
    } else if pct <= LOW_BATTERY {
        [232, 32, 48]
    } else if pct <= 30 {
        [255, 150, 0]
    } else {
        [30, 190, 90]
    }
}

struct Canvas(Vec<u8>);

impl Canvas {
    fn put(&mut self, x: usize, y: usize, c: Rgb, a: u8) {
        if x < SIZE && y < SIZE {
            let i = (y * SIZE + x) * 4;
            self.0[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], a]);
        }
    }

    fn rect(&mut self, x: usize, y: usize, w: usize, h: usize, c: Rgb, a: u8) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.put(xx, yy, c, a);
            }
        }
    }
}

/// `pct` None draws "?". `dim` fades the icon (device asleep, value is the last known one).
/// Returns SIZE*SIZE RGBA bytes.
pub fn render_icon(pct: Option<u8>, charging: bool, dim: bool) -> Vec<u8> {
    let a: u8 = if dim { 140 } else { 255 };
    let mut cv = Canvas(vec![0; SIZE * SIZE * 4]);
    // Dark rounded plate so the digits stay readable on light and dark taskbars alike.
    cv.rect(0, 1, SIZE, SIZE - 2, [28, 28, 32], a);
    cv.rect(1, 0, SIZE - 2, SIZE, [28, 28, 32], a);

    let glyphs: Vec<[u8; 5]> = match pct {
        Some(p) => p.to_string().bytes().map(|b| DIGITS[(b - b'0') as usize]).collect(),
        None => vec![QUESTION],
    };
    let low = pct.is_some_and(|p| !charging && p <= LOW_BATTERY);
    let text: Rgb = if low { [255, 70, 80] } else { [255, 255, 255] };
    // Biggest scale that fits the 32px plate: 1-2 glyphs at 4x (28px wide), "100" at 2x (22px).
    let n = glyphs.len();
    let scale = if n >= 3 { 2 } else { 4 };
    let width = (n * 4 - 1) * scale;
    let x0 = (SIZE - width) / 2;
    let y0 = 3 + (20 - 5 * scale) / 2; // centre vertically in the area above the bar
    for (gi, g) in glyphs.iter().enumerate() {
        for (row, bits) in g.iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) != 0 {
                    cv.rect(x0 + (gi * 4 + col) * scale, y0 + row * scale, scale, scale, text, a);
                }
            }
        }
    }
    // Level bar along the bottom.
    let p = pct.unwrap_or(0) as usize;
    cv.rect(2, SIZE - 5, SIZE - 4, 3, [70, 70, 76], a);
    if p > 0 {
        cv.rect(2, SIZE - 5, (SIZE - 4) * p / 100, 3, bar_color(p as u8, charging), a);
    }
    cv.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(buf: &[u8], x: usize, y: usize) -> [u8; 4] {
        let i = (y * SIZE + x) * 4;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    #[test]
    fn right_size_and_corners_transparent() {
        let b = render_icon(Some(50), false, false);
        assert_eq!(b.len(), SIZE * SIZE * 4);
        assert_eq!(px(&b, 0, 0)[3], 0);
        assert_eq!(px(&b, 16, 16)[3], 255);
    }

    #[test]
    fn bar_length_and_colour_follow_level() {
        let full = render_icon(Some(100), false, false);
        assert_eq!(px(&full, SIZE - 3, SIZE - 4), [30, 190, 90, 255]);
        let low = render_icon(Some(10), false, false);
        assert_eq!(px(&low, 3, SIZE - 4), [232, 32, 48, 255]);
        assert_eq!(px(&low, SIZE - 3, SIZE - 4), [70, 70, 76, 255]); // unfilled part
        let chg = render_icon(Some(10), true, false);
        assert_eq!(px(&chg, 3, SIZE - 4), [0, 170, 255, 255]);
    }

    #[test]
    fn dim_and_distinct_digits() {
        assert_eq!(px(&render_icon(Some(50), false, true), 16, 16)[3], 140);
        assert_ne!(render_icon(Some(1), false, false), render_icon(Some(7), false, false));
        assert_ne!(render_icon(None, false, false), render_icon(Some(0), false, false));
    }

    #[test]
    fn three_digits_fit_inside_canvas() {
        // 100%: text must stay inside the plate, not touch the right edge.
        let b = render_icon(Some(100), false, false);
        for y in 0..SIZE - 5 {
            assert_ne!(px(&b, SIZE - 1, y)[..3], [255, 255, 255]);
            assert_ne!(px(&b, 0, y)[..3], [255, 255, 255]);
        }
        // 99%: two glyphs at 4x are 28px wide, also inside.
        let b = render_icon(Some(99), false, false);
        assert_ne!(px(&b, SIZE - 1, 10)[..3], [255, 255, 255]);
    }
}
