//! Pure software compositing. No GPU, no Wayland, no Android — so it is fully
//! unit-testable on the CI host.
//!
//! Everything here works on `RGBA8` buffers in premultiplied-free straight alpha.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w as i32
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h as i32
    }

    pub fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.right() && py < self.bottom()
    }

    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        if r <= x || b <= y {
            None
        } else {
            Some(Rect::new(x, y, (r - x) as u32, (b - y) as u32))
        }
    }
}

/// A composited frame ready for upload into a GPU texture.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Straight RGBA8, top-down rows.
    pub rgba: Vec<u8>,
    /// Monotonic revision; the UI uploads only when it changes.
    pub seq: u64,
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Self {
        let mut rgba = vec![0u8; (width as usize * height as usize) * 4];
        // Fully transparent: the desktop wallpaper lives in the egui layer below.
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 0;
        }
        Self { width, height, rgba, seq: 0 }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if self.width == width && self.height == height {
            return;
        }
        *self = Frame::new(width, height);
        self.bump();
    }

    pub fn bump(&mut self) {
        self.seq = self.seq.wrapping_add(1);
    }

    pub fn clear_transparent(&mut self) {
        for px in self.rgba.chunks_exact_mut(4) {
            px.copy_from_slice(&[0, 0, 0, 0]);
        }
    }

    #[inline]
    fn put(&mut self, x: i32, y: i32, r: u8, g: u8, b: u8, a: u8) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 || a == 0 {
            return;
        }
        let idx = ((y as usize * self.width as usize) + x as usize) * 4;
        if a == 255 {
            self.rgba[idx] = r;
            self.rgba[idx + 1] = g;
            self.rgba[idx + 2] = b;
            self.rgba[idx + 3] = 255;
            return;
        }
        // Straight-alpha "over" blend.
        let sa = a as u32;
        let da = self.rgba[idx + 3] as u32;
        let inv = 255 - sa;
        let out_a = sa + da * inv / 255;
        if out_a == 0 {
            return;
        }
        for (i, s) in [(0usize, r as u32), (1, g as u32), (2, b as u32)] {
            let d = self.rgba[idx + i] as u32;
            let val = (s * sa + d * inv) / out_a.max(1).min(255) / 1;
            self.rgba[idx + i] = val.min(255) as u8;
        }
        self.rgba[idx + 3] = out_a.min(255) as u8;
    }
}

/// Blit a Wayland `wl_shm` ARGB8888/XRGB8888 buffer into the frame.
///
/// `dst` is the on-screen rectangle; the source is scaled with nearest-neighbour
/// sampling, which is what you want for a UI (no blurry text) while keeping the
/// compositor branch-light enough to run on one Cortex-A720 core.
#[allow(clippy::too_many_arguments)]
pub fn blit_argb(
    frame: &mut Frame,
    dst: Rect,
    src: &[u8],
    src_w: u32,
    src_h: u32,
    src_stride: u32,
    opaque: bool,
) {
    if src_w == 0 || src_h == 0 || dst.w == 0 || dst.h == 0 {
        return;
    }
    let needed = src_stride as usize * src_h as usize;
    if src.len() < needed {
        return;
    }
    for dy in 0..dst.h {
        let sy = (dy as u64 * src_h as u64 / dst.h as u64) as u32;
        if sy >= src_h {
            continue;
        }
        let row = (sy as usize) * (src_stride as usize);
        for dx in 0..dst.w {
            let sx = (dx as u64 * src_w as u64 / dst.w as u64) as u32;
            if sx >= src_w {
                continue;
            }
            let i = row + (sx as usize) * 4;
            // Little-endian ARGB8888 == BGRA byte order.
            let b = src[i];
            let g = src[i + 1];
            let r = src[i + 2];
            let a = if opaque { 255 } else { src[i + 3] };
            frame.put(dst.x + dx as i32, dst.y + dy as i32, r, g, b, a);
        }
    }
}

/// Classic 12x19 arrow cursor, upscaled by `scale`.
///
/// Drawn in software so the pointer exists even before any client sets a cursor
/// image — "PC gibi" needs a pointer from the very first frame.
pub const CURSOR_BITMAP: [&str; 19] = [
    "X...........",
    "XX..........",
    "XWX.........",
    "XWWX........",
    "XWWWX.......",
    "XWWWWX......",
    "XWWWWWX.....",
    "XWWWWWWX....",
    "XWWWWWWWX...",
    "XWWWWWWWWX..",
    "XWWWWWWWWWX.",
    "XWWWWWWWWWWX",
    "XWWWWWXXXXXX",
    "XWWXWWX.....",
    "XWX.XWWX....",
    "XX..XWWX....",
    "X....XWWX...",
    ".....XWWX...",
    "......XX....",
];

pub fn draw_cursor(frame: &mut Frame, x: i32, y: i32, scale: u32) {
    let scale = scale.max(1);
    for (row, line) in CURSOR_BITMAP.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            let (r, g, b, a) = match ch {
                'X' => (20, 20, 24, 255),
                'W' => (250, 250, 252, 255),
                _ => continue,
            };
            for sy in 0..scale {
                for sx in 0..scale {
                    frame.put(
                        x + col as i32 * scale as i32 + sx as i32,
                        y + row as i32 * scale as i32 + sy as i32,
                        r,
                        g,
                        b,
                        a,
                    );
                }
            }
        }
    }
}

/// Rounded-corner shadow plate used behind windows (gives the desktop depth
/// without a GPU blur).
pub fn draw_shadow(frame: &mut Frame, around: Rect, spread: i32) {
    for i in 0..spread {
        let a = (26 - i * 2).clamp(0, 26) as u8;
        let r = Rect::new(
            around.x - i,
            around.y - i,
            around.w + (i as u32) * 2,
            around.h + (i as u32) * 2,
        );
        let edge_col = 0u8;
        for x in r.x..r.right() {
            frame.put(x, r.y, edge_col, edge_col, edge_col, a);
            frame.put(x, r.bottom() - 1, edge_col, edge_col, edge_col, a);
        }
        for y in r.y..r.bottom() {
            frame.put(r.x, y, edge_col, edge_col, edge_col, a);
            frame.put(r.right() - 1, y, edge_col, edge_col, edge_col, a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(f: &Frame, x: u32, y: u32) -> [u8; 4] {
        let i = ((y as usize * f.width as usize) + x as usize) * 4;
        [f.rgba[i], f.rgba[i + 1], f.rgba[i + 2], f.rgba[i + 3]]
    }

    #[test]
    fn rect_math() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b).unwrap(), Rect::new(5, 5, 5, 5));
        assert!(a.contains(9, 9));
        assert!(!a.contains(10, 9));
        assert!(Rect::new(0, 0, 2, 2).intersect(&Rect::new(5, 5, 1, 1)).is_none());
    }

    #[test]
    fn blit_scales_and_swizzles_bgra_to_rgba() {
        let mut f = Frame::new(4, 4);
        // 2x2 source, pure blue in BGRA byte order.
        let src: Vec<u8> = vec![255, 0, 0, 255].repeat(4);
        blit_argb(&mut f, Rect::new(0, 0, 4, 4), &src, 2, 2, 8, true);
        assert_eq!(px(&f, 0, 0), [0, 0, 255, 255]);
        assert_eq!(px(&f, 3, 3), [0, 0, 255, 255]);
    }

    #[test]
    fn blit_respects_destination_offset() {
        let mut f = Frame::new(8, 8);
        let src: Vec<u8> = vec![0, 255, 0, 255].repeat(4);
        blit_argb(&mut f, Rect::new(6, 6, 2, 2), &src, 2, 2, 8, true);
        assert_eq!(px(&f, 6, 6), [0, 255, 0, 255]);
        assert_eq!(px(&f, 5, 5), [0, 0, 0, 0]);
        // Clipped, must not panic.
        blit_argb(&mut f, Rect::new(7, 7, 4, 4), &src, 2, 2, 8, true);
    }

    #[test]
    fn blit_rejects_short_buffer() {
        let mut f = Frame::new(4, 4);
        blit_argb(&mut f, Rect::new(0, 0, 4, 4), &[1, 2, 3], 2, 2, 8, true);
        assert_eq!(px(&f, 0, 0), [0, 0, 0, 0]);
    }

    #[test]
    fn cursor_draws_inside_bounds_only() {
        let mut f = Frame::new(32, 32);
        draw_cursor(&mut f, 28, 28, 2);
        // Corner pixel of the cursor arrow is opaque black.
        assert_eq!(px(&f, 28, 28), [20, 20, 24, 255]);
        // Nothing outside the buffer, no panic.
        draw_cursor(&mut f, -50, -50, 1);
    }

    #[test]
    fn alpha_blend_keeps_transparency() {
        let mut f = Frame::new(2, 2);
        // wl_shm ARGB8888 on little endian means the bytes are B, G, R, A.
        let src: Vec<u8> = vec![255, 0, 0, 128].repeat(4);
        blit_argb(&mut f, Rect::new(0, 0, 2, 2), &src, 2, 2, 8, false);
        let p = px(&f, 0, 0);
        assert_eq!(p[3], 128, "alpha must survive");
        assert_eq!(p[2], 255, "blue must survive");
        assert_eq!(p[0], 0, "red must stay empty");
    }

    #[test]
    fn resize_resets_and_bumps_seq() {
        let mut f = Frame::new(2, 2);
        let before = f.seq;
        f.resize(4, 4);
        assert_eq!((f.width, f.height), (4, 4));
        assert!(f.seq > before);
        assert_eq!(f.rgba.len(), 64);
    }
}
