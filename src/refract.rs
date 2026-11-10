//! The glass edge: a snapshot of the desktop behind the notch, bent along the rim like thick glass.
//!
//! A snapshot of the screen is taken when the notch opens (while the window is still small, so it
//! does not photograph itself). The rim is worked out from it on the CPU for the notch's current
//! size and uploaded as one small picture, drawn with a single call; it is only made again when the
//! size changes (while a page change resizes the notch, and never while nothing moves).
//!
//! Live glass: while the notch is open the screen is looked at again every so often, on a
//! background thread (`refresh`). The notch is hidden from screen copies for that (the window
//! is excluded from capture), so the copy shows only what is really behind it. A copy that turns
//! out identical to the last one (as far as the rim can see) is thrown away at once.

use std::ffi::c_void;

use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, CAPTUREBLT, DIB_RGB_COLORS, HGDIOBJ, ROP_CODE, SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// A piece of the screen as it looked at one moment: opaque BGRA pixels, `w` x `h`.
pub struct Shot {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u32>,
    /// Where the copy's top-left corner lies in the notch window (0, 0 for a copy of all of it).
    pub ox: i32,
    pub oy: i32,
    /// A copy of the whole window (a live refresh copies only the part the glass can read).
    pub full: bool,
    /// For a partial copy: the notch shape it was made for.
    pub made_for: Option<Shape>,
}

/// Copy the screen area at (`x`, `y`) of size `w` x `h` (screen pixels): the whole notch window.
/// None if it cannot be had.
pub fn grab(x: i32, y: i32, w: i32, h: i32) -> Option<Shot> {
    grab_at(x, y, w, h, 0, 0).map(|mut s| {
        s.full = true;
        s
    })
}

/// Copy a piece of the screen that sits at (`ox`, `oy`) in the notch window.
fn grab_at(x: i32, y: i32, w: i32, h: i32, ox: i32, oy: i32) -> Option<Shot> {
    if !(8..=4096).contains(&w) || !(8..=2400).contains(&h) {
        return None;
    }
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let hdc = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let out = match CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bmp) => {
                let mut shot = None;
                if !bits.is_null() {
                    let old = SelectObject(hdc, HGDIOBJ(bmp.0));
                    let ok = BitBlt(hdc, 0, 0, w, h, Some(screen), x, y, ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0)).is_ok();
                    if ok {
                        let len = w as usize * h as usize;
                        // (a screen copy has no alpha; only the colours are ever read)
                        let px = std::slice::from_raw_parts(bits as *const u32, len).to_vec();
                        shot = Some(Shot { w: w as usize, h: h as usize, px, ox, oy, full: false, made_for: None });
                    }
                    SelectObject(hdc, old);
                }
                let _ = DeleteObject(HGDIOBJ(bmp.0));
                shot
            }
            Err(_) => None,
        };
        let _ = DeleteDC(hdc);
        ReleaseDC(None, screen);
        out
    }
}

/// The rim, ready to upload: premultiplied BGRA pixels, `w` x `h`, at (`x0`, `y0`) in window pixels.
pub struct Rim {
    pub x0: i32,
    pub y0: i32,
    pub w: usize,
    pub h: usize,
    /// Each picture pixel stands for `step` x `step` window pixels (1 = pixel for pixel).
    pub step: usize,
    pub px: Vec<u32>,
}

/// The look of the glass (sizes in logical pixels, scaled by `Shape::scale`). The defaults are the
/// look the glass has always had.
#[derive(Clone, Copy, PartialEq)]
pub struct Tune {
    pub width: f32,
    pub pull: f32,
    pub bend: f32,
    pub strength: f32,
    pub corner: f32,
    pub fringe: f32,
    pub reach: f32,
    pub soft: f32,
    pub mid: f32,
    pub mid_warp: f32,
    pub mid_blur: f32,
    pub sharp: f32,
    /// 1 pulls the picture from outside the notch in; -1 stretches the picture under it outward.
    pub dir: f32,
    /// The bright line along the edge, the frost, the lean of the stretch, its length and the
    /// dimming for a bright background (see `GlassCfg`).
    pub edge_light: f32,
    pub tint: f32,
    pub fan: f32,
    pub len: f32,
    pub readable: f32,
    /// How far the picture is bent at each step in from the edge (see `snell_lut`); None = the old curve.
    pub lut: Option<[f32; 65]>,
    /// The rim light: the direction it comes from (a unit vector, y down), how much the highlight
    /// follows it, and the width of the line.
    pub lx: f32,
    pub ly: f32,
    pub ldir: f32,
    pub lw: f32,
}

impl From<&crate::config::GlassCfg> for Tune {
    fn from(g: &crate::config::GlassCfg) -> Self {
        Self {
            width: g.edge_width.clamp(20.0, 160.0),
            pull: g.pull.clamp(0.0, 60.0),
            bend: g.bend.clamp(0.8, 3.0),
            strength: g.strength.clamp(0.3, 1.0),
            corner: g.corner.clamp(0.0, 1.0),
            fringe: g.fringe.clamp(0.0, 0.2),
            reach: 1.0, // extra height above the corner curve; the rim follows the bottom band (see draw)
            soft: g.softness.clamp(0.5, 12.0),
            mid: g.middle.clamp(0.0, 1.0),
            mid_warp: if g.stretch { 0.0 } else { g.middle_warp.clamp(0.0, 1.0) },
            mid_blur: g.middle_blur.clamp(0.0, 6.0) * 0.7,
            sharp: g.sharp_zone.clamp(0.3, 4.0),
            dir: if g.stretch { -1.0 } else { 1.0 },
            edge_light: g.edge_light.clamp(0.0, 1.0),
            tint: g.tint.clamp(0.0, 1.0),
            fan: g.fan.clamp(0.0, 1.2),
            len: g.stretch_len.clamp(0.5, 1.6),
            readable: g.readable.clamp(0.0, 1.0),
            lut: g.snell.then(|| {
                snell_lut(g.snell_ior.clamp(1.05, 2.4), g.snell_thick.clamp(0.3, 4.0), g.snell_round.clamp(2.0, 6.0))
            }),
            lx: g.light_angle.to_radians().cos(),
            ly: g.light_angle.to_radians().sin(),
            ldir: g.light_dir.clamp(0.0, 1.0),
            lw: g.light_width.clamp(0.8, 6.0),
        }
    }
}

/// Where the notch is, in window pixels: left and right edges, the bottom edge and the radius of
/// its bottom corners; `scale` is device pixels per logical unit.
#[derive(Clone, Copy, PartialEq)]
pub struct Shape {
    pub left: f32,
    pub right: f32,
    pub bottom: f32,
    pub radius: f32,
    pub scale: f32,
    pub tune: Tune,
    /// Top edge of the notch (window pixels).
    pub top: f32,
    /// Height of the see-through strip along the bottom, window pixels (0 = none), and how far the
    /// black thins out at its very bottom (0 - 1).
    pub band: f32,
    pub fade: f32,
    /// How see-through the whole notch is (0 = solid black).
    pub clear: f32,
}

/// A quick fingerprint of the part of the screen copy the rim actually reads: a thin ring around
/// the notch (the strip along its sides and bottom, plus the reach of the glass pull), sampled
/// sparsely. Equal fingerprints mean "nothing the rim would show has changed", so a video playing
/// in the middle of the screen, or under the notch's inside, costs nothing.
pub fn ring_sum(s: &Shot, sh: Shape) -> u64 {
    let sc = sh.scale.max(0.5);
    let r = sh.radius.max(2.0 * sc).min((sh.right - sh.left) * 0.5);
    let t = sh.tune;
    let pad = (t.pull * (1.0 + t.corner) + 4.0) * sc; // the glass pulls from up to this far outside the edge
    let band = (t.width * sc + pad).ceil() as i32; // how far in from the edge the rim reaches
    let (sx0, sy0) = (s.ox, s.oy);
    let (sx1, sy1) = (s.ox + s.w as i32, s.oy + s.h as i32);
    let x0 = ((sh.left - pad).floor() as i32).clamp(sx0, sx1);
    let x1 = ((sh.right + pad).ceil() as i32).clamp(sx0, sx1);
    let y0 = ((sh.bottom - r - t.reach * sc - pad).floor() as i32).clamp(sy0, sy1);
    let y1 = ((sh.bottom + pad).ceil() as i32).clamp(sy0, sy1);
    let mid = t.mid > 0.01 && ((sh.band > 0.0 && sh.fade > 0.004) || sh.clear > 0.004);
    let int_top = if sh.clear > 0.004 { sh.top } else { sh.bottom - sh.band };
    let (y0, full_from) = if mid {
        (y0.min(((int_top - pad).floor() as i32).clamp(sy0, sy1)), (sh.bottom - band as f32).min(int_top).floor() as i32)
    } else {
        (y0, (sh.bottom - band as f32).floor() as i32)
    };
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    let mut mix = |p: u32| h = (h ^ (p & 0x00FF_FFFF) as u64).wrapping_mul(0x0000_0100_0000_01b3);
    for y in (y0..y1).step_by(2) {
        let ly = (y - sy0) as usize;
        let row = &s.px[ly * s.w..(ly + 1) * s.w];
        if y >= full_from {
            row[(x0 - sx0) as usize..(x1 - sx0) as usize].iter().step_by(3).for_each(|&p| mix(p));
        } else {
            let left_end = (x0 + band).min(x1);
            let right_start = (x1 - band).max(left_end);
            row[(x0 - sx0) as usize..(left_end - sx0) as usize].iter().step_by(3).for_each(|&p| mix(p));
            row[(right_start - sx0) as usize..(x1 - sx0) as usize].iter().step_by(3).for_each(|&p| mix(p));
        }
    }
    h
}

/// What a refresh brings back.
pub struct Fresh {
    pub shot: Shot,
    pub sum: u64,
    /// The notch shape the rim was made for.
    pub shape: Shape,
    pub rim: Option<Rim>,
}

pub enum Fetched {
    /// Looks exactly like the previous copy: nothing to do.
    Same,
    Fresh(Fresh),
}

/// Copy the screen again and, when its ring differs from the one with fingerprint `prev`, work out
/// the rim for `shape` as well, at half detail (it is a soft gradient and this runs often). Meant
/// for a background thread; None when the screen could not be copied.
pub fn refresh(x: i32, y: i32, w: i32, h: i32, prev: u64, shape: Shape) -> Option<Fetched> {
    // only the part of the screen the glass can read (a fraction of the window): far less to copy
    let (cx0, cy0, cx1, cy1) = crop_for(shape, w, h);
    let mut shot = grab_at(x + cx0, y + cy0, cx1 - cx0, cy1 - cy0, cx0, cy0)?;
    shot.made_for = Some(shape);
    let sum = ring_sum(&shot, shape);
    if sum == prev {
        return Some(Fetched::Same);
    }
    let made = rim(&shot, shape, 1);
    Some(Fetched::Fresh(Fresh { shot, sum, shape, rim: made }))
}

/// The part of the notch window (x0, y0, x1, y1) the glass for `sh` can read: the notch itself and
/// a margin for how far the glass looks outside it. A live refresh copies only that.
fn crop_for(sh: Shape, w: i32, h: i32) -> (i32, i32, i32, i32) {
    let sc = sh.scale.max(0.5);
    let t = sh.tune;
    let m = ((t.pull * (1.0 + t.corner) + t.mid_blur + 6.0) * sc).ceil();
    let r = sh.radius.max(2.0 * sc).min((sh.right - sh.left) * 0.5);
    let mid = t.mid > 0.01 && ((sh.band > 0.0 && sh.fade > 0.004) || sh.clear > 0.004);
    let int_top = if sh.clear > 0.004 { sh.top } else { sh.bottom - sh.band };
    let rim_top = sh.bottom - r - t.reach * sc;
    let top = if mid { rim_top.min(int_top) } else { rim_top };
    let x0 = ((sh.left - m).floor() as i32).clamp(0, w);
    let x1 = ((sh.right + m).ceil() as i32).clamp(0, w);
    let y0 = ((top - m).floor() as i32).clamp(0, h);
    let y1 = ((sh.bottom + m).ceil() as i32).clamp(0, h);
    (x0, y0, x1, y1)
}

/// How far a ray is shifted by a glass edge, for 65 steps from the inner end of the rim (0) to the very
/// edge (1), as a share of the shift at the edge. The edge is a rounded bevel (height `thickness` times its
/// width); a ray coming straight down meets the surface at the angle of its slope, bends toward the normal
/// as it enters glass (Snell's law, index `n`) and so lands `tan(angle - bent angle)` sideways for every
/// unit of glass it crosses. Worked out once per setting, so drawing only reads the table.
fn snell_lut(n: f32, thickness: f32, p: f32) -> [f32; 65] {
    // `p` is how square the bevel is: 2 is a quarter circle, higher is flatter on top
    let edge = (std::f32::consts::FRAC_PI_2 - (1.0 / n).asin()).tan();
    let mut out = [0.0_f32; 65];
    for (i, o) in out.iter_mut().enumerate().take(64) {
        let u = i as f32 / 64.0;
        let slope = u.powf(p - 1.0) / (1.0 - u.powf(p)).powf((p - 1.0) / p);
        let a = (thickness * slope).atan();
        let b = (a.sin() / n).asin();
        *o = ((a - b).tan() / edge).clamp(0.0, 1.0);
    }
    out[64] = 1.0;
    out
}

/// The bend at `u` (1 at the edge of the glass, 0 at the inner end of the rim).
#[inline]
fn curve(t: &Tune, u: f32) -> f32 {
    match &t.lut {
        Some(l) => {
            let x = u.clamp(0.0, 1.0) * 64.0;
            let i = (x as usize).min(63);
            l[i] + (l[i + 1] - l[i]) * (x - i as f32)
        }
        None if (t.bend - 1.5).abs() < 0.001 => u * u.sqrt(),
        None => u.powf(t.bend),
    }
}

fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// One colour (`shift` 16 red, 8 green, 0 blue) of the picture at a fractional position, smoothly
/// blended between the four nearest pixels (coordinates are clamped to the edge).
fn sample(s: &Shot, fx: f32, fy: f32, shift: u32) -> f32 {
    let (gx, gy) = (fx - s.ox as f32 - 0.5, fy - s.oy as f32 - 0.5);
    let (x0, y0) = (gx.floor(), gy.floor());
    let (tx, ty) = (gx - x0, gy - y0);
    let (x0, y0) = (x0 as i32, y0 as i32);
    let at = |x: i32, y: i32| -> f32 {
        let xi = x.clamp(0, s.w as i32 - 1) as usize;
        let yi = y.clamp(0, s.h as i32 - 1) as usize;
        ((s.px[yi * s.w + xi] >> shift) & 255) as f32
    };
    let (a, b, c, d) = if x0 >= 0 && y0 >= 0 && (x0 as usize) + 1 < s.w && (y0 as usize) + 1 < s.h {
        // the usual case: all four pixels are inside the copy
        let i = y0 as usize * s.w + x0 as usize;
        let g = |p: u32| ((p >> shift) & 255) as f32;
        (g(s.px[i]), g(s.px[i + 1]), g(s.px[i + s.w]), g(s.px[i + s.w + 1]))
    } else {
        (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1))
    };
    let top = a + (b - a) * tx;
    let bot = c + (d - c) * tx;
    top + (bot - top) * ty
}

/// The picture at a fractional position as red, green and blue, blended between the four nearest
/// pixels (coordinates are clamped to the edge).
fn sample_rgb(s: &Shot, fx: f32, fy: f32) -> [f32; 3] {
    let (gx, gy) = (fx - s.ox as f32 - 0.5, fy - s.oy as f32 - 0.5);
    let (x0, y0) = (gx.floor(), gy.floor());
    let (tx, ty) = (gx - x0, gy - y0);
    let (x0, y0) = (x0 as i32, y0 as i32);
    let at = |x: i32, y: i32| -> u32 {
        let xi = x.clamp(0, s.w as i32 - 1) as usize;
        let yi = y.clamp(0, s.h as i32 - 1) as usize;
        s.px[yi * s.w + xi]
    };
    let (a, b, c, d) = (at(x0, y0), at(x0 + 1, y0), at(x0, y0 + 1), at(x0 + 1, y0 + 1));
    let mut out = [0.0_f32; 3];
    for (n, shift) in [16u32, 8, 0].into_iter().enumerate() {
        let g = |p: u32| ((p >> shift) & 255) as f32;
        let top = g(a) + (g(b) - g(a)) * tx;
        let bot = g(c) + (g(d) - g(c)) * tx;
        out[n] = top + (bot - top) * ty;
    }
    out
}

/// Work out the rim along the sides and the bottom corners of the notch. Inside the rim each
/// pixel shows what lies further out (pulled in toward the edge, strongest at the very edge),
/// fading to nothing toward the inside of the notch.
///
/// `step` 1 gives the finest picture; 2 makes it on every other pixel each way (a quarter of the
/// work, drawn enlarged): the rim is a soft gradient, so it looks the same while it is moving.
pub fn rim(shot: &Shot, sh: Shape, step: usize) -> Option<Rim> {
    let st = step.max(1);
    let sc = sh.scale.max(0.5);
    let t = sh.tune;
    let width = t.width * sc; // how far in from the edge the rim reaches
    let pull = t.pull * sc; // how far out the strongest pixels look
    let soft = t.soft * sc;
    let even_bend = (t.bend - 1.5).abs() < 0.001;
    // the most the picture may stretch before it would fold over (see the rim loop)
    let stretch_cap = -0.92 * t.len * width / if even_bend { 1.5 } else { t.bend.max(1.0) };
    // the linear fan: how far the stretch turns sideways at the very ends of the bottom edge
    // (radians), and the middle and half width it is measured from. Straight down in the middle,
    // turning more and more to the left on the left half and to the right on the right half.
    let fan_a = t.fan;
    let (fan_c, fan_h) = ((sh.left + sh.right) * 0.5, ((sh.right - sh.left) * 0.5).max(1.0));
    let r = sh.radius.max(2.0 * sc).min((sh.right - sh.left) * 0.5);
    let reach = r + t.reach * sc; // how far up the sides it is drawn, fading out toward the top
    // The middle: wherever the notch is even a little see-through (the strip along the bottom, or
    // the whole notch when the panel opacity is below 100 %) the glass carries on across it.
    let mid = t.mid > 0.01 && ((sh.band > 0.0 && sh.fade > 0.004) || sh.clear > 0.004);
    // the corners rise exactly as high as the strip along the bottom and fade out with it, so the
    // edge is even all the way across (without the strip they stay within the curve)
    let reach = if mid { reach.max(sh.band) } else { reach };
    let int_top = if sh.clear > 0.004 { sh.top } else { sh.bottom - sh.band };
    let top_y = if mid { (sh.bottom - reach).min(int_top) } else { sh.bottom - reach };
    let x0 = ((sh.left - 2.0).floor() as i32).max(shot.ox);
    let x1 = ((sh.right + 2.0).ceil() as i32).min(shot.ox + shot.w as i32);
    let y0 = (top_y.floor() as i32).max(shot.oy);
    let y1 = ((sh.bottom + 2.0).ceil() as i32).min(shot.oy + shot.h as i32);
    if x1 - x0 < 8 || y1 - y0 < 4 {
        return None;
    }
    let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
    let (w, h) = (w.div_ceil(st), h.div_ceil(st));
    let half = st as f32 * 0.5;
    let mut px = vec![0u32; w * h];
    let (cl, cr, cb) = (sh.left + r, sh.right - r, sh.bottom - r);
    let top = sh.bottom - reach;
    // Rows higher up only carry the rim along the two sides, so the middle is skipped there;
    // from `full_from` down the whole row is drawn. The corners reach further in than the sides.
    let full_from = if mid { (sh.bottom - width - 1.0).min(int_top) } else { sh.bottom - width - 1.0 };
    let side = ((width.max(r) + 2.0).ceil() as usize).div_ceil(st);
    let w2 = width + 70.0 * sc; // how far in the middle's gentle bend fades out
    let band = sh.band.max(1.0);
    // deeper in, the outline the middle follows is rounder and rounder (bigger circles at the corners)
    let rb = (r + 70.0 * sc).min((sh.right - sh.left) * 0.5);
    let (cl2, cr2, cb2) = (sh.left + rb, sh.right - rb, sh.bottom - rb);
    // Step 1 is sharp where it matters and half detail elsewhere: the glass bends the screen
    // hardest right along the edge, where fine text shows. Deeper in the rim is faint and the
    // middle is soft anyway, so each 2 x 2 block there is worked out once.
    let hybrid = st == 1;
    // (the top setting of the sharp zone means every pixel, so there are no blocks at all)
    let everywhere = t.sharp >= 3.95;
    let cell = if hybrid && !everywhere { 2_usize } else { 1 };
    let fine_d = if hybrid && !everywhere { width * t.sharp } else { f32::MAX };
    // the corner curve of the Alcove notch is a superellipse (see `gfx::notch_path`); elsewhere a circle
    let sq_n = if crate::ui::look_alcove() { crate::gfx::CORNER_N } else { 2.0 };
    // distance inside the edge, the direction pointing out of the notch, and the corner weight
    let geom = |x: f32, y: f32| -> (f32, f32, f32, f32) {
        if y > cb && (x < cl || x > cr) {
            let cx = if x < cl { cl } else { cr };
            let (dx, dy) = (x - cx, y - cb);
            let dist = (dx * dx + dy * dy).sqrt().max(0.001);
            if sq_n > 2.01 {
                // distance along the ray from the corner's centre to the curve, and the curve's normal
                let (ax, ay) = ((dx.abs() / r).max(1e-4), (dy.abs() / r).max(1e-4));
                let f = ax.powf(sq_n) + ay.powf(sq_n);
                let d = (dist * (f.powf(-1.0 / sq_n) - 1.0)).min(r);
                let (gx, gy) = (ax.powf(sq_n - 1.0) * dx.signum(), ay.powf(sq_n - 1.0) * dy.signum());
                let gl = (gx * gx + gy * gy).sqrt().max(1e-4);
                return (d, gx / gl, gy / gl, (2.0 * dx.abs() * dy / (dist * dist)).clamp(0.0, 1.0));
            }
            // corner: 0 where the curve meets the straight edges, 1 in the middle of the curve (no seam)
            (r - dist, dx / dist, dy / dist, (2.0 * dx.abs() * dy / (dist * dist)).clamp(0.0, 1.0))
        } else {
            let (dl, dr, db) = (x - sh.left, sh.right - x, sh.bottom - y);
            if db <= dl && db <= dr {
                (db, 0.0, 1.0, 0.0)
            } else if dl <= dr {
                (dl, -1.0, 0.0, 0.0)
            } else {
                (dr, 1.0, 0.0, 0.0)
            }
        }
    };
    // one pixel of the picture (0 = nothing there)
    let params = |x: f32, y: f32| -> Option<(f32, f32, f32, f32)> {
        let up = smooth(0.0, 0.5, (y - top) / reach);
        let in_mid = mid && y >= int_top;
        if up <= 0.0 && !in_mid {
            return None;
        }
        // how much of the middle shows on this row: the black thins toward the bottom edge
        let a_row = if in_mid {
            let bt = ((y - (sh.bottom - sh.band)) / band).clamp(0.0, 1.0);
            let e = bt * bt * (3.0 - 2.0 * bt); // an even, gentle rise from black up to the colour
            (sh.fade * e).max(sh.clear) * t.mid
        } else {
            0.0
        };
        let (d, nx, ny, corner) = geom(x, y);
        if d <= -1.0 {
            return None;
        }
        let (mut alpha, mut ox, mut oy) = (0.0_f32, 0.0_f32, 0.0_f32);
        if d < width && up > 0.0 {
            // u: 1 at the edge, 0 at the inner end of the rim. The glass is strongest right at the
            // edge, and strongest of all in the corners, then eases away quickly so the black of the
            // notch takes over; the tail is smooth, with no visible line where the rim stops.
            let u = 1.0 - d / width;
            let fade = u * u * (1.5 - 0.5 * u);
            let base = (t.strength + 0.1 * corner).min(1.0);
            alpha = (base + (1.0 - base) * smooth(soft, 0.0, d)) * fade * (d + 1.0).clamp(0.0, 1.0) * up;
            let k = curve(&t, u);
            // stretch mode: the straight edges stretch nearly as much as the corners do
            // (corner = 0 along them, so they used to get no boost at all)
            let cw = if t.dir < 0.0 { corner.max(0.85) } else { corner };
            let mut p = pull * t.dir * (1.0 + t.corner * cw);
            if t.dir < 0.0 {
                // stretching only works while the picture keeps its order across the rim; past this
                // limit it folds over and the text reads backwards, so the stretch never folds
                p = p.max(stretch_cap);
            }
            let (nx, ny) = if t.dir < 0.0 && ny > 0.0 {
                // turn the direction sideways in step with the distance from the middle (and by less the
                // closer it already points sideways, so it still meets the straight sides)
                let s = ((x - fan_c) / fan_h).clamp(-1.0, 1.0);
                let (sn, cs) = (fan_a * s * ny).sin_cos();
                (nx * cs + ny * sn, ny * cs - nx * sn)
            } else {
                (nx, ny)
            };
            ox = nx * p * k;
            oy = ny * p * k;
        }
        let mut blur = 0.0_f32;
        if a_row > 0.0 {
            // the middle lies under the rim; it bends gently toward the nearest edges
            let ai = a_row * (d + 1.0).clamp(0.0, 1.0);
            alpha += ai * (1.0 - alpha);
            if d > width && t.mid_warp > 0.0 {
                let dm;
                let dirx;
                let diry;
                if y > cb2 && (x < cl2 || x > cr2) {
                    // around a corner: along circles, with the outline getting rounder inward
                    let cx = if x < cl2 { cl2 } else { cr2 };
                    let (dx, dy) = (x - cx, y - cb2);
                    let dist = (dx * dx + dy * dy).sqrt().max(0.001);
                    dm = d + ((rb - dist) - d) * smooth(0.0, width, d);
                    dirx = dx / dist;
                    diry = dy / dist;
                } else {
                    // a soft blend of the nearest edges' directions: no crease where they meet
                    let wl = 1.0 / (x - sh.left + 6.0).max(1.0).powi(3);
                    let wr = 1.0 / (sh.right - x + 6.0).max(1.0).powi(3);
                    let wb = 1.0 / (sh.bottom - y + 6.0).max(1.0).powi(3);
                    let inv = 1.0 / (wl + wr + wb);
                    dm = d;
                    dirx = (wr - wl) * inv;
                    diry = wb * inv;
                }
                if t.mid_warp > 0.0 {
                    let wi = smooth(width, width * 1.8, d);
                    let u2 = (1.0 - dm / w2).clamp(0.0, 1.0);
                    let k2 = curve(&t, u2);
                    let m = pull * t.mid_warp * k2 * wi;
                    ox += dirx * m;
                    oy += diry * m;
                }
            }
            // the middle is a little soft; the sharp rim keeps its fine detail
            blur = t.mid_blur * sc * smooth(width * 0.6, width * 1.6, d);
        }
        if alpha < 0.004 {
            return None;
        }
        Some((alpha, ox, oy, blur))
    };
    // The light along the edge: a thin bright line right at the outer boundary, stronger in the
    // corners, fading out up the sides with the glass. `lift` lays that white over a finished pixel.
    let hw = t.lw * sc;
    let light = |x: f32, y: f32| -> f32 {
        if t.edge_light <= 0.0 {
            return 0.0;
        }
        let (d, nx, ny, corner) = geom(x, y);
        if d >= hw || d <= -1.0 {
            return 0.0;
        }
        let f = 1.0 - d.max(0.0) / hw;
        let up = smooth(0.0, 0.5, (y - top) / reach);
        // like Apple's glass: bright where the edge faces the light, a softer glint on the far side
        let face = nx * t.lx + ny * t.ly;
        let lit = (0.2 + face.max(0.0) + 0.6 * (-face).max(0.0)).min(1.0);
        let k = 1.0 - t.ldir + t.ldir * lit;
        (t.edge_light * k * f * f.sqrt() * (1.0 + 0.6 * corner) * up).clamp(0.0, 1.0)
    };
    let lift = |p: u32, h: f32| -> u32 {
        let (m, w) = (1.0 - h, 255.0 * h);
        let c = |sft: u32| -> u32 { (((p >> sft) & 255) as f32 * m + w).round().min(255.0) as u32 };
        (c(24) << 24) | (c(16) << 16) | (c(8) << 8) | c(0)
    };
    let shade = |x: f32, y: f32| -> u32 {
        let Some((alpha, ox, oy, blur)) = params(x, y) else {
            return 0;
        };
        let (rc, gc, bc) = if blur > 0.25 {
            let (bx, by) = (x + ox, y + oy);
            let mut acc = [0.0_f32; 3];
            for (sx, sy) in [(-blur, -blur), (blur, -blur), (-blur, blur), (blur, blur)] {
                let c = sample_rgb(shot, bx + sx, by + sy);
                acc[0] += c[0];
                acc[1] += c[1];
                acc[2] += c[2];
            }
            (acc[0] * 0.25, acc[1] * 0.25, acc[2] * 0.25)
        } else if st == 1 && ox == 0.0 && oy == 0.0 {
            // nothing bends it here, so the pixel behind is copied as it is (the same result as the
            // three blended reads below, without the blending)
            let xi = (x - shot.ox as f32).floor().clamp(0.0, shot.w as f32 - 1.0) as usize;
            let yi = (y - shot.oy as f32).floor().clamp(0.0, shot.h as f32 - 1.0) as usize;
            let p = shot.px[yi * shot.w + xi];
            (((p >> 16) & 255) as f32, ((p >> 8) & 255) as f32, (p & 255) as f32)
        } else {
            // red, green and blue look a slightly different distance out, like light through glass
            (
                sample(shot, x + ox, y + oy, 16),
                sample(shot, x + ox * (1.0 - t.fringe), y + oy * (1.0 - t.fringe), 8),
                sample(shot, x + ox * (1.0 - 2.0 * t.fringe), y + oy * (1.0 - 2.0 * t.fringe), 0),
            )
        };
        let ch = |v: f32| (v * alpha).round().clamp(0.0, 255.0) as u32;
        let out = (((alpha * 255.0).round().clamp(0.0, 255.0) as u32) << 24) | (ch(rc) << 16) | (ch(gc) << 8) | ch(bc);
        let hl = light(x, y);
        if hl > 0.0 {
            lift(out, hl)
        } else {
            out
        }
    };
    // Whole stretches of a row come out the same: the straight middle of the bottom edge, and the
    // inside of the notch. They are worked out once for the row instead of once per pixel (the
    // result is the same pixel for pixel). Only for the full-detail picture (one row per band) when
    // the middle is sharp.
    let fast = cell == 1 && st == 1 && t.mid_blur <= 0.0;
    // the columns [a, b) of the row at window height `y` that all look alike (empty when none)
    let row_span = |y: f32| -> (usize, usize) {
        let db = sh.bottom - y;
        let (mut xa, mut xb) = if db < width {
            if t.dir < 0.0 {
                return (usize::MAX, 0);
            }
            (sh.left + db, sh.right - db)
        } else if t.mid_warp <= 0.0 {
            (sh.left + width, sh.right - width)
        } else if db > w2 + 1.0 {
            // beyond the reach of the middle's bend nothing moves: the same straight copy
            (sh.left + w2 + 1.0, sh.right - w2 - 1.0)
        } else {
            return (usize::MAX, 0);
        };
        if y > cb {
            xa = xa.max(cl);
            xb = xb.min(cr);
        }
        // a pixel's x is x0 + its column + 0.5; stay a pixel inside the limits
        let a = ((xa - x0 as f32 - 0.5).ceil() + 1.0).max(0.0);
        let b = ((xb - x0 as f32 - 0.5).floor() - 1.0).min(w as f32 - 1.0);
        if b < a {
            (usize::MAX, 0)
        } else {
            (a as usize, b as usize + 1)
        }
    };
    // fill the columns [a, b) of that row; false when this row needs the per-pixel way after all
    let fill_span = |y: f32, a: usize, b: usize, out: &mut [u32]| -> bool {
        let xr = x0 as f32 + ((a + b) / 2) as f32 + 0.5;
        let Some((alpha, ox, oy, blur)) = params(xr, y) else {
            return true; // nothing there, and the row is still empty
        };
        if ox != 0.0 || blur > 0.25 {
            return false;
        }
        let ch = |v: f32| (v * alpha).round().clamp(0.0, 255.0) as u32;
        let a24 = ((alpha * 255.0).round().clamp(0.0, 255.0) as u32) << 24;
        // the edge light is the same along the whole row here
        let hl = light(xr, y);
        let col0 = x0 - shot.ox;
        let last = shot.w as i32 - 1;
        let hmax = shot.h as i32 - 1;
        if oy == 0.0 {
            // nothing bends it: the pixel behind is copied as it is
            let ri = (y - shot.oy as f32).floor().clamp(0.0, shot.h as f32 - 1.0) as usize;
            let row = &shot.px[ri * shot.w..(ri + 1) * shot.w];
            for (k, o) in out[a..b].iter_mut().enumerate() {
                let p = row[(col0 + (a + k) as i32).clamp(0, last) as usize];
                let v = a24 | (ch(((p >> 16) & 255) as f32) << 16) | (ch(((p >> 8) & 255) as f32) << 8) | ch((p & 255) as f32);
                *o = if hl > 0.0 { lift(v, hl) } else { v };
            }
        } else {
            // pulled straight up or down: the same two rows of the screen for the whole stretch,
            // one pair for each of red, green and blue
            let src = |o: f32| -> (usize, usize, f32) {
                let gy = y + o - shot.oy as f32 - 0.5;
                let fl = gy.floor();
                ((fl as i32).clamp(0, hmax) as usize, (fl as i32 + 1).clamp(0, hmax) as usize, gy - fl)
            };
            let chans = [(src(oy), 16u32), (src(oy * (1.0 - t.fringe)), 8), (src(oy * (1.0 - 2.0 * t.fringe)), 0)];
            for (k, o) in out[a..b].iter_mut().enumerate() {
                let xi = (col0 + (a + k) as i32).clamp(0, last) as usize;
                let mut v = a24;
                for ((r0, r1, ty), sft) in chans {
                    let lo = ((shot.px[r0 * shot.w + xi] >> sft) & 255) as f32;
                    let hi = ((shot.px[r1 * shot.w + xi] >> sft) & 255) as f32;
                    v |= ch(lo + (hi - lo) * ty) << sft;
                }
                *o = if hl > 0.0 { lift(v, hl) } else { v };
            }
        }
        true
    };
    // The deep part of a blurred middle: the picture behind, softened by four taps at +-b. There
    // nothing bends it and the glass is the same along a row, so the four taps come out of two cheap
    // passes (along, then down) in place of four full samples for every pixel. The result is the
    // same as the per-pixel way (to rounding); the pixels are written here and skipped later.
    let mut deep_spans: Vec<(usize, usize)> = Vec::new();
    if cell == 1 && st == 1 && mid && t.mid_blur * sc > 0.25 && w > 8 {
        let bl = t.mid_blur * sc;
        let dmin = (if t.mid_warp > 0.0 { (width * 1.6).max(w2) } else { width * 1.6 }) + 2.0;
        let mut spans = vec![(0usize, 0usize); h];
        let mut alphas = vec![0.0_f32; h];
        let (mut ca, mut cb2) = (usize::MAX, 0usize);
        let (mut ra, mut rb) = (usize::MAX, 0usize);
        for j in 0..h {
            let y = (y0 + j as i32) as f32 + 0.5;
            let db = sh.bottom - y;
            if db < dmin {
                continue;
            }
            let (xa, xb) = if y > cb {
                let sv = r - dmin;
                let dy = y - cb;
                if sv <= 0.0 || dy >= sv {
                    continue;
                }
                let sx = (sv * sv - dy * dy).sqrt();
                (cl - sx, cr + sx)
            } else {
                (sh.left + dmin, sh.right - dmin)
            };
            let a = ((xa - x0 as f32 - 0.5).ceil() + 1.0).max(0.0);
            let b = ((xb - x0 as f32 - 0.5).floor() - 1.0).min(w as f32 - 1.0);
            if b < a + 4.0 {
                continue;
            }
            let (a, b) = (a as usize, b as usize + 1);
            // the row must really look alike across the span: same glass at both ends and the middle
            let at = |i: usize| params(x0 as f32 + i as f32 + 0.5, y);
            let Some((al, ox, oy, bk)) = at((a + b) / 2) else { continue };
            let same = |p: Option<(f32, f32, f32, f32)>| {
                p.is_some_and(|(a2, x2, y2, b2)| a2 == al && x2 == 0.0 && y2 == 0.0 && (b2 - bl).abs() < 1e-3)
            };
            if ox != 0.0 || oy != 0.0 || !same(at((a + b) / 2)) || !same(at(a)) || !same(at(b - 1)) {
                continue;
            }
            spans[j] = (a, b);
            alphas[j] = al;
            ca = ca.min(a);
            cb2 = cb2.max(b);
            ra = ra.min(j);
            rb = rb.max(j);
        }
        if ra != usize::MAX && cb2 > ca {
            // two taps each way, at +b and -b, each blended between its two pixels
            let taps = |b: f32| -> [(i32, f32); 4] {
                let mut o = [(0, 0.0); 4];
                for (n, q) in [-b, b].into_iter().enumerate() {
                    let fl = q.floor();
                    let f = q - fl;
                    o[n * 2] = (fl as i32, 0.5 * (1.0 - f));
                    o[n * 2 + 1] = (fl as i32 + 1, 0.5 * f);
                }
                o
            };
            let k = taps(bl);
            let gy = |j: usize| y0 + j as i32 - shot.oy;
            let src_row = |j: usize, n: usize| (gy(j) + k[n].0).clamp(0, shot.h as i32 - 1) as usize;
            let (mut lo, mut hi) = (usize::MAX, 0usize);
            for j in ra..=rb {
                if spans[j].0 < spans[j].1 {
                    for n in 0..4 {
                        lo = lo.min(src_row(j, n));
                        hi = hi.max(src_row(j, n));
                    }
                }
            }
            let cols = cb2 - ca;
            let rows = hi - lo + 1;
            if rows * cols <= 3_000_000 {
                // along: each needed row of the picture, blurred sideways, for the columns wanted
                let mut hb = vec![[0.0_f32; 3]; rows * cols];
                let last = shot.w as i32 - 1;
                let threads = if rows * cols >= PAR_MIN / 2 { workers() } else { 1 };
                par_rows(&mut hb, cols, threads, |ri0, chunk| {
                    for (dr, row) in chunk.chunks_mut(cols).enumerate() {
                        let ri = ri0 + dr;
                        let src = &shot.px[(lo + ri) * shot.w..(lo + ri + 1) * shot.w];
                        for (ci, o) in row.iter_mut().enumerate() {
                            let gx = x0 + (ca + ci) as i32 - shot.ox;
                            let mut acc = [0.0_f32; 3];
                            for (off, wgt) in k {
                                let p = src[(gx + off).clamp(0, last) as usize];
                                acc[0] += wgt * ((p >> 16) & 255) as f32;
                                acc[1] += wgt * ((p >> 8) & 255) as f32;
                                acc[2] += wgt * (p & 255) as f32;
                            }
                            *o = acc;
                        }
                    }
                });
                // down: the four wanted rows blended, then the glass over it
                par_rows(&mut px, w, threads, |j0, chunk| {
                    for (dj, out) in chunk.chunks_mut(w).enumerate() {
                        let j = j0 + dj;
                        let (a, b) = spans[j];
                        if a >= b {
                            continue;
                        }
                        let al = alphas[j];
                        let a24 = ((al * 255.0).round().clamp(0.0, 255.0) as u32) << 24;
                        let r4: [&[[f32; 3]]; 4] = std::array::from_fn(|n| {
                            let ri = src_row(j, n) - lo;
                            &hb[ri * cols..(ri + 1) * cols]
                        });
                        for i in a..b {
                            let c = i - ca;
                            let mut acc = [0.0_f32; 3];
                            for n in 0..4 {
                                let wgt = k[n].1;
                                let v = r4[n][c];
                                acc[0] += wgt * v[0];
                                acc[1] += wgt * v[1];
                                acc[2] += wgt * v[2];
                            }
                            let ch = |v: f32| (v * al).round().clamp(0.0, 255.0) as u32;
                            out[i] = a24 | (ch(acc[0]) << 16) | (ch(acc[1]) << 8) | ch(acc[2]);
                        }
                    }
                });
                deep_spans = spans;
            }
        }
    }
    // a band of `cell` rows of the picture
    let band_fn = |bj: usize, out: &mut [u32]| {
        let rows = out.len() / w;
        let j0 = bj * cell;
        let yc = y0 as f32 + (j0 * st) as f32 + (rows * st) as f32 * 0.5;
        let up = smooth(0.0, 0.5, (yc - top) / reach);
        if up <= 0.0 && !(mid && yc >= int_top) {
            return;
        }
        let cw = w.div_ceil(cell);
        let (a_end, b_start) = if yc >= full_from { (w, w) } else { (side.min(w), w.saturating_sub(side).max(side.min(w))) };
        let mut done = (usize::MAX, 0usize);
        if cell == 1 && rows == 1 {
            if let Some(&(a, b)) = deep_spans.get(bj) {
                if a < b {
                    done = (a, b);
                }
            }
        }
        if done.0 == usize::MAX && fast && rows == 1 && yc >= full_from {
            let (a, b) = row_span(yc);
            if a < b && fill_span(yc, a, b, out) {
                done = (a, b);
            }
        }
        for ci in (0..a_end.div_ceil(cell)).chain(b_start / cell..cw) {
            let i0 = ci * cell;
            if i0 >= done.0 && i0 < done.1 {
                continue;
            }
            let cols = cell.min(w - i0);
            let xc = x0 as f32 + (i0 * st) as f32 + (cols * st) as f32 * 0.5;
            if cell == 1 || geom(xc, yc).0 < fine_d {
                for rr in 0..rows {
                    for cc in 0..cols {
                        let x = (x0 + ((i0 + cc) * st) as i32) as f32 + half;
                        let y = (y0 + ((j0 + rr) * st) as i32) as f32 + half;
                        out[rr * w + i0 + cc] = shade(x, y);
                    }
                }
            } else {
                let v = shade(xc, yc);
                if v != 0 {
                    for rr in 0..rows {
                        for cc in 0..cols {
                            out[rr * w + i0 + cc] = v;
                        }
                    }
                }
            }
        }
    };
    // a big picture is split into bands of rows worked out side by side; small ones are not worth
    // the hand-over
    let bands = h.div_ceil(cell);
    let threads = if w * h >= PAR_MIN { workers() } else { 1 };
    if threads > 1 {
        let per = bands.div_ceil(threads);
        let f = &band_fn;
        std::thread::scope(|scope| {
            for (n, chunk) in px.chunks_mut(per * cell * w).enumerate() {
                scope.spawn(move || {
                    for (jj, out) in chunk.chunks_mut(cell * w).enumerate() {
                        f(n * per + jj, out);
                    }
                });
            }
        });
    } else {
        for (bj, out) in px.chunks_mut(cell * w).enumerate() {
            band_fn(bj, out);
        }
    }
    // Frost and dimming: a light veil of white over the glass, and a little less light when the
    // picture behind the notch is bright (white text then keeps its contrast).
    let dim = if t.readable > 0.0 && t.mid > 0.0 { backdrop_dim(shot, sh) * t.readable } else { 0.0 };
    let keep = ((1.0 - dim) * 256.0).round().clamp(0.0, 256.0) as u32;
    let veil = (t.tint * 0.30 * 256.0).round().clamp(0.0, 256.0) as u32;
    if keep < 256 || veil > 0 {
        par_rows(&mut px, w, threads, |_, chunk| {
            for p in chunk.iter_mut() {
                let a = *p >> 24;
                if a == 0 {
                    continue;
                }
                let add = (a * veil) >> 8;
                let c = |sft: u32| -> u32 { (((((*p >> sft) & 255) * keep) >> 8) + add).min(255) };
                *p = (a << 24) | (c(16) << 16) | (c(8) << 8) | c(0);
            }
        });
    }
    Some(Rim { x0, y0, w, h, step: st, px })
}

/// Run `f(first_row, rows)` on pieces of `data` (rows of `width` items) side by side; one piece on
/// this thread when `threads` is 1.
fn par_rows<T: Send>(data: &mut [T], width: usize, threads: usize, f: impl Fn(usize, &mut [T]) + Sync) {
    let rows = data.len() / width.max(1);
    if threads <= 1 || rows < 2 {
        f(0, data);
        return;
    }
    let per = rows.div_ceil(threads);
    let f = &f;
    std::thread::scope(|scope| {
        for (n, chunk) in data.chunks_mut(per * width).enumerate() {
            scope.spawn(move || f(n * per, chunk));
        }
    });
}

/// How much to dim the glass for the picture behind the notch: 0 for a dark or middling one, up to
/// 0.45 for a bright one. Looks at a sparse grid of the copy over the notch.
fn backdrop_dim(shot: &Shot, sh: Shape) -> f32 {
    let (x0, x1) = ((sh.left as i32 - shot.ox).max(0), (sh.right as i32 - shot.ox).min(shot.w as i32));
    let (y0, y1) = ((sh.top as i32 - shot.oy).max(0), (sh.bottom as i32 - shot.oy).min(shot.h as i32));
    if x1 - x0 < 8 || y1 - y0 < 8 {
        return 0.0;
    }
    let (sx, sy) = (((x1 - x0) / 32).max(1) as usize, ((y1 - y0) / 16).max(1) as usize);
    let (mut sum, mut n) = (0.0_f32, 0u32);
    for y in (y0..y1).step_by(sy) {
        let row = &shot.px[y as usize * shot.w..(y as usize + 1) * shot.w];
        for x in (x0..x1).step_by(sx) {
            let p = row[x as usize];
            sum += 0.2126 * ((p >> 16) & 255) as f32 + 0.7152 * ((p >> 8) & 255) as f32 + 0.0722 * (p & 255) as f32;
            n += 1;
        }
    }
    if n == 0 {
        return 0.0;
    }
    0.45 * smooth(0.55, 0.92, sum / (n as f32 * 255.0))
}

/// Pictures smaller than this are worked out on the one thread (starting threads costs more CPU
/// than it saves); only a very big one is split up.
const PAR_MIN: usize = 400_000;

/// How many threads a big rim may use (looked up once).
fn workers() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 4))
}

/// What `bar_rim` brings back: a fingerprint of the screen around the bar and, when that changed, the new rim.
pub struct BarRim {
    pub sum: u64,
    /// Placed relative to the bar's top-left corner (device pixels).
    pub rim: Option<Rim>,
}

/// A fingerprint of the part of the copy the rim reads: the rows along the top and bottom and the columns
/// along the two ends, sampled sparsely.
fn bar_sum(s: &Shot, bw: i32, bh: i32, reach: i32) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    let mut mix = |p: u32| h = (h ^ (p & 0x00FF_FFFF) as u64).wrapping_mul(0x0000_0100_0000_01b3);
    for row in (0..s.h).step_by(2) {
        let ly = row as i32 + s.oy;
        let line = &s.px[row * s.w..(row + 1) * s.w];
        if ly < reach || ly > bh - reach {
            line.iter().step_by(3).for_each(|&p| mix(p));
        } else {
            let left = ((reach - s.ox).max(0) as usize).min(s.w);
            let right = ((bw - reach - s.ox).max(0) as usize).min(s.w);
            line[..left].iter().step_by(3).for_each(|&p| mix(p));
            line[right..].iter().step_by(3).for_each(|&p| mix(p));
        }
    }
    h
}

/// The glass edge of a rounded bar (the dock): the screen behind it is copied, and along all four edges
/// each pixel shows what lies further out, pulled in toward the edge like through thick glass, with the
/// bright line of light on the rim. `x`, `y`, `bw`, `bh` are the bar in screen pixels; the look is the
/// notch's (`Tune`), made smaller to fit a bar that is only a few dozen pixels tall. When the copy looks
/// like the one with fingerprint `prev`, no rim is made. Meant for a background thread.
pub fn bar_rim(x: i32, y: i32, bw: i32, bh: i32, radius: f32, scale: f32, t: Tune, prev: u64) -> Option<BarRim> {
    if bw < 16 || bh < 16 {
        return None;
    }
    let sc = scale.max(0.5);
    // the glass reaches in at most a third of the bar's height; a smaller reach pulls less too
    let k = ((0.36 * bh as f32) / (t.width * sc).max(1.0)).min(1.0);
    let width = t.width * sc * k;
    let pull = t.pull * sc * k.max(0.45);
    let soft = (t.soft * sc).min(width * 0.5);
    let m = (pull * (1.0 + t.corner) + 6.0).ceil() as i32;
    // the copy covers the bar and a margin, cut to the desktop
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let (gx0, gy0) = ((x - m).max(vx), (y - m).max(vy));
    let (gx1, gy1) = ((x + bw + m).min(vx + vw), (y + bh + m).min(vy + vh));
    let shot = grab_at(gx0, gy0, gx1 - gx0, gy1 - gy0, gx0 - x, gy0 - y)?;
    let sum = bar_sum(&shot, bw, bh, (width + pull + 4.0).ceil() as i32);
    if sum == prev {
        return Some(BarRim { sum, rim: None });
    }
    let (x0, y0) = (-2_i32, -2_i32);
    let (w, h) = ((bw + 4) as usize, (bh + 4) as usize);
    let mut px = vec![0u32; w * h];
    let (hx, hy) = (bw as f32 * 0.5, bh as f32 * 0.5);
    let r = radius.min(hx).min(hy).max(1.0);
    let (ix, iy) = (hx - r, hy - r);
    let hw = t.lw * sc;
    let shade = |fx: f32, fy: f32| -> u32 {
        let (dx, dy) = (fx - hx, fy - hy);
        let (qx, qy) = (dx.abs() - ix, dy.abs() - iy);
        let (ox, oy) = (qx.max(0.0), qy.max(0.0));
        let out = (ox * ox + oy * oy).sqrt();
        // distance inside the edge, the direction pointing out of the bar, and the corner weight
        let d = r - (out + qx.max(qy).min(0.0));
        if d >= width || d <= -1.0 {
            return 0;
        }
        let (nx, ny, corner) = if qx > 0.0 && qy > 0.0 {
            let n = out.max(0.001);
            (dx.signum() * ox / n, dy.signum() * oy / n, (2.0 * ox * oy / (n * n)).clamp(0.0, 1.0))
        } else if qx > qy {
            (dx.signum(), 0.0, 0.0)
        } else {
            (0.0, dy.signum(), 0.0)
        };
        let u = 1.0 - d / width;
        let fade = u * u * (1.5 - 0.5 * u);
        let base = (t.strength + 0.1 * corner).min(1.0);
        let alpha = (base + (1.0 - base) * smooth(soft, 0.0, d)) * fade * (d + 1.0).clamp(0.0, 1.0);
        if alpha < 0.004 {
            return 0;
        }
        let shift = pull * (1.0 + t.corner * corner) * curve(&t, u);
        let (sx, sy) = (nx * shift, ny * shift);
        // red, green and blue look a slightly different distance out, like light through glass
        let rc = sample(&shot, fx + sx, fy + sy, 16);
        let gc = sample(&shot, fx + sx * (1.0 - t.fringe), fy + sy * (1.0 - t.fringe), 8);
        let bc = sample(&shot, fx + sx * (1.0 - 2.0 * t.fringe), fy + sy * (1.0 - 2.0 * t.fringe), 0);
        let ch = |v: f32| (v * alpha).round().clamp(0.0, 255.0) as u32;
        let mut c =
            (((alpha * 255.0).round().clamp(0.0, 255.0) as u32) << 24) | (ch(rc) << 16) | (ch(gc) << 8) | ch(bc);
        // the light along the edge: bright where the edge faces the light, a softer glint on the far side
        if t.edge_light > 0.0 && d < hw {
            let f = 1.0 - d.max(0.0) / hw;
            let face = nx * t.lx + ny * t.ly;
            let lit = (0.2 + face.max(0.0) + 0.6 * (-face).max(0.0)).min(1.0);
            let kk = 1.0 - t.ldir + t.ldir * lit;
            let hl = (t.edge_light * kk * f * f.sqrt() * (1.0 + 0.6 * corner)).clamp(0.0, 1.0);
            let (mu, wh) = (1.0 - hl, 255.0 * hl);
            let lift = |sft: u32| -> u32 { (((c >> sft) & 255) as f32 * mu + wh).round().min(255.0) as u32 };
            c = (lift(24) << 24) | (lift(16) << 16) | (lift(8) << 8) | lift(0);
        }
        c
    };
    for row in 0..h {
        let fy = y0 as f32 + row as f32 + 0.5;
        let qy = (fy - hy).abs() - iy;
        // rows well inside the bar only have glass at the two ends
        let skip = qy <= -width && ix - width > 1.0;
        let (a, b) = if skip {
            (
                ((hx - (ix - width)) - x0 as f32).floor().max(0.0) as usize,
                ((hx + (ix - width)) - x0 as f32).ceil() as usize,
            )
        } else {
            (w, w)
        };
        let line = &mut px[row * w..(row + 1) * w];
        for col in (0..a.min(w)).chain(b.min(w)..w) {
            line[col] = shade(x0 as f32 + col as f32 + 0.5, fy);
        }
    }
    Some(BarRim { sum, rim: Some(Rim { x0, y0, w, h, step: 1, px }) })
}
