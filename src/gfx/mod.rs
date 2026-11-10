//! Rendering layer: Direct3D 11 device + Direct2D + DirectWrite + WIC.
//!
//! `Gfx` owns the long-lived device objects, `Res` caches device dependent
//! resources (brushes, text formats, bitmaps) across frames and `Painter`
//! is the immediate drawing API handed to the UI each frame.

pub mod icons;
pub mod image;
mod pack;

use std::collections::HashMap;
use std::mem::ManuallyDrop;

use windows::core::{Interface, Result, BOOL, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows_numerics::{Matrix3x2, Vector2};

pub use icons::Icon;
pub use image::ImageData;

use crate::util::{utf16, wide, Color, Rect};

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

pub struct Gfx {
    pub d3d: ID3D11Device,
    pub d2d_factory: ID2D1Factory1,
    pub d2d_device: ID2D1Device,
    pub dwrite: IDWriteFactory,
    pub fonts: FontNames,
}

#[derive(Clone, Debug)]
pub struct FontNames {
    pub text: Vec<u16>,
    pub display: Vec<u16>,
    pub icons: Vec<u16>,
    pub mono: Vec<u16>,
    /// Handwriting face for the unlock "Hello".
    pub script: Vec<u16>,
}

impl Gfx {
    pub fn new() -> Result<Self> {
        unsafe {
            let d3d = create_d3d_device()?;
            let opts = D2D1_FACTORY_OPTIONS { debugLevel: D2D1_DEBUG_LEVEL_NONE };
            let d2d_factory: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&opts))?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let d2d_device = d2d_factory.CreateDevice(&dxgi)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let fonts = pick_fonts(&dwrite);
            Ok(Self { d3d, d2d_factory, d2d_device, dwrite, fonts })
        }
    }

    /// Give driver scratch memory back once rendering goes idle. On laptops
    /// with integrated GPUs this memory is charged to the process.
    pub fn trim(&self) {
        unsafe {
            self.d2d_device.ClearResources(0);
            if let Ok(d) = self.d3d.cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice3>() {
                d.Trim();
            }
        }
    }

    /// Recreate the GPU devices after a device-lost error.
    pub fn recreate_devices(&mut self) -> Result<()> {
        unsafe {
            let d3d = create_d3d_device()?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            self.d2d_device = self.d2d_factory.CreateDevice(&dxgi)?;
            self.d3d = d3d;
        }
        Ok(())
    }
}

unsafe fn create_d3d_device() -> Result<ID3D11Device> {
    let levels = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
        D3D_FEATURE_LEVEL_9_3,
    ];
    let mut dev = None;
    let hr = D3D11CreateDevice(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&mut dev),
        None,
        None,
    );
    if hr.is_err() || dev.is_none() {
        // Fall back to the WARP software rasterizer (VMs, broken drivers).
        dev = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            None,
        )?;
    }
    Ok(dev.unwrap())
}

fn pick_fonts(dwrite: &IDWriteFactory) -> FontNames {
    let exists = |name: &str| -> bool {
        unsafe {
            let mut coll = None;
            if dwrite.GetSystemFontCollection(&mut coll, false).is_err() {
                return false;
            }
            let Some(coll) = coll else { return false };
            let mut idx = 0u32;
            let mut found = BOOL(0);
            let w = wide(name);
            coll.FindFamilyName(PCWSTR(w.as_ptr()), &mut idx, &mut found).is_ok() && found.as_bool()
        }
    };
    let first = |cands: &[&str]| -> Vec<u16> {
        for c in cands {
            if exists(c) {
                return wide(c);
            }
        }
        wide(cands[cands.len() - 1])
    };
    FontNames {
        text: first(&["Inter", "Segoe UI Variable Text", "Segoe UI"]),
        display: first(&["Inter", "Segoe UI Variable Display", "Segoe UI"]),
        icons: first(&["Segoe Fluent Icons", "Segoe MDL2 Assets"]),
        mono: first(&["Cascadia Mono", "Consolas"]),
        script: first(&["Segoe Script", "Ink Free", "Lucida Handwriting", "Brush Script MT", "Segoe UI"]),
    }
}

// ---------------------------------------------------------------------------
// Text styles
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Font {
    Text,
    Display,
    #[allow(dead_code)]
    Icons,
    Mono,
    Script,
    /// The hello's font: `[greeting] font` when set (see `set_hello_font`).
    Hello,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug)]
pub struct TextStyle {
    pub size: f32,
    pub weight: u16,
    pub color: Color,
    pub font: Font,
    pub align: Align,
    pub valign: Align,
    pub wrap: bool,
}

impl TextStyle {
    pub fn new(size: f32, color: Color) -> Self {
        Self { size, weight: 400, color, font: Font::Text, align: Align::Start, valign: Align::Center, wrap: false }
    }
    pub fn weight(mut self, w: u16) -> Self {
        self.weight = w;
        self
    }
    pub fn semibold(self) -> Self {
        self.weight(600)
    }
    pub fn medium(self) -> Self {
        self.weight(500)
    }
    pub fn display(mut self) -> Self {
        self.font = Font::Display;
        self
    }
    pub fn mono(mut self) -> Self {
        self.font = Font::Mono;
        self
    }
    pub fn hello_font(mut self) -> Self {
        self.font = Font::Hello;
        self
    }
    pub fn script(mut self) -> Self {
        self.font = Font::Script;
        self
    }
    pub fn center(mut self) -> Self {
        self.align = Align::Center;
        self
    }
    pub fn right(mut self) -> Self {
        self.align = Align::End;
        self
    }
    pub fn top(mut self) -> Self {
        self.valign = Align::Start;
        self
    }
    pub fn wrap(mut self) -> Self {
        self.wrap = true;
        self
    }
    pub fn color(mut self, c: Color) -> Self {
        self.color = c;
        self
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FmtKey {
    font: Font,
    /// Hash of the chosen family for `Font::Hello` (0 otherwise), so a changed setting gets a new format.
    family: u64,
    size_x100: u32,
    weight: u16,
}

/// Cache key of a finished text layout: the text (hashed), the font and the box it was laid out in.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct LKey {
    text: u64,
    len: u32,
    fmt: FmtKey,
    w: i32,
    h: i32,
    align: u8,
    valign: u8,
    wrap: bool,
}

struct Fmt {
    format: IDWriteTextFormat,
    ellipsis: Option<IDWriteInlineObject>,
}

struct CachedBitmap {
    brush: ID2D1BitmapBrush1,
    w: u32,
    h: u32,
    last_used: u64,
}

// ---------------------------------------------------------------------------
// Resource cache (bound to the D2D device)
// ---------------------------------------------------------------------------

pub struct Res {
    dc: ID2D1DeviceContext,
    brush: ID2D1SolidColorBrush,
    formats: HashMap<FmtKey, Fmt>,
    /// Finished text layouts with (width, height, last frame used): the same text in the same
    /// box is laid out once, not on every frame.
    layouts: HashMap<LKey, (IDWriteTextLayout, f32, f32, u64)>,
    bitmaps: HashMap<u64, CachedBitmap>,
    tnum: Option<IDWriteTypography>,
    shadow: Option<ID2D1Bitmap1>,
    /// The same shadow with squircle corners (the Alcove notch's shape).
    shadow_sq: Option<ID2D1Bitmap1>,
    /// Tessellated shapes kept on the GPU (see `Painter::fill_cached`).
    realizations: std::cell::RefCell<HashMap<RKey, (ID2D1GeometryRealization, u64)>>,
    frame: u64,
    /// The fade masks and the full-strip rectangle used by `bake_band`, made once per strip height.
    band_mask: Option<(u32, Vec<ID2D1LinearGradientBrush>)>,
    band_geo: Option<((u32, u32), ID2D1Geometry)>,
    /// Glass pictures that are no longer shown, kept to be filled again instead of allocating a new
    /// one for every rebuild (see `rim_pic`).
    rim_pool: RimPool,
    /// Text whose middle lies below this height (logical units) is over the see-through glass at the
    /// bottom of the notch, and gets a thin dark outline so it stays readable (None = no outline).
    halo_y: Option<f32>,
}

/// Spare glass pictures: (width, height of the picture, the picture).
type RimPool = std::sync::Arc<std::sync::Mutex<Vec<(u32, u32, ID2D1Bitmap1)>>>;

/// Cache key for a realized shape: kind + quantized dimensions.
pub type RKey = (u8, i32, i32, i32, i32);

/// The dashes of a rounded-rectangle outline as little quads (`half` = half the line width). The
/// dash count is rounded so the pattern closes evenly instead of ending in a stub.
fn dash_quads(w: f32, h: f32, rad: f32, dash: f32, gap: f32, half: f32) -> Vec<[(f32, f32); 4]> {
    let rad = rad.min(w * 0.5).min(h * 0.5).max(0.0);
    let (a, b) = (w - 2.0 * rad, h - 2.0 * rad);
    let arc = std::f32::consts::FRAC_PI_2 * rad;
    let total = 2.0 * (a + b) + 4.0 * arc;
    if total < dash + gap {
        return Vec::new();
    }
    let n = (total / (dash + gap)).round().max(1.0) as usize;
    let step = total / n as f32;
    let segs = [(a, 0), (arc, 1), (b, 2), (arc, 3), (a, 4), (arc, 5), (b, 6), (arc, 7)];
    let at = |s: f32| -> (f32, f32) {
        let mut s = s.rem_euclid(total);
        for (len, k) in segs {
            if s <= len {
                let t = if rad > 0.0 { s / rad } else { 0.0 };
                let pi = std::f32::consts::PI;
                let on = |cx: f32, cy: f32, t: f32| (cx + rad * t.sin(), cy - rad * t.cos());
                return match k {
                    0 => (rad + s, 0.0),
                    1 => on(w - rad, rad, t),
                    2 => (w, rad + s),
                    3 => on(w - rad, h - rad, pi * 0.5 + t),
                    4 => (w - rad - s, h),
                    5 => on(rad, h - rad, pi + t),
                    6 => (0.0, h - rad - s),
                    _ => on(rad, rad, pi * 1.5 + t),
                };
            }
            s -= len;
        }
        (rad, 0.0)
    };
    (0..n)
        .map(|i| {
            let s0 = i as f32 * step + (step - dash) * 0.5;
            let (p0, p1) = (at(s0), at(s0 + dash));
            let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
            let l = (dx * dx + dy * dy).sqrt().max(1e-4);
            let (nx, ny) = (-dy / l * half, dx / l * half);
            [(p0.0 + nx, p0.1 + ny), (p1.0 + nx, p1.1 + ny), (p1.0 - nx, p1.1 - ny), (p0.0 - nx, p0.1 - ny)]
        })
        .collect()
}

/// Quantize to 1/4 DIP so nearly identical shapes share one realization.
pub fn q(v: f32) -> i32 {
    (v * 4.0).round() as i32
}

pub fn uq(v: i32) -> f32 {
    v as f32 * 0.25
}

impl Res {
    pub fn new(gfx: &Gfx) -> Result<Self> {
        unsafe {
            let dc = gfx.d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let brush = dc.CreateSolidColorBrush(&d2c(Color::white(1.0)), None)?;
            let tnum = gfx.dwrite.CreateTypography().ok();
            if let Some(t) = &tnum {
                let _ = t.AddFontFeature(DWRITE_FONT_FEATURE {
                    nameTag: DWRITE_FONT_FEATURE_TAG_TABULAR_FIGURES,
                    parameter: 1,
                });
            }
            Ok(Self {
                dc,
                brush,
                formats: HashMap::new(),
                layouts: HashMap::new(),
                bitmaps: HashMap::new(),
                tnum,
                shadow: None,
                shadow_sq: None,
                realizations: std::cell::RefCell::new(HashMap::new()),
                frame: 0,
                band_mask: None,
                band_geo: None,
                rim_pool: RimPool::default(),
                halo_y: None,
            })
        }
    }

    /// Set where the see-through glass begins for the text that is drawn next (see `halo_y`).
    pub fn set_halo(&mut self, y: Option<f32>) {
        self.halo_y = y;
    }

    /// Drop one cached picture (its id is not used again).
    pub fn forget(&mut self, id: u64) {
        self.bitmaps.remove(&id);
    }

    /// Drop cached bitmaps that are not used right now (they are re-uploaded on demand).
    pub fn trim_bitmaps(&mut self) {
        // after a while with nothing on screen, give back everything that can be rebuilt on demand
        self.layouts.clear();
        self.realizations.borrow_mut().clear();
        if let Ok(mut pool) = self.rim_pool.lock() {
            pool.clear();
        }
        let f = self.frame;
        self.bitmaps.retain(|_, b| f.saturating_sub(b.last_used) < 2);
    }

    /// Called once per frame; evicts bitmaps unused for a while.
    pub fn end_frame(&mut self) {
        self.frame += 1;
        let f = self.frame;
        if f.is_multiple_of(120) {
            self.bitmaps.retain(|_, b| f - b.last_used < 300);
            self.layouts.retain(|_, l| f - l.3 < 120);
            self.realizations.borrow_mut().retain(|_, r| f - r.1 < 300);
        }
    }

    fn format(&mut self, gfx: &Gfx, font: Font, size: f32, weight: u16) -> Option<&Fmt> {
        // degenerate sizes happen mid-animation; DirectWrite rejects them
        if !(1.0..2000.0).contains(&size) {
            return None;
        }
        let family = if font == Font::Hello { hello_font_hash() } else { 0 };
        let key = FmtKey { font, family, size_x100: (size * 100.0) as u32, weight };
        if let std::collections::hash_map::Entry::Vacant(e) = self.formats.entry(key) {
            let fmt = unsafe { Self::create_format(gfx, font, size, weight) }?;
            e.insert(fmt);
        }
        self.formats.get(&key)
    }

    unsafe fn create_format(gfx: &Gfx, font: Font, size: f32, weight: u16) -> Option<Fmt> {
        {
            let hello;
            let family = match font {
                Font::Text => &gfx.fonts.text,
                Font::Display => &gfx.fonts.display,
                Font::Icons => &gfx.fonts.icons,
                Font::Mono => &gfx.fonts.mono,
                Font::Script => &gfx.fonts.script,
                Font::Hello => {
                    hello = HELLO_FONT.lock().map(|g| g.clone()).unwrap_or_default();
                    if hello.is_empty() {
                        &gfx.fonts.script
                    } else {
                        &hello
                    }
                }
            };
            let format = gfx
                .dwrite
                .CreateTextFormat(
                    PCWSTR(family.as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT(weight as i32),
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    windows::core::w!("en-us"),
                )
                .ok()?;
            let ellipsis = gfx.dwrite.CreateEllipsisTrimmingSign(&format).ok();
            Some(Fmt { format, ellipsis })
        }
    }
}

static HELLO_FONT: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());

/// Choose the family `Font::Hello` text is drawn with.
pub fn set_hello_font(name: &str) {
    let w = wide(name);
    if let Ok(mut g) = HELLO_FONT.lock() {
        if *g != w {
            *g = w;
        }
    }
}

fn hello_font_hash() -> u64 {
    HELLO_FONT.lock().map(|g| g.iter().fold(7u64, |h, c| h.wrapping_mul(31).wrapping_add(*c as u64))).unwrap_or(0)
}

thread_local! {
    static FAMILIES: std::cell::RefCell<HashMap<String, bool>> = std::cell::RefCell::new(HashMap::new());
}

impl Gfx {
    /// Whether a font family with this name is installed (remembered per name).
    pub fn font_installed(&self, name: &str) -> bool {
        if let Some(v) = FAMILIES.with(|m| m.borrow().get(name).copied()) {
            return v;
        }
        let found = unsafe {
            let mut coll = None;
            let mut idx = 0u32;
            let mut found = BOOL(0);
            let w = wide(name);
            self.dwrite.GetSystemFontCollection(&mut coll, false).is_ok()
                && coll.is_some_and(|c| {
                    c.FindFamilyName(PCWSTR(w.as_ptr()), &mut idx, &mut found).is_ok() && found.as_bool()
                })
        };
        FAMILIES.with(|m| m.borrow_mut().insert(name.to_string(), found));
        found
    }
}

pub fn d2c(c: Color) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: c.r, g: c.g, b: c.b, a: c.a }
}

fn rf(r: Rect) -> D2D_RECT_F {
    D2D_RECT_F { left: r.x, top: r.y, right: r.x + r.w, bottom: r.y + r.h }
}

fn v2(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
}

// ---------------------------------------------------------------------------
// Painter
// ---------------------------------------------------------------------------

pub struct TextLayout {
    pub layout: IDWriteTextLayout,
    pub width: f32,
    pub height: f32,
}

impl TextLayout {
    /// Caret slot right after UTF-16 index `pos - 1` (the end of the text
    /// when `pos` is its length): (x, line top, line height), layout-relative.
    pub fn caret(&self, pos: u32) -> (f32, f32, f32) {
        let (mut x, mut y) = (0.0, 0.0);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            let _ = self.layout.HitTestTextPosition(pos.saturating_sub(1), pos > 0, &mut x, &mut y, &mut m);
        }
        (x, m.top, m.height)
    }
}

#[allow(dead_code)]
pub struct Painter<'a> {
    pub dc: ID2D1DeviceContext,
    pub gfx: &'a Gfx,
    res: &'a mut Res,
    /// Global opacity multiplier for everything drawn.
    pub alpha: f32,
    /// Device scale (pixels per logical unit) — used for pixel snapping.
    pub scale: f32,
    base: Matrix3x2,
    local: Matrix3x2,
    dc1: Option<ID2D1DeviceContext1>,
}

impl<'a> Painter<'a> {
    pub fn new(dc: ID2D1DeviceContext, gfx: &'a Gfx, res: &'a mut Res, scale: f32, ox: f32, oy: f32) -> Self {
        let base = Matrix3x2 { M11: scale, M12: 0.0, M21: 0.0, M22: scale, M31: ox, M32: oy };
        unsafe {
            dc.SetTransform(&base);
            dc.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        }
        let dc1 = dc.cast::<ID2D1DeviceContext1>().ok();
        Self { dc, gfx, res, alpha: 1.0, scale, base, local: Matrix3x2::identity(), dc1 }
    }

    #[allow(dead_code)]
    pub fn frame_number(&self) -> u64 {
        self.res.frame
    }

    pub fn clear(&self) {
        unsafe { self.dc.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 })) }
    }

    /// Set an extra local transform: uniform scale `s` around (cx, cy) then translate (dx, dy).
    pub fn set_local(&mut self, s: f32, cx: f32, cy: f32, dx: f32, dy: f32) {
        self.local = Matrix3x2 { M11: s, M12: 0.0, M21: 0.0, M22: s, M31: cx - s * cx + dx, M32: cy - s * cy + dy };
        let m = self.local * self.base;
        unsafe { self.dc.SetTransform(&m) }
    }

    /// Squash everything drawn next vertically around the line `py` (a flap
    /// folding). Returns the previous transform for `pop_local`; it stacks on
    /// top of any transform set with `set_local`.
    pub fn push_scale_y(&mut self, sy: f32, py: f32) -> Matrix3x2 {
        let prev = self.local;
        let sy = sy.max(0.001);
        let m = Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: sy, M31: 0.0, M32: py - sy * py };
        self.local = m * prev;
        let t = self.local * self.base;
        unsafe { self.dc.SetTransform(&t) }
        prev
    }

    pub fn pop_local(&mut self, prev: Matrix3x2) {
        self.local = prev;
        let t = prev * self.base;
        unsafe { self.dc.SetTransform(&t) }
    }

    pub fn reset_local(&mut self) {
        self.local = Matrix3x2::identity();
        unsafe { self.dc.SetTransform(&self.base) }
    }

    fn brush(&self, c: Color) -> &ID2D1SolidColorBrush {
        unsafe { self.res.brush.SetColor(&d2c(c.alpha(self.alpha))) };
        &self.res.brush
    }

    pub fn push_clip(&self, r: Rect) {
        unsafe { self.dc.PushAxisAlignedClip(&rf(r), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) }
    }

    pub fn pop_clip(&self) {
        unsafe { self.dc.PopAxisAlignedClip() }
    }

    pub fn fill_rect(&self, r: Rect, c: Color) {
        if c.a * self.alpha <= 0.001 {
            return;
        }
        unsafe { self.dc.FillRectangle(&rf(r), self.brush(c)) }
    }

    /// Rounded rect drawn straight on the context, with no cached geometry. For shapes whose size
    /// changes every frame (a growing bar, a shimmer), where caching one entry per size would only
    /// churn memory.
    pub fn fill_rounded_live(&self, r: Rect, radius: f32, c: Color) {
        if c.a * self.alpha <= 0.001 || r.w <= 0.0 || r.h <= 0.0 {
            return;
        }
        let rad = radius.min(r.w * 0.5).min(r.h * 0.5);
        let rr = D2D1_ROUNDED_RECT { rect: rf(r), radiusX: rad, radiusY: rad };
        unsafe { self.dc.FillRoundedRectangle(&rr, self.brush(c)) }
    }

    /// A polygon filled with a horizontal gradient running from `x0` to `x1`. `stops` are
    /// (position 0..1, color). For soft, gradient-shaded glows whose outline changes every frame.
    pub fn fill_gradient_poly(&self, pts: &[(f32, f32)], stops: &[(f32, Color)], x0: f32, x1: f32) {
        if pts.len() < 3 || stops.len() < 2 || x1 - x0 < 0.5 {
            return;
        }
        let Some(g) = self.path(|b| {
            b.move_to(pts[0].0, pts[0].1);
            for p in &pts[1..] {
                b.line_to(p.0, p.1);
            }
        }) else {
            return;
        };
        let mut buf = [D2D1_GRADIENT_STOP::default(); 8];
        let n = stops.len().min(buf.len());
        for (slot, (pos, c)) in buf.iter_mut().zip(stops) {
            *slot = D2D1_GRADIENT_STOP { position: *pos, color: d2c(c.alpha(self.alpha)) };
        }
        let stops = &buf[..n];
        let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: v2(x0, 0.0), endPoint: v2(x1, 0.0) };
        unsafe {
            let Ok(coll) = ID2D1RenderTarget::CreateGradientStopCollection(
                &self.dc,
                stops,
                D2D1_GAMMA_2_2,
                D2D1_EXTEND_MODE_CLAMP,
            ) else {
                return;
            };
            let Ok(brush) = self.dc.CreateLinearGradientBrush(&props, None, &coll) else { return };
            self.dc.FillGeometry(&g, &brush, None);
        }
    }

    /// A rounded rectangle filled with a radial gradient centred on (`cx`, `cy`) and reaching `spread`
    /// pixels out; `stops` are (position 0..1, color), at most six. Made for short-lived light (the press
    /// glow), so nothing is cached.
    pub fn glow_rounded(&self, r: Rect, radius: f32, cx: f32, cy: f32, spread: f32, stops: &[(f32, Color)]) {
        if stops.len() < 2 || spread < 0.5 || r.w <= 0.0 || r.h <= 0.0 {
            return;
        }
        let mut buf = [D2D1_GRADIENT_STOP::default(); 6];
        let n = stops.len().min(buf.len());
        for (slot, (pos, c)) in buf.iter_mut().zip(stops) {
            *slot = D2D1_GRADIENT_STOP { position: *pos, color: d2c(c.alpha(self.alpha)) };
        }
        let rad = radius.min(r.w * 0.5).min(r.h * 0.5);
        let rr = D2D1_ROUNDED_RECT { rect: rf(r), radiusX: rad, radiusY: rad };
        let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
            center: v2(cx, cy),
            gradientOriginOffset: v2(0.0, 0.0),
            radiusX: spread,
            radiusY: spread,
        };
        unsafe {
            let Ok(coll) = ID2D1RenderTarget::CreateGradientStopCollection(
                &self.dc,
                &buf[..n],
                D2D1_GAMMA_2_2,
                D2D1_EXTEND_MODE_CLAMP,
            ) else {
                return;
            };
            let Ok(brush) = self.dc.CreateRadialGradientBrush(&props, None, &coll) else { return };
            self.dc.FillRoundedRectangle(&rr, &brush);
        }
    }

    /// A shape filled with a vertical gradient from `y0` to `y1`; `stops` are (position 0..1, color).
    pub fn fill_geometry_vgrad(&self, g: &ID2D1Geometry, y0: f32, y1: f32, stops: &[(f32, Color)]) {
        if stops.len() < 2 || y1 - y0 < 0.5 {
            return;
        }
        let mut buf = [D2D1_GRADIENT_STOP::default(); 8];
        let n = stops.len().min(buf.len());
        for (slot, (pos, c)) in buf.iter_mut().zip(stops) {
            *slot = D2D1_GRADIENT_STOP { position: *pos, color: d2c(c.alpha(self.alpha)) };
        }
        let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: v2(0.0, y0), endPoint: v2(0.0, y1) };
        unsafe {
            let Ok(coll) = ID2D1RenderTarget::CreateGradientStopCollection(
                &self.dc,
                &buf[..n],
                D2D1_GAMMA_2_2,
                D2D1_EXTEND_MODE_CLAMP,
            ) else {
                return;
            };
            let Ok(brush) = self.dc.CreateLinearGradientBrush(&props, None, &coll) else { return };
            self.dc.FillGeometry(g, &brush, None);
        }
    }

    pub fn fill_rounded(&self, r: Rect, radius: f32, c: Color) {
        if c.a * self.alpha <= 0.001 || r.w <= 0.0 || r.h <= 0.0 {
            return;
        }
        let rad = radius.min(r.w * 0.5).min(r.h * 0.5);
        if rad <= 0.01 {
            self.fill_rect(r, c);
            return;
        }
        let key = (1, q(r.w), q(r.h), q(rad), 0);
        self.fill_cached(key, r.x, r.y, c, |b| {
            b.rounded_rect(Rect::new(0.0, 0.0, uq(key.1), uq(key.2)), uq(key.3), true)
        });
    }

    /// Fill a shape that is built at the origin and drawn at (x, y).
    ///
    /// The tessellation is cached on the GPU as an ID2D1GeometryRealization
    /// keyed by `key`. Re-tessellating and streaming every rounded shape each
    /// frame makes some drivers (e.g. on ARM laptops) grow their dynamic
    /// upload heap by ~64 MB; realized geometry is uploaded once.
    pub fn fill_cached(&self, key: RKey, x: f32, y: f32, c: Color, build: impl FnOnce(&PathBuilder)) {
        if c.a * self.alpha <= 0.001 {
            return;
        }
        let at = Matrix3x2::translation(x, y) * self.local * self.base;
        let restore = self.local * self.base;
        let Some(dc1) = &self.dc1 else {
            // pre-8.1 fallback: plain path fill
            if let Some(g) = self.path(build) {
                unsafe {
                    self.dc.SetTransform(&at);
                    self.dc.FillGeometry(&g, self.brush(c), None);
                    self.dc.SetTransform(&restore);
                }
            }
            return;
        };
        let frame = self.res.frame;
        let mut cache = self.res.realizations.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(key) {
            let Some(g) = self.path(build) else { return };
            let tol = D2D1_DEFAULT_FLATTENING_TOLERANCE / self.scale.max(1.0);
            let Ok(real) = (unsafe { dc1.CreateFilledGeometryRealization(&g, tol) }) else { return };
            e.insert((real, frame));
        }
        let entry = cache.get_mut(&key).unwrap();
        entry.1 = frame;
        unsafe {
            self.dc.SetTransform(&at);
            dc1.DrawGeometryRealization(&entry.0, self.brush(c));
            self.dc.SetTransform(&restore);
        }
    }

    // Outlines, lines and arcs are built as *filled* paths on purpose: on
    // some GPU drivers Direct2D's stroke pipeline allocates ~65 MB of scratch
    // memory on first use, while fills stay at a few MB.

    /// Rounded-rect outline of `width`, centered on the edge of `r`.
    pub fn stroke_rounded(&self, r: Rect, radius: f32, width: f32, c: Color) {
        if c.a * self.alpha <= 0.001 || r.w <= 0.0 || r.h <= 0.0 {
            return;
        }
        let rad = radius.min(r.w * 0.5).min(r.h * 0.5);
        let key = (2, q(r.w), q(r.h), q(rad), q(width));
        self.fill_cached(key, r.x, r.y, c, |b| {
            let (w, ht, rad, h) = (uq(key.1), uq(key.2), uq(key.3), uq(key.4) * 0.5);
            let local = Rect::new(0.0, 0.0, w, ht);
            b.rounded_rect(local.expand(h), rad + h, true);
            let inner = local.inset(h);
            if inner.w > 0.0 && inner.h > 0.0 {
                b.rounded_rect(inner, (rad - h).max(0.0), false);
            }
        });
    }

    pub fn fill_circle(&self, cx: f32, cy: f32, radius: f32, c: Color) {
        if c.a * self.alpha <= 0.001 {
            return;
        }
        let key = (5, q(radius), 0, 0, 0);
        self.fill_cached(key, cx, cy, c, |b| b.circle(0.0, 0.0, uq(key.1), true));
    }

    /// `arc` for arcs that are drawn every frame (a badge's corner highlights): the outline is built
    /// once per size and angle (angles to 1/256 rad) and later frames only move and tint it.
    #[allow(clippy::too_many_arguments)]
    pub fn arc_cached(&self, cx: f32, cy: f32, radius: f32, start: f32, sweep: f32, width: f32, c: Color) {
        if sweep.abs() < 0.001 || c.a * self.alpha <= 0.001 {
            return;
        }
        if sweep.abs() >= std::f32::consts::TAU - 0.001 {
            self.stroke_circle(cx, cy, radius, width, c);
            return;
        }
        let (start, sweep) = if sweep < 0.0 { (start + sweep, -sweep) } else { (start, sweep) };
        let ang = |v: f32| (v * 256.0).round() as i32;
        let key = (31, q(radius), ang(start), ang(sweep), q(width));
        self.fill_cached(key, cx, cy, c, |b| {
            b.arc_stroke(0.0, 0.0, uq(key.1), key.2 as f32 / 256.0, key.3 as f32 / 256.0, uq(key.4))
        });
    }

    /// A dashed outline in the style of a drop zone: short straight dashes along a rounded rectangle.
    /// Built once per size (all dashes in one cached shape), so a frame costs a single draw.
    pub fn dashed_rounded(&self, r: Rect, radius: f32, width: f32, c: Color) {
        if c.a * self.alpha <= 0.001 || r.w <= 8.0 || r.h <= 8.0 {
            return;
        }
        let rad = radius.min(r.w * 0.5).min(r.h * 0.5);
        let key = (32, q(r.w), q(r.h), q(rad), q(width));
        self.fill_cached(key, r.x, r.y, c, |b| {
            for d in dash_quads(uq(key.1), uq(key.2), uq(key.3), 7.0, 6.0, uq(key.4) * 0.5) {
                b.move_to(d[0].0, d[0].1);
                b.line_to(d[1].0, d[1].1);
                b.line_to(d[2].0, d[2].1);
                b.line_to(d[3].0, d[3].1);
                b.close();
            }
        });
    }

    /// Ring of `width` centered on a circle of `radius`.
    pub fn stroke_circle(&self, cx: f32, cy: f32, radius: f32, width: f32, c: Color) {
        if c.a * self.alpha <= 0.001 {
            return;
        }
        let key = (3, q(radius), q(width), 0, 0);
        self.fill_cached(key, cx, cy, c, |b| {
            let (r, h) = (uq(key.1), uq(key.2) * 0.5);
            b.circle(0.0, 0.0, r + h, true);
            if r > h {
                b.circle(0.0, 0.0, r - h, false);
            }
        });
    }

    /// Line with round caps (a filled capsule).
    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {
        if c.a * self.alpha <= 0.001 {
            return;
        }
        let h = width * 0.5;
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 0.01 {
            self.fill_circle(x0, y0, h, c);
            return;
        }
        let key = (4, q(dx), q(dy), q(width), 0);
        self.fill_cached(key, x0, y0, c, |b| {
            let (dx, dy, h) = (uq(key.1), uq(key.2), uq(key.3) * 0.5);
            let len = (dx * dx + dy * dy).sqrt().max(0.001);
            let (nx, ny) = (-dy / len * h, dx / len * h);
            b.move_to(nx, ny);
            b.line_to(dx + nx, dy + ny);
            b.arc_to(dx - nx, dy - ny, h, false);
            b.line_to(-nx, -ny);
            b.arc_to(nx, ny, h, false);
            b.close();
        });
    }

    pub fn fill_geometry(&self, g: &ID2D1Geometry, c: Color) {
        unsafe { self.dc.FillGeometry(g, self.brush(c), None) }
    }

    /// Circular arc with round caps from `start` (radians, 0 = 12 o'clock,
    /// clockwise) spanning `sweep`.
    #[allow(clippy::too_many_arguments)]
    pub fn arc(&self, cx: f32, cy: f32, radius: f32, start: f32, sweep: f32, width: f32, c: Color) {
        if sweep.abs() < 0.001 || c.a * self.alpha <= 0.001 {
            return;
        }
        if sweep.abs() >= std::f32::consts::TAU - 0.001 {
            self.stroke_circle(cx, cy, radius, width, c);
            return;
        }
        let (start, sweep) = if sweep < 0.0 { (start + sweep, -sweep) } else { (start, sweep) };
        if let Some(g) = self.path(|b| b.arc_stroke(cx, cy, radius, start, sweep, width)) {
            self.fill_geometry(&g, c);
        }
    }

    /// Path builder using the shared factory.
    pub fn path(&self, build: impl FnOnce(&PathBuilder)) -> Option<ID2D1Geometry> {
        unsafe {
            let path = self.gfx.d2d_factory.CreatePathGeometry().ok()?;
            let sink = path.Open().ok()?;
            sink.SetFillMode(D2D1_FILL_MODE_WINDING);
            let b = PathBuilder { sink: sink.clone(), open: std::cell::Cell::new(false) };
            build(&b);
            b.finish();
            sink.Close().ok()?;
            path.cast().ok()
        }
    }

    // -- text ---------------------------------------------------------------

    pub fn layout(&mut self, s: &str, style: &TextStyle, max_w: f32, max_h: f32) -> Option<TextLayout> {
        let gfx = self.gfx;
        if !(max_w.is_finite() && max_h.is_finite()) {
            return None;
        }
        // the same text in the same box and font: reuse the finished layout
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            s.hash(&mut h);
            LKey {
                text: h.finish(),
                len: s.len() as u32,
                fmt: FmtKey {
                    font: style.font,
                    family: if style.font == Font::Hello { hello_font_hash() } else { 0 },
                    size_x100: (style.size * 100.0) as u32,
                    weight: style.weight,
                },
                w: q(max_w.max(1.0)),
                h: q(max_h.max(1.0)),
                align: style.align as u8,
                valign: style.valign as u8,
                wrap: style.wrap,
            }
        };
        let frame = self.res.frame;
        if let Some(e) = self.res.layouts.get_mut(&key) {
            e.3 = frame;
            return Some(TextLayout { layout: e.0.clone(), width: e.1, height: e.2 });
        }
        let fmt = self.res.format(gfx, style.font, style.size, style.weight)?;
        let text = utf16(s);
        unsafe {
            let layout = gfx.dwrite.CreateTextLayout(&text, &fmt.format, max_w.max(1.0), max_h.max(1.0)).ok()?;
            let _ = layout.SetTextAlignment(match style.align {
                Align::Start => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            let _ = layout.SetParagraphAlignment(match style.valign {
                Align::Start => DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
                Align::Center => DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
                Align::End => DWRITE_PARAGRAPH_ALIGNMENT_FAR,
            });
            if !style.wrap {
                let _ = layout.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
                if let Some(e) = &fmt.ellipsis {
                    let t = DWRITE_TRIMMING {
                        granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                        delimiter: 0,
                        delimiterCount: 0,
                    };
                    let _ = layout.SetTrimming(&t, e);
                }
            } else {
                let _ = layout.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
            }
            if matches!(style.font, Font::Display | Font::Mono) {
                if let Some(t) = &self.res.tnum {
                    let _ = layout.SetTypography(t, DWRITE_TEXT_RANGE { startPosition: 0, length: text.len() as u32 });
                }
            }
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut m);
            let (width, height) = (m.widthIncludingTrailingWhitespace, m.height);
            // animations produce a new box every frame: keep the cache from growing without bound
            if self.res.layouts.len() >= 300 {
                self.res.layouts.retain(|_, l| frame.saturating_sub(l.3) < 30);
                if self.res.layouts.len() >= 300 {
                    self.res.layouts.clear();
                }
            }
            self.res.layouts.insert(key, (layout.clone(), width, height, frame));
            Some(TextLayout { layout, width, height })
        }
    }

    pub fn draw_layout(&self, l: &TextLayout, x: f32, y: f32, c: Color) {
        unsafe { self.dc.DrawTextLayout(v2(x, y), &l.layout, self.brush(c), D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT) }
    }

    /// Draw text inside `r` (single line with ellipsis unless style.wrap).
    pub fn text(&mut self, s: &str, r: Rect, style: &TextStyle) {
        if s.is_empty() || style.color.a * self.alpha <= 0.001 {
            return;
        }
        if let Some(l) = self.layout(s, style, r.w, r.h) {
            // over the glass the screen behind can be any colour: a thin dark outline keeps the text
            // readable on light and busy backgrounds alike
            if self.res.halo_y.is_some_and(|y| r.y + r.h * 0.5 > y) {
                let halo = Color::black(0.6 * style.color.a);
                let d = 0.9;
                for (dx, dy) in [(-d, 0.0), (d, 0.0), (0.0, -d), (0.0, d)] {
                    self.draw_layout(&l, r.x + dx, r.y + dy, halo);
                }
            }
            self.draw_layout(&l, r.x, r.y, style.color);
        }
    }

    /// Text in `style.color` with a soft band of `hi` centred on x = `cx`,
    /// fading out over `half` on each side (the loading-label shimmer).
    pub fn text_shimmer(&mut self, s: &str, r: Rect, style: &TextStyle, hi: Color, cx: f32, half: f32) {
        if s.is_empty() || half <= 0.0 {
            return;
        }
        let Some(l) = self.layout(s, style, r.w, r.h) else { return };
        let base = d2c(style.color.alpha(self.alpha));
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: base },
            D2D1_GRADIENT_STOP { position: 0.5, color: d2c(hi.alpha(self.alpha)) },
            D2D1_GRADIENT_STOP { position: 1.0, color: base },
        ];
        let props =
            D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: v2(cx - half, 0.0), endPoint: v2(cx + half, 0.0) };
        unsafe {
            let Ok(coll) = ID2D1RenderTarget::CreateGradientStopCollection(
                &self.dc,
                &stops,
                D2D1_GAMMA_2_2,
                D2D1_EXTEND_MODE_CLAMP,
            ) else {
                return;
            };
            let Ok(brush) = self.dc.CreateLinearGradientBrush(&props, None, &coll) else { return };
            self.dc.DrawTextLayout(v2(r.x, r.y), &l.layout, &brush, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT);
        }
    }

    /// Width/height of a single line of text.
    pub fn measure(&mut self, s: &str, style: &TextStyle) -> (f32, f32) {
        let st = TextStyle { wrap: false, ..*style };
        match self.layout(s, &st, 10_000.0, 1_000.0) {
            Some(l) => (l.width, l.height),
            None => (0.0, 0.0),
        }
    }

    /// Height of wrapped text in the given width.
    pub fn measure_wrapped(&mut self, s: &str, style: &TextStyle, w: f32) -> f32 {
        let st = TextStyle { wrap: true, valign: Align::Start, ..*style };
        self.layout(s, &st, w, 100_000.0).map(|l| l.height).unwrap_or(0.0)
    }

    /// Draw a glyph from the system icon font, centered in `r`.
    #[allow(dead_code)]
    pub fn glyph(&mut self, ch: char, r: Rect, size: f32, c: Color) {
        let st = TextStyle {
            size,
            weight: 400,
            color: c,
            font: Font::Icons,
            align: Align::Center,
            valign: Align::Center,
            wrap: false,
        };
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        if let Some(l) = self.layout(s, &st, r.w.max(size * 2.0), r.h.max(size * 2.0)) {
            let x = r.cx() - r.w.max(size * 2.0) * 0.5;
            let y = r.cy() - r.h.max(size * 2.0) * 0.5;
            self.draw_layout(&l, x, y, c);
        }
    }

    pub fn icon(&mut self, icon: Icon, r: Rect, c: Color) {
        icons::draw(self, icon, r, c);
    }

    // -- notch shadow -------------------------------------------------------

    /// A soft shadow that follows the notch's own outline: a stack of many thin rings, each the notch
    /// grown a little more, with the notch itself cut out (so the shadow never darkens the glass and
    /// there is no gap). The rings' opacities add up to a smooth Gaussian-like falloff, and the whole
    /// shadow fades to nothing toward the top edge so it has no hard corners there.
    pub fn notch_shadow(&self, body: Rect, radius: f32, ear: f32, opacity: f32) {
        if opacity <= 0.004 || body.w <= 4.0 || body.h <= 4.0 {
            return;
        }
        let (x0, x1) = (body.x, body.right());
        let h = body.h.max(1.0);
        let r0 = radius.min(body.w * 0.5).min(h - ear.min(h * 0.5)).max(0.0);
        let e = ear.min(h * 0.5).max(0.0);
        let alcove = crate::ui::look_alcove() && r0 * 2.0 < body.w - 0.5;
        let (ex, ey, tp, cn, en) =
            if alcove { (e, (e * EAR_TALL).min(h * 0.5), wall_taper(h), CORNER_N, EAR_N) } else { (e, e, 0.0, 2.0, 2.0) };
        let r = if alcove { r0.min(h - ey).max(0.0) } else { r0 };
        // the notch itself (the same 20 steps as `notch_path`), walked backwards so it cuts a hole out of each ring
        let hole: Vec<(f32, f32)> = notch_outline(x0, x1, h, r, ex, ey, tp, cn, en, 20).into_iter().rev().collect();
        const LAYERS: usize = 24;
        const SPREAD: f32 = 28.0;
        let sigma = SPREAD / 2.0;
        // how dark the shadow is in ring `k` (1 = next to the notch); nothing beyond the last ring
        let target = |k: usize| -> f32 {
            if k > LAYERS {
                return 0.0;
            }
            let d = SPREAD * (k as f32 - 0.5) / LAYERS as f32;
            opacity * (-0.5 * (d / sigma).powi(2)).exp()
        };
        // toward the top edge the shadow thins out to nothing
        let ramp = (h * 0.45).clamp(12.0, 70.0);
        for k in 1..=LAYERS {
            let a = 1.0 - (1.0 - target(k)) / (1.0 - target(k + 1));
            if a <= 0.0015 {
                continue;
            }
            let d = SPREAD * k as f32 / LAYERS as f32;
            // a little more room below than at the sides, like a light from above
            let outer = notch_outline(x0 - d, x1 + d, h + d * 1.2, r + d, ex, ey, tp, cn, en, 8);
            let Some(g) = self.path(|b| {
                for pts in [&outer, &hole] {
                    let mut it = pts.iter();
                    if let Some(&(x, y)) = it.next() {
                        b.move_to(x, y);
                        for &(x, y) in it {
                            b.line_to(x, y);
                        }
                    }
                }
            }) else {
                continue;
            };
            self.fill_geometry_vgrad(&g, 0.0, ramp, &[(0.0, Color::black(0.0)), (1.0, Color::black(a))]);
        }
    }

    // -- shadow -------------------------------------------------------------

    /// Soft outer shadow around a rounded rect, drawn as a 9-slice of one
    /// small precomputed bitmap (cheap, and no per-size geometry caches).
    pub fn shadow(&mut self, r: Rect, radius: f32, opacity: f32) {
        if opacity * self.alpha <= 0.002 || r.w <= 1.0 || r.h <= 1.0 {
            return;
        }
        // Alcove: squircle corners, so the shadow follows the notch's own corners
        let sq = crate::ui::look_alcove();
        if sq && self.res.shadow_sq.is_none() {
            self.res.shadow_sq = make_shadow_bitmap(&self.res.dc, 3.1);
        }
        if !sq && self.res.shadow.is_none() {
            self.res.shadow = make_shadow_bitmap(&self.res.dc, 2.0);
        }
        let Some(bmp) = (if sq { &self.res.shadow_sq } else { &self.res.shadow }) else { return };
        let src_corner = (SHADOW_R + SHADOW_B) * SHADOW_SCALE;
        let size = 2.0 * src_corner + 2.0;
        // destination corner scales with the current corner radius
        let dst_corner = (radius.max(1.0) + SHADOW_B).min(r.w * 0.5 + SHADOW_B).min(r.h * 0.5 + SHADOW_B);
        let outer = r.expand(SHADOW_B);
        let xs_d = [outer.x, outer.x + dst_corner, outer.right() - dst_corner, outer.right()];
        let ys_d = [outer.y, outer.y + dst_corner, outer.bottom() - dst_corner, outer.bottom()];
        let xs_s = [0.0, src_corner, size - src_corner, size];
        for i in 0..3 {
            for j in 0..3 {
                let d = D2D_RECT_F { left: xs_d[i], top: ys_d[j], right: xs_d[i + 1], bottom: ys_d[j + 1] };
                if d.right <= d.left || d.bottom <= d.top {
                    continue;
                }
                let s = D2D_RECT_F { left: xs_s[i], top: xs_s[j], right: xs_s[i + 1], bottom: xs_s[j + 1] };
                unsafe {
                    self.dc.DrawBitmap(
                        bmp,
                        Some(&d),
                        opacity * self.alpha,
                        D2D1_INTERPOLATION_MODE_LINEAR,
                        Some(&s),
                        None,
                    );
                }
            }
        }
    }

    // -- images -------------------------------------------------------------

    /// Draw an image "aspect fill" into `r` with rounded corners.
    /// Drop the GPU copy of an image that will not be drawn again (live camera frames).
    pub fn forget_image(&mut self, id: u64) {
        self.res.bitmaps.remove(&id);
    }

    /// Draw an image as a circle of radius `rad` around (cx, cy), turned by `angle` radians.
    pub fn image_disc(&mut self, img: &ImageData, cx: f32, cy: f32, rad: f32, angle: f32) {
        if rad <= 0.5 {
            return;
        }
        let frame = self.res.frame;
        if !self.res.bitmaps.contains_key(&img.id) {
            let Some(cb) = self.create_bitmap(img) else { return };
            self.res.bitmaps.insert(img.id, cb);
        }
        let cb = self.res.bitmaps.get_mut(&img.id).unwrap();
        cb.last_used = frame;
        let (iw, ih) = (cb.w as f32, cb.h as f32);
        let s = (2.0 * rad / iw).max(2.0 * rad / ih);
        let (sn, cs) = (angle.sin() * s, angle.cos() * s);
        // image centre lands on (cx, cy); the whole picture turns with the record
        let m = Matrix3x2 {
            M11: cs,
            M12: sn,
            M21: -sn,
            M22: cs,
            M31: cx - cs * iw * 0.5 + sn * ih * 0.5,
            M32: cy - sn * iw * 0.5 - cs * ih * 0.5,
        };
        unsafe {
            cb.brush.SetTransform(&m);
            cb.brush.SetOpacity(self.alpha);
            let el = D2D1_ELLIPSE { point: v2(cx, cy), radiusX: rad, radiusY: rad };
            self.dc.FillEllipse(&el, &cb.brush);
        }
    }

    pub fn image(&mut self, img: &ImageData, r: Rect, radius: f32, opacity: f32) {
        self.image_zoom(img, r, radius, opacity, 1.0);
    }

    /// Like `image`, but `zoom` picks how much of the picture is shown: 1.0 fills the box (the
    /// edges are cropped), 0.0 shows the whole picture (dark bars may remain), values between mix.
    pub fn image_zoom(&mut self, img: &ImageData, r: Rect, radius: f32, opacity: f32, zoom: f32) {
        if r.w <= 0.5 || r.h <= 0.5 {
            return;
        }
        let frame = self.res.frame;
        if !self.res.bitmaps.contains_key(&img.id) {
            let Some(cb) = self.create_bitmap(img) else { return };
            self.res.bitmaps.insert(img.id, cb);
        }
        let cb = self.res.bitmaps.get_mut(&img.id).unwrap();
        cb.last_used = frame;
        let (iw, ih) = (cb.w as f32, cb.h as f32);
        // centered; zoom 1 = cover (fills r), zoom 0 = contain (the whole picture)
        let (cover, contain) = ((r.w / iw).max(r.h / ih), (r.w / iw).min(r.h / ih));
        let s = contain + (cover - contain) * zoom.clamp(0.0, 1.0);
        let ox = r.x + (r.w - iw * s) * 0.5;
        let oy = r.y + (r.h - ih * s) * 0.5;
        // only the part of the box the picture really covers is filled
        let x0 = r.x.max(ox);
        let y0 = r.y.max(oy);
        let x1 = r.right().min(ox + iw * s);
        let y1 = r.bottom().min(oy + ih * s);
        if x1 - x0 <= 0.5 || y1 - y0 <= 0.5 {
            return;
        }
        let area = Rect::new(x0, y0, x1 - x0, y1 - y0);
        let m = Matrix3x2 { M11: s, M12: 0.0, M21: 0.0, M22: s, M31: ox, M32: oy };
        unsafe {
            cb.brush.SetTransform(&m);
            cb.brush.SetOpacity(opacity * self.alpha);
            let rad = radius.min(area.w * 0.5).min(area.h * 0.5);
            let rr = D2D1_ROUNDED_RECT { rect: rf(area), radiusX: rad, radiusY: rad };
            self.dc.FillRoundedRectangle(&rr, &cb.brush);
        }
    }

    /// A picture stretched over `dest`, showing only the rounded rectangle `clip` of it. The picture stays
    /// where it is while `clip` changes size, so a growing card uncovers more of the same picture.
    pub fn image_window(&mut self, img: &ImageData, dest: Rect, clip: Rect, radius: f32, opacity: f32) {
        if dest.w <= 0.5 || dest.h <= 0.5 || clip.w <= 0.5 || clip.h <= 0.5 {
            return;
        }
        let frame = self.res.frame;
        if !self.res.bitmaps.contains_key(&img.id) {
            let Some(cb) = self.create_bitmap(img) else { return };
            self.res.bitmaps.insert(img.id, cb);
        }
        let cb = self.res.bitmaps.get_mut(&img.id).unwrap();
        cb.last_used = frame;
        let m = Matrix3x2 {
            M11: dest.w / cb.w as f32,
            M12: 0.0,
            M21: 0.0,
            M22: dest.h / cb.h as f32,
            M31: dest.x,
            M32: dest.y,
        };
        unsafe {
            cb.brush.SetTransform(&m);
            cb.brush.SetOpacity(opacity * self.alpha);
            let rad = radius.min(clip.w * 0.5).min(clip.h * 0.5);
            let rr = D2D1_ROUNDED_RECT { rect: rf(clip), radiusX: rad, radiusY: rad };
            self.dc.FillRoundedRectangle(&rr, &cb.brush);
        }
    }

    fn create_bitmap(&self, img: &ImageData) -> Option<CachedBitmap> {
        unsafe {
            let props = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
                colorContext: ManuallyDrop::new(None),
            };
            let bmp = self
                .res
                .dc
                .CreateBitmap(
                    D2D_SIZE_U { width: img.width, height: img.height },
                    Some(img.pixels.as_ptr() as *const _),
                    img.width * 4,
                    &props,
                )
                .ok()?;
            let bprops = D2D1_BITMAP_BRUSH_PROPERTIES1 {
                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                interpolationMode: D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
            };
            let brush = self.res.dc.CreateBitmapBrush(&bmp, Some(&bprops), None).ok()?;
            Some(CachedBitmap { brush, w: img.width, h: img.height, last_used: self.res.frame })
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshots: a finished piece of UI kept as one GPU picture
// ---------------------------------------------------------------------------

/// A piece of UI drawn once into an off-screen picture. While the notch opens, closes or
/// switches pages, the content does not change, so it is drawn once and then only moved and
/// faded (one rectangle fill per frame) instead of being laid out and drawn again every frame.
pub struct Snapshot {
    brush: ID2D1BitmapBrush1,
    /// The same picture at a quarter of the size; drawn stretched back up it is a soft blur.
    soft: Option<ID2D1BitmapBrush1>,
    /// The same picture at half and at an eighth of the size (lighter and heavier blur).
    half: Option<ID2D1BitmapBrush1>,
    far: Option<ID2D1BitmapBrush1>,
    /// Where the picture sits, in logical units (snapped to whole device pixels).
    pub rect: Rect,
    pub key: u64,
    pub born: std::time::Instant,
    /// The pictures behind the brushes (full size, then half, quarter, eighth) and the size in
    /// device pixels, so a strip that is redrawn often reuses them instead of allocating again.
    bufs: Vec<ID2D1Bitmap1>,
    px: (u32, u32),
    /// The strip with its blur already blended in (see `Res::bake_band`).
    baked: Option<(ID2D1Bitmap1, ID2D1BitmapBrush1)>,
    /// The full-size picture itself (the one behind `brush`).
    src: ID2D1Bitmap1,
    /// Smaller copies for blurring: entry k is the picture at 1/2^(k+1) of its size (made when first needed).
    mips: Vec<ID2D1Bitmap1>,
    /// Where a blur is drawn into, and the part of it that holds this frame's blur (width, height in px).
    blur: Option<ID2D1Bitmap1>,
    blur_used: Option<(u32, u32)>,
}

/// The glass rim as one small picture (see `refract`), and where it sits in logical units.
pub struct RimPic {
    bmp: ID2D1Bitmap1,
    pub rect: Rect,
    step: usize,
    /// The part of the picture that holds the rim (the picture itself is rounded up to a multiple
    /// of 64, so one picture fits many slightly different sizes), and the picture's own size.
    used: (u32, u32),
    cap: (u32, u32),
    pool: RimPool,
}

impl Drop for RimPic {
    /// A picture that is no longer shown goes back to the pool to be filled again.
    fn drop(&mut self) {
        if let Ok(mut pool) = self.pool.lock() {
            pool.retain(|e| (e.0, e.1) == self.cap);
            if pool.len() < 2 {
                pool.push((self.cap.0, self.cap.1, self.bmp.clone()));
            }
        }
    }
}

impl Res {
    /// Upload premultiplied BGRA pixels (`w` x `h`, at (`x0`, `y0`) in window pixels) as a picture.
    /// The picture is taken from the pool when one of the right size is spare, and only the rim's
    /// pixels are copied into it.
    pub fn rim_pic(&self, x0: i32, y0: i32, w: usize, h: usize, step: usize, px: &[u32], scale: f32) -> Option<RimPic> {
        if w == 0 || h == 0 || px.len() < w * h || w > 4096 || h > 4096 {
            return None;
        }
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: ManuallyDrop::new(None),
        };
        let up = |v: usize| (v.div_ceil(64) * 64) as u32;
        let cap = (up(w), up(h));
        let used = (w as u32, h as u32);
        unsafe {
            let data = px.as_ptr() as *const std::ffi::c_void;
            let pitch = (w * 4) as u32;
            let region = D2D_RECT_U { left: 0, top: 0, right: used.0, bottom: used.1 };
            let spare = self
                .rim_pool
                .lock()
                .ok()
                .and_then(|mut p| p.iter().position(|e| (e.0, e.1) == cap).map(|i| p.swap_remove(i).2));
            let bmp = match spare {
                Some(b) if b.CopyFromMemory(Some(&region), data, pitch).is_ok() => b,
                _ => {
                    let b = self.dc.CreateBitmap(D2D_SIZE_U { width: cap.0, height: cap.1 }, None, 0, &props).ok()?;
                    b.CopyFromMemory(Some(&region), data, pitch).ok()?;
                    b
                }
            };
            let k = step.max(1) as f32;
            Some(RimPic {
                bmp,
                rect: Rect::new(x0 as f32 / scale, y0 as f32 / scale, w as f32 * k / scale, h as f32 * k / scale),
                step,
                used,
                cap,
                pool: self.rim_pool.clone(),
            })
        }
    }
}

impl<'a> Painter<'a> {
    /// Draw the glass rim: one picture, one call, pixel for pixel.
    pub fn draw_rim(&self, r: &RimPic, opacity: f32) {
        self.draw_rim_to(r, r.rect, opacity);
    }

    /// The same, drawn into `dst` (a picture made for an earlier size is stretched to the size the
    /// notch has now, so no gap shows at its edges).
    pub fn draw_rim_to(&self, r: &RimPic, dst: Rect, opacity: f32) {
        let a = opacity * self.alpha;
        if a <= 0.002 {
            return;
        }
        let Ok(b) = r.bmp.cast::<ID2D1Bitmap>() else { return };
        unsafe {
            let mode = if r.step > 1 { D2D1_INTERPOLATION_MODE_LINEAR } else { D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR };
            // only the part that holds the rim (the picture may be a little bigger)
            let src = D2D_RECT_F { left: 0.0, top: 0.0, right: r.used.0 as f32, bottom: r.used.1 as f32 };
            self.dc.DrawBitmap(&b, Some(&rf(dst)), a, mode, Some(&src), None);
        }
    }
}

impl Res {
    /// Blend the strip's pictures (sharp, then lighter to heavier blur) into ONE picture. Each
    /// layer is laid down through a smooth vertical fade, so the blur grows steadily toward the
    /// rim with no slices and no steps, and the screen then draws a single picture. Called each
    /// time the strip is redrawn; the masks and the rectangle are made once per strip height.
    /// Returns false (and clears the baked picture) when something is missing.
    pub fn bake_band(&mut self, gfx: &Gfx, s: &mut Snapshot) -> bool {
        if s.bufs.len() != 4 || s.half.is_none() || s.soft.is_none() || s.far.is_none() {
            s.baked = None;
            return false;
        }
        let ok = self.bake_band_inner(gfx, s).is_some();
        if !ok {
            s.baked = None;
        }
        ok
    }

    fn bake_band_inner(&mut self, gfx: &Gfx, s: &mut Snapshot) -> Option<()> {
        let (w, h) = s.px;
        let dc = self.dc.clone();
        let ease = |a: f32, b: f32, x: f32| {
            let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        };
        unsafe {
            if self.band_mask.as_ref().map(|m| m.0) != Some(h) {
                const N: usize = 40;
                let mut masks = Vec::with_capacity(4);
                for l in 0..4usize {
                    let mut stops = [D2D1_GRADIENT_STOP::default(); N];
                    for (j, st) in stops.iter_mut().enumerate() {
                        let u = j as f32 / (N - 1) as f32;
                        // how much of each picture a row takes: two neighbouring ones are blended so
                        // that together they add up to exactly `fade` of coverage
                        let pos = ease(0.0, 1.0, u) * 2.5;
                        let lo = pos.floor();
                        let mix = pos - lo;
                        let fade = 1.0 - 0.72 * ease(0.25, 1.0, u);
                        let under = fade * (1.0 - mix) / (1.0 - mix * fade).max(0.05);
                        let lo = lo as usize;
                        let a = if l == lo {
                            under
                        } else if l == lo + 1 && mix > 0.01 {
                            mix * fade
                        } else {
                            0.0
                        };
                        *st = D2D1_GRADIENT_STOP { position: u, color: D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a } };
                    }
                    let coll = ID2D1RenderTarget::CreateGradientStopCollection(
                        &dc,
                        &stops,
                        D2D1_GAMMA_2_2,
                        D2D1_EXTEND_MODE_CLAMP,
                    )
                    .ok()?;
                    let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: v2(0.0, 0.0), endPoint: v2(0.0, h as f32) };
                    masks.push(dc.CreateLinearGradientBrush(&props, None, &coll).ok()?);
                }
                self.band_mask = Some((h, masks));
            }
            if self.band_geo.as_ref().map(|g| g.0) != Some((w, h)) {
                let r = D2D_RECT_F { left: 0.0, top: 0.0, right: w as f32, bottom: h as f32 };
                let geo: ID2D1Geometry = gfx.d2d_factory.CreateRectangleGeometry(&r).ok()?.cast().ok()?;
                self.band_geo = Some(((w, h), geo));
            }
            if s.baked.is_none() {
                let props = D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
                    colorContext: ManuallyDrop::new(None),
                };
                let bmp = dc.CreateBitmap(D2D_SIZE_U { width: w, height: h }, None, 0, &props).ok()?;
                let bprops = D2D1_BITMAP_BRUSH_PROPERTIES1 {
                    extendModeX: D2D1_EXTEND_MODE_CLAMP,
                    extendModeY: D2D1_EXTEND_MODE_CLAMP,
                    interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR,
                };
                let brush = dc.CreateBitmapBrush(&bmp, Some(&bprops), None).ok()?;
                s.baked = Some((bmp, brush));
            }
            let (bmp, _) = s.baked.as_ref()?;
            let (_, masks) = self.band_mask.as_ref()?;
            let (_, geo) = self.band_geo.as_ref()?;
            let layers = [Some(&s.brush), s.half.as_ref(), s.soft.as_ref(), s.far.as_ref()];
            dc.SetTarget(bmp);
            dc.SetTransform(&Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 });
            dc.BeginDraw();
            dc.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
            // a mask brush needs aliased drawing; a pixel-aligned rectangle has no edge to smooth
            dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_ALIASED);
            for (l, (layer, mask)) in layers.iter().zip(masks.iter()).enumerate() {
                let Some(b) = layer else { continue };
                let f = (1u32 << l) as f32;
                b.SetTransform(&Matrix3x2 { M11: f, M12: 0.0, M21: 0.0, M22: f, M31: 0.0, M32: 0.0 });
                b.SetOpacity(1.0);
                dc.FillGeometry(geo, *b, mask);
            }
            dc.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            let ok = dc.EndDraw(None, None).is_ok();
            dc.SetTarget(None);
            ok.then_some(())
        }
    }
}

impl Snapshot {
    /// True once the blended strip picture exists.
    pub fn is_baked(&self) -> bool {
        self.baked.is_some()
    }
}

impl<'a> Painter<'a> {
    /// Draw the blended strip through `shape` (the notch outline, so the rounded corners are
    /// smooth), limited to the strip's own rectangle. One fill, whatever the strip height.
    pub fn snapshot_baked(&self, s: &Snapshot, shape: &ID2D1Geometry) -> bool {
        let Some((_, b)) = &s.baked else { return false };
        let a = self.alpha;
        if a <= 0.002 {
            return true;
        }
        let k = 1.0 / self.scale;
        let m = Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: s.rect.x, M32: s.rect.y };
        self.push_clip(s.rect);
        unsafe {
            b.SetTransform(&m);
            b.SetOpacity(a);
            self.dc.FillGeometry(shape, b, None);
        }
        self.pop_clip();
        true
    }
}

impl Res {
    /// Draw `draw` into a new picture covering `region` (logical units). Everything is painted
    /// with the same coordinates as on screen. Returns None if the picture could not be made
    /// (the caller then simply draws live).
    pub fn render_snapshot(
        &mut self,
        gfx: &Gfx,
        scale: f32,
        region: Rect,
        key: u64,
        reuse: Option<Snapshot>,
        deep: bool,
        draw: impl FnOnce(&mut Painter),
    ) -> Option<Snapshot> {
        let x0 = (region.x * scale).floor();
        let y0 = (region.y * scale).floor();
        let x1 = ((region.x + region.w) * scale).ceil();
        let y1 = ((region.y + region.h) * scale).ceil();
        let (w, h) = ((x1 - x0).max(0.0) as u32, (y1 - y0).max(0.0) as u32);
        if w < 2 || h < 2 || w > 4096 || h > 4096 {
            return None;
        }
        let dc = self.dc.clone();
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
            colorContext: ManuallyDrop::new(None),
        };
        let ident = Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 };
        // one halving step: `src` averaged down into `dst`
        let down = |src: &ID2D1Bitmap1, dst: &ID2D1Bitmap1, nw: u32, nh: u32| -> bool {
            let Ok(from) = src.cast::<ID2D1Bitmap>() else { return false };
            unsafe {
                dc.SetTarget(dst);
                dc.SetTransform(&ident);
                dc.BeginDraw();
                dc.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
                let d = D2D_RECT_F { left: 0.0, top: 0.0, right: nw as f32, bottom: nh as f32 };
                dc.DrawBitmap(&from, Some(&d), 1.0, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None);
                let ok = dc.EndDraw(None, None).is_ok();
                dc.SetTarget(None);
                ok
            }
        };
        unsafe {
            // the same size as last time: draw into the pictures that already exist
            if let Some(mut old) = reuse.filter(|r| r.px == (w, h) && r.bufs.len() == 4) {
                dc.SetTarget(&old.bufs[0]);
                dc.BeginDraw();
                {
                    let mut p = Painter::new(dc.clone(), gfx, &mut *self, scale, -x0, -y0);
                    p.clear();
                    draw(&mut p);
                }
                let ok = dc.EndDraw(None, None).is_ok();
                dc.SetTarget(None);
                if !ok {
                    return None;
                }
                let (mut sw, mut sh) = (w, h);
                for i in 1..4 {
                    let (nw, nh) = (sw.div_ceil(2).max(1), sh.div_ceil(2).max(1));
                    if !down(&old.bufs[i - 1], &old.bufs[i], nw, nh) {
                        return None;
                    }
                    sw = nw;
                    sh = nh;
                }
                old.rect = Rect::new(x0 / scale, y0 / scale, w as f32 / scale, h as f32 / scale);
                old.key = key;
                old.born = std::time::Instant::now();
                old.blur_used = None;
                old.mips.clear();
                return Some(old);
            }
            let bmp = dc.CreateBitmap(D2D_SIZE_U { width: w, height: h }, None, 0, &props).ok()?;
            dc.SetTarget(&bmp);
            dc.BeginDraw();
            {
                let mut p = Painter::new(dc.clone(), gfx, &mut *self, scale, -x0, -y0);
                p.clear();
                draw(&mut p);
            }
            let ok = dc.EndDraw(None, None).is_ok();
            dc.SetTarget(None);
            if !ok {
                return None;
            }
            let bprops = D2D1_BITMAP_BRUSH_PROPERTIES1 {
                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR,
            };
            let brush = dc.CreateBitmapBrush(&bmp, Some(&bprops), None).ok()?;
            // Halve the picture three times (each step averages 2x2 pixels). Stretched back up with a
            // smooth filter each step reads as a blur, a stronger one the smaller it is, for a
            // fraction of a real blur's cost. A page picture only ever needs the quarter-size step
            // (`deep` keeps all three, for the bottom strip); the others are let go straight away.
            let smooth = D2D1_BITMAP_BRUSH_PROPERTIES1 {
                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                interpolationMode: D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
            };
            let mut bufs: Vec<ID2D1Bitmap1> = Vec::new();
            let mut brushes: [Option<ID2D1BitmapBrush1>; 3] = [None, None, None];
            let built = (|| -> Option<()> {
                let mut src: ID2D1Bitmap1 = bmp.clone();
                let (mut sw, mut sh) = (w, h);
                if deep {
                    bufs.push(bmp.clone());
                }
                for (i, slot) in brushes.iter_mut().enumerate() {
                    if !deep && i >= 2 {
                        break; // the eighth-size step is only used by the bottom strip
                    }
                    let (nw, nh) = (sw.div_ceil(2).max(1), sh.div_ceil(2).max(1));
                    let small = dc.CreateBitmap(D2D_SIZE_U { width: nw, height: nh }, None, 0, &props).ok()?;
                    if !down(&src, &small, nw, nh) {
                        return None;
                    }
                    if deep || i == 1 {
                        *slot = Some(dc.CreateBitmapBrush(&small, Some(&smooth), None).ok()?);
                    }
                    if deep {
                        bufs.push(small.clone());
                    }
                    src = small;
                    sw = nw;
                    sh = nh;
                }
                Some(())
            })();
            if built.is_none() {
                bufs.clear();
                brushes = [None, None, None];
            }
            let [half, soft, far] = brushes;
            Some(Snapshot {
                brush,
                soft,
                half,
                far,
                rect: Rect::new(x0 / scale, y0 / scale, w as f32 / scale, h as f32 / scale),
                key,
                born: std::time::Instant::now(),
                bufs,
                px: (w, h),
                baked: None,
                src: bmp.clone(),
                mips: Vec::new(),
                blur: None,
                blur_used: None,
            })
        }
    }
}

impl Snapshot {
    /// Let go of the blur pictures (memory) once nothing is blurring any more.
    pub fn release_blur(&mut self) {
        self.blur = None;
        self.mips.clear();
        self.blur_used = None;
    }
}

impl Res {
    /// Blur a picture by about `sigma` device pixels. It is shrunk in halves to a small size, the
    /// last step lands on an exact size (so the strength changes smoothly), and it is then stretched
    /// back up in steps of at most 2x, so the result is smooth instead of blocky. The only pictures
    /// kept are the small ones, and they are let go when the blur is over. Returns false when there
    /// is nothing to blur (the sharp picture is then drawn instead).
    pub fn blur_snapshot(&mut self, s: &mut Snapshot, sigma: f32) -> bool {
        s.blur_used = None;
        if sigma < 0.9 {
            s.release_blur();
            return false;
        }
        let (w, h) = s.px;
        let f = (sigma / 0.43).clamp(2.0, 64.0);
        let k = (f.log2().floor().max(1.0) as usize).min(6);
        let dc = self.dc.clone();
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
            colorContext: ManuallyDrop::new(None),
        };
        let ident = Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 };
        let clear = D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
        // `from` (or just its top-left `part` of pixels) drawn into `to`, stretched to nw x nh pixels
        let draw = |from: &ID2D1Bitmap1, to: &ID2D1Bitmap1, nw: u32, nh: u32, part: Option<(u32, u32)>| -> bool {
            let Ok(src) = from.cast::<ID2D1Bitmap>() else { return false };
            let part = part.map(|(a, b)| D2D_RECT_F { left: 0.0, top: 0.0, right: a as f32, bottom: b as f32 });
            unsafe {
                dc.SetTarget(to);
                dc.SetTransform(&ident);
                dc.BeginDraw();
                dc.Clear(Some(&clear));
                let d = D2D_RECT_F { left: 0.0, top: 0.0, right: nw as f32, bottom: nh as f32 };
                match &part {
                    Some(r) => dc.DrawBitmap(&src, Some(&d), 1.0, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, Some(r), None),
                    None => dc.DrawBitmap(&src, Some(&d), 1.0, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None),
                }
                let ok = dc.EndDraw(None, None).is_ok();
                dc.SetTarget(None);
                ok
            }
        };
        let dims = |lvl: usize| (w.div_ceil(1u32 << lvl).max(1), h.div_ceil(1u32 << lvl).max(1));
        // the small pictures (made once; they are redrawn every frame)
        while s.mips.len() < k {
            let (nw, nh) = dims(s.mips.len() + 1);
            let Ok(next) = (unsafe { dc.CreateBitmap(D2D_SIZE_U { width: nw, height: nh }, None, 0, &props) }) else {
                s.release_blur();
                return false;
            };
            s.mips.push(next);
        }
        // the exact-size picture: never larger than half the original
        let (sw, sh) = (((w as f32 / f).ceil() as u32).max(2), ((h as f32 / f).ceil() as u32).max(2));
        if s.blur.is_none() {
            let (bw, bh) = dims(1);
            s.blur = unsafe { dc.CreateBitmap(D2D_SIZE_U { width: bw + 2, height: bh + 2 }, None, 0, &props) }.ok();
        }
        let Some(blur) = &s.blur else { return false };
        // shrink
        for lvl in 1..=k {
            let from = if lvl == 1 { &s.src } else { &s.mips[lvl - 2] };
            let (nw, nh) = dims(lvl);
            if !draw(from, &s.mips[lvl - 1], nw, nh, None) {
                return false;
            }
        }
        if !draw(&s.mips[k - 1], blur, sw, sh, None) {
            return false;
        }
        // stretch back up, never more than 2x at a time
        let (nw, nh) = dims(k);
        if !draw(blur, &s.mips[k - 1], nw, nh, Some((sw, sh))) {
            return false;
        }
        for lvl in (1..k).rev() {
            let (nw, nh) = dims(lvl);
            if !draw(&s.mips[lvl], &s.mips[lvl - 1], nw, nh, None) {
                return false;
            }
        }
        s.blur_used = Some((0, 0));
        true
    }
}

impl<'a> Painter<'a> {
    /// Draw a snapshot as prepared by `Res::blur_snapshot` (the sharp picture if it was not blurred).
    pub fn snapshot_blurred(&self, s: &Snapshot, dx: f32, dy: f32, opacity: f32) {
        let a = opacity * self.alpha;
        if a <= 0.002 {
            return;
        }
        let Some(bm) = s.blur_used.and(s.mips.first()).and_then(|m| m.cast::<ID2D1Bitmap>().ok()) else {
            self.snapshot(s, dx, dy, opacity);
            return;
        };
        let (x, y) = (s.rect.x + dx, s.rect.y + dy);
        let dest = D2D_RECT_F { left: x, top: y, right: x + s.rect.w, bottom: y + s.rect.h };
        unsafe {
            self.dc.DrawBitmap(&bm, Some(&dest), a, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None);
        }
    }
}

impl<'a> Painter<'a> {
    /// Draw a snapshot, shifted by (`dx`, `dy`), with the painter's alpha times `opacity`.
    pub fn snapshot(&self, s: &Snapshot, dx: f32, dy: f32, opacity: f32) {
        let a = opacity * self.alpha;
        if a <= 0.002 {
            return;
        }
        let (x, y) = (s.rect.x + dx, s.rect.y + dy);
        let k = 1.0 / self.scale;
        let m = Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: x, M32: y };
        unsafe {
            s.brush.SetTransform(&m);
            s.brush.SetOpacity(a);
            self.dc.FillRectangle(&rf(Rect::new(x, y, s.rect.w, s.rect.h)), &s.brush);
        }
    }
}

impl<'a> Painter<'a> {
    /// Draw a snapshot scaled about its own centre by `sc`, shifted by (`dx`, `dy`); `soft` picks
    /// the blurred copy (falls back to the sharp one when there is none).
    pub fn snapshot_scaled(&self, s: &Snapshot, sc: f32, dx: f32, dy: f32, opacity: f32, soft: bool) {
        let a = opacity * self.alpha;
        if a <= 0.002 || sc <= 0.01 {
            return;
        }
        let (w, h) = (s.rect.w * sc, s.rect.h * sc);
        let x = s.rect.x + (s.rect.w - w) * 0.5 + dx;
        let y = s.rect.y + (s.rect.h - h) * 0.5 + dy;
        let soft_brush = if soft { s.soft.as_ref() } else { None };
        let k = if soft_brush.is_some() { 4.0 } else { 1.0 } * sc / self.scale;
        let m = Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: x, M32: y };
        unsafe {
            match soft_brush {
                Some(b) => {
                    b.SetTransform(&m);
                    b.SetOpacity(a);
                    self.dc.FillRectangle(&rf(Rect::new(x, y, w, h)), b);
                }
                None => {
                    s.brush.SetTransform(&m);
                    s.brush.SetOpacity(a);
                    self.dc.FillRectangle(&rf(Rect::new(x, y, w, h)), &s.brush);
                }
            }
        }
    }
}

impl<'a> Painter<'a> {
    /// Like `snapshot`, but the soft (blurred) copy of the picture. Falls back to the sharp
    /// picture if there is no soft copy.
    pub fn snapshot_soft(&self, s: &Snapshot, dx: f32, dy: f32, opacity: f32) {
        let Some(soft) = &s.soft else {
            self.snapshot(s, dx, dy, opacity);
            return;
        };
        let a = opacity * self.alpha;
        if a <= 0.002 {
            return;
        }
        let (x, y) = (s.rect.x + dx, s.rect.y + dy);
        let k = 4.0 / self.scale;
        let m = Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: x, M32: y };
        unsafe {
            soft.SetTransform(&m);
            soft.SetOpacity(a);
            self.dc.FillRectangle(&rf(Rect::new(x, y, s.rect.w, s.rect.h)), soft);
        }
    }
}

const SHADOW_R: f32 = 34.0; // corner radius baked into the bitmap (DIPs)
const SHADOW_B: f32 = 22.0; // blur extent (DIPs)
const SHADOW_SCALE: f32 = 2.0; // bitmap pixels per DIP

/// Black gaussian falloff *outside* a rounded rect; transparent inside, so
/// it never darkens translucent glass drawn on top.
fn make_shadow_bitmap(dc: &ID2D1DeviceContext, corner_n: f32) -> Option<ID2D1Bitmap1> {
    let corner = (SHADOW_R + SHADOW_B) * SHADOW_SCALE;
    let n = (2.0 * corner + 2.0) as usize;
    let c = n as f32 * 0.5;
    let half = c - SHADOW_B * SHADOW_SCALE; // half-extent of the shape
    let r = SHADOW_R * SHADOW_SCALE;
    let sigma = SHADOW_B * SHADOW_SCALE / 2.6;
    let mut px = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let qx = ((x as f32 + 0.5 - c).abs() - (half - r)).max(0.0);
            let qy = ((y as f32 + 0.5 - c).abs() - (half - r)).max(0.0);
            let d = (qx.powf(corner_n) + qy.powf(corner_n)).powf(1.0 / corner_n) - r;
            let a = if d <= 0.0 { 0.0 } else { (-0.5 * (d / sigma).powi(2)).exp() * (d / 1.5).min(1.0) };
            px[(y * n + x) * 4 + 3] = (a * 255.0).round() as u8; // premultiplied black
        }
    }
    let props = D2D1_BITMAP_PROPERTIES1 {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
        bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
        colorContext: ManuallyDrop::new(None),
    };
    unsafe {
        dc.CreateBitmap(
            D2D_SIZE_U { width: n as u32, height: n as u32 },
            Some(px.as_ptr() as *const _),
            (n * 4) as u32,
            &props,
        )
        .ok()
    }
}

/// The Alcove notch silhouette as a list of points: exactly the shape `notch_path` draws (ears, narrowing
/// walls, squircle bottom corners) when `steps` is 20, so a shadow built from it fits with no gap.
#[allow(clippy::too_many_arguments)]
fn notch_outline(x0: f32, x1: f32, h: f32, r: f32, ex: f32, ey: f32, tp: f32, cn: f32, en: f32, steps: usize) -> Vec<(f32, f32)> {
    let se = |n: f32, i: usize| {
        let t = i as f32 / steps as f32 * std::f32::consts::FRAC_PI_2;
        (t.cos().max(0.0).powf(2.0 / n), t.sin().max(0.0).powf(2.0 / n))
    };
    let mut v = Vec::with_capacity(4 * steps + 8);
    v.push((x0 - ex, 0.0));
    if ex > 0.0 {
        for i in 1..=steps {
            let (c, s) = se(en, steps - i);
            v.push((x0 - ex + ex * c, ey - ey * s));
        }
    }
    v.push((x0 + tp, h - r));
    if r > 0.0 {
        for i in 1..=steps {
            let (c, s) = se(cn, i);
            v.push((x0 + tp + r - r * c, h - r + r * s));
        }
    }
    v.push((x1 - tp - r, h));
    if r > 0.0 {
        for i in 1..=steps {
            let (c, s) = se(cn, i);
            v.push((x1 - tp - r + r * s, h - r + r * c));
        }
    }
    v.push((x1, ey));
    if ex > 0.0 {
        for i in 1..=steps {
            let (c, s) = se(en, i);
            v.push((x1 + ex - ex * c, ey - ey * s));
        }
    }
    v
}

/// Helper to build path geometries without exposing the sink everywhere.
pub struct PathBuilder {
    sink: ID2D1GeometrySink,
    open: std::cell::Cell<bool>,
}

impl PathBuilder {
    pub fn move_to(&self, x: f32, y: f32) {
        unsafe {
            if self.open.get() {
                self.sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            }
            self.sink.BeginFigure(v2(x, y), D2D1_FIGURE_BEGIN_FILLED);
        }
        self.open.set(true);
    }
    pub fn line_to(&self, x: f32, y: f32) {
        unsafe { self.sink.AddLine(v2(x, y)) }
    }
    /// Circular arc to (x, y).
    pub fn arc_to(&self, x: f32, y: f32, radius: f32, clockwise: bool) {
        unsafe {
            self.sink.AddArc(&D2D1_ARC_SEGMENT {
                point: v2(x, y),
                size: D2D_SIZE_F { width: radius, height: radius },
                rotationAngle: 0.0,
                sweepDirection: if clockwise {
                    D2D1_SWEEP_DIRECTION_CLOCKWISE
                } else {
                    D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
                },
                arcSize: D2D1_ARC_SIZE_SMALL,
            })
        }
    }
    pub fn arc_to_ex(&self, x: f32, y: f32, radius: f32, clockwise: bool, large: bool) {
        unsafe {
            self.sink.AddArc(&D2D1_ARC_SEGMENT {
                point: v2(x, y),
                size: D2D_SIZE_F { width: radius, height: radius },
                rotationAngle: 0.0,
                sweepDirection: if clockwise {
                    D2D1_SWEEP_DIRECTION_CLOCKWISE
                } else {
                    D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
                },
                arcSize: if large { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
            })
        }
    }

    /// The outline of a stroked arc with round caps (`start` clockwise from 12 o'clock, `sweep` > 0).
    pub fn arc_stroke(&self, cx: f32, cy: f32, radius: f32, start: f32, sweep: f32, width: f32) {
        let end = start + sweep;
        let h = width * 0.5;
        let pt = |a: f32, r: f32| (cx + r * a.sin(), cy - r * a.cos());
        let large = sweep > std::f32::consts::PI;
        let ri = (radius - h).max(0.0);
        let (x, y) = pt(start, radius + h);
        self.move_to(x, y);
        let (x, y) = pt(end, radius + h);
        self.arc_to_ex(x, y, radius + h, true, large);
        let (x, y) = pt(end, ri);
        self.arc_to(x, y, h, true); // end cap
        let (x, y) = pt(start, ri);
        self.arc_to_ex(x, y, ri, false, large);
        let (x, y) = pt(start, radius + h);
        self.arc_to(x, y, h, true); // start cap
        self.close();
    }

    /// Closed rounded-rect figure. Opposite windings punch holes (winding fill).
    pub fn rounded_rect(&self, r: Rect, radius: f32, clockwise: bool) {
        let rad = radius.min(r.w * 0.5).min(r.h * 0.5).max(0.0);
        let (l, t, rt, b) = (r.x, r.y, r.right(), r.bottom());
        if clockwise {
            self.move_to(l + rad, t);
            self.line_to(rt - rad, t);
            self.arc_to(rt, t + rad, rad, true);
            self.line_to(rt, b - rad);
            self.arc_to(rt - rad, b, rad, true);
            self.line_to(l + rad, b);
            self.arc_to(l, b - rad, rad, true);
            self.line_to(l, t + rad);
            self.arc_to(l + rad, t, rad, true);
        } else {
            self.move_to(l + rad, t);
            self.arc_to(l, t + rad, rad, false);
            self.line_to(l, b - rad);
            self.arc_to(l + rad, b, rad, false);
            self.line_to(rt - rad, b);
            self.arc_to(rt, b - rad, rad, false);
            self.line_to(rt, t + rad);
            self.arc_to(rt - rad, t, rad, false);
            self.line_to(l + rad, t);
        }
        self.close();
    }

    pub fn circle(&self, cx: f32, cy: f32, r: f32, clockwise: bool) {
        self.move_to(cx, cy - r);
        self.arc_to(cx, cy + r, r, clockwise);
        self.arc_to(cx, cy - r, r, clockwise);
        self.close();
    }

    /// Closed polygon with corners rounded by `radius`.
    pub fn rounded_poly(&self, pts: &[(f32, f32)], radius: f32) {
        let n = pts.len();
        if n < 3 {
            return;
        }
        let corner = |i: usize| {
            let (x, y) = pts[i];
            let (px, py) = pts[(i + n - 1) % n];
            let (nx, ny) = pts[(i + 1) % n];
            let lp = ((px - x).powi(2) + (py - y).powi(2)).sqrt().max(0.001);
            let ln = ((nx - x).powi(2) + (ny - y).powi(2)).sqrt().max(0.001);
            let r = radius.min(lp * 0.5).min(ln * 0.5);
            ((x + (px - x) / lp * r, y + (py - y) / lp * r), (x + (nx - x) / ln * r, y + (ny - y) / ln * r))
        };
        let (_, out0) = corner(0);
        self.move_to(out0.0, out0.1);
        for i in (1..n).chain(std::iter::once(0)) {
            let (inp, out) = corner(i);
            self.line_to(inp.0, inp.1);
            self.quad_to(pts[i].0, pts[i].1, out.0, out.1);
        }
        self.close();
    }

    pub fn cubic_to(&self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        unsafe {
            self.sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: v2(x1, y1), point2: v2(x2, y2), point3: v2(x, y) })
        }
    }
    pub fn quad_to(&self, cx: f32, cy: f32, x: f32, y: f32) {
        unsafe { self.sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT { point1: v2(cx, cy), point2: v2(x, y) }) }
    }
    pub fn close(&self) {
        if self.open.get() {
            unsafe { self.sink.EndFigure(D2D1_FIGURE_END_CLOSED) };
            self.open.set(false);
        }
    }
    fn finish(&self) {
        self.close();
    }
}

/// How square the bottom corners of the Alcove notch are: the curve is a superellipse, |x|^n + |y|^n = 1
/// (2 is a circle, 4 a squircle). Measured from the reference notch: about 3.1.
pub const CORNER_N: f32 = 3.1;
/// The same for the concave ears at the top (a little softer than a circle).
const EAR_N: f32 = 2.3;
/// An ear is taller than it is wide, like the reference's (about 1.35 times).
const EAR_TALL: f32 = 1.35;
/// How far (in px, per side) the side walls of a tall Alcove notch lean inward by the bottom.
pub const WALL_TAPER: f32 = 3.5;
pub fn wall_taper(h: f32) -> f32 {
    if crate::ui::look_alcove() {
        ((h - 70.0) / 90.0).clamp(0.0, 1.0) * WALL_TAPER
    } else {
        0.0
    }
}

/// Notch silhouette: flush with the top edge, concave "ears" where it meets
/// the screen edge and rounded bottom corners. In the Alcove look the corners are squircles and the
/// ears long and soft (see `CORNER_N`); everything else keeps plain circular arcs.
pub fn notch_path(p: &Painter, body: Rect, bottom_radius: f32, ear: f32) -> Option<ID2D1Geometry> {
    let x0 = body.x;
    let x1 = body.right();
    let h = body.h.max(1.0);
    let r = bottom_radius.min(body.w * 0.5).min(h - ear.min(h * 0.5)).max(0.0);
    let e = ear.min(h * 0.5).max(0.0);
    // (a full half-circle, the round bubbles, stays a circle)
    if crate::ui::look_alcove() && r * 2.0 < body.w - 0.5 {
        let ex = e;
        let ey = (e * EAR_TALL).min(h * 0.5);
        let r = r.min(h - ey).max(0.0);
        let tp = wall_taper(h);
        const STEPS: usize = 20;
        // a point a quarter of the way round a superellipse: (cos^(2/n), sin^(2/n)) for t in 0..1
        let se = |n: f32, i: usize| {
            let t = i as f32 / STEPS as f32 * std::f32::consts::FRAC_PI_2;
            (t.cos().max(0.0).powf(2.0 / n), t.sin().max(0.0).powf(2.0 / n))
        };
        let squircle = p.path(|b| {
            b.move_to(x0 - ex, 0.0);
            if ex > 0.0 {
                // left ear: from the top edge, round to the side wall
                for i in 1..=STEPS {
                    let (c, s) = se(EAR_N, STEPS - i);
                    b.line_to(x0 - ex + ex * c, ey - ey * s);
                }
            }
            b.line_to(x0 + tp, h - r);
            if r > 0.0 {
                // bottom left
                for i in 1..=STEPS {
                    let (c, s) = se(CORNER_N, i);
                    b.line_to(x0 + tp + r - r * c, h - r + r * s);
                }
            }
            b.line_to(x1 - tp - r, h);
            if r > 0.0 {
                // bottom right
                for i in 1..=STEPS {
                    let (c, s) = se(CORNER_N, i);
                    b.line_to(x1 - tp - r + r * s, h - r + r * c);
                }
            }
            b.line_to(x1, ey);
            if ex > 0.0 {
                // right ear
                for i in 1..=STEPS {
                    let (c, s) = se(EAR_N, i);
                    b.line_to(x1 + ex - ex * c, ey - ey * s);
                }
            }
            b.close();
        });
        if squircle.is_some() {
            return squircle;
        }
    }
    p.path(|b| {
        b.move_to(x0 - e, 0.0);
        if e > 0.0 {
            b.arc_to(x0, e, e, true);
        }
        b.line_to(x0, h - r);
        if r > 0.0 {
            b.arc_to(x0 + r, h, r, false);
        }
        b.line_to(x1 - r, h);
        if r > 0.0 {
            b.arc_to(x1, h - r, r, false);
        }
        b.line_to(x1, e);
        if e > 0.0 {
            b.arc_to(x1 + e, 0.0, e, true);
        }
        b.close();
    })
}
