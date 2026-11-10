//! Mirror: a small camera preview that only runs while you ask for it.
//!
//! The camera is off until you press the round camera button on the Mirror
//! tab. It grows into a camera view and is switched off again with the close
//! button, when you leave the tab, or when the notch closes. A worker thread owns the camera (Windows
//! `MediaCapture` + a frame reader), converts every frame to a small mirrored
//! picture and hands it to the UI thread. Nothing runs while the mirror is off.

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Capture::Frames::{
    MediaFrameReader, MediaFrameReaderStartStatus, MediaFrameReference, MediaFrameSourceGroup, MediaFrameSourceKind,
};
use windows::Media::Capture::{
    MediaCapture, MediaCaptureInitializationSettings, MediaCaptureMemoryPreference, MediaCaptureSharingMode,
    StreamingCaptureMode,
};
use windows::Media::MediaProperties::MediaEncodingSubtypes;
use windows::Storage::Streams::{Buffer, DataReader};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use super::{Cx, Module, ModuleId, SystemEvent};
use crate::anim::{lerp, smoothstep};
use crate::bus::Bus;
use crate::gfx::{Icon, ImageData, TextStyle};
use crate::ui::{gkey, ButtonStyle, Ui};
use crate::util::{palette, Color, Rect};

const ID: ModuleId = "mirror";
/// The preview is shrunk to at most this many pixels wide (averaged, so it stays smooth).
const MAX_W: usize = 640;
/// How much of the camera picture is shown: 1.0 fills the page (cropped), 0.0 shows all of it.
const ZOOM: f32 = 0.15;
/// ~22 frames per second is plenty for a mirror and keeps the cost low.
const FRAME_GAP: Duration = Duration::from_millis(45);

/// True while the camera view is open on the home page (or still shrinking away): the page is taller then.
static SHOWING: AtomicBool = AtomicBool::new(false);

pub fn showing() -> bool {
    SHOWING.load(Ordering::Relaxed)
}

#[derive(Clone, PartialEq)]
enum State {
    Idle,
    Starting,
    Live,
    Failed(String),
}

enum Kind {
    Frame(ImageData),
    Failed(String),
    Ended,
}

/// Worker → UI. `gen` tells which start request a message belongs to.
struct Msg {
    gen: u32,
    kind: Kind,
}

pub struct Mirror {
    state: State,
    frame: Option<ImageData>,
    /// Set to stop the running worker.
    stop: Option<Arc<AtomicBool>>,
    gen: u32,
    /// Last time the tab was drawn (the camera stops when you leave it).
    page_seen: Instant,
    /// Bitmap of the frame on screen, so the old one can be released.
    drawn_id: Option<u64>,
    /// How far the home page's camera view is open (0..1) and when it was last moved.
    p: f32,
    p_at: Instant,
}

impl Mirror {
    pub fn new() -> Self {
        Self { state: State::Idle, frame: None, stop: None, gen: 0, page_seen: Instant::now(), drawn_id: None, p: 0.0, p_at: Instant::now() }
    }

    fn begin(&mut self, bus: &Bus) {
        if self.stop.is_some() {
            return;
        }
        self.gen = self.gen.wrapping_add(1);
        let gen = self.gen;
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(stop.clone());
        self.state = State::Starting;
        self.frame = None;
        self.page_seen = Instant::now();
        SHOWING.store(true, Ordering::Relaxed);
        let bus = bus.clone();
        if std::thread::Builder::new().name("mirror".into()).spawn(move || worker(gen, stop, bus)).is_err() {
            self.stop = None;
            self.state = State::Failed("Couldn't start the camera".into());
        }
    }

    /// Switch the camera off and drop the picture.
    fn shutdown(&mut self) {
        if !crate::ui::look_alcove() {
            SHOWING.store(false, Ordering::Relaxed);
        }
        if let Some(s) = self.stop.take() {
            s.store(true, Ordering::Release);
        }
        self.frame = None;
        if !matches!(self.state, State::Failed(_)) {
            self.state = State::Idle;
        }
    }
}

impl Module for Mirror {
    fn id(&self) -> ModuleId {
        ID
    }
    fn title(&self) -> &str {
        "Mirror"
    }
    fn icon(&self) -> Icon {
        Icon::Camera
    }

    fn stop(&mut self) {
        self.shutdown();
    }

    fn on_system(&mut self, ev: &SystemEvent, cx: &mut Cx) {
        if let SystemEvent::Expanded(false) = ev {
            let was_on = self.stop.is_some();
            self.shutdown();
            // the next time the page opens the view starts closed
            self.p = 0.0;
            SHOWING.store(false, Ordering::Relaxed);
            if was_on {
                cx.fx.redraw = true;
            }
        }
    }

    fn on_message(&mut self, msg: Box<dyn Any + Send>, cx: &mut Cx) {
        let Ok(m) = msg.downcast::<Msg>() else { return };
        if m.gen != self.gen {
            return; // from a camera session that was already closed
        }
        match m.kind {
            Kind::Frame(img) => {
                if self.stop.is_some() {
                    self.frame = Some(img);
                    self.state = State::Live;
                    cx.fx.redraw = true;
                }
            }
            Kind::Failed(why) => {
                self.stop = None;
                self.frame = None;
                self.state = State::Failed(why);
                cx.fx.redraw = true;
            }
            Kind::Ended => {
                self.stop = None;
                if matches!(self.state, State::Live | State::Starting) {
                    self.state = State::Idle;
                }
                self.frame = None;
                cx.fx.redraw = true;
            }
        }
    }

    /// While the camera runs, check twice a second that the tab is still open.
    fn next_tick(&self) -> Option<Instant> {
        self.stop.as_ref().map(|_| Instant::now() + Duration::from_millis(500))
    }

    fn on_tick(&mut self, cx: &mut Cx) {
        if self.stop.is_none() {
            return;
        }
        let limit = if self.state == State::Live { Duration::from_millis(1200) } else { Duration::from_secs(20) };
        if self.page_seen.elapsed() > limit {
            self.shutdown();
            self.p = 0.0;
            SHOWING.store(false, Ordering::Relaxed);
            cx.fx.redraw = true;
        }
    }

    /// In the Alcove look the mirror lives on the home page instead of having a tab.
    fn has_page(&self) -> bool {
        !crate::ui::look_alcove()
    }

    fn page_height(&self, _width: f32) -> f32 {
        if crate::ui::look_alcove() {
            210.0
        } else {
            230.0
        }
    }

    /// The mirror on the Alcove home page: a small camera icon beside the settings gear. Pressing it
    /// grows a camera view out of the icon over the whole page (which gets taller for it); the close
    /// button, leaving the page or closing the notch switches the camera off again. `r` is the home
    /// page's whole content area.
    fn draw_card(&mut self, ui: &mut Ui, r: Rect) {
        let now = ui.now;
        let open = self.stop.is_some();
        if open {
            self.page_seen = Instant::now();
        }
        // ease the view open or shut with a timer of its own (so it can start closed when the notch reopens)
        let dt = now.saturating_duration_since(self.p_at).as_secs_f32().min(0.05);
        self.p_at = now;
        let want = if open { 1.0 } else { 0.0 };
        if (self.p - want).abs() > 0.004 {
            self.p += (want - self.p) * (1.0 - (-dt * 11.0).exp());
            if (self.p - want).abs() <= 0.004 {
                self.p = want;
            }
            ui.animate();
        }
        let p = self.p;
        SHOWING.store(open || p > 0.01, Ordering::Relaxed);

        // the camera icon, in the gear's style
        let btn = Rect::new(r.x + 24.0, r.y - 8.0, 22.0, 22.0);
        if p < 0.995 {
            let hover = p < 0.02 && ui.hovered(btn.expand(4.0));
            if hover {
                ui.hot = true;
            }
            let a = ui.glide(gkey("mirror-btn", 0), if hover { 1.0 } else { 0.0 });
            let outer = ui.p.alpha;
            ui.p.alpha = outer * (1.0 - smoothstep(0.1, 0.5, p));
            let failed = matches!(self.state, State::Failed(_));
            let tint = if failed { Color::white(0.28) } else { Color::white(0.45 + 0.45 * a) };
            ui.p.icon(Icon::Camera, btn, tint);
            if let State::Failed(why) = &self.state {
                ui.p.text(
                    &format!("Camera unavailable. {why}"),
                    Rect::new(btn.right() + 8.0, btn.y, 260.0, btn.h),
                    &TextStyle::new(11.5, palette::TEXT_FAINT),
                );
            }
            ui.p.alpha = outer;
            if p < 0.02 && !open && ui.clicked(btn.expand(4.0)) {
                let bus = ui.bus.clone();
                self.begin(&bus);
            }
        }
        if p <= 0.003 {
            return;
        }

        // the view grows out of the icon and fills the page
        let view = Rect::new(lerp(btn.x, r.x, p), lerp(btn.y, r.y, p), lerp(btn.w, r.w, p), lerp(btn.h, r.h, p));
        let radius = lerp(btn.h * 0.5, 30.0, p);
        let outer = ui.p.alpha;
        ui.p.alpha = outer * smoothstep(0.0, 0.25, p);
        ui.p.fill_rounded(view, radius, Color::hex(0x0B0B0D));
        ui.p.alpha = outer;
        let show = smoothstep(0.55, 0.95, p);
        if show > 0.01 {
            ui.p.alpha = outer * show;
            ui.p.push_clip(view);
            match &self.frame {
                Some(img) => {
                    // release the previous frame's GPU copy right away
                    if self.drawn_id != Some(img.id) {
                        if let Some(old) = self.drawn_id.replace(img.id) {
                            ui.p.forget_image(old);
                        }
                    }
                    ui.p.image_zoom(img, view, radius, 1.0, ZOOM);
                    // a small red dot shows the camera is live
                    ui.p.fill_circle(view.x + 24.0, view.y + 24.0, 5.0, palette::RED);
                }
                None => {
                    ui.p.text(
                        "Starting camera…",
                        Rect::new(view.x, view.cy() - 22.0, view.w, 20.0),
                        &TextStyle::new(13.0, palette::TEXT_DIM).center(),
                    );
                    ui.ticks_sweep(Rect::new(view.cx() - 50.0, view.cy() + 6.0, 100.0, 10.0), Color::white(0.9));
                }
            }
            ui.p.pop_clip();
            ui.p.stroke_rounded(view.inset(0.5), radius, 1.0, Color::white(0.10));
            if open && p > 0.97 {
                let close = Rect::new(view.right() - 38.0, view.y + 10.0, 28.0, 28.0);
                if ui.icon_button(close, Icon::Close, ButtonStyle::filled(Color::black(0.55)).scale(0.5)) {
                    self.shutdown();
                    ui.fx.redraw = true;
                }
            }
            ui.p.alpha = outer;
        }
    }

    /// The Mirror tab: a round camera button with a ring of beams. Pressing it grows the button
    /// into the camera view over the whole tab; the close button (or leaving the tab) shrinks it
    /// back while its beams unload.
    fn draw_page(&mut self, ui: &mut Ui, r: Rect) {
        let open = self.stop.is_some();
        if open {
            self.page_seen = Instant::now();
        }
        let state = self.state.clone();
        let al = crate::ui::look_alcove();
        let btn = if al {
            Rect::centered(r.cx(), r.cy() - 14.0, 64.0, 64.0)
        } else {
            Rect::centered(r.cx(), r.cy() - 6.0, 52.0, 52.0)
        };
        let p = ui.glide(gkey("mirror-open", 0), if open { 1.0 } else { 0.0 });

        // the button and the ring of beams around it
        if p < 0.995 {
            let hover = p < 0.02 && ui.hovered(btn.expand(6.0));
            if hover {
                ui.hot = true;
            }
            let alpha = 1.0 - smoothstep(0.45, 0.9, p);
            let outer = ui.p.alpha;
            ui.p.alpha = outer * alpha;
            if !al {
                ui.ring(
                    btn.cx(),
                    btn.cy(),
                    btn.w + 26.0,
                    2.8,
                    if open || hover { 1.0 } else { 0.0 },
                    Color::white(0.9),
                );
            }
            ui.p.alpha = outer;
            if p < 0.02 {
                let style = if al {
                    ButtonStyle::filled(Color::white(0.94)).fg(Color::hex(0x0E0E10)).scale(0.5)
                } else {
                    ButtonStyle::filled(Color::white(0.09)).scale(0.52)
                };
                if ui.icon_button(btn, Icon::Camera, style) && !open {
                    let bus = ui.bus.clone();
                    self.begin(&bus);
                }
                let (msg, col) = match &state {
                    State::Failed(why) => (format!("Camera unavailable. {why}"), palette::TEXT_FAINT),
                    _ => ("Camera is off".to_string(), palette::TEXT_FAINT),
                };
                ui.p.text(&msg, Rect::new(r.x, btn.bottom() + 24.0, r.w, 18.0), &TextStyle::new(11.5, col).center());
            }
        }
        if p <= 0.003 {
            return;
        }

        // the view grows out of the button and fills the tab
        let view = Rect::new(lerp(btn.x, r.x, p), lerp(btn.y, r.y, p), lerp(btn.w, r.w, p), lerp(btn.h, r.h, p));
        let radius = lerp(btn.h * 0.5, if al { 30.0 } else { 22.0 }, p);
        let outer = ui.p.alpha;
        ui.p.alpha = outer * smoothstep(0.0, 0.25, p);
        ui.p.fill_rounded(view, radius, Color::hex(0x0B0B0D));
        ui.p.alpha = outer;
        let show = smoothstep(0.55, 0.95, p);
        if show > 0.01 {
            ui.p.alpha = outer * show;
            ui.p.push_clip(view);
            match &self.frame {
                Some(img) => {
                    // release the previous frame's GPU copy right away
                    if self.drawn_id != Some(img.id) {
                        if let Some(old) = self.drawn_id.replace(img.id) {
                            ui.p.forget_image(old);
                        }
                    }
                    ui.p.image_zoom(img, view, radius, 1.0, ZOOM);
                    // a quiet sweep along the bottom edge shows the camera is live
                    if al {
                        ui.p.fill_circle(view.x + 24.0, view.y + 24.0, 5.0, palette::RED);
                    } else {
                        ui.ticks_sweep(
                            Rect::new(view.cx() - 40.0, view.bottom() - 22.0, 80.0, 8.0),
                            Color::white(0.55),
                        );
                    }
                }
                None => {
                    ui.p.text(
                        "Starting camera…",
                        Rect::new(view.x, view.cy() - 22.0, view.w, 20.0),
                        &TextStyle::new(13.0, palette::TEXT_DIM).center(),
                    );
                    ui.ticks_sweep(Rect::new(view.cx() - 50.0, view.cy() + 6.0, 100.0, 10.0), Color::white(0.9));
                }
            }
            ui.p.pop_clip();
            ui.p.stroke_rounded(view.inset(0.5), radius, 1.0, Color::white(0.10));
            if open && p > 0.97 {
                let close = Rect::new(view.right() - 38.0, view.y + 10.0, 28.0, 28.0);
                if ui.icon_button(close, Icon::Close, ButtonStyle::filled(Color::black(0.55)).scale(0.5)) {
                    self.shutdown();
                    ui.fx.redraw = true;
                }
            }
            ui.p.alpha = outer;
        }
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

fn worker(gen: u32, stop: Arc<AtomicBool>, bus: Bus) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    if let Err(why) = run(gen, &stop, &bus) {
        bus.to_module(ID, Msg { gen, kind: Kind::Failed(why) });
    } else {
        bus.to_module(ID, Msg { gen, kind: Kind::Ended });
    }
}

fn step<T>(what: &str, r: windows::core::Result<T>) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {e}"))
}

fn run(gen: u32, stop: &AtomicBool, bus: &Bus) -> Result<(), String> {
    // the first camera that offers a colour stream
    let groups = step("camera list", MediaFrameSourceGroup::FindAllAsync().and_then(|o| o.join()))?;
    let mut chosen = None;
    for g in groups {
        let has_color = g
            .SourceInfos()
            .map(|infos| infos.into_iter().any(|i| i.SourceKind().is_ok_and(|k| k == MediaFrameSourceKind::Color)))
            .unwrap_or(false);
        if has_color {
            chosen = Some(g);
            break;
        }
    }
    let group = chosen.ok_or_else(|| "No camera found.".to_string())?;

    let settings = step("camera settings", MediaCaptureInitializationSettings::new())?;
    step("camera settings", settings.SetSourceGroup(&group))?;
    step("camera settings", settings.SetSharingMode(MediaCaptureSharingMode::SharedReadOnly))?;
    step("camera settings", settings.SetMemoryPreference(MediaCaptureMemoryPreference::Cpu))?;
    step("camera settings", settings.SetStreamingCaptureMode(StreamingCaptureMode::Video))?;
    let capture = step("camera", MediaCapture::new())?;
    step("camera access", capture.InitializeWithSettingsAsync(&settings).and_then(|o| o.join()))?;

    let source = step(
        "camera stream",
        capture.FrameSources().map(|m| {
            m.into_iter()
                .filter_map(|kv| kv.Value().ok())
                .find(|s| s.Info().and_then(|i| i.SourceKind()).is_ok_and(|k| k == MediaFrameSourceKind::Color))
        }),
    )?
    .ok_or_else(|| "No colour stream on this camera.".to_string())?;

    // ask for BGRA frames so every camera format ends up the same
    let subtype = step("camera format", MediaEncodingSubtypes::Bgra8())?;
    let reader: MediaFrameReader =
        step("camera reader", capture.CreateFrameReaderWithSubtypeAsync(&source, &subtype).and_then(|o| o.join()))?;
    let status = step("camera start", reader.StartAsync().and_then(|o| o.join()))?;
    if status != MediaFrameReaderStartStatus::Success {
        return Err("The camera is in use or blocked.".to_string());
    }

    while !stop.load(Ordering::Acquire) {
        if let Ok(frame) = reader.TryAcquireLatestFrame() {
            if let Some(img) = to_image(&frame) {
                bus.to_module(ID, Msg { gen, kind: Kind::Frame(img) });
            }
        }
        std::thread::sleep(FRAME_GAP);
    }

    let _ = reader.StopAsync().and_then(|o| o.join());
    let _ = capture.Close();
    Ok(())
}

/// One camera frame → a small, horizontally mirrored picture.
fn to_image(frame: &MediaFrameReference) -> Option<ImageData> {
    let bmp = frame.VideoMediaFrame().ok()?.SoftwareBitmap().ok()?;
    let bmp = if bmp.BitmapPixelFormat().ok()? == BitmapPixelFormat::Bgra8 {
        bmp
    } else {
        SoftwareBitmap::Convert(&bmp, BitmapPixelFormat::Bgra8).ok()?
    };
    let w = bmp.PixelWidth().ok()?.max(0) as usize;
    let h = bmp.PixelHeight().ok()?.max(0) as usize;
    if w == 0 || h == 0 {
        return None;
    }
    let len = (w * h * 4) as u32;
    let buf = Buffer::Create(len).ok()?;
    bmp.CopyToBuffer(&buf).ok()?;
    buf.SetLength(len).ok()?;
    let mut src = vec![0u8; len as usize];
    DataReader::FromBuffer(&buf).ok()?.ReadBytes(&mut src).ok()?;
    Some(shrink_mirrored(&src, w, h))
}

/// Box-average downscale to at most `MAX_W` wide, flipped left to right,
/// fully opaque (camera frames have no useful alpha).
fn shrink_mirrored(src: &[u8], w: usize, h: usize) -> ImageData {
    let step = w.div_ceil(MAX_W).max(1);
    let (ow, oh) = ((w / step).max(1), (h / step).max(1));
    let mut out = vec![0u8; ow * oh * 4];
    let area = (step * step) as u32;
    for y in 0..oh {
        for x in 0..ow {
            // the block of source pixels behind this output pixel (mirrored left to right)
            let bx = (ow - 1 - x) * step;
            let (mut b, mut g, mut r) = (0u32, 0u32, 0u32);
            for dy in 0..step {
                let sy = (y * step + dy).min(h - 1);
                for dx in 0..step {
                    let s = (sy * w + (bx + dx).min(w - 1)) * 4;
                    b += src[s] as u32;
                    g += src[s + 1] as u32;
                    r += src[s + 2] as u32;
                }
            }
            let d = (y * ow + x) * 4;
            out[d] = (b / area) as u8;
            out[d + 1] = (g / area) as u8;
            out[d + 2] = (r / area) as u8;
            out[d + 3] = 255;
        }
    }
    ImageData::from_pbgra(ow as u32, oh as u32, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_and_shrinks() {
        // 4x1 picture: pixels 0..3 with distinct blue values
        let src: Vec<u8> = (0..4u8).flat_map(|i| [i * 10, 0, 0, 0]).collect();
        let img = shrink_mirrored(&src, 4, 1);
        assert_eq!((img.width, img.height), (4, 1));
        let blues: Vec<u8> = img.pixels.chunks(4).map(|p| p[0]).collect();
        assert_eq!(blues, vec![30, 20, 10, 0]);
        assert!(img.pixels.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn large_frames_are_shrunk() {
        let src = vec![0u8; 1280 * 720 * 4];
        let img = shrink_mirrored(&src, 1280, 720);
        assert!(img.width as usize <= MAX_W);
    }
}
