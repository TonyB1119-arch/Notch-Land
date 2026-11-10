//! The notch shell: owns the window, renderer, modules and the state
//! machine (idle → peek → expanded), turns state into spring targets and
//! draws a frame whenever something moves.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Globalization::{GetTimeFormatEx, TIME_NOSECONDS};
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, SetWindowRgn, HDC,
    HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::DataExchange::AddClipboardFormatListener;
use windows::Win32::System::Power::RegisterPowerSettingNotification;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::SystemServices::{GUID_ACDC_POWER_SOURCE, GUID_BATTERY_PERCENTAGE_REMAINING};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, NIN_SELECT, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
};
const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1;
const WM_MOUSELEAVE: u32 = 0x02A3;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::anim::{lerp, smoothstep, Spring};
use crate::bus::{Bus, Msg};
use crate::config::{self, Config, LoadResult};
use crate::gfx::{self, icons, Gfx, Icon, Painter, Res, Snapshot, TextStyle};
use crate::host::Host;
use crate::hotkeys::{self, Action};
use crate::modules::{
    self, Activity, CardSize, Cx, Effects, Indicator, Key, Module, ModuleId, Peek, Slot, SystemEvent, Trailing,
};
use crate::pacer::Pacer;
use crate::sys::dragdrop::{self, DropEvent};
use crate::tray::{self, MenuState, Tray};
use crate::ui::{gkey, ButtonStyle, Input, Ui};
use crate::util::{palette, Color, Rect, SendHwnd};
use crate::window::{WM_APP_BUS, WM_APP_DRAG, WM_APP_FOREGROUND, WM_APP_FRAME, WM_APP_GLASS, WM_APP_TRAY};
use crate::{ipc, log, sys};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK,
    RID_INPUT, RIM_TYPEKEYBOARD,
};

const HEADER_H: f32 = 54.0;

/// Height of the header (and the tab strip under it in the Bloom look).
fn top_h() -> f32 {
    if crate::ui::look_alcove() {
        crate::alcove::TOP
    } else if crate::ui::look_tuna() {
        crate::tuna::TOP_H
    } else if crate::ui::look_bloom() {
        crate::bloom::TOP_H
    } else {
        HEADER_H
    }
}

/// Space left and right of the expanded content, and below it.
fn side_pad() -> f32 {
    if crate::ui::look_alcove() {
        crate::alcove::LEFT
    } else if crate::ui::look_tuna() {
        crate::tuna::PAD
    } else if crate::ui::look_bloom() {
        crate::bloom::SIDE
    } else {
        PAD
    }
}

/// Space right of the content (Alcove leaves room for its page dots).
fn right_pad() -> f32 {
    if crate::ui::look_alcove() {
        crate::alcove::RIGHT
    } else {
        side_pad()
    }
}

fn bottom_pad() -> f32 {
    if crate::ui::look_alcove() {
        crate::alcove::BOTTOM
    } else if crate::ui::look_tuna() {
        crate::tuna::BOTTOM
    } else if crate::ui::look_bloom() {
        crate::bloom::BOTTOM
    } else {
        BOTTOM_PAD
    }
}
const PAD: f32 = 28.0;
const BOTTOM_PAD: f32 = 28.0;
const MARGIN: f32 = 48.0;
const WIN_H: f32 = 600.0;
const HOME: ModuleId = "home";
const TIMER_WAKE: usize = 1;
const HOME_HEIGHT: f32 = 176.0;
/// With four tiles in the right column the home page grows a little.
const HOME_HEIGHT_TALL: f32 = 200.0;
const HOME_TILES: usize = 4;

thread_local! {
    /// When the wheel last flipped a tab (a touchpad would otherwise race through all of them).
    static WHEEL_FLIP: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}
/// Height of the clock strip under the media player on the home page.
const CLOCK_STRIP_H: f32 = 52.0;
/// Quiet time before GPU caches are released.
/// How long the notch must be idle before GPU scratch memory is handed back.
/// Trimming right after every burst made the next open re-allocate everything.
const TRIM_DELAY: Duration = Duration::from_secs(10);
/// Minimum time between purely ambient frames (see `frame`).
const AMBIENT_GAP: Duration = Duration::from_millis(12);
/// Live glass carries on this long after the media stops (pausing a video for a moment must not
/// switch it off and on again).
const LIVE_LINGER: Duration = Duration::from_secs(6);

static NOTCH_HWND: AtomicIsize = AtomicIsize::new(0);
/// A peek would be visible right now (not hidden, not expanded).
static PEEKS_SHOWN: AtomicBool = AtomicBool::new(false);

/// Whether a peek pushed now would actually be seen. Changes are also
/// broadcast as `SystemEvent::PeeksShown`.
pub fn peeks_shown() -> bool {
    PEEKS_SHOWN.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Idle,
    Peek,
    Expanded,
}

struct ActivePeek {
    peek: Peek,
    until: Instant,
    /// The content that was showing before a quiet follow-up swapped it, and when that happened.
    swap: Option<(Instant, Box<Peek>)>,
}

/// A small round bubble that pops out beside the notch, stays a moment and is pulled back in.
#[derive(Clone, Copy)]
struct Bubble {
    at: Instant,
    icon: Icon,
    color: Color,
    /// Drawn with a slash through the icon (caps lock off, microphone muted).
    off: bool,
    /// Page opened when the bubble is clicked.
    page: Option<ModuleId>,
    in_played: bool,
    out_played: bool,
}

/// How many bubbles can sit side by side.
const MAX_BUBBLES: usize = 3;
/// Seconds a bubble takes to pop out, when it starts to leave, and when it is gone.
const BUBBLE_OUT: f32 = 0.3;
const BUBBLE_HOLD: f32 = 1.1;
const BUBBLE_END: f32 = 1.5;

/// Add a bubble starting at `at`; the oldest goes when there are too many.
fn push_bubble(list: &mut Vec<Bubble>, at: Instant, icon: Icon, color: Color, off: bool, page: Option<ModuleId>) {
    list.push(Bubble { at, icon, color, off, page, in_played: false, out_played: false });
    if list.len() > MAX_BUBBLES {
        list.remove(0);
    }
}

/// How far out (0..1+), how visible, how big and how much room a bubble `t` seconds old takes.
fn bubble_curve(t: f32) -> Option<(f32, f32, f32, f32)> {
    if t >= BUBBLE_END {
        return None;
    }
    Some(if t < BUBBLE_OUT {
        let u = t / BUBBLE_OUT;
        // ease-out with a touch of overshoot
        let c = 1.5;
        let e = 1.0 + (c + 1.0) * (u - 1.0).powi(3) + c * (u - 1.0).powi(2);
        (e, u.min(1.0), 0.6 + 0.4 * u, 1.0)
    } else if t < BUBBLE_HOLD {
        (1.0, 1.0, 1.0, 1.0)
    } else {
        let u = (t - BUBBLE_HOLD) / (BUBBLE_END - BUBBLE_HOLD);
        let e = u * u * u;
        (1.0 - e, 1.0 - u * u, 1.0 - 0.55 * e, 1.0 - e)
    })
}

/// Peeks that Alcove shows as one small bubble instead: caps lock and the microphone switch.
fn small_bubble(p: &Peek) -> bool {
    match p.key {
        Some("osd-mic") => true,
        Some("locks") => matches!(&p.trailing, Trailing::Level { text: Some(t), .. } if t.starts_with("Caps")),
        _ => false,
    }
}

/// Whether such a bubble shows the "off" state (a slash through the icon).
fn bubble_off(p: &Peek) -> bool {
    match p.key {
        Some("osd-mic") => p.subtitle == "Muted",
        Some("locks") => matches!(&p.trailing, Trailing::Level { frac, .. } if *frac < 0.5),
        _ => false,
    }
}

/// Alcove shows some notices as one big round dial. This is how much taller than the notch the
/// dial is for this notice, or `None` when it is shown the usual way. Volume and brightness can
/// be switched back to the line (Settings, General); battery and Bluetooth are always dials.
fn dial_height(p: &Peek, cfg: &Config) -> Option<f32> {
    if !crate::ui::look_alcove() {
        return None;
    }
    // everything is the wide pill now (volume, brightness, battery, Bluetooth)
    let _ = (p, cfg);
    None
}

/// How wide each page wants the open notch, as a share of the widest setting (Appearance, width).
/// A short list needs far less room than the dashboard, so the notch only opens as wide as the
/// page it shows.
fn page_share(id: ModuleId) -> f32 {
    match id {
        "notifications" | "timer" => 0.72,
        "downloads" | "clipboard" | "mirror" | "notes" => 0.74,
        "control" | "shelf" => 0.76,
        "media" | "stats" | "weather" | "mixer" => 0.78,
        "calendar" | "ports" | "voice" | "ai" => 0.84,
        "gemini" | "alcove-launcher" => 0.86,
        "home" => 0.80,
        _ => 1.0,
    }
}

/// How wide the open notch is for a page (the widest setting outside the Alcove look).
fn page_width_of(cfg: &Config, id: ModuleId) -> f32 {
    let max = cfg.appearance.expanded_width;
    if !crate::ui::look_alcove() {
        return max;
    }
    (max * page_share(id)).max(420.0_f32.min(max))
}

/// The height a page wants under the header, for the given content width.
fn page_content_h(cfg: &Config, modules: &[Box<dyn Module>], started: &[bool], id: ModuleId, content_w: f32) -> f32 {
    if id == HOME && crate::ui::look_alcove() {
        // taller while the mirror's camera view is open
        if cfg.modules.is_enabled("mirror") && modules::mirror::showing() {
            crate::alcove::HOME_MIRROR_H
        } else {
            crate::alcove::HOME_H
        }
    } else if id == HOME && crate::ui::look_tuna() {
        crate::tuna::HOME_H
    } else if id == HOME && crate::ui::look_bloom() {
        crate::bloom::HOME_H
    } else if id == HOME {
        if home_tiles(modules, started, cfg).len() >= HOME_TILES {
            HOME_HEIGHT_TALL
        } else {
            HOME_HEIGHT
        }
    } else {
        modules
            .iter()
            .position(|m| m.id() == id)
            .map(|i| modules[i].page_height(content_w))
            .unwrap_or(HOME_HEIGHT)
            .clamp(60.0, WIN_H - MARGIN - top_h() - bottom_pad())
    }
}

/// The size of the open notch showing a page: (width, height). Every page is laid out at its own
/// size, also while another page is sliding in or out, so nothing reflows to the wrong width.
fn page_size(cfg: &Config, modules: &[Box<dyn Module>], started: &[bool], id: ModuleId) -> (f32, f32) {
    let w = page_width_of(cfg, id);
    (w, top_h() + page_content_h(cfg, modules, started, id, w - side_pad() - right_pad()) + bottom_pad())
}

/// The area a page draws in, for an open notch of this size centred on `cx`.
fn page_content(cx: f32, y: f32, (w, h): (f32, f32)) -> Rect {
    let full = Rect::new(cx - w * 0.5, y, w, h);
    Rect::new(full.x + side_pad(), full.y + top_h(), full.w - side_pad() - right_pad(), full.h - top_h() - bottom_pad())
}

/// How long a quiet peek swap (next track) takes to play.
const SWAP_SECS: f32 = 0.55;

/// Spring response of the notch body when it grows (seconds); it shrinks a bit faster.
const BODY_RESPONSE: f32 = 0.42;
/// How long the body waits for the content to fade before it shrinks (seconds).
const CLOSE_HOLD: f32 = 0.09;
// ---------------------------------------------------------------------------
// The soft blur of content coming and going (Alcove). Timed frame by frame from a reference
// recording (30 fps): each curve is a list of (seconds, value) points joined by straight lines.
// Opacity is 0..1; blur is the Gaussian radius in px at scale 1 (multiplied by BLUR_K).
// ---------------------------------------------------------------------------
/// Overall strength of the blur (1.0 = as strong as the reference).
const BLUR_K: f32 = 0.85;
// switching pages: the old page blurs and fades fast, the new one clears from a blur
const PAGE_OLD_A: [(f32, f32); 6] = [(0.0, 1.0), (0.033, 0.95), (0.067, 0.7), (0.1, 0.52), (0.133, 0.38), (0.17, 0.0)];
const PAGE_OLD_S: [(f32, f32); 6] = [(0.0, 0.0), (0.033, 1.5), (0.067, 5.0), (0.1, 8.0), (0.133, 9.0), (0.17, 9.0)];
const PAGE_NEW_A: [(f32, f32); 10] = [
    (0.0, 0.0),
    (0.033, 0.49),
    (0.067, 0.5),
    (0.1, 0.63),
    (0.133, 0.77),
    (0.167, 0.84),
    (0.2, 0.91),
    (0.233, 0.97),
    (0.267, 0.99),
    (0.3, 1.0),
];
const PAGE_NEW_S: [(f32, f32); 11] = [
    (0.0, 12.0),
    (0.033, 12.0),
    (0.067, 10.0),
    (0.1, 7.0),
    (0.133, 5.0),
    (0.167, 3.0),
    (0.2, 2.0),
    (0.233, 1.5),
    (0.267, 1.0),
    (0.3, 0.5),
    (0.367, 0.0),
];
/// How much slower the page switch plays than the reference (1.0 = same speed).
const PAGE_T: f32 = 1.7;
const PAGE_SECS: f32 = 0.37 * PAGE_T;
/// How much faster the close plays than the reference (curve time = real time / this).
const CLOSE_T: f32 = 0.8;
// the open notch's content appearing (after the body has grown) ...
const IN_A: [(f32, f32); 5] = [(0.0, 0.0), (0.033, 0.44), (0.067, 0.78), (0.1, 0.92), (0.133, 1.0)];
const IN_S: [(f32, f32); 10] = [
    (0.0, 16.0),
    (0.033, 16.0),
    (0.067, 8.0),
    (0.1, 7.0),
    (0.133, 6.0),
    (0.167, 5.0),
    (0.2, 4.0),
    (0.233, 1.5),
    (0.267, 0.5),
    (0.3, 0.0),
];
const IN_END: f32 = 0.3;
const OPEN_DELAY: f32 = 0.08;
/// How much faster the content comes in than the reference (curve time = real time / this).
const IN_T: f32 = 0.75;
/// The body grows this much quicker when the notch opens (a spring response factor; smaller is faster).
const OPEN_SPEED: f32 = 0.75;
// ... and going: it blurs for a long moment, vanishes, and only then does the body snap small
const OUT_A: [(f32, f32); 11] = [
    (0.0, 1.0),
    (0.033, 0.95),
    (0.1, 0.9),
    (0.267, 0.88),
    (0.33, 0.85),
    (0.43, 0.79),
    (0.53, 0.7),
    (0.63, 0.67),
    (0.7, 0.45),
    (0.77, 0.12),
    (0.83, 0.0),
];
const OUT_S: [(f32, f32); 12] = [
    (0.0, 0.0),
    (0.033, 1.5),
    (0.067, 2.0),
    (0.1, 3.0),
    (0.167, 5.0),
    (0.2, 7.0),
    (0.233, 8.0),
    (0.267, 9.0),
    (0.3, 10.0),
    (0.4, 12.0),
    (0.53, 14.0),
    (0.83, 14.0),
];
const OUT_END: f32 = 0.83;
/// When the body starts to shrink after the open notch is told to close.
const OUT_BODY: f32 = 0.57;
/// Content that comes back after a close (the media in the notch) waits for the body to be small.
const RETURN_AFTER_CLOSE: f32 = 0.78;
// peeks and the compact media content: the same idea, smaller and quicker
const PIN_A: [(f32, f32); 3] = [(0.0, 0.0), (0.04, 0.5), (0.1, 1.0)];
const PIN_S: [(f32, f32); 7] = [(0.0, 8.0), (0.04, 8.0), (0.08, 5.0), (0.12, 3.0), (0.16, 1.5), (0.22, 0.5), (0.26, 0.0)];
const PIN_END: f32 = 0.26;
const PEEK_DELAY: f32 = 0.06;
const POUT_A: [(f32, f32); 5] = [(0.0, 1.0), (0.05, 0.92), (0.15, 0.8), (0.21, 0.5), (0.27, 0.0)];
const POUT_S: [(f32, f32); 5] = [(0.0, 0.0), (0.05, 2.0), (0.1, 4.0), (0.16, 6.0), (0.27, 7.0)];
const POUT_END: f32 = 0.27;
/// When a peek's body starts to shrink after the peek has ended.
const POUT_BODY: f32 = 0.22;

/// How a piece of content looks right now while it comes or goes.
#[derive(Clone, Copy)]
struct Layer {
    alpha: f32,
    /// blur radius in reference px (before `BLUR_K` and the display scale)
    sigma: f32,
}

/// What each kind of content is doing this frame; None when it is settled (crisp, or not there).
#[derive(Clone, Copy, Default)]
struct VeilFrame {
    exp: Option<Layer>,
    peek: Option<Layer>,
    act: Option<Layer>,
}

/// When a piece of content last started to appear (`on`) or disappear, so its blur can be timed.
#[derive(Clone, Copy)]
struct Veil {
    on: bool,
    at: Instant,
}

impl Veil {
    fn idle() -> Veil {
        // long ago: nothing is animating at start-up
        Veil { on: false, at: Instant::now().checked_sub(Duration::from_secs(60)).unwrap_or_else(Instant::now) }
    }
}

/// The value of a curve at `t` seconds.
fn curve(pts: &[(f32, f32)], t: f32) -> f32 {
    if t <= pts[0].0 {
        return pts[0].1;
    }
    for w in pts.windows(2) {
        if t <= w[1].0 {
            let k = (t - w[0].0) / (w[1].0 - w[0].0).max(1e-6);
            return w[0].1 + (w[1].1 - w[0].1) * k;
        }
    }
    pts[pts.len() - 1].1
}

/// A pair of curves `t` seconds in; None once it is over.
fn run(a: &[(f32, f32)], s: &[(f32, f32)], t: f32, end: f32) -> Option<Layer> {
    if t >= end {
        return None;
    }
    let t = t.max(0.0);
    Some(Layer { alpha: curve(a, t), sigma: curve(s, t) })
}

/// The old and the new page, `t` seconds after a page switch.
fn page_layers(t: f32, m: f32) -> (Option<Layer>, Layer) {
    let t = t / (PAGE_T * m);
    let old = run(&PAGE_OLD_A, &PAGE_OLD_S, t, PAGE_OLD_A[5].0);
    let new = Layer { alpha: curve(&PAGE_NEW_A, t), sigma: curve(&PAGE_NEW_S, t) };
    (old, new)
}

struct Anim {
    w: Spring,
    h: Spring,
    r: Spring,
    ear: Spring,
    glass: Spring,
    compact: Spring,
    peek: Spring,
    exp: Spring,
    shown: Spring,
    /// Secondary live-activity bubble (0 hidden .. 1 fully out).
    sec: Spring,
    /// Alcove media: cover and level bars sit in their own bubbles beside a blank notch (0..1).
    split: Spring,
    /// Page switch progress 0..1 (slow and soft on purpose).
    page: Spring,
}

impl Anim {
    fn new(w: f32, h: f32) -> Self {
        Self {
            w: Spring::new(w * 0.6, 0.45, 0.85),
            h: Spring::new(0.0, 0.45, 0.85),
            r: Spring::new(10.0, 0.45, 0.9),
            ear: Spring::new(6.0, 0.4, 1.0),
            glass: Spring::unit(0.0, 0.3, 1.0),
            compact: Spring::unit(0.0, 0.18, 1.0),
            peek: Spring::unit(0.0, 0.2, 1.0),
            exp: Spring::unit(0.0, 0.22, 1.0),
            shown: Spring::unit(0.0, 0.5, 0.8),
            sec: Spring::unit(0.0, 0.45, 0.8),
            split: Spring::unit(0.0, 0.5, 0.78),
            page: Spring::unit(1.0, 0.64, 1.0),
        }
        .with_h(h)
    }
    fn with_h(mut self, _h: f32) -> Self {
        self.h.snap(0.0);
        self
    }
    fn step(&mut self, dt: f32) -> bool {
        let mut m = false;
        for s in [
            &mut self.w,
            &mut self.h,
            &mut self.r,
            &mut self.ear,
            &mut self.glass,
            &mut self.compact,
            &mut self.peek,
            &mut self.exp,
            &mut self.shown,
            &mut self.sec,
            &mut self.split,
            &mut self.page,
        ] {
            m |= s.step(dt);
        }
        m
    }

    /// True while every spring is creeping its last bit into place: within a hair of where it is
    /// going and moving by a fraction of a pixel a frame.
    fn creeping(&self) -> bool {
        let px = |s: &Spring| (s.value - s.target).abs() < 3.0 && s.velocity.abs() < 30.0;
        let un = |s: &Spring| (s.value - s.target).abs() < 0.03 && s.velocity.abs() < 0.3;
        px(&self.w)
            && px(&self.h)
            && px(&self.r)
            && px(&self.ear)
            && un(&self.glass)
            && un(&self.compact)
            && un(&self.peek)
            && un(&self.exp)
            && un(&self.shown)
            && un(&self.sec)
            && un(&self.split)
            && un(&self.page)
    }
}

/// Cache key of the glass rim: position, size, corner radius and scale, in quarter pixels.
type RimKey = (i32, i32, i32, i32, i32, usize);

/// A glass picture made while the notch moves carries 10 in its last field (reduced detail).
const RIM_LOW: usize = 10;

/// The two pictures belong to the same notch size (give or take a pixel).
fn rim_near(a: &RimKey, b: &RimKey) -> bool {
    // (whole pixels while moving, quarter pixels once settled)
    let (ua, ub) = (if a.5 >= RIM_LOW { 1 } else { 4 }, if b.5 >= RIM_LOW { 1 } else { 4 });
    let tol = if ua == ub { 0 } else { 4 };
    a.4 == b.4 && [(a.0, b.0), (a.1, b.1), (a.2, b.2), (a.3, b.3)].iter().all(|&(x, y)| (x * ub - y * ua).abs() <= tol)
}

/// The few look settings the drawing code reads every frame, copied out so no text is cloned.
#[derive(Clone, Copy)]
struct Look {
    shadow: bool,
    blur: bool,
    expanded_opacity: f32,
    collapsed_opacity: f32,
    notch_height: f32,
    glass_look: bool,
}

struct Targets {
    w: f32,
    h: f32,
    r: f32,
    ear: f32,
    glass: f32,
    compact: bool,
    peek: bool,
    exp: bool,
    shown: bool,
    sec: bool,
}

pub struct App {
    hwnd: HWND,
    cfg: Config,
    gfx: Gfx,
    res: Res,
    host: Host,
    /// The open notch drawn once while it fades in or out (see `prepare_snapshots`).
    snap_all: Option<Snapshot>,
    /// The old and the new page, drawn once while the page slide runs.
    snap_pages: Option<(Snapshot, Snapshot)>,
    /// The bottom strip of the open page, for the soft blur along the bottom edge (Alcove).
    snap_band: Option<Snapshot>,
    /// A peek and the compact live activity while they fade in or out, redrawn into re-used pictures
    /// each frame so they can be shown going soft (see `prepare_small`). None whenever they are crisp.
    snap_peek: Option<Snapshot>,
    /// When the open notch, the peeks and the compact activity last came or went (for the soft blur).
    veil_exp: Veil,
    veil_peek: Veil,
    veil_act: Veil,
    /// What they are doing this frame.
    vf: VeilFrame,
    /// The peek that has just ended, kept while it blurs away.
    peek_out: Option<ActivePeek>,
    /// When the page last changed.
    page_at: Instant,
    snap_act: Option<Snapshot>,
    /// The widest the notch gets during the current move: the pictures are made this size, so their buffers are re-used.
    small_dim: (f32, f32),
    /// Where the compositor-drawn bars go, as worked out when the activity picture was drawn.
    small_bars: Option<(Rect, Color, bool)>,
    /// Set if the blended bottom strip could not be made: the notch then stays solid, no see-through edge.
    band_failed: bool,
    /// Alcove glass edge: a copy of the screen behind the notch, taken as it opens, the rim worked
    /// out from it, when the notch last went idle, and when a copy was last tried.
    refr_shot: Option<crate::refract::Shot>,
    refr_rim: Option<(RimKey, gfx::RimPic)>,
    /// The notch's shape at the previous frame (to tell whether it is still changing size).
    rim_geo: Option<(i32, i32, i32, i32, i32)>,
    /// When the glass edge was last rebuilt (lighter animations spread the rebuilds out).
    rim_t: Option<Instant>,
    /// How long a glass rebuild takes lately (smoothed); sets how often the moving glass is redone.
    rim_cost: Duration,
    /// How far (in logical px of width + height) the notch had to go when the current move began.
    rim_far: f32,
    /// The shape (and its quantised key) the current rim was made for.
    rim_last: Option<((i32, i32, i32, i32, i32), crate::refract::Shape)>,
    /// The glass settings the current rim was made with (a change makes it again).
    rim_tune: Option<crate::refract::Tune>,
    /// The strip / see-through settings the current rim was made with (quantised).
    rim_extra: Option<(i32, i32, i32)>,
    /// Live glass: a refresh running on a background thread.
    live_rx: Option<Receiver<Option<crate::refract::Fetched>>>,
    /// When the next refresh is due.
    live_next: Option<Instant>,
    /// How many refreshes in a row found nothing new.
    live_same: u8,
    /// Fingerprint of the current screen copy.
    live_sum: u64,
    /// This Windows cannot hide the notch from screen copies: no live glass.
    live_dead: bool,
    /// When media was last seen playing.
    play_seen: Option<Instant>,
    /// The notch is hidden from screen copies right now.
    excl: bool,
    /// The previous rim picture, while the new one melts in over it.
    rim_old: Option<(RimKey, gfx::RimPic)>,
    rim_x: f32,
    /// The notch's rectangle when the glass pictures were made (they are stretched to fit the size now).
    rim_rect: Option<Rect>,
    rim_old_rect: Option<Rect>,
    rim_last_rect: Option<Rect>,
    refr_retry: Option<Instant>,
    /// The window in front when the copy was taken; if another comes forward the copy is out of date.
    refr_fg: isize,
    refr_dead: bool,
    /// The rim may be shown (right size, no page change running), how strongly it is shown now
    /// (it fades in and out instead of popping), and when that was last stepped.
    rim_ok: bool,
    rim_k: f32,
    rim_at: Instant,
    /// How much of the frosted layer behind the notch is let through (0 while the notch changes
    /// size, because that layer is moved by Windows a moment later and would trail the edge) and
    /// the body size at the last frame.
    bd_k: f32,
    bd_prev: (f32, f32),
    /// When the notch last changed size.
    bd_move: Instant,
    /// The last frame only asked for a slow animation: every other frame is skipped.
    slow_only: bool,
    /// When the blur strip first appeared (it fades in from here).
    band_since: Option<Instant>,
    pacer: Pacer,
    bus: Bus,
    rx: Receiver<Msg>,
    modules: Vec<Box<dyn Module>>,
    started: Vec<bool>,
    /// Tick deadline each module asked for when the wake timer was last armed.
    /// `next_tick()` is relative to "now", so it must be cached to ever come due.
    tick_at: Vec<Option<Instant>>,

    monitor: HMONITOR,
    scale: f32,
    win_w: f32,
    win_h: f32,
    win_px: (i32, i32, i32, i32),

    mode: Mode,
    page: ModuleId,
    /// Page being faded out while the next one slides in, and the slide direction.
    page_from: Option<(ModuleId, f32)>,
    peek: Option<ActivePeek>,
    /// Alcove: a finished clipboard / notification / download peek lingers as a bubble, then is sucked in.
    bubbles: Vec<Bubble>,
    /// A bubble to show once the notch has closed again (files just parked on the shelf).
    pending_bubble: Option<(Icon, Color)>,
    /// When the notch last opened (drives the light sweep across it).
    opened_at: Option<Instant>,
    peek_queue: VecDeque<Peek>,

    hovering: bool,
    hover_since: Option<Instant>,
    leave_since: Option<Instant>,
    /// Opened by click/hotkey/CLI: auto-close if the mouse never comes by.
    auto_close: Option<Instant>,
    tracking: bool,
    input: Input,
    keyboard: bool,
    high_surrogate: Option<u16>,

    anim: Anim,
    last_frame: Instant,
    dirty: bool,
    /// When to release GPU caches (set when a frame burst ends).
    trim_at: Option<Instant>,
    dock: Option<crate::dock::Dock>,
    launcher: Option<crate::launcher::Launcher>,
    radial: Option<crate::radial::Radial>,
    /// The radial menu's option hid the Windows taskbar.
    radial_hid: bool,
    /// No spring is moving: the backdrop blur may be shown.
    settled: bool,
    region: Option<(i32, i32, i32, i32)>,
    region_anim: Option<Rect>,
    fs_hidden: bool,
    user_hidden: bool,
    shown_once: bool,

    tray: Option<Tray>,
    hotkeys: Vec<hotkeys::Registered>,
    msg_taskbar_created: u32,
    accent: Color,
    hooks: Vec<HWINEVENTHOOK>,
    loc_hook: Option<HWINEVENTHOOK>,
    loc_pid: u32,
    activity: Option<Activity>,
    secondary: Option<Activity>,
    indicators: Vec<Indicator>,
    /// Second shown on the home flip clock when it was last drawn.
    clock_min: u16,
    /// Virtual key of the dictation hotkey (its release ends push-to-talk).
    dictate_vk: u16,
    /// Watched keys currently held (raw input repeats are dropped).
    raw_down: Vec<u16>,
    /// Files are being dragged over the notch (keeps it open).
    drag_hover: bool,
    /// Files to drag out once the window proc is outside the app borrow.
    pending_drag: Option<Vec<String>>,
    /// Frame statistics for the DN_DEBUG log.
    /// Frames in the current burst: (start, spring, animate, dirty).
    stats: (Option<Instant>, u32, u32, u32),
    debug: bool,
    /// The slow last stretch of a move (see `Anim::creeping`): every other frame is left out.
    tail: bool,
    /// DN_DEBUG timings of the current burst, in microseconds: [draw total, draw worst, frames drawn,
    /// pictures, bottom blur, glass check, glass rebuilds total, rebuild count, tail frames skipped].
    tm: [u64; 9],
}

impl App {
    pub fn new(hwnd: HWND, cfg: Config) -> Result<Box<App>> {
        let gfx = Gfx::new()?;
        let res = Res::new(&gfx)?;
        let host = Host::new(hwnd, &gfx, 64, 64)?;
        let shwnd = SendHwnd::new(hwnd);
        let pacer = Pacer::new(shwnd);
        let (bus, rx) = Bus::new(shwnd);
        let modules = modules::all();
        let n = modules.len();
        NOTCH_HWND.store(hwnd.0 as isize, Ordering::Release);
        crate::ui::set_white_beams(cfg.appearance.white_beams);
        crate::ui::set_look(&cfg.appearance.look);
        crate::ui::set_glow(cfg.glass.glow, cfg.glass.glow_amount);
        crate::ui::set_mono_icons(cfg.appearance.mono_icons);
        let anim = Anim::new(cfg.appearance.notch_width, cfg.appearance.notch_height);
        Ok(Box::new(App {
            hwnd,
            gfx,
            res,
            host,
            snap_all: None,
            snap_pages: None,
            snap_band: None,
            snap_peek: None,
            veil_exp: Veil::idle(),
            veil_peek: Veil::idle(),
            veil_act: Veil::idle(),
            vf: VeilFrame::default(),
            peek_out: None,
            page_at: Instant::now(),
            snap_act: None,
            small_dim: (0.0, 0.0),
            small_bars: None,
            band_failed: false,
            refr_shot: None,
            refr_rim: None,
            rim_geo: None,
            rim_t: None,
            rim_cost: Duration::from_millis(12),
            rim_far: 0.0,
            rim_last: None,
            rim_tune: None,
            rim_extra: None,
            live_rx: None,
            live_next: None,
            live_same: 0,
            live_sum: 0,
            live_dead: false,
            play_seen: None,
            excl: false,
            rim_old: None,
            rim_x: 1.0,
            rim_rect: None,
            rim_old_rect: None,
            rim_last_rect: None,
            refr_retry: None,
            refr_fg: 0,
            refr_dead: false,
            rim_ok: false,
            rim_k: 0.0,
            rim_at: Instant::now(),
            bd_k: 1.0,
            bd_prev: (0.0, 0.0),
            bd_move: Instant::now(),
            slow_only: false,
            band_since: None,
            pacer,
            bus,
            rx,
            modules,
            started: vec![false; n],
            tick_at: vec![None; n],
            monitor: HMONITOR::default(),
            scale: 1.0,
            win_w: 0.0,
            win_h: 0.0,
            win_px: (0, 0, 0, 0),
            mode: Mode::Idle,
            page: HOME,
            page_from: None,
            peek: None,
            bubbles: Vec::new(),
            pending_bubble: None,
            opened_at: None,
            peek_queue: VecDeque::new(),
            hovering: false,
            hover_since: None,
            leave_since: None,
            auto_close: None,
            tracking: false,
            input: Input::default(),
            keyboard: false,
            high_surrogate: None,
            anim,
            last_frame: Instant::now(),
            dirty: true,
            trim_at: None,
            dock: None,
            launcher: None,
            radial: None,
            radial_hid: false,
            settled: true,
            region: None,
            region_anim: None,
            fs_hidden: false,
            user_hidden: false,
            shown_once: false,
            tray: None,
            hotkeys: Vec::new(),
            msg_taskbar_created: unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) },
            accent: Color::white(0.92),
            hooks: Vec::new(),
            loc_hook: None,
            loc_pid: 0,
            activity: None,
            secondary: None,
            indicators: Vec::new(),
            clock_min: 99,
            dictate_vk: 0,
            raw_down: Vec::new(),
            drag_hover: false,
            pending_drag: None,
            stats: (None, 0, 0, 0),
            debug: std::env::var_os("DN_DEBUG").is_some(),
            tail: false,
            tm: [0; 9],
            cfg,
        }))
    }

    /// Second-stage init once the app is reachable from the window proc.
    pub fn start(&mut self) {
        self.place();
        self.dock_apply();
        self.sync_taskbar();
        self.tray = Some(Tray::new(self.hwnd));
        self.register_hotkeys();
        unsafe {
            let _ = AddClipboardFormatListener(self.hwnd);
            // tells us when the PC is locked / unlocked (the greeting)
            let _ = WTSRegisterSessionNotification(self.hwnd, NOTIFY_FOR_THIS_SESSION);
            let h = HANDLE(self.hwnd.0);
            let _ = RegisterPowerSettingNotification(h, &GUID_ACDC_POWER_SOURCE, DEVICE_NOTIFY_WINDOW_HANDLE);
            let _ =
                RegisterPowerSettingNotification(h, &GUID_BATTERY_PERCENTAGE_REMAINING, DEVICE_NOTIFY_WINDOW_HANDLE);
            let fg = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(win_event_cb),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            if !fg.is_invalid() {
                self.hooks.push(fg);
            }
            // Keyboard raw input in the background: lock-key toggles and the
            // release of the dictation hotkey. Nothing is blocked or recorded.
            let rid =
                RAWINPUTDEVICE { usUsagePage: 0x01, usUsage: 0x06, dwFlags: RIDEV_INPUTSINK, hwndTarget: self.hwnd };
            if let Err(e) = RegisterRawInputDevices(&[rid], std::mem::size_of::<RAWINPUTDEVICE>() as u32) {
                log!("raw input unavailable: {e}");
            }
        }
        dragdrop::register_target(self.hwnd, |ev| crate::window::with_app(|a| a.on_drop_event(ev)).unwrap_or(false));
        sys::watch_config(self.bus.clone());
        if self.cfg.general.start_with_windows != sys::is_autostart() {
            sys::set_autostart(self.cfg.general.start_with_windows);
        }
        self.sync_modules();
        self.on_foreground_changed();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        self.request_frame();
        log!("started: scale={} monitor={:?}", self.scale, self.win_px);
    }

    pub fn shutdown(&mut self) {
        for (i, m) in self.modules.iter_mut().enumerate() {
            if self.started[i] {
                m.stop();
                self.started[i] = false;
            }
        }
        for h in self.hooks.drain(..) {
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
        if let Some(h) = self.loc_hook.take() {
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
        hotkeys::unregister_all(self.hwnd);
        if self.radial_hid {
            self.radial_hid = false;
            crate::taskbar::recover_taskbar();
        }
        unsafe {
            let _ = WTSUnRegisterSessionNotification(self.hwnd);
        }
        dragdrop::revoke_target(self.hwnd);
        self.tray = None;
        self.pacer.stop();
    }

    // -----------------------------------------------------------------------
    // Message handling
    // -----------------------------------------------------------------------

    pub fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_APP_FRAME => {
                self.pacer.frame_consumed();
                self.frame();
                Some(LRESULT(0))
            }
            WM_APP_BUS => {
                self.drain_bus();
                Some(LRESULT(0))
            }
            WM_APP_GLASS => {
                self.live_done();
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == TIMER_WAKE => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), TIMER_WAKE);
                }
                self.on_wake();
                Some(LRESULT(0))
            }
            // WM_WTSSESSION_CHANGE: wParam 8 = WTS_SESSION_UNLOCK
            0x02B1 => {
                if wp.0 == 8 {
                    self.broadcast(SystemEvent::Command { verb: "unlocked".into(), args: vec![] });
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let (x, y) = self.lp_to_logical(lp);
                self.on_mouse_move(x, y);
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking = false;
                self.set_hover(false);
                self.input.mouse = None;
                self.input.down = None;
                self.request_frame();
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let p = self.lp_to_logical(lp);
                self.input.down = Some(p);
                self.request_frame();
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let p = self.lp_to_logical(lp);
                self.on_click(p);
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16;
                self.input.wheel += delta as f32 / 120.0;
                self.request_frame();
                Some(LRESULT(0))
            }
            WM_MOUSEHWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16;
                self.input.hwheel += delta as f32 / 120.0;
                self.request_frame();
                Some(LRESULT(0))
            }
            WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
            WM_HOTKEY => {
                let id = wp.0 as i32;
                if let Some(h) = self.hotkeys.iter().find(|h| h.id == id) {
                    let a = h.action;
                    self.on_hotkey(a);
                } else {
                    self.broadcast(SystemEvent::Hotkey(id));
                }
                Some(LRESULT(0))
            }
            WM_APP_TRAY => {
                let ev = (lp.0 as u32) & 0xFFFF;
                if ev == NIN_SELECT || ev == NIN_KEYSELECT || ev == WM_LBUTTONUP {
                    self.toggle();
                }
                Some(LRESULT(0))
            }
            WM_APP_FOREGROUND => {
                self.on_foreground_changed();
                Some(LRESULT(0))
            }
            WM_INPUT => {
                self.on_raw_input(lp);
                None // DefWindowProc must still see WM_INPUT
            }
            WM_CLIPBOARDUPDATE => {
                self.clipboard_changed();
                Some(LRESULT(0))
            }
            WM_POWERBROADCAST => {
                self.broadcast(SystemEvent::PowerChanged);
                Some(LRESULT(1))
            }
            0x0219 => {
                // WM_DEVICECHANGE: a drive letter appeared or went away
                if let Some(args) = usb_event(wp.0 as u32, lp.0) {
                    self.broadcast(SystemEvent::Command { verb: "usb".into(), args });
                }
                Some(LRESULT(1))
            }
            WM_DISPLAYCHANGE | WM_DPICHANGED => {
                self.place();
                Some(LRESULT(0))
            }
            WM_SETTINGCHANGE => {
                if wp.0 as u32 == SPI_SETWORKAREA.0 {
                    self.place();
                }
                if let Some(t) = self.tray.as_mut() {
                    t.refresh_icon();
                }
                None
            }
            WM_COPYDATA => {
                if let Some(args) = ipc::decode(lp) {
                    self.on_ipc(args);
                }
                Some(LRESULT(1))
            }
            WM_ACTIVATE => {
                let state = (wp.0 & 0xFFFF) as u32;
                if state == WA_INACTIVE && self.keyboard {
                    self.set_keyboard(false);
                    if self.mode == Mode::Expanded && !self.hovering {
                        self.collapse();
                    }
                }
                None
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                if self.keyboard {
                    if let Some(k) = translate_key(wp.0 as u32) {
                        self.on_key(k);
                        return Some(LRESULT(0));
                    }
                }
                None
            }
            WM_CHAR => {
                if self.keyboard {
                    let u = wp.0 as u16;
                    let ch = if (0xD800..0xDC00).contains(&u) {
                        self.high_surrogate = Some(u);
                        None
                    } else if (0xDC00..0xE000).contains(&u) {
                        self.high_surrogate.take().and_then(|hi| char::decode_utf16([hi, u]).next()?.ok())
                    } else {
                        char::from_u32(u as u32)
                    };
                    if let Some(c) = ch {
                        if !c.is_control() {
                            self.on_key(Key::Char(c));
                        }
                    }
                    return Some(LRESULT(0));
                }
                None
            }
            WM_CLOSE => {
                self.quit();
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                self.shutdown();
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }
            _ if msg == self.msg_taskbar_created && msg != 0 => {
                if let Some(t) = self.tray.as_mut() {
                    t.readd();
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn on_raw_input(&mut self, lp: LPARAM) {
        let mut ri = RAWINPUT::default();
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        let n = unsafe {
            GetRawInputData(
                HRAWINPUT(lp.0 as *mut _),
                RID_INPUT,
                Some(&mut ri as *mut _ as *mut _),
                &mut size,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if n == u32::MAX || ri.header.dwType != RIM_TYPEKEYBOARD.0 {
            return;
        }
        let kb = unsafe { ri.data.keyboard };
        let vk = kb.VKey;
        let watched = [VK_CAPITAL.0, VK_NUMLOCK.0, VK_SCROLL.0].contains(&vk) || (vk != 0 && vk == self.dictate_vk);
        if !watched {
            return;
        }
        let down = kb.Flags as u32 & RI_KEY_BREAK == 0;
        if down {
            if self.raw_down.contains(&vk) {
                return; // auto-repeat
            }
            self.raw_down.push(vk);
        } else {
            // A registered hotkey's key-down never reaches raw input, its key-up does.
            self.raw_down.retain(|&k| k != vk);
        }
        self.broadcast(SystemEvent::RawKey { vk, down });
    }

    /// OLE drop target callbacks (files dragged onto the notch).
    pub fn on_drop_event(&mut self, ev: DropEvent) -> bool {
        if !self.module_index("shelf").is_some_and(|i| self.started[i]) {
            return false;
        }
        match ev {
            DropEvent::Enter => {
                self.drag_hover = true;
                self.user_hidden = false;
                if self.mode != Mode::Expanded || self.page != "shelf" {
                    self.expand(Some("shelf"));
                }
                self.broadcast(SystemEvent::DragHover(true));
            }
            DropEvent::Leave => {
                self.drag_hover = false;
                self.leave_since = Some(Instant::now());
                self.broadcast(SystemEvent::DragHover(false));
            }
            DropEvent::Drop(files) => {
                self.drag_hover = false;
                // the cursor is on the notch but sends no moves until it does
                self.leave_since = None;
                self.auto_close = Some(Instant::now() + Duration::from_secs(4));
                self.broadcast(SystemEvent::DragHover(false));
                self.pending_bubble = Some((Icon::Package, palette::YELLOW));
                self.broadcast(SystemEvent::Dropped(files));
            }
        }
        self.request_frame();
        self.schedule_wake();
        true
    }

    /// Files to drag out (see `WM_APP_DRAG` in window.rs). The button-up ends
    /// up in the drag loop, so the press is forgotten here.
    pub fn take_drag(&mut self) -> Option<Vec<String>> {
        self.input.down = None;
        self.pending_drag.take()
    }

    pub fn drag_finished(&mut self) {
        // The drag loop held the mouse capture, so WM_MOUSELEAVE may never
        // have arrived; resync hover from the real cursor position.
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(self.hwnd, &mut pt);
        }
        let (x, y) = (pt.x as f32 / self.scale, pt.y as f32 / self.scale);
        if !self.hover_zone().contains(x, y) {
            self.tracking = false;
            self.input.mouse = None;
            self.set_hover(false);
        }
        self.broadcast(SystemEvent::DragOutDone);
        self.request_frame();
    }

    fn lp_to_logical(&self, lp: LPARAM) -> (f32, f32) {
        let x = (lp.0 & 0xFFFF) as u16 as i16 as f32;
        let y = ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as f32;
        (x / self.scale, y / self.scale)
    }

    fn drain_bus(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Module(id, payload) => {
                    if let Some(i) = self.modules.iter().position(|m| m.id() == id) {
                        if self.started[i] {
                            let mut fx = Effects::default();
                            {
                                let mut cx = Cx {
                                    bus: &self.bus,
                                    cfg: &self.cfg,
                                    fx: &mut fx,
                                    hwnd: SendHwnd::new(self.hwnd),
                                    now: Instant::now(),
                                };
                                self.modules[i].on_message(payload, &mut cx);
                            }
                            fx.redraw = true;
                            self.apply_effects(fx);
                        }
                    }
                }
                Msg::ConfigFileChanged => self.reload_config(),
            }
        }
        self.schedule_wake();
    }

    /// Something was copied: the launcher's history takes it (also when the Clipboard module is off), then the
    /// modules are told.
    fn clipboard_changed(&mut self) {
        if !self.module_index("clipboard").is_some_and(|i| self.started[i]) {
            crate::modules::clipboard::record_history(self.hwnd);
        }
        self.broadcast(SystemEvent::ClipboardChanged);
    }

    fn broadcast(&mut self, ev: SystemEvent) {
        let mut fx = Effects::default();
        {
            let now = Instant::now();
            let hwnd = SendHwnd::new(self.hwnd);
            for (i, m) in self.modules.iter_mut().enumerate() {
                if self.started[i] {
                    let mut cx = Cx { bus: &self.bus, cfg: &self.cfg, fx: &mut fx, hwnd, now };
                    m.on_system(&ev, &mut cx);
                }
            }
        }
        self.apply_effects(fx);
        self.schedule_wake();
    }

    fn apply_effects(&mut self, fx: Effects) {
        if fx.nudge && self.cfg.general.haptics && crate::ui::look_alcove() && self.mode == Mode::Expanded {
            // a tiny give, like a key travelling down and back up
            self.anim.w.velocity -= 30.0;
            self.anim.h.velocity -= 10.0;
            self.request_frame();
        }
        for (s, k, v) in &fx.save_config {
            config::set_value(s, k, v);
        }
        for p in fx.peeks {
            self.push_peek(p);
        }
        if let Some(v) = fx.set_volume {
            modules::osd::set_volume(&self.bus, v);
        }
        if let Some(b) = fx.set_brightness {
            modules::osd::set_brightness(b);
        }
        if fx.reload_config {
            self.reload_config();
        }
        if let Some(page) = fx.open_page {
            self.expand(Some(page));
        }
        if fx.expand && self.mode != Mode::Expanded {
            self.expand(None);
        }
        if fx.collapse {
            self.collapse();
        }
        if let Some(k) = fx.keyboard_focus {
            self.set_keyboard(k);
        }
        if let Some(files) = fx.drag_files {
            if !files.is_empty() && self.pending_drag.is_none() {
                self.pending_drag = Some(files);
                unsafe {
                    let _ = PostMessageW(Some(self.hwnd), WM_APP_DRAG, WPARAM(0), LPARAM(0));
                }
            }
        }
        if fx.redraw || fx.animate || fx.layout_changed {
            self.request_frame();
        } else if fx.animate_slow {
            // only a slow animation: frames keep coming, every other one is skipped in `frame`
            self.dirty = true;
            self.slow_only = true;
            self.pacer.start();
        }
    }

    // -----------------------------------------------------------------------
    // Modules
    // -----------------------------------------------------------------------

    fn sync_modules(&mut self) {
        let hwnd = SendHwnd::new(self.hwnd);
        let mut fx = Effects::default();
        for (i, m) in self.modules.iter_mut().enumerate() {
            let want = self.cfg.modules.is_enabled(m.id());
            if want && !self.started[i] {
                let mut cx = Cx { bus: &self.bus, cfg: &self.cfg, fx: &mut fx, hwnd, now: Instant::now() };
                m.start(&mut cx);
                self.started[i] = true;
            } else if !want && self.started[i] {
                m.stop();
                self.started[i] = false;
            }
        }
        if !self.page_available(self.page) {
            self.page = HOME;
        }
        self.apply_effects(fx);
        self.request_frame();
    }

    fn page_available(&self, page: ModuleId) -> bool {
        page == HOME || self.modules.iter().enumerate().any(|(i, m)| self.started[i] && m.id() == page && m.has_page())
    }

    /// Change the open page with a soft slide and cross-fade.
    fn switch_page(&mut self, new: ModuleId) {
        if new == self.page {
            return;
        }
        // later tabs slide in from the right, earlier ones from the left
        // (by the order of the header tabs, which the user can rearrange; settings sits last)
        let order = &self.cfg.tabs.order;
        let pos = |id: ModuleId| -> i32 {
            if id == HOME {
                return -1;
            }
            if id == "settings" {
                return 10_000;
            }
            match order.iter().position(|o| o == id) {
                Some(i) => i as i32,
                None => 1_000 + self.module_index(id).map_or(0, |i| i as i32),
            }
        };
        let dir = if pos(new) >= pos(self.page) { 1.0 } else { -1.0 };
        self.page_from = Some((self.page, dir));
        self.page_at = Instant::now();
        self.page = new;
        self.anim.page.snap(0.0);
        self.anim.page.set_target(1.0);
        self.request_frame();
    }

    /// Whether the hello is written with an installed font (`[greeting] font`) instead of the pen drawing.
    fn hello_custom(&self) -> bool {
        self.gfx.font_installed(self.cfg.greeting.font_name())
    }

    /// How wide the open notch is for the page shown: the page's own share of the widest setting,
    /// or the full width on the home page.
    fn page_width(&self) -> f32 {
        page_width_of(&self.cfg, self.page)
    }

    fn module_index(&self, id: ModuleId) -> Option<usize> {
        self.modules.iter().position(|m| m.id() == id)
    }

    fn refresh_activity(&mut self) {
        let mut acts: Vec<Activity> = Vec::new();
        self.indicators.clear();
        for (i, m) in self.modules.iter().enumerate() {
            if !self.started[i] {
                continue;
            }
            if let Some(a) = m.activity() {
                acts.push(a);
            }
            m.indicators(&mut self.indicators);
        }
        // stable sort: equal priorities keep module order
        acts.sort_by_key(|a| std::cmp::Reverse(a.priority));
        let mut it = acts.into_iter();
        let before = self.activity.is_some() as u8 + self.secondary.is_some() as u8;
        self.activity = it.next();
        self.secondary = it.next();
        // Alcove: a small give in the notch when a live activity starts or ends
        if crate::ui::look_alcove()
            && self.mode == Mode::Idle
            && before != self.activity.is_some() as u8 + self.secondary.is_some() as u8
        {
            self.anim.w.velocity += 70.0;
            self.anim.h.velocity += 22.0;
        }
        // accent from album art if configured
        self.accent = match Color::parse(&self.cfg.appearance.accent) {
            Some(c) => c,
            None => self
                .module_index("media")
                .filter(|&i| self.started[i])
                .and_then(|i| self.modules[i].activity())
                .and_then(|a| match a.left {
                    Slot::Image(img) => img.accent,
                    _ => None,
                })
                .unwrap_or(if crate::ui::look_nothing() {
                    crate::ui::NOTHING_RED
                } else if crate::ui::look_tuna() {
                    crate::ui::TUNA_BLUE
                } else if crate::ui::look_bloom() {
                    crate::ui::bloom_blue()
                } else {
                    Color::white(0.92)
                }),
        };
    }

    fn on_wake(&mut self) {
        let now = Instant::now();
        // module ticks
        let mut fx = Effects::default();
        {
            let hwnd = SendHwnd::new(self.hwnd);
            for (i, m) in self.modules.iter_mut().enumerate() {
                if self.started[i] && self.tick_at[i].is_some_and(|t| t <= now + Duration::from_millis(2)) {
                    let mut cx = Cx { bus: &self.bus, cfg: &self.cfg, fx: &mut fx, hwnd, now };
                    m.on_tick(&mut cx);
                    fx.redraw = true;
                }
            }
        }
        self.apply_effects(fx);
        self.process_timeouts(now);
        if self.trim_at.is_some_and(|t| t <= now + Duration::from_millis(2)) {
            self.trim_at = None;
            if self.snap_band.is_some() {
                log!("trim while the bottom blur is showing");
            }
            self.res.trim_bitmaps();
            self.host.reset_dc(&self.gfx);
            self.gfx.trim();
            // hand the freed pages back to Windows (Task Manager shows the real footprint)
            unsafe {
                let _ = windows::Win32::System::Memory::SetProcessWorkingSetSizeEx(
                    windows::Win32::System::Threading::GetCurrentProcess(),
                    usize::MAX,
                    usize::MAX,
                    windows::Win32::System::Memory::SETPROCESSWORKINGSETSIZEEX_FLAGS(0),
                );
            }
        }
        if self.mode == Mode::Expanded && self.page == HOME {
            let st = unsafe { GetLocalTime() };
            if st.wSecond != self.clock_min {
                self.request_frame();
            }
        }
        self.live_tick(now);
        self.schedule_wake();
    }

    /// Arm a single Win32 timer for the earliest pending deadline.
    fn schedule_wake(&mut self) {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut consider = |t: Instant| {
            next = Some(next.map_or(t, |n: Instant| n.min(t)));
        };
        for (i, m) in self.modules.iter().enumerate() {
            self.tick_at[i] = if self.started[i] { m.next_tick() } else { None };
            if let Some(t) = self.tick_at[i] {
                consider(t);
            }
        }
        let g = &self.cfg.general;
        if self.mode != Mode::Expanded && self.hovering && g.expand_on_hover {
            if let Some(h) = self.hover_since {
                consider(h + Duration::from_millis(g.hover_delay_ms as u64));
            }
        }
        if self.mode == Mode::Expanded && !self.hovering && !self.keyboard && !self.drag_hover {
            if let Some(l) = self.leave_since {
                consider(l + Duration::from_millis(g.collapse_delay_ms as u64));
            }
            if let Some(t) = self.auto_close {
                consider(t);
            }
        }
        if let Some(p) = &self.peek {
            consider(p.until);
        }
        if let Some(t) = self.trim_at {
            consider(t);
        }
        if self.live_armed() {
            if let Some(t) = self.live_next {
                consider(t);
            }
            if self.live_rx.is_some() {
                // the answer normally arrives as a message; this is only a safety net
                consider(now + Duration::from_millis(60));
            }
        }
        if self.mode == Mode::Expanded && self.page == HOME {
            // the flip clock shows seconds: wake at the next full second
            let ms = 1000 - (unsafe { GetLocalTime() }.wMilliseconds as u64).min(999);
            consider(now + Duration::from_millis(ms + 2));
        }
        unsafe {
            match next {
                Some(t) => {
                    let ms = t.saturating_duration_since(now).as_millis().clamp(1, 0x7FFF_FFFF) as u32;
                    SetTimer(Some(self.hwnd), TIMER_WAKE, ms, None);
                }
                None => {
                    let _ = KillTimer(Some(self.hwnd), TIMER_WAKE);
                }
            }
        }
    }

    fn process_timeouts(&mut self, now: Instant) {
        let (expand_on_hover, hover_delay_ms, collapse_delay_ms) = {
            let g = &self.cfg.general;
            (g.expand_on_hover, g.hover_delay_ms, g.collapse_delay_ms)
        };
        if self.mode != Mode::Expanded && self.hovering && expand_on_hover && !self.user_hidden {
            if let Some(h) = self.hover_since {
                if now >= h + Duration::from_millis(hover_delay_ms as u64) {
                    self.hover_since = None;
                    let page = self.peek.as_ref().and_then(|p| p.peek.page).or_else(|| self.media_target(self.input.mouse));
                    self.expand(page);
                }
            }
        }
        if self.mode == Mode::Expanded && !self.hovering && !self.keyboard && !self.drag_hover {
            let leave = self.leave_since.map(|l| l + Duration::from_millis(collapse_delay_ms as u64));
            if leave.into_iter().chain(self.auto_close).any(|t| now >= t) {
                self.collapse();
            }
        }
        if let Some(p) = &self.peek {
            if now >= p.until && !(self.hovering && self.mode == Mode::Peek) {
                if crate::ui::look_alcove()
                    && self.cfg.general.bubbles
                    && matches!(p.peek.key, Some("notify" | "clipboard" | "download"))
                    && !p.peek.is_hud()
                {
                    push_bubble(&mut self.bubbles, now, p.peek.icon, p.peek.accent, false, p.peek.page);
                }
                let gone = self.peek.take();
                if crate::ui::look_alcove() && self.cfg.appearance.motion_cache {
                    self.peek_out = gone;
                }
                self.next_peek();
                if self.peek.is_some() {
                    self.peek_out = None;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // State transitions
    // -----------------------------------------------------------------------

    fn expand(&mut self, page: Option<ModuleId>) {
        let target = page.filter(|p| self.page_available(p)).unwrap_or_else(|| {
            if self.mode == Mode::Expanded {
                self.page
            } else {
                self.activity.as_ref().and_then(|a| a.page).filter(|p| self.page_available(p)).unwrap_or(HOME)
            }
        });
        let was = self.mode;
        if was == Mode::Expanded {
            self.switch_page(target);
        } else {
            self.page = target;
            self.page_from = None;
        }
        self.mode = Mode::Expanded;
        self.peek = None;
        self.leave_since = None;
        self.auto_close = if self.hovering { None } else { Some(Instant::now() + Duration::from_secs(8)) };
        if was != Mode::Expanded {
            self.opened_at = Some(Instant::now());
            self.broadcast(SystemEvent::Expanded(true));
        }
        self.request_frame();
        self.schedule_wake();
    }

    fn collapse(&mut self) {
        if self.mode != Mode::Expanded {
            return;
        }
        self.mode = Mode::Idle;
        self.leave_since = None;
        self.auto_close = None;
        self.hover_since = None;
        if self.keyboard {
            self.set_keyboard(false);
        }
        self.broadcast(SystemEvent::Expanded(false));
        // files were just parked on the shelf: a bubble once the notch has closed
        if let Some((icon, color)) = self.pending_bubble.take() {
            if crate::ui::look_alcove() && self.cfg.general.bubbles {
                push_bubble(
                    &mut self.bubbles,
                    Instant::now() + Duration::from_millis(350),
                    icon,
                    color,
                    false,
                    Some("shelf"),
                );
            }
        }
        self.next_peek();
        self.request_frame();
        self.schedule_wake();
    }

    fn toggle(&mut self) {
        if self.mode == Mode::Expanded {
            self.collapse();
        } else {
            if self.user_hidden {
                self.user_hidden = false;
            }
            // with a track playing the hotkey opens Home too, not the media page
            self.expand(self.media_target(None));
        }
    }

    fn push_peek(&mut self, p: Peek) {
        if self.user_hidden || self.fs_hidden {
            return;
        }
        // Do not disturb (the moon in the control center): only volume and brightness still show
        if crate::bloom::dnd() && !p.is_hud() {
            return;
        }
        if self.mode == Mode::Expanded {
            return; // the user is already looking at the notch
        }
        // A new track only changes the closed notch (cover and bars); it never pops a peek
        if p.key == Some("media") {
            return;
        }
        // Alcove: a finished copy or download is just a bubble that pops out beside the notch and
        // is pulled back in, not a full notification
        if self.peek.is_none()
            && (small_bubble(&p) || (matches!(p.key, Some("clipboard" | "download")) && !p.is_hud()))
            && crate::ui::look_alcove()
            && self.cfg.general.bubbles
        {
            push_bubble(&mut self.bubbles, Instant::now(), p.icon, p.accent, bubble_off(&p), p.page);
            self.request_frame();
            self.schedule_wake();
            return;
        }
        if p.quiet {
            // a follow-up (the next track): swap the visible peek with an animation, never pop a new one
            if let (Some(k), Some(cur)) = (p.key, self.peek.as_mut()) {
                if cur.peek.key == Some(k) {
                    let old = std::mem::replace(&mut cur.peek, p);
                    cur.swap = Some((Instant::now(), Box::new(old)));
                    self.request_frame();
                    self.schedule_wake();
                }
            }
            return;
        }
        if let (Some(k), Some(cur)) = (p.key, self.peek.as_mut()) {
            if cur.peek.key == Some(k) {
                cur.swap = None;
                cur.until = Instant::now() + p.duration;
                cur.peek = p;
                self.request_frame();
                self.schedule_wake();
                return;
            }
        }
        if let Some(k) = p.key {
            self.peek_queue.retain(|q| q.key != Some(k));
        }
        if p.instant {
            if let Some(cur) = self.peek.take() {
                self.peek_queue.push_front(cur.peek);
                self.peek_queue.truncate(4);
            }
        }
        if self.peek.is_some() {
            if self.peek_queue.len() >= 4 {
                self.peek_queue.pop_front();
            }
            self.peek_queue.push_back(p);
        } else {
            self.peek = Some(ActivePeek { until: Instant::now() + p.duration, peek: p, swap: None });
            self.mode = Mode::Peek;
        }
        self.request_frame();
        self.schedule_wake();
    }

    fn next_peek(&mut self) {
        if self.mode == Mode::Expanded {
            return;
        }
        match self.peek_queue.pop_front() {
            Some(p) => {
                self.peek = Some(ActivePeek { until: Instant::now() + p.duration, peek: p, swap: None });
                self.mode = Mode::Peek;
                if crate::ui::look_alcove() {
                    // a small pulse as it arrives
                    self.anim.w.velocity += 90.0;
                    self.anim.h.velocity += 30.0;
                }
            }
            None => {
                self.peek = None;
                self.mode = Mode::Idle;
            }
        }
        self.request_frame();
    }

    fn set_hover(&mut self, on: bool) {
        if on == self.hovering {
            return;
        }
        self.hovering = on;
        let now = Instant::now();
        if on {
            if self.cfg.general.haptics && self.mode != Mode::Expanded && !self.user_hidden {
                sys::haptic(false);
            }
            self.hover_since = Some(now);
            self.leave_since = None;
            self.auto_close = None;
        } else {
            self.hover_since = None;
            self.leave_since = Some(now);
        }
        self.request_frame();
        self.schedule_wake();
    }

    fn set_keyboard(&mut self, on: bool) {
        if on == self.keyboard {
            return;
        }
        self.keyboard = on;
        unsafe {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            if on {
                SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex & !(WS_EX_NOACTIVATE.0 as isize));
                if !SetForegroundWindow(self.hwnd).as_bool() || GetForegroundWindow() != self.hwnd {
                    // Unlock foreground rights with a harmless synthetic key press.
                    let inputs = [key_input(VK_MENU, false), key_input(VK_MENU, true)];
                    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
                    let _ = SetForegroundWindow(self.hwnd);
                }
                let _ = SetFocus(Some(self.hwnd));
            } else {
                SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize);
            }
        }
        self.request_frame();
    }

    /// What the selected closed-notch widgets show right now.
    fn idle_glances(&self) -> Vec<Glance> {
        let mut out = Vec::new();
        if self.mode != Mode::Idle {
            return out;
        }
        for id in self.cfg.notch.widgets.iter().take(3) {
            match id.as_str() {
                "clock" => out.push((None, clock_text(), palette::TEXT, None)),
                "date" => out.push((None, date_text(), palette::TEXT, None)),
                "battery" => {
                    if let Some((level, charging)) = self.indicators.iter().find_map(|i| i.battery) {
                        let icon = if charging { Some(Icon::Bolt) } else { None };
                        out.push((icon, format!("{:.0}%", level * 100.0), battery_color(level, charging), Some(level)));
                    }
                }
                other => {
                    let module = if matches!(other, "wifi" | "bluetooth") { "control" } else { other };
                    if let Some(i) = self.modules.iter().position(|m| m.id() == module) {
                        if self.started[i] {
                            if let Some((icon, text)) = self.modules[i].glance_for(other) {
                                out.push((Some(icon), text, palette::TEXT, None));
                            }
                        }
                    }
                }
            }
        }
        out
    }

    fn quit(&mut self) {
        self.shutdown();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
            // WM_DESTROY arrives while the app is still borrowed (re-entrant),
            // so its handler never runs; end the message loop here.
            PostQuitMessage(0);
        }
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    fn body_rect(&self) -> Rect {
        let w = self.anim.w.value;
        let h = self.anim.h.value;
        let shift = (1.0 - self.anim.shown.value) * (h + 14.0);
        Rect::new(self.win_w * 0.5 - w * 0.5, -shift, w, h)
    }

    /// Alcove: a playing track shows its cover and level bars as two bubbles beside the notch.
    fn split_media(&self) -> bool {
        crate::ui::look_alcove()
            && self.cfg.general.bubbles
            && self.mode != Mode::Expanded
            && self.activity.as_ref().is_some_and(|a| matches!(a.left, Slot::Vinyl { .. }))
    }

    /// The two media bubbles (left, right); they slide out from under the notch edges.
    fn split_rects(&self) -> (Rect, Rect) {
        let s = self.cfg.appearance.notch_height;
        let b = self.body_rect();
        let k = self.anim.split.value;
        let y = b.y.min(0.0);
        (Rect::new(b.x - (s + SPLIT_GAP) * k, y, s, s), Rect::new(b.right() - s + (s + SPLIT_GAP) * k, y, s, s))
    }

    /// The lingering bubbles (oldest nearest the notch): rect, opacity, icon and colour. Each pops
    /// out beside the notch, stays about a second, then shrinks back under the edge as if the
    /// notch pulled it in; the ones further out slide in to fill the gap as it goes.
    fn bubble_states(&self, now: Instant) -> Vec<(Rect, f32, Icon, Color, bool, Option<ModuleId>)> {
        let mut out = Vec::new();
        // the common case (no bubble showing) is asked on every mouse move and every frame
        if self.bubbles.is_empty() || self.mode != Mode::Idle || self.peek.is_some() {
            return out;
        }
        let s = self.cfg.appearance.notch_height;
        // Anchored to the full-size notch, so a bubble looks the same whatever the idle style
        // (notch, pill or hidden); beside the music bubble when there is one
        let right = self.body_rect().right().max(self.win_w * 0.5 + self.cfg.appearance.notch_width * 0.5);
        let split = self.anim.split.value;
        let mut off = 0.0;
        for b in &self.bubbles {
            let Some((k, alpha, scale, room)) = bubble_curve(now.saturating_duration_since(b.at).as_secs_f32()) else {
                continue;
            };
            let x = right - s + (s + 8.0) * ((1.0 + off) * k + split);
            let r = Rect::new(x, 0.0, s, s);
            out.push((Rect::centered(r.cx(), r.cy(), r.w * scale, r.h * scale), alpha, b.icon, b.color, b.off, b.page));
            off += room;
        }
        out
    }

    /// The secondary activity bubble slides out from under the right edge.
    fn bubble_rect(&self) -> Rect {
        let s = self.cfg.appearance.notch_height;
        let b = self.body_rect();
        let k = self.anim.sec.value;
        Rect::new(b.right() - s + (s + 8.0) * k, b.y.min(0.0), s, s)
    }

    /// With a track playing in the Alcove look, only its cover and level bars lead to the media
    /// page; the rest of the closed notch opens Home. `None`: no media activity is showing, so the
    /// caller keeps its usual choice of page.
    fn media_target(&self, at: Option<(f32, f32)>) -> Option<ModuleId> {
        let a = self.activity.as_ref().filter(|a| a.page == Some("media"))?;
        let on_media = at.is_some_and(|(x, y)| {
            if self.anim.split.value > 0.5 {
                let (l, r) = self.split_rects();
                l.expand(4.0).contains(x, y) || r.expand(4.0).contains(x, y)
            } else {
                let b = self.body_rect();
                let w = a.wing * self.cfg.appearance.notch_height + 4.0;
                Rect::new(b.x, b.y, w, b.h).contains(x, y) || Rect::new(b.right() - w, b.y, w, b.h).contains(x, y)
            }
        });
        Some(if on_media { "media" } else { HOME })
    }

    fn hover_zone(&self) -> Rect {
        let b = self.body_rect();
        let strip_w = self.cfg.appearance.notch_width;
        let strip = Rect::new(self.win_w * 0.5 - strip_w * 0.5, 0.0, strip_w, 5.0);
        let b = Rect::new(b.x, 0.0, b.w, b.bottom().max(0.0));
        let z = b.union(&strip);
        let z = if self.anim.split.value > 0.5 {
            let (l, r) = self.split_rects();
            z.union(&l).union(&r)
        } else {
            z
        };
        let mut z = if self.anim.sec.value > 0.5 { z.union(&self.bubble_rect()) } else { z };
        // a bubble that opens a page on click counts as part of the notch while it shows
        for b in self.bubble_states(Instant::now()) {
            if b.5.is_some() {
                z = z.union(&b.0);
            }
        }
        z
    }

    /// A right-click on a listed (not announced) tray icon: its index, for the menu in window.rs.
    pub fn tray_fb_at(&self, lp: LPARAM) -> Option<usize> {
        if self.mode != Mode::Expanded || self.page != "control" {
            return None;
        }
        let (x, y) = self.lp_to_logical(lp);
        crate::modules::traylist::fb_at(x, y)
    }

    /// A right-click on a tray icon of the Wi-Fi & Bluetooth page: true when it was taken.
    pub fn tray_right_click(&mut self, lp: LPARAM) -> bool {
        if self.mode != Mode::Expanded || self.page != "control" {
            return false;
        }
        let (x, y) = self.lp_to_logical(lp);
        if !crate::modules::traylist::over_tile(x, y) {
            return false;
        }
        self.input.right_click = Some((x, y));
        self.request_frame();
        true
    }

    fn on_mouse_move(&mut self, x: f32, y: f32) {
        if !self.tracking {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            unsafe {
                let _ = TrackMouseEvent(&mut tme);
            }
            self.tracking = true;
        }
        let inside = self.hover_zone().contains(x, y) && !self.fs_hidden;
        self.set_hover(inside);
        let before = self.input.mouse;
        self.input.mouse = if inside { Some((x, y)) } else { None };
        // Only the expanded/peek UI has hover states worth a frame.
        if self.mode != Mode::Idle && before != self.input.mouse {
            self.request_frame();
        }
    }

    fn on_click(&mut self, p: (f32, f32)) {
        let down = self.input.down.take();
        let inside = self.body_rect().contains(p.0, p.1);
        match self.mode {
            Mode::Expanded => {
                if !inside {
                    self.collapse();
                    return;
                }
                if let Some(d) = down {
                    self.input.click = Some((d, p));
                }
            }
            Mode::Peek => match self.peek.as_ref().and_then(|a| a.peek.focus) {
                Some(h) => {
                    sys::activate_window(h);
                    self.peek = None;
                    self.next_peek();
                }
                None => {
                    // the volume text of the volume peek opens the mixer
                    let on_volume = self.peek.as_ref().is_some_and(|a| a.peek.key == Some("osd-volume"))
                        && HUD_TEXT.with(|z| z.get()).is_some_and(|z| z.contains(p.0, p.1));
                    let page = if on_volume { Some(modules::mixer::ID) } else { self.peek.as_ref().and_then(|a| a.peek.page) };
                    self.expand(page);
                }
            },
            Mode::Idle => {
                let hit = self
                    .bubble_states(Instant::now())
                    .into_iter()
                    .find(|b| b.5.is_some() && b.1 > 0.4 && b.0.expand(4.0).contains(p.0, p.1))
                    .and_then(|b| b.5);
                if let Some(page) = hit {
                    // a click on a lingering bubble opens what it is about
                    self.bubbles.clear();
                    self.user_hidden = false;
                    if self.cfg.general.haptics {
                        sys::haptic(true);
                    }
                    self.expand(Some(page));
                } else if self.anim.split.value > 0.5 && {
                    let (l, r) = self.split_rects();
                    l.contains(p.0, p.1) || r.contains(p.0, p.1)
                } {
                    let page = self.activity.as_ref().and_then(|a| a.page);
                    self.expand(page);
                } else if self.anim.sec.value > 0.5 && self.bubble_rect().contains(p.0, p.1) {
                    let page = self.secondary.as_ref().and_then(|a| a.page);
                    self.expand(page);
                } else if inside || self.hover_zone().contains(p.0, p.1) {
                    self.user_hidden = false;
                    if self.cfg.general.haptics {
                        sys::haptic(true);
                    }
                    // with the media split into two bubbles the notch between them is blank: a click
                    // there opens Home, while the bubbles themselves open the media page
                    let page = self.media_target(Some(p)).or(if self.anim.split.value > 0.5 { Some(HOME) } else { None });
                    self.expand(page);
                }
            }
        }
        self.request_frame();
    }

    fn on_key(&mut self, k: Key) {
        if k == Key::Escape {
            self.set_keyboard(false);
            self.collapse();
            return;
        }
        let Some(i) = self.module_index(self.page) else { return };
        if !self.started[i] || !self.modules[i].wants_keyboard() {
            return;
        }
        let mut fx = Effects::default();
        {
            let mut cx =
                Cx { bus: &self.bus, cfg: &self.cfg, fx: &mut fx, hwnd: SendHwnd::new(self.hwnd), now: Instant::now() };
            self.modules[i].on_key(k, &mut cx);
        }
        fx.redraw = true;
        self.apply_effects(fx);
        self.schedule_wake();
    }

    fn on_hotkey(&mut self, a: Action) {
        match a {
            Action::Toggle => self.toggle(),
            Action::Ai => {
                if self.mode == Mode::Expanded && self.page == "ai" {
                    self.collapse();
                } else {
                    self.user_hidden = false;
                    self.expand(Some("ai"));
                    if self.page == "ai" {
                        self.set_keyboard(true);
                    }
                }
            }
            Action::Timer => self.expand(Some("timer")),
            Action::Clipboard => self.expand(Some("clipboard")),
            Action::PlayPause => self.broadcast(SystemEvent::Command { verb: "playpause".into(), args: vec![] }),
            Action::Dictate => {
                self.broadcast(SystemEvent::Command { verb: "dictate".into(), args: vec!["hotkey".into()] })
            }
            Action::Launcher => self.launcher_toggle(),
            Action::LauncherClip => self.launcher_clip_toggle(),
            Action::LauncherAi => self.launcher_ai_toggle(),
            Action::LauncherRun => self.launcher_run_toggle(),
            Action::LauncherSwitch => self.launcher_switch(false),
            Action::LauncherSwitchBack => self.launcher_switch(true),
            Action::Radial => self.radial_toggle(),
            Action::WindowMax => hotkeys::maximize_active(),
            Action::WindowClose => hotkeys::close_active(),
            Action::Combo(i) => {
                if let Some(item) = self.cfg.combos.items.get(i).cloned() {
                    let (_, target, label) = hotkeys::split_item(&item);
                    // the launcher shows what is opening, for a moment
                    self.launcher_flash(label, target);
                    hotkeys::open_target(target, &self.cfg.launcher.search_url);
                }
            }
        }
    }

    pub fn on_ipc(&mut self, args: Vec<String>) {
        let Some(verb) = args.first().map(|s| s.to_ascii_lowercase()) else {
            self.expand(None);
            return;
        };
        let rest: Vec<String> = args[1..].to_vec();
        log!("ipc: {verb} {rest:?}");
        match verb.as_str() {
            "toggle" => self.toggle(),
            "expand" | "show" | "open" => {
                self.user_hidden = false;
                self.expand(rest.first().map(|p| leak_page(p)));
            }
            "collapse" | "close" => self.collapse(),
            "ai" => self.on_hotkey(Action::Ai),
            "reload" => self.reload_config(),
            "notify" => {
                // Scriptable banner: dynamic-notch notify "Title" "Body"
                let title = rest.first().cloned().unwrap_or_else(|| "Notification".into());
                let body = rest.get(1..).map(|a| a.join(" ")).unwrap_or_default();
                self.push_peek(Peek::new(Icon::Bell, palette::BLUE, title, body).duration_ms(4500));
            }
            "claude" => self.push_peek(claude_peek(&rest)),
            "quit" | "exit" => self.quit(),
            _ => self.broadcast(SystemEvent::Command { verb, args: rest }),
        }
    }

    // -----------------------------------------------------------------------
    // Tray menu
    // -----------------------------------------------------------------------

    pub fn menu_state(&self) -> MenuState {
        MenuState {
            expanded: self.mode == Mode::Expanded,
            user_hidden: self.user_hidden,
            modules: self
                .modules
                .iter()
                .map(|m| (m.title().to_string(), self.cfg.modules.is_enabled(m.id())))
                .collect(),
            monitor_count: monitors().len(),
            monitor: self.cfg.general.monitor.clone(),
            autostart: self.cfg.general.start_with_windows,
            blur: self.cfg.appearance.blur,
            hover: self.cfg.general.expand_on_hover,
            hide_fullscreen: self.cfg.general.hide_in_fullscreen,
        }
    }

    pub fn on_menu(&mut self, cmd: u32) {
        let b = |v: bool| if v { "true".to_string() } else { "false".to_string() };
        match cmd {
            tray::CMD_TOGGLE => self.toggle(),
            tray::CMD_AI => self.on_hotkey(Action::Ai),
            tray::CMD_TIMER => self.expand(Some("timer")),
            tray::CMD_HIDE => {
                self.user_hidden = !self.user_hidden;
                if self.user_hidden {
                    self.collapse();
                    self.peek = None;
                    self.peek_queue.clear();
                    self.mode = Mode::Idle;
                }
                self.request_frame();
            }
            tray::CMD_MON_PRIMARY => config::set_value("general", "monitor", "\"primary\""),
            tray::CMD_MON_ACTIVE => config::set_value("general", "monitor", "\"active\""),
            c if (tray::CMD_MON_BASE..tray::CMD_MON_BASE + 16).contains(&c) => {
                config::set_value("general", "monitor", &format!("\"{}\"", c - tray::CMD_MON_BASE + 1))
            }
            c if (tray::CMD_MODULE_BASE..tray::CMD_MODULE_BASE + 32).contains(&c) => {
                let i = (c - tray::CMD_MODULE_BASE) as usize;
                if let Some(m) = self.modules.get(i) {
                    let id = m.id();
                    let on = !self.cfg.modules.is_enabled(id);
                    config::set_value("modules", id, &b(on));
                }
            }
            tray::CMD_AUTOSTART => {
                config::set_value("general", "start_with_windows", &b(!self.cfg.general.start_with_windows))
            }
            tray::CMD_BLUR => config::set_value("appearance", "blur", &b(!self.cfg.appearance.blur)),
            tray::CMD_HOVER => config::set_value("general", "expand_on_hover", &b(!self.cfg.general.expand_on_hover)),
            tray::CMD_FULLSCREEN => {
                config::set_value("general", "hide_in_fullscreen", &b(!self.cfg.general.hide_in_fullscreen))
            }
            tray::CMD_EDIT_CONFIG => {
                let path = config::config_path();
                if !path.exists() {
                    let _ = std::fs::write(&path, config::DEFAULT_TEMPLATE);
                }
                sys::shell_open(&path.to_string_lossy());
            }
            tray::CMD_OPEN_FOLDER => sys::shell_open(&crate::util::app_dir().to_string_lossy()),
            tray::CMD_RELOAD => self.reload_config(),
            tray::CMD_QUIT => self.quit(),
            _ => {}
        }
    }

    fn reload_config(&mut self) {
        match config::load() {
            LoadResult::Ok(c) => {
                let autostart_changed = c.general.start_with_windows != self.cfg.general.start_with_windows;
                self.cfg = c;
                crate::ui::set_white_beams(self.cfg.appearance.white_beams);
                crate::ui::set_look(&self.cfg.appearance.look);
                crate::ui::set_glow(self.cfg.glass.glow, self.cfg.glass.glow_amount);
                crate::ui::set_mono_icons(self.cfg.appearance.mono_icons);
                self.dock_apply();
                self.sync_taskbar();
                if autostart_changed {
                    sys::set_autostart(self.cfg.general.start_with_windows);
                }
                self.register_hotkeys();
                self.sync_modules();
                self.place();
                self.broadcast(SystemEvent::ConfigReloaded);
                log!("config reloaded");
            }
            LoadResult::Invalid(e) => {
                log!("config error: {e}");
                let msg = crate::util::one_line(&e);
                self.push_peek(
                    Peek::new(Icon::Settings, palette::ORANGE, "Settings file has an error", msg)
                        .duration_ms(6000)
                        .key("config"),
                );
            }
        }
    }

    fn register_hotkeys(&mut self) {
        let (ok, mut errors) = hotkeys::register_all(self.hwnd, &self.cfg.hotkeys);
        let (combos, combo_errors) = hotkeys::register_combos(
            self.hwnd,
            &self.cfg.combos.items,
            [
                self.cfg.hotkeys.launcher_clip.as_str(),
                self.cfg.hotkeys.launcher_ai.as_str(),
                self.cfg.hotkeys.window_max.as_str(),
                self.cfg.hotkeys.window_close.as_str(),
                self.cfg.hotkeys.radial.as_str(),
                self.cfg.hotkeys.launcher_run.as_str(),
                self.cfg.hotkeys.launcher_switch.as_str(),
            ],
        );
        self.hotkeys = ok;
        self.hotkeys.extend(combos);
        errors.extend(combo_errors);
        self.dictate_vk = hotkeys::parse(&self.cfg.hotkeys.dictate).map_or(0, |(_, vk)| vk as u16);
        if !errors.is_empty() {
            log!("hotkey problems: {errors:?}");
            self.push_peek(
                Peek::new(Icon::Settings, palette::ORANGE, "Hotkey unavailable", errors.join(", "))
                    .duration_ms(5000)
                    .key("hotkeys"),
            );
        }
    }

    // -----------------------------------------------------------------------
    // Placement (multi-monitor + per-monitor DPI)
    // -----------------------------------------------------------------------

    fn place(&mut self) {
        let mon = self.pick_monitor();
        unsafe {
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            if !GetMonitorInfoW(mon, &mut mi).as_bool() {
                return;
            }
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let scale = dx as f32 / 96.0 * self.cfg.appearance.scale;
            let win_w = self.cfg.appearance.expanded_width + 2.0 * MARGIN;
            let win_h = WIN_H;
            let wpx = (win_w * scale).ceil() as i32;
            let hpx = (win_h * scale).ceil() as i32;
            let m = mi.rcMonitor;
            let top = if mi.rcWork.top > m.top { mi.rcWork.top } else { m.top };
            let x = (m.left + m.right) / 2 - wpx / 2;
            let geom = (x, top, wpx, hpx);
            self.monitor = mon;
            if geom != self.win_px || (scale - self.scale).abs() > 0.001 {
                self.scale = scale;
                self.win_w = win_w;
                self.win_h = win_h;
                self.win_px = geom;
                self.refr_shot = None;
                self.refr_rim = None;
                let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), x, top, wpx, hpx, SWP_NOACTIVATE);
                if let Err(e) = self.host.resize(wpx, hpx) {
                    log!("surface resize failed: {e}");
                }
                self.region = None;
                self.region_anim = None;
            }
        }
        self.dock_relocate();
        self.check_fullscreen();
        self.request_frame();
    }

    fn pick_monitor(&self) -> HMONITOR {
        let primary = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        match self.cfg.general.monitor.trim() {
            "active" => unsafe {
                let fg = GetForegroundWindow();
                if fg.is_invalid() || fg == self.hwnd {
                    if self.monitor.is_invalid() {
                        primary
                    } else {
                        self.monitor
                    }
                } else {
                    MonitorFromWindow(fg, MONITOR_DEFAULTTOPRIMARY)
                }
            },
            s => match s.parse::<usize>() {
                Ok(n) if n >= 1 => monitors().get(n - 1).copied().unwrap_or(primary),
                _ => primary,
            },
        }
    }

    fn on_foreground_changed(&mut self) {
        unsafe {
            let fg = GetForegroundWindow();
            // Re-hook location changes for the foreground process only, so a
            // window becoming fullscreen (F11, videos) is noticed without
            // listening to every window in the system.
            let mut pid = 0u32;
            GetWindowThreadProcessId(fg, Some(&mut pid));
            if pid != self.loc_pid && pid != std::process::id() {
                if let Some(h) = self.loc_hook.take() {
                    let _ = UnhookWinEvent(h);
                }
                let h = SetWinEventHook(
                    EVENT_OBJECT_LOCATIONCHANGE,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    None,
                    Some(win_event_cb),
                    pid,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                );
                if !h.is_invalid() {
                    self.loc_hook = Some(h);
                }
                self.loc_pid = pid;
            }
        }
        if self.cfg.general.monitor == "active" {
            let m = self.pick_monitor();
            if m != self.monitor {
                self.place();
                return;
            }
        }
        self.check_fullscreen();
        if !self.fs_hidden {
            unsafe {
                let _ =
                    SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }

    fn check_fullscreen(&mut self) {
        let hide = self.cfg.general.hide_in_fullscreen && unsafe { is_fullscreen_on(self.monitor, self.hwnd) };
        if hide != self.fs_hidden {
            self.fs_hidden = hide;
            if hide {
                self.collapse();
            }
            log!("fullscreen hidden = {hide}");
            self.request_frame();
        }
    }

    // -----------------------------------------------------------------------
    // Frame
    // -----------------------------------------------------------------------

    fn request_frame(&mut self) {
        self.dirty = true;
        self.slow_only = false;
        self.pacer.set_gap_us(self.frame_gap_us());
        self.pacer.start();
    }

    /// Shortest time between two frames, microseconds: the frame-rate cap from Settings → Look,
    /// a little under the refresh period so a 60 Hz screen is never held back.
    fn frame_gap_us(&self) -> u32 {
        let fps = self.cfg.appearance.max_fps.clamp(30.0, 240.0);
        (1_000_000.0 / fps * 0.8) as u32
    }

    fn targets(&self) -> Targets {
        let ap = &self.cfg.appearance;
        let base_w = ap.notch_width;
        let base_h = ap.notch_height;
        let mut t = Targets {
            w: base_w,
            h: base_h,
            // collapsed and hovering: only a gentle curve at the bottom corners
            r: (base_h * 0.28).clamp(6.0, 10.0),
            ear: (base_h * 0.26).clamp(5.0, 9.0),
            glass: 0.0,
            compact: false,
            peek: false,
            exp: false,
            shown: !(self.user_hidden || self.fs_hidden),
            sec: false,
        };
        match self.mode {
            Mode::Expanded => {
                let page_w = self.page_width();
                let content_w = page_w - side_pad() - right_pad();
                let content_h = page_content_h(&self.cfg, &self.modules, &self.started, self.page, content_w);
                t.w = page_w;
                t.h = top_h() + content_h + bottom_pad();
                t.r = if crate::ui::look_alcove() { (t.h * 0.23).clamp(34.0, 86.0) } else { 34.0 };
                t.ear = 30.0;
                t.glass = 1.0;
                t.exp = true;
            }
            Mode::Peek => {
                let pk = self.peek.as_ref().map(|a| &a.peek);
                if pk.is_some_and(|p| p.scan) && crate::ui::look_alcove() {
                    // unlock: the notch just gets wider, a padlock at the left and the word at the right
                    t.w = 260.0_f32.min(ap.expanded_width);
                    t.h = base_h;
                } else if pk.is_some_and(|p| p.scan) {
                    // unlock scan: the notch grows into a small square stage, then
                    // settles into a smaller one for the handwritten hello
                    let prog = self.peek.as_ref().map_or(0.0, |a| {
                        1.0 - a.until.saturating_duration_since(Instant::now()).as_secs_f32()
                            / a.peek.duration.as_secs_f32().max(0.1)
                    });
                    if prog > SCAN_HELLO_AT {
                        let title = pk.map_or("", |p| p.title.as_str());
                        let (w, h) = if crate::ui::look_alcove() {
                            crate::alcove::hello_stage(hello_name(title))
                        } else {
                            hello_stage(title, self.hello_custom())
                        };
                        t.w = w.min(ap.expanded_width);
                        t.h = h;
                    } else {
                        t.w = 230.0_f32.min(ap.expanded_width);
                        t.h = 168.0;
                    }
                    t.r = 34.0;
                    t.ear = 28.0;
                } else if pk.is_some_and(status_pill) {
                    // caps lock, microphone, USB drive: the name at the left, the state at the right
                    t.w = 300.0_f32.min(ap.expanded_width);
                    t.h = base_h;
                } else if pk.is_some_and(bt_pill) {
                    // a device connecting: the notch just gets wider, earbuds at the left, level ring at the right
                    t.w = (base_w + 40.0).max(220.0).min(ap.expanded_width);
                    t.h = base_h;
                } else if let Some(dh) = pk.and_then(|p| dial_height(p, &self.cfg)) {
                    // volume / brightness / battery / Bluetooth as one big round dial
                    t.w = 190.0_f32.min(ap.expanded_width);
                    t.h = base_h + dh;
                    t.r = 44.0;
                    t.ear = 26.0;
                } else if pk.is_some_and(|p| p.is_hud()) {
                    // volume / brightness / battery: one row, icon + bar + value (Alcove: only wider)
                    if crate::ui::look_alcove() {
                        t.w = 350.0_f32.min(ap.expanded_width);
                        t.h = base_h;
                    } else {
                        t.w = 300.0_f32.min(ap.expanded_width);
                        t.h = base_h + 34.0 + band_room(&self.cfg);
                        t.r = 26.0;
                        t.ear = 18.0;
                    }
                } else if pk.is_some_and(|p| p.is_compact()) {
                    // icon-only pill, like the unlock animation
                    t.w = (base_w + 70.0).max(250.0).min(ap.expanded_width);
                    t.h = base_h + 10.0;
                    t.r = 21.0;
                    t.ear = 10.0;
                } else if pk.is_some_and(crate::alcove::peek_notify) && crate::ui::look_alcove() {
                    // a message: avatar, name and two lines of text
                    t.w = (base_w + 190.0).max(430.0).min(ap.expanded_width);
                    t.h = base_h + 58.0 + band_room(&self.cfg);
                    // smooth, generous corners in the proportion of the open notch's
                    t.r = (t.h * 0.30).clamp(20.0, 34.0);
                    t.ear = 16.0;
                } else if pk.is_some_and(crate::alcove::peek_conn) && crate::ui::look_alcove() {
                    // a device or network: small and left-weighted
                    t.w = (base_w + 70.0).max(280.0).min(ap.expanded_width);
                    t.h = base_h + 24.0 + band_room(&self.cfg);
                    t.r = 30.0;
                    t.ear = 18.0;
                } else if pk.is_some_and(crate::alcove::peek_tall) && crate::ui::look_alcove() {
                    // a notice with a message: the text sits in its own box under the title
                    t.w = (base_w + 190.0).max(430.0).min(ap.expanded_width);
                    t.h = base_h + 92.0 + band_room(&self.cfg);
                    // smooth, generous corners in the proportion of the open notch's
                    t.r = (t.h * 0.30).clamp(20.0, 34.0);
                    t.ear = 16.0;
                } else if crate::ui::look_alcove() {
                    t.w = (base_w + 170.0).max(400.0).min(ap.expanded_width);
                    t.h = base_h + 46.0 + band_room(&self.cfg);
                    t.r = 32.0;
                    t.ear = 22.0;
                } else {
                    t.w = (base_w + 150.0).max(360.0).min(ap.expanded_width);
                    t.h = base_h + 44.0;
                    t.r = 24.0;
                    t.ear = 11.0;
                }
                t.glass = 0.55;
                t.peek = true;
            }
            Mode::Idle => {
                if let Some(a) = &self.activity {
                    t.w = if self.split_media() { base_w } else { base_w + 2.0 * a.wing * base_h };
                    if self.split_media() {
                        t.ear = 5.0;
                    }
                    t.compact = true;
                    t.sec = self.secondary.is_some() && t.shown;
                } else {
                    match self.cfg.general.idle_style.as_str() {
                        "pill" if !self.hovering => {
                            t.w = base_w * 0.55;
                            t.h = 7.0;
                            t.r = 3.5;
                            t.ear = 3.0;
                        }
                        "hidden" if !self.hovering => t.shown = false,
                        _ => {}
                    }
                }
                if self.activity.is_none() && !self.cfg.notch.widgets.is_empty() {
                    let shows = !matches!(self.cfg.general.idle_style.as_str(), "pill" | "hidden") || self.hovering;
                    if shows {
                        let n = self.cfg.notch.widgets.len().min(3) as f32;
                        t.w = t.w.max(26.0 + 66.0 * n);
                    }
                }
                if self.hovering && !self.user_hidden && crate::ui::look_alcove() && self.input.down.is_some() {
                    // pressed: the notch gives a little under the finger, then springs open on release
                    t.w -= 4.0;
                    t.h -= 2.0;
                } else if self.hovering && !self.user_hidden {
                    t.w += 10.0;
                    t.h += 3.0;
                    t.r += 1.0;
                }
            }
        }
        t
    }

    fn retarget(&mut self) {
        let t = self.targets();
        // note when the open notch, a peek and the compact activity start to come or go
        {
            let now = Instant::now();
            let sp = self.cfg.appearance.animation_speed.max(0.25);
            let cm = self.cfg.appearance.close_time;
            if self.veil_exp.on != t.exp {
                self.veil_exp = Veil { on: t.exp, at: now };
            }
            // content that comes back right after a close waits until the body has shrunk
            let wait = |exp: &Veil, now: Instant| -> Instant {
                let back = exp.at + Duration::from_secs_f32(RETURN_AFTER_CLOSE * cm / sp);
                if !exp.on && back > now {
                    back
                } else {
                    now
                }
            };
            if self.veil_peek.on != t.peek {
                self.veil_peek = Veil { on: t.peek, at: if t.peek { wait(&self.veil_exp, now) } else { now } };
            }
            if self.veil_act.on != t.compact {
                self.veil_act = Veil { on: t.compact, at: if t.compact { wait(&self.veil_exp, now) } else { now } };
            }
        }
        let split = self.split_media();
        let ap = &self.cfg.appearance;
        let speed = ap.animation_speed;
        let nothing = crate::ui::look_nothing();
        let alcove = crate::ui::look_alcove();
        let veils = alcove && self.cfg.appearance.motion_cache;
        let bloom = (crate::ui::look_bloom() && !alcove) || crate::ui::look_tuna();
        // Bloom settles like a smooth ease-out: almost no overshoot
        let bounce = if nothing {
            0.0
        } else if bloom {
            ap.bounce * 0.35
        } else {
            ap.bounce
        };
        // Nothing is crisp and mechanical
        let speed = if nothing {
            speed * 1.15
        } else if alcove {
            speed * 1.1
        } else if bloom {
            speed * 1.05
        } else {
            speed
        };
        let sp = speed.max(0.25);
        let (cm, om) = (self.cfg.appearance.close_time, self.cfg.appearance.open_time);
        let e_exp = Instant::now().saturating_duration_since(self.veil_exp.at).as_secs_f32() * sp;
        let e_peek = Instant::now().saturating_duration_since(self.veil_peek.at).as_secs_f32() * sp;
        // the open notch is on its way out (only that close is made quicker)
        let leaving = !t.exp && if veils { e_exp < OUT_BODY * cm + 0.4 } else { self.anim.exp.value > 0.05 };
        let a = &mut self.anim;
        // page changes follow the animation speed setting too
        a.page.set_params(1.05 / speed, 1.0);
        let grow = t.w > a.w.value + 0.5 || t.h > a.h.value + 0.5;
        // One spring character for the whole body (size, corner radius and ears move as one
        // object): a little softer on the way out, snappier on the way back in.
        let (resp, damp) = if grow {
            // Alcove: a visible but gentle overshoot on the way out
            (
                BODY_RESPONSE * if alcove && t.exp { OPEN_SPEED * om } else { 1.0 } / speed,
                if alcove { 1.0 - 0.8 * bounce } else { 1.0 - 0.45 * bounce },
            )
        } else {
            (BODY_RESPONSE * if alcove && leaving { 0.5 } else { 0.7 } / speed, 1.0 - 0.1 * bounce)
        };
        // Shape morphing: height leads, width follows a beat later and the corners keep
        // catching up, so the notch flows from one form to the other instead of scaling.
        let (kh, kw, kr) = if alcove { (0.9, 1.08, 1.3) } else { (1.0, 1.0, 1.0) };
        if t.exp && a.exp.value > 0.9 {
            // moving between open pages: the body resizes with the very same slow, smooth motion
            // as the page sliding up or down, so the two read as one movement
            a.h.set_params(0.77 / speed, 1.0);
            a.w.set_params(0.77 / speed, 1.0);
        } else {
            a.h.set_params(resp * kh, damp);
            a.w.set_params(resp * kw, damp);
        }
        a.r.set_params(resp * kr, damp.max(0.9));
        a.ear.set_params(resp * kr, damp.max(0.9));
        // Staged like the Mac apps: when closing, the content fades first and the body only then
        // shrinks around the empty space; when opening, the body leads and the content follows.
        let closing = !grow && !t.exp && if veils { e_exp < OUT_BODY * cm } else { a.exp.value > 0.05 };
        let hold = if closing {
            if veils {
                (OUT_BODY * cm - e_exp).max(0.0) / sp
            } else {
                CLOSE_HOLD / speed
            }
        } else if veils && !grow && !t.exp && !t.peek && self.peek_out.is_some() && e_peek < POUT_BODY {
            (POUT_BODY - e_peek).max(0.0) / sp
        } else {
            0.0
        };
        a.w.set_target_delayed(t.w, hold);
        a.h.set_target_delayed(t.h, hold);
        a.r.set_target_delayed(t.r, hold);
        a.ear.set_target_delayed(t.ear, hold);
        a.glass.set_target(t.glass);
        a.shown.set_params(0.5 / speed, 1.0 - 0.5 * bounce);
        a.shown.set_target(if t.shown { 1.0 } else { 0.0 });
        a.split.set_params(0.5 / speed, 1.0 - 0.7 * bounce);
        a.split.set_target(if split && t.shown { 1.0 } else { 0.0 });
        a.sec.set_params(0.45 / speed, 1.0 - 0.6 * bounce);
        a.sec.set_target(if t.sec { 1.0 } else { 0.0 });
        let out = 0.12;
        let fade = |s: &mut Spring, on: bool, delay: f32| {
            if on {
                s.set_params(0.24 / speed, 1.0);
                s.set_target_delayed(1.0, delay / speed);
            } else {
                s.set_params(out / speed, 1.0);
                s.set_target(0.0);
            }
        };
        let back = if veils && closing { RETURN_AFTER_CLOSE * cm } else { 0.0 };
        fade(&mut a.compact, t.compact, 0.03 + back);
        fade(&mut a.peek, t.peek, 0.04 + back);
        fade(&mut a.exp, t.exp, 0.09);
    }

    fn frame(&mut self) {
        let now = Instant::now();
        // (the pacer already leaves out refreshes beyond the cap; this catches the rest)
        if now.duration_since(self.last_frame).as_micros() < self.frame_gap_us() as u128 * 3 / 4 {
            return;
        }
        // Ambient animation only (a sweeping shimmer, the record turning): nothing is being moved by
        // a spring and nobody touched anything. Frames closer together than AMBIENT_GAP are skipped,
        // which only trims displays faster than 60 Hz; on a 60 Hz screen every frame is drawn, since
        // halving to 30 fps made sweeps and shimmers look choppy.
        if !self.dirty && self.settled && now.duration_since(self.last_frame) < AMBIENT_GAP {
            return;
        }
        // a slow animation (floating icon, scanning spinner) is drawn at about 30 frames a second;
        // anything else that wants a frame (the pointer moving, a click, a spring) clears this flag
        if self.slow_only && self.settled && now.duration_since(self.last_frame) < Duration::from_millis(30) {
            return;
        }
        // the slow last stretch of a move: every spring is creeping the final bit into place, a
        // fraction of a pixel a frame, so every other frame is left out (about 30 a second; the
        // motion is too small to show it). Input or anything else that wants a frame ends it.
        if self.tail
            && !self.dirty
            && now.duration_since(self.last_frame)
                < Duration::from_micros(self.frame_gap_us() as u64 * 15 / 8)
        {
            self.tm[8] += 1;
            return;
        }
        self.slow_only = false;
        let dt = (now - self.last_frame).as_secs_f32().min(1.0 / 20.0);
        self.last_frame = now;
        self.dirty = false;
        self.trim_at = None;
        let shown = !(self.user_hidden || self.fs_hidden) && self.mode != Mode::Expanded;
        if PEEKS_SHOWN.swap(shown, Ordering::Relaxed) != shown {
            self.broadcast(SystemEvent::PeeksShown(shown));
        }
        self.process_timeouts(now);
        self.refresh_activity();
        self.retarget();
        let moving = self.anim.step(dt);
        self.tail = moving && self.anim.creeping();
        if self.page_from.is_some() {
            let done = if crate::ui::look_alcove() && self.cfg.appearance.motion_cache {
                now.saturating_duration_since(self.page_at).as_secs_f32() * self.cfg.appearance.animation_speed.max(0.25)
                    >= PAGE_SECS * self.cfg.appearance.page_time
            } else {
                self.anim.page.value > 0.998 && self.anim.page.target >= 1.0
            };
            if done {
                self.page_from = None;
            }
        }
        self.settled = !moving;
        let t_draw = self.debug.then(Instant::now);
        let fx = self.render(now);
        if let Some(t) = t_draw {
            let us = t.elapsed().as_micros() as u64;
            self.tm[0] += us;
            self.tm[1] = self.tm[1].max(us);
            self.tm[2] += 1;
        }
        let animate = fx.animate;
        self.apply_effects(fx);
        self.update_region(!moving);
        self.input.click = None;
        self.input.right_click = None;
        self.input.wheel = 0.0;
        self.input.hwheel = 0.0;
        if self.debug {
            let st = &mut self.stats;
            st.0.get_or_insert(now);
            if moving {
                st.1 += 1;
            } else if animate {
                st.2 += 1;
            } else if self.dirty {
                st.3 += 1;
            }
        }
        if !moving && !animate && !self.dirty {
            if let (true, Some(t0)) = (self.debug, self.stats.0) {
                let (_, a, b, c) = self.stats;
                log!(
                    "burst: {} frames in {} ms (spring {a}, animate {b}, dirty {c}) mode {:?}",
                    a + b + c + 1,
                    t0.elapsed().as_millis(),
                    self.mode
                );
                self.stats = (None, 0, 0, 0);
                let tm = self.tm;
                let ms = |us: u64| us as f64 / 1000.0;
                log!(
                    "  timing: draw {:.2} ms avg, {:.2} ms worst | pictures {:.1} ms, bottom blur {:.1} ms, glass check {:.1} ms (totals) | {} glass rebuilds, {:.1} ms (total) | {} frames skipped in the slow tail | glass: middle warp {:.2}, middle blur {:.2}, full-detail zone {:.1}",
                    ms(tm[0]) / tm[2].max(1) as f64,
                    ms(tm[1]),
                    ms(tm[3]),
                    ms(tm[4]),
                    ms(tm[5]),
                    tm[7],
                    ms(tm[6]),
                    tm[8],
                    self.cfg.glass.middle_warp,
                    self.cfg.glass.middle_blur,
                    self.cfg.glass.sharp_zone
                );
                self.tm = [0; 9];
            }
            self.pacer.stop();
            // Release GPU scratch memory once nothing has been drawn for a
            // while. Doing it after every burst would rebuild the glyph and
            // image caches on each once-a-second update (stats, timer, media).
            self.trim_at = Some(now + TRIM_DELAY);
        }
        if !self.shown_once {
            self.shown_once = true;
        }
        self.schedule_wake();
    }

    fn update_region(&mut self, settled: bool) {
        let ear = self.anim.ear.target.max(self.anim.ear.value);
        let t = self.targets();
        let shown = t.shown;
        let bubble = if t.sec { self.cfg.appearance.notch_height + 8.0 + ear } else { 0.0 };
        let side = if self.split_media() { self.cfg.appearance.notch_height + SPLIT_GAP } else { 0.0 };
        let tb = Rect::new(self.win_w * 0.5 - t.w * 0.5 - ear - side, 0.0, t.w + 2.0 * (ear + side) + bubble, t.h);
        let cur = self.body_rect();
        let mut cb = Rect::new(cur.x - ear, 0.0, cur.w + 2.0 * ear, cur.bottom().max(0.0));
        if self.anim.sec.value > 0.01 {
            cb = cb.union(&self.bubble_rect().expand(ear));
        }
        for (r, ..) in self.bubble_states(Instant::now()) {
            cb = cb.union(&r.expand(ear + 6.0));
        }
        if self.anim.split.value > 0.01 {
            let (l, r) = self.split_rects();
            cb = cb.union(&l.expand(ear)).union(&r.expand(ear));
        }
        let strip_w = self.cfg.appearance.notch_width;
        let strip = Rect::new(self.win_w * 0.5 - strip_w * 0.5, 0.0, strip_w, 5.0);
        let want = if self.fs_hidden {
            None
        } else if settled {
            self.region_anim = None;
            let mut r = if shown { tb.union(&strip) } else { strip };
            // the lingering bubble sits outside the resting notch
            for (lr, ..) in self.bubble_states(Instant::now()) {
                r = r.union(&lr.expand(ear + 6.0));
            }
            Some(r)
        } else {
            let mut r = tb.expand(16.0).union(&cb).union(&strip);
            if let Some(prev) = self.region_anim {
                r = r.union(&prev);
            }
            self.region_anim = Some(r);
            Some(r)
        };
        let s = self.scale;
        let px = match want {
            Some(r) => (
                (r.x * s).floor().max(0.0) as i32,
                0,
                (r.right() * s).ceil() as i32,
                ((r.bottom() * s).ceil() as i32).min(self.win_px.3),
            ),
            None => (0, 0, 0, 0),
        };
        if self.region != Some(px) {
            self.region = Some(px);
            unsafe {
                let rgn = CreateRectRgn(px.0, px.1, px.2, px.3);
                SetWindowRgn(self.hwnd, Some(rgn), false);
            }
        }
    }

    /// The header tabs in the user's order (Home first).
    fn tab_list(&self) -> Vec<(ModuleId, Icon, String)> {
        let mut tabs: Vec<(ModuleId, Icon, String)> = std::iter::once((HOME, Icon::Home, String::new()))
            .chain(
                self.modules
                    .iter()
                    .enumerate()
                    .filter(|(i, m)| {
                        self.started[*i]
                            && m.has_page()
                            && m.id() != "settings"
                            && m.id() != modules::alcove_launcher::ID
                            && m.id() != modules::mixer::ID
                    })
                    .map(|(_, m)| (m.id(), m.icon(), String::new())),
            )
            .collect();
        // the user's tab order (Home stays first; unlisted tabs keep their place at the end)
        if !self.cfg.tabs.order.is_empty() && tabs.len() > 1 {
            let order = &self.cfg.tabs.order;
            tabs[1..].sort_by_key(|t| order.iter().position(|o| o == t.0).unwrap_or(usize::MAX));
        }
        tabs
    }

    /// Alcove: the bottom edge of the open notch (a page, or a bigger peek such as the volume dial
    /// or a message) softens into a light blur. Only the thin strip along the bottom is drawn a
    /// second time into a small picture (and only on frames that are drawn anyway), then laid over
    /// the live drawing in `bottom_fade`. It is there as soon as the notch is open, also while pages
    /// switch.
    fn prepare_band(&mut self, now: Instant) {
        let t = self.targets();
        let tw = self.page_width();
        let body = self.body_rect();
        let ap = &self.cfg.appearance;
        let on = ap.bottom_fade && ap.blur && crate::ui::look_alcove() && !self.band_failed;
        let page_band = on
            && self.mode == Mode::Expanded
            && self.page != "settings"
            && self.anim.exp.value >= 0.88
            && body.h >= 56.0;
        let peek_band = on
            && self.mode == Mode::Peek
            && self.anim.peek.value >= 0.95
            && body.h >= 56.0
            && (body.h - t.h).abs() < 24.0
            && self.peek.as_ref().is_some_and(|a| a.swap.is_none());
        if !page_band && !peek_band {
            if self.band_since.is_some() {
                log!("bottom blur off: mode {:?} page {} exp {:.3} dh {:.1}", self.mode, self.page, self.anim.exp.value, body.h - t.h);
            }
            self.snap_band = None;
            self.band_since = None;
            return;
        }
        self.band_since.get_or_insert(now);
        let input = Input::default();
        let mut scratch = Effects::default();
        let (accent, page, scale) = (self.accent, self.page, self.scale);
        if page_band {
            let full = Rect::new(body.cx() - tw * 0.5, body.y, tw, t.h);
            let content =
                page_content(body.cx(), full.y, page_size(&self.cfg, &self.modules, &self.started, self.page));
            // the strip sits on the notch's real bottom edge, also while it is still growing or
            // shrinking to the next page's size, and is as wide as the wider of the two
            let h = BAND_H.min(body.h * 0.4);
            let (rx, rr) = (full.x.min(body.x), full.right().max(body.right()));
            let region = Rect::new(rx, body.bottom() - h, rr - rx, h);
            let trans = self.page_from.map(|(from, dir)| (from, dir, self.anim.page.value.clamp(0.0, 1.0)));
            let tabs = if trans.is_some() { self.tab_list() } else { Vec::new() };
            let pages = self.snap_pages.as_ref().map(|(a, b)| (a, b));
            self.snap_band = self.res.render_snapshot(&self.gfx, scale, region, 0xB4D, self.snap_band.take(), true, |p| {
                let mut ui = Ui {
                    p,
                    input: &input,
                    fx: &mut scratch,
                    cfg: &self.cfg,
                    bus: &self.bus,
                    now,
                    accent,
                    hot: false,
                    interactive: false,
                };
                match trans {
                    Some(_) => draw_expanded_core(
                        &mut ui,
                        full,
                        &mut self.modules,
                        &self.started,
                        &tabs,
                        page,
                        trans,
                        &self.indicators,
                        pages,
                    ),
                    None => draw_body(&mut ui, content, &mut self.modules, &self.started, page),
                }
            });
        } else if let Some(act) = &self.peek {
            let h = BAND_H.min(body.h * 0.4);
            let region = Rect::new(body.x, body.bottom() - h, body.w, h);
            let left = (act.until.saturating_duration_since(now).as_secs_f32()
                / act.peek.duration.as_secs_f32().max(0.1))
            .clamp(0.0, 1.0);
            self.snap_band = self.res.render_snapshot(&self.gfx, scale, region, 0xB4E, self.snap_band.take(), true, |p| {
                let mut ui = Ui {
                    p,
                    input: &input,
                    fx: &mut scratch,
                    cfg: &self.cfg,
                    bus: &self.bus,
                    now,
                    accent,
                    hot: false,
                    interactive: false,
                };
                draw_peek(&mut ui, body, &act.peek, left, None);
            });
        }
        // blend the strip's blur levels into one picture, drawn later in a single fill
        let baked = match self.snap_band.as_mut() {
            Some(s) => self.res.bake_band(&self.gfx, s),
            None => true,
        };
        if !baked {
            log!("bottom blur could not be built: staying solid");
            self.band_failed = true;
            self.snap_band = None;
            self.band_since = None;
        }
    }

    /// Alcove glass edge. The screen behind the notch is copied once, on the first frame after the
    /// notch leaves its resting size (the window is still small then, so the copy is clean), and
    /// let go a moment after the notch closes. Once the notch has settled, the rim is worked out
    /// from that copy and uploaded as one small picture; that is redone only if the size changes.
    fn prepare_rim(&mut self, now: Instant) {
        if !(self.cfg.appearance.glass_look && crate::ui::look_glass()) || self.mode == Mode::Idle {
            // nothing is kept while the notch is closed: the next opening takes a fresh copy
            self.refr_shot = None;
            self.refr_rim = None;
            self.refr_dead = false;
            self.rim_ok = false;
            self.rim_k = 0.0;
            self.live_off();
            return;
        }
        // a change in Settings → Glass: the rim is made again with the new look
        let tune = crate::refract::Tune::from(&self.cfg.glass);
        if self.rim_tune != Some(tune) {
            self.rim_tune = Some(tune);
            self.refr_rim = None;
            self.rim_old = None;
            self.live_sum = 0;
        }
        let fg = unsafe { GetForegroundWindow() };
        // (with live glass the copy is renewed anyway, so a new window in front is no reason to stop)
        if self.refr_shot.is_some() && !self.excl && fg != self.hwnd && fg.0 as isize != self.refr_fg {
            // another window came to the front: the copy no longer matches what is behind the
            // notch, so the rim goes rather than show an old picture (until the next opening)
            self.refr_shot = None;
            self.refr_rim = None;
            self.refr_dead = true;
        }
        if self.refr_shot.is_none()
            && !self.refr_dead
            && self.refr_retry.is_none_or(|t| now.duration_since(t) > Duration::from_secs(2))
        {
            let (x, y, w, h) = self.win_px;
            self.refr_shot = crate::refract::grab(x, y, w, h);
            // (no fingerprint yet: the first refresh always counts as new)
            self.live_sum = 0;
            self.refr_retry = Some(now);
            self.refr_fg = fg.0 as isize;
            if self.refr_shot.is_none() {
                log!("glass edge: the screen could not be copied");
            }
        }
        // the rim is there as soon as the notch is open and follows it: while a page change
        // resizes the notch it is made again for every size on the way (a small picture), so it
        // moves with the page instead of waiting for it. Not while the notch itself opens or closes.
        let body = self.body_rect();
        let open = self.anim.exp.value > 0.98 || self.anim.peek.value > 0.95;
        if !open {
            self.rim_ok = false;
            self.live_off();
            return;
        }
        self.live_sync(now);
        let Some(shot) = self.refr_shot.as_ref() else {
            self.rim_ok = false;
            return;
        };
        if body.h < 56.0 {
            self.refr_rim = None;
            self.rim_ok = false;
            return;
        }
        let sc = self.scale;
        let r = self.anim.r.value.min(body.w * 0.5).min(body.h - self.anim.ear.value.min(body.h * 0.5)).max(0.0);
        // while the size changes the shape is followed to a whole pixel (far fewer rebuilds in the slow
        // tail of a move); once it has settled, to a quarter pixel
        let qs = if self.settled { 4.0 } else { 1.0 };
        let q4 = |v: f32| (v * sc * qs).round() as i32;
        let geo = (q4(body.x), q4(body.w), q4(body.bottom()), q4(r), (sc * 100.0).round() as i32);
        // while the size is still changing a coarse rim is enough (a quarter of the work); the
        // frame after it has stopped makes the fine one
        // (live glass keeps making half-detail rims, so that is what "fine" means while it is on)
        let fine = 1;
        let still = self.rim_geo == Some(geo);
        // how far the notch still has to go, and how far it had to go when this move began: in the
        // last part of a real move (the last ~12 %, where it is slow) the rim is already made at full
        // detail, so it is sharp by the time the notch arrives
        let left = (self.anim.w.target - self.anim.w.value).abs() + (self.anim.h.target - self.anim.h.value).abs();
        self.rim_far = if still { 0.0 } else { self.rim_far.max(left) };
        let late = self.rim_far >= 24.0 && left <= self.rim_far * self.cfg.glass.sharpen_at.clamp(0.02, 0.5);
        // (the picture is sharp along the edge and half detail elsewhere, cheap enough to use always)
        let step = fine;
        let _ = late;
        let lite = self.cfg.appearance.lite_motion;
        // lighter animations: while the size changes the glass edge is rebuilt at most every 45 ms and
        // the picture already there keeps showing in between (it is a few pixels behind at worst)
        if lite
            && !self.settled
            && !still
            && self.refr_rim.is_some()
            && self.rim_t.is_some_and(|t| now.saturating_duration_since(t) < (self.rim_cost * 3).clamp(Duration::from_millis(16), Duration::from_millis(45)))
        {
            self.rim_ok = true;
            return;
        }
        self.rim_geo = Some(geo);
        // While the size is still changing the sharp zone is capped: the glass is rebuilt for nearly
        // every frame then, and the full-detail band along the edge is what costs most. The full
        // picture follows as soon as the notch stops and melts in over this one.
        let moving_zone: f32 = if lite { 0.4 } else { 0.55 };
        let low = !self.settled && !still && (lite || tune.sharp > moving_zone);
        // (the moving glass keeps full pixel detail along the edge: coarser pictures looked pixelated)
        let mstep = step;
        let key: RimKey = (geo.0, geo.1, geo.2, geo.3, geo.4, mstep + if low { RIM_LOW } else { 0 });
        // where the notch is see-through: the strip along the bottom and, below 100 % panel opacity,
        // all of it (the middle glass follows that)
        let ap = &self.cfg.appearance;
        let bhh = BAND_H.min(body.h * 0.4);
        let band_on = ap.bottom_fade && ap.blur && self.page != "settings" && body.h > bhh * 2.0;
        let opaque = if ap.blur { ap.expanded_opacity } else { ap.expanded_opacity.max(0.95) };
        // the colour fades out slowly and is gone (pure black) at half the height of the notch (the
        // "Bottom fade start" setting moves that point; 1.7 is half). Rounded to steps of 8 so the glass
        // is not rebuilt for every tiny change while the size settles.
        let band_px = if band_on {
            ((body.h * (self.cfg.glass.fade_start.clamp(0.5, 3.0) / 3.4) * sc) / 8.0).round().max(1.0) * 8.0
        } else {
            0.0
        };
        let fade_to = if band_on { self.cfg.glass.fade_amount.clamp(0.0, 1.0) } else { 0.0 };
        let clear = (1.0 - opaque).clamp(0.0, 1.0);
        let extra = ((band_px * 4.0) as i32, (fade_to * 1000.0) as i32, (clear * 1000.0) as i32);
        if self.rim_extra != Some(extra) {
            self.rim_extra = Some(extra);
            self.refr_rim = None;
            self.rim_old = None;
            self.live_sum = 0;
        }
        let shape = crate::refract::Shape {
            top: body.y * sc,
            band: band_px,
            fade: fade_to,
            clear,
            left: (body.x + crate::gfx::wall_taper(body.h) * 0.5) * sc,
            right: (body.right() - crate::gfx::wall_taper(body.h) * 0.5) * sc,
            bottom: body.bottom() * sc,
            radius: r * sc,
            scale: sc,
            tune,
        };
        // a live copy covers only the notch at the size it had; if the size has changed, the whole
        // screen is copied again (once), so the new shape has pixels to read
        if !shot.full && shot.made_for != Some(shape) {
            self.refr_shot = None;
            self.refr_retry = None;
            self.request_frame();
            return;
        }
        self.rim_last = Some((geo, shape));
        self.rim_last_rect = Some(body);
        if self.refr_rim.as_ref().is_some_and(|(k, _)| *k == key) {
            self.rim_ok = true;
            return;
        }
        let prev = self.refr_rim.take();
        let prev_rect = self.rim_rect;
        let t_rim = self.debug.then(Instant::now);
        let mut made = shape;
        if low {
            made.tune.sharp = moving_zone;
            if lite {
                // lighter animations: while the notch moves the middle is left clear (no bend, no blur,
                // about half the work); the full look melts in as soon as it rests
                made.tune.mid_warp = 0.0;
                made.tune.mid_blur = 0.0;
            }
        }
        let t_made = Instant::now();
        let built = crate::refract::rim(shot, made, mstep);
        self.rim_cost = (self.rim_cost * 3 + t_made.elapsed()) / 4;
        if let Some(rim) = built {
            if let Some(pic) = self.res.rim_pic(rim.x0, rim.y0, rim.w, rim.h, rim.step, &rim.px, sc) {
                self.refr_rim = Some((key, pic));
                self.rim_rect = Some(body);
                // the notch has just stopped and the fine rim replaces the coarse one made while it
                // moved: the two melt into each other instead of one jumping to the other
                if let Some(old) = prev {
                    if old.0 .5 > key.5 && rim_near(&old.0, &key) {
                        self.rim_old = Some(old);
                        self.rim_old_rect = prev_rect;
                        self.rim_x = 0.0;
                    }
                }
            }
        }
        self.rim_t = Some(now);
        if let Some(t) = t_rim {
            self.tm[6] += t.elapsed().as_micros() as u64;
            self.tm[7] += 1;
        }
        self.rim_ok = self.refr_rim.is_some();
        if low && self.rim_ok {
            // one more frame so the fine picture follows as soon as the notch has stopped
            self.request_frame();
        }
    }

    // ----- live glass -----------------------------------------------------------------------

    /// Live glass is switched on, the notch is hidden from screen copies and there is a copy.
    fn live_armed(&self) -> bool {
        self.excl && self.refr_shot.is_some() && self.cfg.appearance.glass_live
    }

    /// ... and the rim is showing, so a refresh would be seen.
    fn live_wanted(&self) -> bool {
        self.live_armed() && self.rim_ok
    }

    /// Media is playing, or stopped only a moment ago: the screen behind the notch may be moving.
    fn live_media(&mut self, now: Instant) -> bool {
        let playing = self.module_index("media").is_some_and(|i| self.started[i] && self.modules[i].is_playing());
        if playing {
            self.play_seen = Some(now);
        }
        self.play_seen.is_some_and(|t| now.saturating_duration_since(t) < LIVE_LINGER)
    }

    /// Hide the notch from screen copies (or show it again). Needs Windows 10 version 2004 or newer;
    /// when that fails live glass is simply off.
    fn set_excl(&mut self, on: bool) {
        if self.excl == on {
            return;
        }
        let how = if on { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE };
        match unsafe { SetWindowDisplayAffinity(self.hwnd, how) } {
            Ok(()) => self.excl = on,
            Err(e) => {
                if on {
                    log!("live glass: the notch cannot be hidden from screen copies here ({e}); not used");
                    self.live_dead = true;
                }
            }
        }
    }

    /// Time between looks at the screen while live glass is busy (Settings → Glass).
    fn live_fast(&self) -> Duration {
        Duration::from_millis(self.cfg.glass.live_ms.clamp(80.0, 500.0) as u64)
    }

    /// The notch is open: switch live glass on or off to match the setting and arm the first refresh.
    fn live_sync(&mut self, now: Instant) {
        let want = self.cfg.appearance.glass_live && !self.live_dead && self.live_media(now);
        self.set_excl(want);
        if !(want && self.excl) {
            self.live_next = None;
            self.live_rx = None;
            return;
        }
        if self.live_next.is_none() && self.live_rx.is_none() && self.refr_shot.is_some() {
            self.live_same = 0;
            self.live_next = Some(now + self.live_fast());
        }
    }

    /// The notch is closed (or the look is off): everything live stops and the notch shows in
    /// screen copies again.
    fn live_off(&mut self) {
        self.set_excl(false);
        self.live_next = None;
        self.live_rx = None;
        self.live_same = 0;
        self.rim_old = None;
        self.rim_x = 1.0;
        self.play_seen = None;
    }

    /// Timer: pick up a finished refresh, and start the next one when it is due and the notch holds
    /// still (a refresh is made for one size; a moving notch would leave it out of date).
    fn live_tick(&mut self, now: Instant) {
        self.live_done();
        if !self.live_armed() || self.live_rx.is_some() {
            return;
        }
        if !self.live_media(now) {
            // the media has been quiet for a while: the picture behind is as good as it will get
            self.live_off();
            return;
        }
        if self.live_next.is_none_or(|t| t > now + Duration::from_millis(2)) {
            return;
        }
        let steady = self.rim_last.is_some_and(|(geo, _)| self.rim_geo == Some(geo));
        let (Some((_, shape)), true) = (self.rim_last, steady && self.rim_ok && self.page_from.is_none() && self.settled)
        else {
            self.live_next = Some(now + Duration::from_millis(120));
            return;
        };
        let (x, y, w, h) = self.win_px;
        let prev = self.live_sum;
        let hwnd = SendHwnd::new(self.hwnd);
        let (tx, rx) = std::sync::mpsc::channel();
        let job = std::thread::Builder::new().name("glass".into()).spawn(move || {
            // behind everything else: the notch's own drawing always comes first
            unsafe {
                use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            }
            let out = crate::refract::refresh(x, y, w, h, prev, shape);
            let _ = tx.send(out);
            unsafe {
                let _ = PostMessageW(Some(hwnd.hwnd()), WM_APP_GLASS, WPARAM(0), LPARAM(0));
            }
        });
        if job.is_ok() {
            self.live_rx = Some(rx);
            self.live_next = None;
        } else {
            self.live_next = Some(now + self.live_fast() * 4);
        }
    }

    /// A refresh has finished: use the new copy, and melt the new rim in over the old one.
    fn live_done(&mut self) {
        let Some(rx) = self.live_rx.as_ref() else { return };
        let out = match rx.try_recv() {
            Ok(out) => out,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        self.live_rx = None;
        let fresh = match out {
            Some(crate::refract::Fetched::Fresh(f)) => Some(f),
            _ => None,
        };
        // a failed copy counts like "nothing new": it is simply tried again later
        self.live_same = if fresh.is_some() { 0 } else { (self.live_same + 1).min(8) };
        let gap = if self.live_same >= 3 { self.live_fast() * 4 } else { self.live_fast() };
        self.live_next = Some(Instant::now() + gap);
        if let Some(f) = fresh {
            if self.live_wanted() {
                self.live_sum = f.sum;
                self.refr_shot = Some(f.shot);
                let sc = self.scale;
                // the rim made on the thread is only good if the notch kept its size meanwhile;
                // otherwise the next frame makes it again from the new copy
                if let (Some(rim), Some((geo, shape))) = (f.rim, self.rim_last) {
                    if shape == f.shape && self.page_from.is_none() {
                        if let Some(pic) = self.res.rim_pic(rim.x0, rim.y0, rim.w, rim.h, rim.step, &rim.px, sc) {
                            let key: RimKey = (geo.0, geo.1, geo.2, geo.3, geo.4, rim.step);
                            self.rim_old = self.refr_rim.take();
                            self.rim_old_rect = self.rim_rect;
                            self.rim_rect = self.rim_last_rect;
                            self.rim_x = if self.rim_old.is_some() { 0.0 } else { 1.0 };
                            self.refr_rim = Some((key, pic));
                            self.request_frame();
                        }
                    }
                }
            }
        }
        self.schedule_wake();
    }

    /// 1 while the soft bottom edge is showing, else 0.
    fn band_k(&self) -> f32 {
        if self.snap_band.is_some() {
            1.0
        } else {
            0.0
        }
    }

    /// While the notch opens or closes, and while a page slides in, the content itself does not
    /// change: it is only moved and faded. So it is drawn once into a picture and the frames of the
    /// animation just place that picture (one rectangle fill instead of laying out and drawing
    /// every text, icon and beam again). Drawn live again as soon as the motion has settled.
    fn prepare_snapshots(&mut self, now: Instant) {
        let t0 = self.debug.then(Instant::now);
        self.update_veils(now);
        self.prepare_pictures(now);
        self.prepare_small(now);
        self.prepare_blur(now);
        let t1 = self.debug.then(Instant::now);
        // (the blurred strip is drawn without the outline: it is a soft copy of the page anyway)
        self.res.set_halo(None);
        self.prepare_band(now);
        let t2 = self.debug.then(Instant::now);
        self.prepare_rim(now);
        if let (Some(t0), Some(t1), Some(t2)) = (t0, t1, t2) {
            self.tm[3] += (t1 - t0).as_micros() as u64;
            self.tm[4] += (t2 - t1).as_micros() as u64;
            self.tm[5] += t2.elapsed().as_micros() as u64;
        }
    }

    /// Where the see-through glass begins at the bottom of a notch whose bottom edge is at `bottom` and
    /// that is `h` tall: text below that line gets a thin dark outline (see `Res::set_halo`). With
    /// the whole panel see-through (opacity below 100 %) all of it does.
    fn halo_y(&self, bottom: f32, h: f32) -> Option<f32> {
        let ap = &self.cfg.appearance;
        let opaque = if ap.blur { ap.expanded_opacity } else { ap.expanded_opacity.max(0.95) };
        if 1.0 - opaque > 0.05 {
            return Some(f32::MIN);
        }
        let bhh = BAND_H.min(h * 0.4);
        let band_on = ap.bottom_fade && ap.blur && crate::ui::look_alcove() && self.page != "settings" && h > bhh * 2.0;
        band_on.then(|| bottom - bhh * self.cfg.glass.fade_start.clamp(0.5, 3.0) * 0.6)
    }

    /// While a peek (volume, brightness, battery, a notification, the unlock greeting ...) or the
    /// compact live activity fades in or out, its content is drawn into a small picture on every
    /// frame, so it can be shown going soft: the same look as the page switch. The pictures cover
    /// the notch at its widest during the move, so the buffers are re-used (nothing is allocated per
    /// frame), and both are let go the moment the content is crisp again: nothing is kept while idle.
    /// Work out what the open notch, the peeks and the compact activity are doing this frame.
    fn update_veils(&mut self, now: Instant) {
        let mut f = VeilFrame::default();
        if crate::ui::look_alcove() && self.cfg.appearance.motion_cache {
            let sp = self.cfg.appearance.animation_speed.max(0.25);
            let (cm, om) = (self.cfg.appearance.close_time, self.cfg.appearance.open_time);
            let el = |v: &Veil| now.saturating_duration_since(v.at).as_secs_f32() * sp;
            let e = el(&self.veil_exp);
            f.exp = if self.veil_exp.on {
                run(&IN_A, &IN_S, (e - OPEN_DELAY * om) / (IN_T * om), IN_END)
            } else {
                run(&OUT_A, &OUT_S, e / (CLOSE_T * cm), OUT_END)
            };
            // (with the notch open, peeks and the activity are not shown at all)
            if !self.veil_exp.on {
                let e = el(&self.veil_peek);
                f.peek = if self.veil_peek.on {
                    self.peek.as_ref().and_then(|_| run(&PIN_A, &PIN_S, e - PEEK_DELAY, PIN_END))
                } else {
                    self.peek_out.as_ref().and_then(|_| run(&POUT_A, &POUT_S, e, POUT_END))
                };
                let e = el(&self.veil_act);
                if self.veil_act.on && self.activity.is_some() {
                    f.act = run(&PIN_A, &PIN_S, e - PEEK_DELAY, PIN_END)
                        .map(|l| Layer { sigma: l.sigma * 0.75, ..l });
                }
            }
        }
        // the picture of a peek that has gone is let go once its blur-out is over
        if self.peek_out.is_some() && (self.veil_peek.on || f.peek.is_none()) {
            self.peek_out = None;
        }
        self.vf = f;
    }

    /// Blur the pictures that are fading this frame (see `Res::blur_snapshot`); the others let go
    /// of their blur pictures so no memory is kept while nothing animates.
    fn prepare_blur(&mut self, now: Instant) {
        let k = BLUR_K * self.scale;
        let res = &mut self.res;
        let mut go = |s: &mut Option<Snapshot>, l: Option<Layer>| {
            if let Some(s) = s.as_mut() {
                match l {
                    Some(l) => {
                        res.blur_snapshot(s, l.sigma * k);
                    }
                    None => s.release_blur(),
                }
            }
        };
        go(&mut self.snap_all, self.vf.exp);
        go(&mut self.snap_peek, self.vf.peek);
        go(&mut self.snap_act, self.vf.act);
        let paging = self.page_from.is_some() && crate::ui::look_alcove() && self.cfg.appearance.motion_cache;
        if let Some((a, b)) = self.snap_pages.as_mut() {
            if paging {
                let e = now.saturating_duration_since(self.page_at).as_secs_f32()
                    * self.cfg.appearance.animation_speed.max(0.25);
                let (old_l, new_l) = page_layers(e, self.cfg.appearance.page_time);
                match old_l {
                    Some(l) => {
                        res.blur_snapshot(a, l.sigma * k);
                    }
                    None => a.release_blur(),
                }
                res.blur_snapshot(b, new_l.sigma * k);
            } else {
                a.release_blur();
                b.release_blur();
            }
        }
    }

    fn prepare_small(&mut self, now: Instant) {
        let on = crate::ui::look_alcove()
            && self.cfg.appearance.motion_cache
            && self.snap_band.is_none()
            && self.mode != Mode::Expanded;
        let want_peek = on
            && self.vf.peek.is_some()
            && self.peek.as_ref().or(self.peek_out.as_ref()).is_some_and(|pk| pk.swap.is_none());
        let want_act = on && self.vf.act.is_some() && self.activity.is_some() && self.anim.split.value <= 0.01;
        if !want_peek {
            self.snap_peek = None;
        }
        if !want_act {
            self.snap_act = None;
            self.small_bars = None;
        }
        if !(want_peek || want_act) {
            self.small_dim = (0.0, 0.0);
            return;
        }
        let body = self.body_rect();
        let tgt = self.targets();
        self.small_dim.0 = self.small_dim.0.max(body.w).max(tgt.w);
        self.small_dim.1 = self.small_dim.1.max(body.h).max(tgt.h);
        let rw = (self.small_dim.0 / 16.0).ceil() * 16.0 + 16.0;
        let rh = (self.small_dim.1 / 8.0).ceil() * 8.0 + 8.0;
        let x = (body.cx() - rw * 0.5).round();
        let (x0, y0) = (x.max(0.0), body.y.max(0.0));
        let (x1, y1) = ((x + rw).min(self.win_w), (body.y + rh).min(self.win_h));
        let region = Rect::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0));
        let scale = self.scale;
        let accent = self.accent;
        let input = Input::default();
        let mut scratch = Effects::default();
        if want_peek {
            let reuse = self.snap_peek.take();
            if let Some(pk) = self.peek.as_ref().or(self.peek_out.as_ref()) {
                let left =
                    (pk.until.saturating_duration_since(now).as_secs_f32() / pk.peek.duration.as_secs_f32().max(0.1))
                        .clamp(0.0, 1.0);
                self.snap_peek = self.res.render_snapshot(&self.gfx, scale, region, 7, reuse, true, |p| {
                    let mut ui = Ui {
                        p,
                        input: &input,
                        fx: &mut scratch,
                        cfg: &self.cfg,
                        bus: &self.bus,
                        now,
                        accent,
                        hot: false,
                        interactive: false,
                    };
                    draw_peek(&mut ui, body, &pk.peek, left, None);
                });
            }
        }
        if want_act {
            let reuse = self.snap_act.take();
            let mut bars: Option<(Rect, Color, bool)> = None;
            let acts = self.activity.as_ref();
            self.snap_act = self.res.render_snapshot(&self.gfx, scale, region, 8, reuse, true, |p| {
                let mut ui = Ui {
                    p,
                    input: &input,
                    fx: &mut scratch,
                    cfg: &self.cfg,
                    bus: &self.bus,
                    now,
                    accent,
                    hot: false,
                    interactive: false,
                };
                draw_compact(&mut ui, body, acts, &self.indicators, self.cfg.appearance.notch_height, &mut bars);
            });
            self.small_bars = bars;
        }
    }

    fn prepare_pictures(&mut self, now: Instant) {
        let a_exp = self.anim.exp.value;
        let paging = self.page_from.is_some();
        let on = self.cfg.appearance.motion_cache;
        // the live view takes over a little before the end, so clicks work as soon as they used to
        let want_all = on && !paging && (self.vf.exp.is_some() || (a_exp > 0.003 && a_exp < 0.88));
        let want_pages = on && paging && a_exp >= 0.998;
        if !want_all {
            self.snap_all = None;
        }
        if !want_pages {
            self.snap_pages = None;
        }
        if !(want_all || want_pages) {
            return;
        }
        let scale = self.scale;
        let tw = self.page_width();
        let body = self.body_rect();
        let target_h = self.targets().h;
        let full = Rect::new(
            body.cx() - tw * 0.5,
            body.y,
            tw,
            // an open page is laid out at its own height even while the body is still catching up, so
            // nothing drifts or stretches mid-resize; while closing the page keeps the body's height
            if self.mode == Mode::Expanded { target_h } else { target_h.max(body.h) },
        );
        let page = self.page;
        let accent = self.accent;
        let input = Input::default();
        let mut scratch = Effects::default();
        // keep the pictures inside the window
        let clamp = |r: Rect| {
            let (x0, y0) = (r.x.max(0.0), r.y.max(0.0));
            let (x1, y1) = (r.right().min(self.win_w), r.bottom().min(self.win_h));
            Rect::new(x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
        };
        let hash = |parts: &[i32]| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(parts, &mut h);
            std::hash::Hasher::finish(&h)
        };
        let page_id = |id: ModuleId| id.bytes().fold(0i32, |a, b| a.wrapping_mul(31).wrapping_add(b as i32));
        let sc = (scale * 100.0) as i32;

        self.res.set_halo(self.halo_y(full.bottom(), full.h));
        if want_all {
            // the height is left out of the key: it changes every frame while closing
            let key = hash(&[page_id(page), full.x.round() as i32, full.y.round() as i32, full.w.round() as i32, sc]);
            let fresh =
                self.snap_all.as_ref().is_some_and(|s| s.key == key && s.born.elapsed() < Duration::from_millis(900));
            if !fresh {
                let tabs = self.tab_list();
                let region = clamp(full);
                self.snap_all = self.res.render_snapshot(&self.gfx, scale, region, key, None, false, |p| {
                    let mut ui = Ui {
                        p,
                        input: &input,
                        fx: &mut scratch,
                        cfg: &self.cfg,
                        bus: &self.bus,
                        now,
                        accent,
                        hot: false,
                        interactive: false,
                    };
                    draw_expanded(
                        &mut ui,
                        full,
                        full,
                        &mut self.modules,
                        &self.started,
                        &tabs,
                        page,
                        None,
                        &self.indicators,
                        None,
                    );
                });
            }
        }

        if want_pages {
            let Some((from, _)) = self.page_from else { return };
            // each page is drawn at its own size, centred; the picture is as big as the larger one
            let (gf, gt) = (
                page_size(&self.cfg, &self.modules, &self.started, from),
                page_size(&self.cfg, &self.modules, &self.started, page),
            );
            let cx = body.cx();
            let (cf, ct) = (page_content(cx, full.y, gf), page_content(cx, full.y, gt));
            let region = clamp(Rect::new(cx - gf.0.max(gt.0) * 0.5, ct.y - 4.0, gf.0.max(gt.0), cf.h.max(ct.h) + 8.0));
            let key_of = |id: ModuleId, g: (f32, f32)| {
                hash(&[
                    page_id(id),
                    (cx - g.0 * 0.5).round() as i32,
                    full.y.round() as i32,
                    g.0.round() as i32,
                    g.1.round() as i32,
                    sc,
                ])
            };
            let (kf, kt) = (key_of(from, gf), key_of(page, gt));
            let fresh = self
                .snap_pages
                .as_ref()
                .is_some_and(|(a, b)| a.key == kf && b.key == kt && a.born.elapsed() < Duration::from_millis(1500));
            if !fresh {
                let mut shots: Vec<Option<Snapshot>> = Vec::with_capacity(2);
                for (id, key, content) in [(from, kf, cf), (page, kt, ct)] {
                    let snap = self.res.render_snapshot(&self.gfx, scale, region, key, None, false, |p| {
                        let mut ui = Ui {
                            p,
                            input: &input,
                            fx: &mut scratch,
                            cfg: &self.cfg,
                            bus: &self.bus,
                            now,
                            accent,
                            hot: false,
                            interactive: false,
                        };
                        draw_body(&mut ui, content, &mut self.modules, &self.started, id);
                    });
                    shots.push(snap);
                }
                let (b, a) = (shots.pop().flatten(), shots.pop().flatten());
                self.snap_pages = a.zip(b);
            }
        }
    }

    fn render(&mut self, now: Instant) -> Effects {
        let mut fx = Effects::default();
        self.prepare_snapshots(now);
        let (dc, off) = match self.host.begin_draw() {
            Ok(v) => v,
            Err(e) => {
                log!("BeginDraw failed: {e} — recreating devices");
                self.recover_device();
                return fx;
            }
        };
        let body = self.body_rect();
        self.res.set_halo(self.halo_y(body.bottom(), body.h));
        let scale = self.scale;
        let ap = {
            let a = &self.cfg.appearance;
            Look {
                shadow: a.shadow,
                blur: a.blur,
                expanded_opacity: a.expanded_opacity,
                collapsed_opacity: a.collapsed_opacity,
                notch_height: a.notch_height,
                glass_look: a.glass_look,
            }
        };
        let settled = self.settled;
        let see_through = ap.expanded_opacity < 0.97 || ap.collapsed_opacity < 0.97;
        let glass = ap.glass_look && crate::ui::look_glass();
        let a_glass = self.anim.glass.value;
        // the rim fades in over about a quarter of a second and out twice as fast
        let rim_to = if self.rim_ok { 1.0 } else { 0.0 };
        let rim_dt = now.saturating_duration_since(self.rim_at).as_secs_f32().min(0.1);
        self.rim_at = now;
        let glow = self.cfg.glass.glow_in.clamp(2.0, 30.0);
        let rim_rate = if rim_to > self.rim_k { glow } else { glow * 2.0 };
        self.rim_k += (rim_to - self.rim_k) * (1.0 - (-rim_rate * rim_dt).exp());
        if (rim_to - self.rim_k).abs() < 0.01 {
            self.rim_k = rim_to;
        }
        if self.rim_k != rim_to {
            fx.animate = true;
        }
        // while the notch changes size the frosted layer behind it would trail the edge as a second,
        // fainter outline: it is let go quickly, the bottom edge turns solid with it, and both come
        // back smoothly once the size holds still
        let moved = (body.w - self.bd_prev.0).abs() + (body.h - self.bd_prev.1).abs();
        self.bd_prev = (body.w, body.h);
        // gone the very frame the size starts to change (a fade would still show the trailing
        // outline), and only back a moment after it holds still, as the layer needs a few frames
        // to catch up with the new size
        if moved > 0.25 {
            self.bd_move = now;
            self.bd_k = 0.0;
        }
        let still = now.saturating_duration_since(self.bd_move) > Duration::from_millis(140);
        let bd_to = if still { 1.0 } else { 0.0 };
        if bd_to > self.bd_k {
            self.bd_k += (bd_to - self.bd_k) * (1.0 - (-10.0 * rim_dt).exp());
        }
        if (bd_to - self.bd_k).abs() < 0.01 {
            self.bd_k = bd_to;
        }
        if self.bd_k != bd_to || !still {
            fx.animate = true;
        }
        if self.rim_old.is_some() {
            // a new rim is melting in over the old one (see `live_done`); 30 frames a second is plenty
            self.rim_x += rim_dt / self.cfg.glass.melt.clamp(0.03, 0.5);
            if self.rim_x >= 1.0 {
                self.rim_x = 1.0;
                self.rim_old = None;
            } else {
                fx.animate_slow = true;
            }
        }
        let bd_k = self.bd_k;
        let a_r = self.anim.r.value;
        let a_ear = self.anim.ear.value;
        let a_compact = self.anim.compact.value;
        let a_peek = self.anim.peek.value;
        let a_exp = self.anim.exp.value;
        crate::alcove::set_open(a_exp);
        let (vexp, vpeek, vact) = (self.vf.exp, self.vf.peek, self.vf.act);
        let veils = crate::ui::look_alcove() && self.cfg.appearance.motion_cache;
        let page_e = now.saturating_duration_since(self.page_at).as_secs_f32()
            * self.cfg.appearance.animation_speed.max(0.25);
        let target_h = self.targets().h;
        // read before the painter borrows the window's resources
        let page_w = self.page_width();
        let band_k = self.band_k();
        let bubble = self.bubble_rect();
        if !self.bubbles.is_empty() {
            if self.mode != Mode::Idle || self.peek.is_some() {
                self.bubbles.clear();
            }
            self.bubbles.retain(|b| now.saturating_duration_since(b.at).as_secs_f32() < BUBBLE_END);
            // a quiet sound as a bubble pops out and again as it is pulled in
            let sounds = self.cfg.general.bubble_sounds;
            for b in &mut self.bubbles {
                if !b.in_played && now >= b.at {
                    b.in_played = true;
                    if sounds {
                        crate::sys::bubble_sound(false);
                    }
                }
                if !b.out_played && now.saturating_duration_since(b.at).as_secs_f32() >= BUBBLE_HOLD {
                    b.out_played = true;
                    if sounds {
                        crate::sys::bubble_sound(true);
                    }
                }
            }
        }
        let bubble_list = self.bubble_states(now);
        let split_v = self.anim.split.value;
        let split_on = split_v > 0.01 && self.mode != Mode::Expanded;
        let (lb, rb) = self.split_rects();
        let secondary = self.secondary.clone();
        let glances = self.idle_glances();
        let mut bars: Option<(Rect, Color, bool)> = None;
        let tabs: Vec<(ModuleId, Icon, String)> = if a_exp <= 0.003 || self.snap_all.is_some() {
            Vec::new() // nothing to draw (or one picture covers it): skip the per-frame allocations
        } else {
            self.tab_list()
        };

        {
            let mut p = Painter::new(dc.clone(), &self.gfx, &mut self.res, scale, off.x as f32, off.y as f32);
            p.clear();
            if body.bottom() > 0.25 || !bubble_list.is_empty() {
                // shadow: follows the notch's own outline, and only shows once the notch is open or
                // showing a peek, never for the small collapsed (or hovered) notch
                if ap.shadow {
                    let grown = ((body.h - ap.notch_height - 18.0) / 60.0).clamp(0.0, 1.0);
                    p.notch_shadow(body, a_r, a_ear, 0.5 * smoothstep(0.0, 1.0, grown));
                }
                // secondary activity bubble (drawn first; the main body covers it while sliding out)
                let sec_v = self.anim.sec.value;
                if sec_v > 0.01 {
                    if let Some(sa) = secondary {
                        let br = bubble;
                        if let Some(g) = gfx::notch_path(&p, br, br.w * 0.5, a_ear.min(6.0)) {
                            p.fill_geometry(&g, Color::black(ap.collapsed_opacity));
                        }
                        let input = self.input.clone();
                        let mut ui = Ui {
                            p: &mut p,
                            input: &input,
                            fx: &mut fx,
                            cfg: &self.cfg,
                            bus: &self.bus,
                            now,
                            accent: self.accent,
                            hot: false,
                            interactive: false,
                        };
                        ui.p.alpha = sec_v.clamp(0.0, 1.0);
                        let slot = br.inset(5.0);
                        let mut no_bars = None;
                        draw_slot(&mut ui, &sa.left, slot, slot, true, &mut no_bars);
                        ui.p.alpha = 1.0;
                    }
                }
                for &(br, alpha, icon, accent, off, _) in &bubble_list {
                    if let Some(g) = gfx::notch_path(&p, br, br.w * 0.5, a_ear.min(6.0)) {
                        p.fill_geometry(&g, Color::black(ap.collapsed_opacity * alpha));
                    }
                    p.alpha = alpha;
                    let ic = br.center_square(br.w * 0.5);
                    let tint = accent.lerp(Color::white(1.0), 0.3);
                    p.icon(icon, ic, tint);
                    if off {
                        // a slash through the icon, with a dark edge so it reads on the icon
                        p.line(ic.x - 1.0, ic.y - 1.0, ic.right() + 1.0, ic.bottom() + 1.0, 4.6, Color::black(0.95));
                        p.line(ic.x - 1.0, ic.y - 1.0, ic.right() + 1.0, ic.bottom() + 1.0, 2.2, tint);
                    }
                    p.alpha = 1.0;
                    fx.animate = true;
                }
                if split_on {
                    if let Some(act) = self.activity.clone() {
                        let input = self.input.clone();
                        let mut ui = Ui {
                            p: &mut p,
                            input: &input,
                            fx: &mut fx,
                            cfg: &self.cfg,
                            bus: &self.bus,
                            now,
                            accent: self.accent,
                            hot: false,
                            interactive: false,
                        };
                        for (br, slot, left) in [(lb, &act.left, true), (rb, &act.right, false)] {
                            if let Some(g) = gfx::notch_path(ui.p, br, br.w * 0.5, a_ear.min(5.0)) {
                                ui.p.fill_geometry(&g, Color::black(ap.collapsed_opacity));
                            }
                            ui.p.alpha = split_v.clamp(0.0, 1.0);
                            let inner = br.inset(br.w * 0.2);
                            draw_slot(&mut ui, slot, inner, inner, left, &mut bars);
                        }
                        ui.p.alpha = 1.0;
                    }
                }
                let opaque = if ap.blur { ap.expanded_opacity } else { ap.expanded_opacity.max(0.95) };
                let bg_alpha = lerp(ap.collapsed_opacity, opaque, a_glass);
                // no blur while moving: stay a little more solid so it doesn't flash
                let bg_alpha = if ap.blur && see_through && !settled { bg_alpha.max(0.94) } else { bg_alpha };
                if let Some(g) = gfx::notch_path(&p, body, a_r, a_ear) {
                    let bh = self.snap_band.as_ref().map_or(BAND_H, |b| b.rect.h);
                    if band_k > 0.01 && body.h > bh * 2.0 {
                        // Alcove: the glass turns see-through toward the bottom edge
                        // it starts higher up and follows a gentle S, so there is no point where the
                        // black visibly begins to thin out
                        // pure black down to the halfway mark ("Bottom fade start" moves it; 1.7 is half),
                        // then the black thins out evenly toward the bottom edge: an almost straight ramp
                        // with a soft start, so the colour comes in slowly and there is no visible line
                        let from = 1.0 - self.cfg.glass.fade_start.clamp(0.5, 3.0) / 3.4;
                        let drop = self.cfg.glass.fade_amount.clamp(0.0, 1.0) * band_k * bd_k;
                        let span = 1.0 - from;
                        let mut stops = [(0.0_f32, Color::black(bg_alpha)); 8];
                        stops[1] = (from, Color::black(bg_alpha));
                        for (i, st) in stops.iter_mut().enumerate().skip(2) {
                            let t = (i - 1) as f32 / 6.0;
                            let e = 0.6 * t * t * (3.0 - 2.0 * t) + 0.4 * t;
                            *st = (from + span * t, Color::black(bg_alpha * (1.0 - drop * e)));
                        }
                        p.fill_geometry_vgrad(&g, body.y, body.bottom(), &stops);
                    } else {
                        p.fill_geometry(&g, Color::black(bg_alpha));
                    }
                }

                // the glass lies under the content, so buttons and text stay sharp over it
                p.push_clip(body);
                if glass && self.rim_k > 0.003 && body.h >= 56.0 && a_glass > 0.01 {
                    // the glass edge: the screen behind, bent along the rim (see `prepare_rim`)
                    if let Some((key, pic)) = &self.refr_rim {
                        let a = a_glass * self.rim_k;
                        // a glass picture made a moment ago (lighter animations rebuild it less often)
                        // is stretched to the notch's size now, so nothing shows at its edges
                        let fit = |r: &gfx::RimPic, at: Option<Rect>| -> Rect {
                            match at {
                                Some(o) if o.w > 8.0 && o.h > 8.0 && ((o.w - body.w).abs() + (o.h - body.h).abs()) > 0.05 => {
                                    let (sx, sy) = (body.w / o.w, body.h / o.h);
                                    let (ocx, ncx) = (o.x + o.w * 0.5, body.x + body.w * 0.5);
                                    let r = r.rect;
                                    Rect::new(ncx + (r.x - ocx) * sx, body.bottom() + (r.y - o.bottom()) * sy, r.w * sx, r.h * sy)
                                }
                                _ => r.rect,
                            }
                        };
                        match &self.rim_old {
                            // (only while the old picture still fits the notch's size)
                            Some((old_key, old)) if rim_near(old_key, key) => {
                                let t = self.rim_x.clamp(0.0, 1.0);
                                let t = t * t * (3.0 - 2.0 * t);
                                p.draw_rim_to(old, fit(old, self.rim_old_rect), a * (1.0 - t));
                                p.draw_rim_to(pic, fit(pic, self.rim_rect), a * t);
                            }
                            _ => p.draw_rim_to(pic, fit(pic, self.rim_rect), a),
                        }
                    }
                }
                p.pop_clip();

                p.push_clip(body);
                let input = self.input.clone();
                let accent = self.accent;
                let mut ui = Ui {
                    p: &mut p,
                    input: &input,
                    fx: &mut fx,
                    cfg: &self.cfg,
                    bus: &self.bus,
                    now,
                    accent,
                    hot: false,
                    interactive: false,
                };

                // compact live activity / idle indicators
                if a_compact > 0.003 || self.mode == Mode::Idle {
                    ui.p.alpha = match vact {
                        Some(l) => l.alpha,
                        None if self.activity.is_some() => a_compact,
                        None => 1.0 - if vexp.is_some() { 1.0 } else { a_peek.max(a_exp) },
                    };
                    match &self.snap_act {
                        // coming in: the soft picture (see `prepare_small`)
                        Some(pic) if vact.is_some() => {
                            bars = self.small_bars;
                            ui.p.snapshot_blurred(pic, 0.0, 0.0, 1.0);
                        }
                        _ => {
                            let acts = if split_on { None } else { self.activity.as_ref() };
                            draw_compact(&mut ui, body, acts, &self.indicators, ap.notch_height, &mut bars);
                        }
                    }
                    if self.activity.is_none() && !glances.is_empty() {
                        draw_idle_widgets(&mut ui, body, &glances, ap.notch_height);
                    }
                }
                // peek banner
                if a_peek > 0.003 || vpeek.is_some() {
                    if let Some(pk) = self.peek.as_ref().or(self.peek_out.as_ref()) {
                        ui.p.alpha = vpeek.map_or(a_peek, |l| l.alpha);
                        let s = if vpeek.is_some() { 1.0 } else { 0.92 + 0.08 * a_peek };
                        ui.p.set_local(s, body.cx(), 0.0, 0.0, 0.0);
                        ui.interactive = a_peek > 0.9 && vpeek.is_none();
                        let left = pk.until.saturating_duration_since(now).as_secs_f32()
                            / pk.peek.duration.as_secs_f32().max(0.1);
                        let swap = pk.swap.as_ref().and_then(|(at, old)| {
                            let t = now.saturating_duration_since(*at).as_secs_f32() / SWAP_SECS;
                            (t < 1.0).then_some((t, &**old))
                        });
                        let band = if band_k > 0.01 && self.mode == Mode::Peek && swap.is_none() {
                            self.snap_band.as_ref()
                        } else {
                            None
                        };
                        if let Some(b) = band {
                            ui.p.push_clip(Rect::new(0.0, -200.0, self.win_w, b.rect.y + 200.0));
                        }
                        match &self.snap_peek {
                            Some(pic) if vpeek.is_some() && swap.is_none() => ui.p.snapshot_blurred(pic, 0.0, 0.0, 1.0),
                            _ => draw_peek(&mut ui, body, &pk.peek, left.clamp(0.0, 1.0), swap),
                        }
                        if let Some(b) = band {
                            ui.p.pop_clip();
                            band_draw(&mut ui, b, body, a_r, a_ear);
                        }
                        ui.p.reset_local();
                    }
                }
                // expanded
                if a_exp > 0.003 || vexp.is_some() {
                    ui.p.alpha = vexp.map_or(a_exp, |l| l.alpha);
                    let s = if vexp.is_some() { 1.0 } else { 0.94 + 0.06 * a_exp };
                    ui.p.set_local(s, body.cx(), 0.0, 0.0, 0.0);
                    ui.interactive = a_exp > 0.85 && self.mode == Mode::Expanded && vexp.is_none();
                    let tw = page_w;
                    let full = Rect::new(
                        body.cx() - tw * 0.5,
                        body.y,
                        tw,
                        // an open page is laid out at its own height even while the body is still catching up, so
                        // nothing drifts or stretches mid-resize; while closing the page keeps the body's height
                        if self.mode == Mode::Expanded { target_h } else { target_h.max(body.h) },
                    );
                    match &self.snap_all {
                        // opening or closing: the finished picture is only faded and scaled
                        Some(snap) => {
                            ui.interactive = false;
                            if vexp.is_some() {
                                ui.p.snapshot_blurred(snap, 0.0, 0.0, 1.0);
                            } else {
                                ui.p.snapshot(snap, 0.0, 0.0, 1.0);
                            }
                        }
                        None => {
                            let trans =
                                self.page_from.map(|(from, dir)| {
                                    (from, dir, if veils { page_e } else { self.anim.page.value.clamp(0.0, 1.0) })
                                });
                            let pages = self.snap_pages.as_ref().map(|(a, b)| (a, b));
                            // the strip along the bottom is drawn from its own picture (sharp, then
                            // softer toward the rim), so the live page stops just above it
                            let band = if band_k > 0.01 && self.mode == Mode::Expanded {
                                self.snap_band.as_ref()
                            } else {
                                None
                            };
                            if let Some(b) = band {
                                ui.p.push_clip(Rect::new(0.0, -200.0, self.win_w, b.rect.y + 200.0));
                            }
                            draw_expanded(
                                &mut ui,
                                full,
                                body,
                                &mut self.modules,
                                &self.started,
                                &tabs,
                                self.page,
                                trans,
                                &self.indicators,
                                pages,
                            );
                            if let Some(b) = band {
                                ui.p.pop_clip();
                                band_draw(&mut ui, b, body, a_r, a_ear);
                            }
                        }
                    }
                    ui.p.reset_local();
                }
                ui.p.alpha = 1.0;
                if crate::ui::look_alcove() {
                    if let Some(t0) = self.opened_at {
                        let t = now.saturating_duration_since(t0).as_secs_f32() / 0.6;
                        if t < 1.0 && self.mode == Mode::Expanded {
                            // a soft band of light crosses the top of the notch as it opens
                            let e = 1.0 - (1.0 - t).powi(3);
                            let cx = body.x - 90.0 + (body.w + 180.0) * e;
                            let band = [
                                (0.0, Color::white(0.0)),
                                (0.5, Color::white(0.13 * (1.0 - t))),
                                (1.0, Color::white(0.0)),
                            ];
                            let h = (body.h * 0.55).min(70.0);
                            let pts = [
                                (cx - 90.0, body.y),
                                (cx + 90.0, body.y),
                                (cx + 60.0, body.y + h),
                                (cx - 120.0, body.y + h),
                            ];
                            ui.p.fill_gradient_poly(&pts, &band, cx - 90.0, cx + 90.0);
                            ui.fx.animate = true;
                        }
                    }
                }
                p.pop_clip();
            }
        }
        if let Err(e) = self.host.end_draw() {
            log!("EndDraw failed: {e}");
            self.recover_device();
        }
        self.res.end_frame();

        // compositor-driven visualizer
        let show_bars = self.cfg.media.visualizer && self.mode == Mode::Idle;
        match bars {
            Some((r, c, playing)) if show_bars && a_compact > 0.01 => {
                let s = self.scale;
                let pr = Rect::new(r.x * s, r.y * s, r.w * s, r.h * s);
                self.host.bars.update(pr, c, a_compact * self.anim.shown.value, playing);
            }
            _ => self.host.bars.hide(),
        }

        // blur behind the notch body
        let s = self.scale;
        let bp = Rect::new(body.x * s, body.y * s, body.w * s, body.h.max(0.0) * s);
        // The live blur is the costliest thing the compositor does for us:
        // skip it while the panel is opaque anyway and while it is still moving.
        let blur = if ap.blur && ((see_through && settled) || band_k > 0.01) { a_glass } else { 0.0 };
        // The frosted layer is dark and strong; under the see-through bottom edge it is only
        // blended in lightly, so the page behind still shows through softly.
        let blur = if band_k > 0.01 && !see_through { blur * 0.4 } else { blur };
        self.host.set_backdrop(bp, a_r * s, blur * self.anim.shown.value * bd_k);

        if let Some(page) = fx.open_page.take() {
            if self.mode == Mode::Expanded && self.page_available(page) {
                if self.page != page {
                    self.switch_page(page);
                    self.set_keyboard(page == "ai");
                }
            } else {
                fx.open_page = Some(page);
            }
        }
        if self.mode == Mode::Expanded && self.page == HOME {
            let st = unsafe { GetLocalTime() };
            self.clock_min = st.wSecond;
        }
        fx
    }

    /// Messages for the launcher's own window. When it has been idle for a long time it is dropped
    /// (the window, its drawing memory and its lists); the next hotkey press builds it again.
    pub fn launcher_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let l = self.launcher.as_mut()?;
        let r = l.handle(&self.gfx, msg, wp, lp);
        if l.wants_free() {
            self.launcher = None;
        }
        r
    }

    /// The launcher's settings, with the Gemini settings it asks with.
    fn launcher_cfg(&mut self) -> crate::config::LauncherCfg {
        let mut cfg = self.cfg.launcher.clone();
        cfg.gemini = self.cfg.gemini.clone();
        cfg.media_playing = self.module_index("media").is_some_and(|i| self.started[i] && self.modules[i].is_playing());
        cfg.rim_width = self.cfg.dock.rim_width;
        cfg.rim_bright = self.cfg.dock.rim_bright;
        cfg
    }

    /// The Alcove look with `[launcher] alcove` on: a launcher hotkey opens the launcher inside the notch (the
    /// same hotkey again puts it away). False when Tuna's launcher should answer instead.
    fn alcove_launch(&mut self, what: &str) -> bool {
        if !crate::ui::look_alcove() || !self.cfg.launcher.alcove {
            return false;
        }
        let Some(i) = self.module_index(modules::alcove_launcher::ID) else { return false };
        if !self.started[i] {
            return false;
        }
        let showing = self.mode == Mode::Expanded && self.page == modules::alcove_launcher::ID;
        let mut fx = Effects::default();
        {
            let mut cx =
                Cx { bus: &self.bus, cfg: &self.cfg, fx: &mut fx, hwnd: SendHwnd::new(self.hwnd), now: Instant::now() };
            let ev = SystemEvent::Command {
                verb: "alcove-launcher".into(),
                args: vec![what.into(), if showing { "open".into() } else { "closed".into() }],
            };
            self.modules[i].on_system(&ev, &mut cx);
        }
        self.user_hidden = false;
        self.apply_effects(fx);
        self.schedule_wake();
        true
    }

    /// The Gemini hotkey: the launcher as a one-question chat, or put it away again.
    fn launcher_ai_toggle(&mut self) {
        if self.alcove_launch("ai") {
            return;
        }
        if self.launcher.is_none() {
            match crate::launcher::Launcher::new(&self.gfx, self.host.compositor.clone()) {
                Ok(l) => self.launcher = Some(l),
                Err(e) => {
                    log!("launcher failed to start: {e}");
                    return;
                }
            }
        }
        let (cfg, scale) = (self.launcher_cfg(), self.cfg.appearance.scale);
        if let Some(l) = self.launcher.as_mut() {
            l.configure(cfg);
            l.toggle_ai(&self.gfx, scale);
        }
    }

    /// The launcher, started if it is not there yet and given the current settings. Returns the UI scale.
    fn launcher_ready(&mut self) -> Option<f32> {
        if self.launcher.is_none() {
            match crate::launcher::Launcher::new(&self.gfx, self.host.compositor.clone()) {
                Ok(l) => self.launcher = Some(l),
                Err(e) => {
                    log!("launcher failed to start: {e}");
                    return None;
                }
            }
        }
        let cfg = self.launcher_cfg();
        self.launcher.as_mut()?.configure(cfg);
        Some(self.cfg.appearance.scale)
    }

    /// The switcher hotkey (Alt+Tab): the launcher as a row of the open windows; pressed again it steps on.
    fn launcher_switch(&mut self, back: bool) {
        if self.alcove_launch(if back { "switch-back" } else { "switch" }) {
            return;
        }
        let Some(scale) = self.launcher_ready() else { return };
        if let Some(l) = self.launcher.as_mut() {
            l.windows_step(&self.gfx, scale, back);
        }
    }

    /// The Run hotkey (Win+R): the launcher as one Run box, or put it away again.
    fn launcher_run_toggle(&mut self) {
        if self.alcove_launch("run") {
            return;
        }
        let Some(scale) = self.launcher_ready() else { return };
        if let Some(l) = self.launcher.as_mut() {
            l.toggle_run(&self.gfx, scale);
        }
    }

    /// The clipboard hotkey: the launcher as a history of what was copied, or put it away again.
    fn launcher_clip_toggle(&mut self) {
        if self.alcove_launch("clip") {
            return;
        }
        if self.launcher.is_none() {
            match crate::launcher::Launcher::new(&self.gfx, self.host.compositor.clone()) {
                Ok(l) => self.launcher = Some(l),
                Err(e) => {
                    log!("launcher failed to start: {e}");
                    return;
                }
            }
        }
        let (cfg, scale) = (self.launcher_cfg(), self.cfg.appearance.scale);
        if let Some(l) = self.launcher.as_mut() {
            l.configure(cfg);
            l.toggle_clip(&self.gfx, scale);
        }
    }

    /// A combo fired: the launcher shows the thing that is opening for 0.7 seconds, without taking the
    /// focus away from it.
    fn launcher_flash(&mut self, label: &str, target: &str) {
        if self.launcher.is_none() {
            match crate::launcher::Launcher::new(&self.gfx, self.host.compositor.clone()) {
                Ok(l) => self.launcher = Some(l),
                Err(e) => {
                    log!("launcher failed to start: {e}");
                    return;
                }
            }
        }
        let (cfg, scale) = (self.launcher_cfg(), self.cfg.appearance.scale);
        if let Some(l) = self.launcher.as_mut() {
            l.configure(cfg);
            l.flash(&self.gfx, scale, label, target);
        }
    }

    /// The launcher hotkey: open the launcher, or put it away again.
    fn launcher_toggle(&mut self) {
        if self.alcove_launch("search") {
            return;
        }
        if self.launcher.is_none() {
            match crate::launcher::Launcher::new(&self.gfx, self.host.compositor.clone()) {
                Ok(l) => self.launcher = Some(l),
                Err(e) => {
                    log!("launcher failed to start: {e}");
                    return;
                }
            }
        }
        let (cfg, scale) = (self.launcher_cfg(), self.cfg.appearance.scale);
        if let Some(l) = self.launcher.as_mut() {
            l.configure(cfg);
            l.toggle(&self.gfx, scale);
        }
    }

    /// Messages for the radial menu's own window.
    pub fn radial_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let r = self.radial.as_mut()?;
        r.handle(&self.gfx, msg, wp, lp)
    }

    /// The radial menu hotkey: the menu of apps around the pointer, or put it away again.
    fn radial_toggle(&mut self) {
        if !self.cfg.radial.enabled {
            return;
        }
        if self.radial.is_none() {
            match crate::radial::Radial::new(&self.gfx, self.host.compositor.clone()) {
                Ok(r) => self.radial = Some(r),
                Err(e) => {
                    log!("radial menu failed to start: {e}");
                    return;
                }
            }
        }
        // if Explorer brought the taskbar back in the meantime, it goes away again
        if self.cfg.radial.hide_taskbar {
            crate::taskbar::hide_taskbar();
        }
        let (cfg, url, scale) =
            (self.cfg.radial.clone(), self.cfg.launcher.search_url.clone(), self.cfg.appearance.scale);
        if let Some(r) = self.radial.as_mut() {
            r.configure(cfg, url, scale);
            r.toggle(&self.gfx);
        }
    }

    /// The radial menu's taskbar option: hide the Windows taskbar, or give it back.
    fn sync_taskbar(&mut self) {
        if self.cfg.radial.enabled && self.cfg.radial.hide_taskbar {
            crate::taskbar::hide_taskbar();
            self.radial_hid = true;
        } else if self.radial_hid {
            self.radial_hid = false;
            // the dock may still want it hidden
            if !self.cfg.dock.hide_taskbar {
                crate::taskbar::recover_taskbar();
            }
        }
    }

    /// Messages for the dock's own window.
    pub fn dock_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let hidden = self.fs_hidden;
        let d = self.dock.as_mut()?;
        d.handle(&self.gfx, hidden, msg, wp, lp)
    }

    /// What a right-click at `lp` landed on in the dock: (label, path, pinned).
    pub fn dock_menu_target(&self, lp: LPARAM) -> Option<(String, String, bool)> {
        self.dock.as_ref()?.menu_target(lp)
    }

    /// The pinned list as written in the settings.
    pub fn dock_items(&self) -> Vec<String> {
        match self.dock.as_ref() {
            Some(d) => d.items_cfg(),
            None => self.cfg.dock.items.clone(),
        }
    }

    /// Load the dock's icons again (a new icon file was chosen).
    pub fn dock_reload_icons(&mut self) {
        if let Some(d) = self.dock.as_mut() {
            d.reload_icons(&self.gfx);
        }
    }

    /// Open the pin picker panel above the dock.
    pub fn dock_open_picker(&mut self) {
        if let Some(d) = self.dock.as_mut() {
            d.open_picker(&self.gfx);
        }
    }

    /// Start, update or remove the dock to match the settings.
    fn dock_apply(&mut self) {
        if !self.cfg.dock.enabled {
            self.dock = None;
            return;
        }
        if self.dock.is_none() {
            match crate::dock::Dock::new(&self.gfx, self.host.compositor.clone()) {
                Ok(d) => self.dock = Some(d),
                Err(e) => {
                    log!("dock failed to start: {e}");
                    return;
                }
            }
        }
        let (mon, scale) = (self.monitor, self.cfg.appearance.scale);
        if mon.is_invalid() {
            return;
        }
        if let Some(d) = self.dock.as_mut() {
            d.configure(&self.gfx, &self.cfg.dock, &self.cfg.glass, mon, scale);
        }
    }

    /// The screen or the monitor changed: put the dock where it belongs.
    fn dock_relocate(&mut self) {
        let (mon, scale) = (self.monitor, self.cfg.appearance.scale);
        if mon.is_invalid() {
            return;
        }
        if let Some(d) = self.dock.as_mut() {
            d.relocate(&self.gfx, mon, scale);
        }
    }

    fn recover_device(&mut self) {
        if self.gfx.recreate_devices().is_ok() {
            let _ = self.host.set_rendering_device(&self.gfx);
            if let Some(d) = self.dock.as_mut() {
                d.recover(&self.gfx);
            }
            if let Some(l) = self.launcher.as_mut() {
                l.recover(&self.gfx);
            }
            if let Some(r) = self.radial.as_mut() {
                r.recover(&self.gfx);
            }
            self.snap_all = None;
            self.snap_pages = None;
            if let Ok(r) = Res::new(&self.gfx) {
                self.res = r;
            }
        }
        self.request_frame();
    }
}

// ---------------------------------------------------------------------------
// Drawing helpers (free functions to keep borrows simple)
// ---------------------------------------------------------------------------

fn draw_compact(
    ui: &mut Ui,
    body: Rect,
    activity: Option<&Activity>,
    indicators: &[Indicator],
    base_h: f32,
    bars: &mut Option<(Rect, Color, bool)>,
) {
    let h = body.h.min(base_h + 6.0);
    let slot = (base_h - 10.0).max(10.0);
    let cy = body.y + h * 0.5;
    let mut right_edge = body.right() - 10.0;
    if let Some(a) = activity {
        let wing = a.wing * base_h;
        let lr = Rect::new(body.x + 9.0, cy - slot * 0.5, slot, slot);
        let rr = Rect::new(body.right() - 9.0 - slot, cy - slot * 0.5, slot, slot);
        let lw = Rect::new(body.x + 9.0, cy - slot * 0.5, wing - 6.0, slot);
        let rw = Rect::new(body.right() - 9.0 - (wing - 6.0), cy - slot * 0.5, wing - 6.0, slot);
        draw_slot(ui, &a.left, lr, lw, true, bars);
        draw_slot(ui, &a.right, rr, rw, false, bars);
        right_edge = body.right() - wing - 4.0;
    }
    // privacy dots (mic / camera)
    let mut x = right_edge;
    for ind in indicators.iter().filter(|i| i.dot).rev() {
        ui.p.fill_circle(x - 3.0, cy, 3.2, ind.color);
        x -= 10.0;
    }
}

/// Up to three small widgets inside the closed notch, side by side.
/// One closed-notch widget: icon, text, colour and an optional 0..1 tick gauge.
type Glance = (Option<Icon>, String, Color, Option<f32>);

fn draw_idle_widgets(ui: &mut Ui, body: Rect, items: &[Glance], base_h: f32) {
    // fades in as the pill grows into a notch
    let k = ((body.h - 8.0) / (base_h - 8.0).max(1.0)).clamp(0.0, 1.0);
    if k < 0.05 || items.is_empty() {
        return;
    }
    let alpha = ui.p.alpha;
    ui.p.alpha = alpha * k;
    let h = body.h.min(base_h + 6.0);
    let cy = body.y + h * 0.5;
    let n = items.len().min(3);
    let slot_w = ((body.w - 20.0) / n as f32).min(86.0);
    let x0 = body.cx() - slot_w * n as f32 * 0.5;
    let al = crate::ui::look_alcove();
    let base = if al {
        // Alcove: plain white text, a little larger, no dividers and no tick gauges
        TextStyle::new((base_h * 0.46).clamp(12.0, 15.0), Color::white(0.96)).semibold()
    } else {
        TextStyle::new((base_h * 0.42).clamp(11.0, 14.0), palette::TEXT).semibold().display()
    };
    for (i, (icon, text, color, gauge)) in items.iter().take(3).enumerate() {
        let st = if al { base } else { base.color(*color) };
        let slot = Rect::new(x0 + i as f32 * slot_w, body.y, slot_w, h);
        // a single faint tick between neighbours
        if i > 0 && !al {
            ui.p.fill_rounded(Rect::new(slot.x - 0.75, cy - 5.0, 1.5, 10.0), 0.75, Color::white(0.16));
        }
        let isz = 13.0;
        let (iw, gap) = if icon.is_some() { (isz, 4.0) } else { (0.0, 0.0) };
        let (tw, _) = ui.p.measure(text, &st);
        let gw = if gauge.is_some() {
            if al {
                28.0
            } else {
                30.0
            }
        } else {
            0.0
        };
        let gg = if gauge.is_some() { 6.0 } else { 0.0 };
        let tw = tw.min((slot_w - iw - gap - gw - gg - 6.0).max(0.0));
        let x = slot.cx() - (iw + gap + tw + gg + gw) * 0.5;
        if let Some(ic) = icon {
            let ic_color = if al { Color::white(0.85) } else { color.with_a(0.9) };
            ui.p.icon(*ic, Rect::new(x, cy - isz * 0.5, isz, isz), ic_color);
        }
        ui.p.text(text, Rect::new(x + iw + gap, body.y, tw + 2.0, h), &st);
        if let Some(g) = gauge {
            if al {
                crate::alcove::battery_glyph(ui, Rect::new(x + iw + gap + tw + gg, cy - 6.5, gw, 13.0), *g, false);
            } else {
                ui.beams(|ui| ui.ticks(Rect::new(x + iw + gap + tw + gg, cy - 4.0, gw, 8.0), *g, *color));
            }
        }
    }
    ui.p.alpha = alpha;
}

fn clock_text() -> String {
    let st = unsafe { GetLocalTime() };
    let mut buf = [0u16; 64];
    let n = unsafe { GetTimeFormatEx(PCWSTR::null(), TIME_NOSECONDS, Some(&st), PCWSTR::null(), Some(&mut buf)) };
    String::from_utf16_lossy(&buf[..(n.max(1) - 1) as usize])
}

fn date_text() -> String {
    let st = unsafe { GetLocalTime() };
    let day = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][(st.wDayOfWeek % 7) as usize];
    format!("{day} {}", st.wDay)
}

fn draw_slot(ui: &mut Ui, slot: &Slot, sq: Rect, wing: Rect, left: bool, bars: &mut Option<(Rect, Color, bool)>) {
    match slot {
        Slot::Empty => {}
        Slot::Image(img) => ui.p.image(img, sq, sq.w * 0.26, 1.0),
        Slot::Icon(ic, c) => ui.p.icon(*ic, sq.inset(sq.w * 0.06), *c),
        Slot::Text(s, c) => {
            let st = TextStyle::new((sq.h * 0.62).clamp(11.0, 15.0), *c).semibold().display();
            let st = if left { st } else { st.right() };
            ui.p.text(s, wing, &st);
        }
        Slot::Bars { color, playing } => {
            let r = Rect::new(sq.x + sq.w * 0.12, sq.y + sq.h * 0.18, sq.w * 0.76, sq.h * 0.64);
            *bars = Some((r, *color, *playing));
        }
        Slot::Ring(progress, c) => {
            let (cx, cy) = (sq.cx(), sq.cy());
            let d = sq.w * 0.8;
            let w = (sq.w * 0.12).max(2.0);
            match progress {
                Some(f) => ui.ring(cx, cy, d, w, *f, *c),
                None => ui.ring_spin(cx, cy, d, w, *c),
            }
        }
        Slot::Battery { level, charging } => {
            let c = battery_color(*level, *charging);
            icons::battery(ui.p, sq.inset_xy(0.0, sq.h * 0.18), *level, *charging, c);
        }
        Slot::Vinyl { color, angle, art, flip } if crate::ui::look_alcove() => {
            // Alcove: no record, the cover itself (or a note) in a rounded square
            let (mut img, mut k) = (art.as_ref(), 1.0f32);
            if let Some((p, old)) = flip {
                let th = p.clamp(0.0, 1.0) * std::f32::consts::PI;
                k = th.cos().abs().max(0.02);
                if *p < 0.5 {
                    img = old.as_ref();
                }
                ui.animate();
            }
            let s = sq.inset(sq.w * 0.06);
            let r = Rect::centered(s.cx(), s.cy(), s.w * k, s.h * (0.94 + 0.06 * k));
            match img {
                Some(i) => ui.p.image(i, r, s.w * 0.26 * k.max(0.4), 1.0),
                None => {
                    ui.p.fill_rounded(r, s.w * 0.26 * k.max(0.4), color.lerp(Color::black(1.0), 0.6));
                    ui.p.icon(Icon::Music, Rect::centered(r.cx(), r.cy(), r.w * 0.55, s.h * 0.55), Color::white(0.95));
                }
            }
        }
        Slot::Vinyl { color, angle, art, .. } => ui.vinyl(sq.inset(sq.w * 0.04), *angle, *color, art.as_ref()),
        Slot::Wave { levels, color } => {
            let r = Rect::new(wing.x, wing.y + wing.h * 0.12, wing.w, wing.h * 0.76);
            modules::voice::draw_wave(ui, r, levels, *color);
        }
    }
}

pub fn battery_color(level: f32, charging: bool) -> Color {
    if charging {
        palette::GREEN
    } else if level <= 0.1 {
        palette::RED
    } else if level <= 0.2 {
        palette::YELLOW
    } else {
        Color::white(0.92)
    }
}

/// Glyph inside a peek's status badge: a check for green, a "!" style info
/// mark for warnings, nothing (a plain dot) for other colours.
#[allow(dead_code)]
fn badge_glyph(c: Color) -> Option<Icon> {
    if c == palette::GREEN {
        Some(Icon::Check)
    } else if c == palette::RED || c == palette::ORANGE || c == palette::YELLOW {
        Some(Icon::Info)
    } else {
        None
    }
}

// Volume / brightness: the icon top left, the value top right and a bar of
// thin ticks underneath.
// Pointer dwell on the volume peek's speaker icon: (since, already switched).
thread_local! {
    static HUD_DWELL: std::cell::Cell<(Option<Instant>, bool)> = const { std::cell::Cell::new((None, false)) };
}
const HUD_DWELL_SECS: f32 = 0.7;

thread_local! {
    /// Where the volume peek shows its value: a click there opens the volume mixer.
    static HUD_TEXT: std::cell::Cell<Option<Rect>> = const { std::cell::Cell::new(None) };
}

/// Remember where the volume text of the peek is (and show the pointer over it), so a click on it can open the mixer.
fn hud_text_zone(ui: &mut Ui, pk: &Peek, zone: Rect) {
    if pk.key == Some("osd-volume") {
        HUD_TEXT.with(|z| z.set(Some(zone)));
        if ui.hovered(zone) {
            ui.hot = true;
        }
    }
}

/// The sweep of a dial's ring for a level (0..1): clockwise or counter-clockwise by the setting.
fn dial_sweep(cfg: &Config, level: f32) -> f32 {
    let sweep = std::f32::consts::TAU * level;
    if cfg.general.hud_clockwise {
        sweep
    } else {
        -sweep
    }
}

/// A device connecting or leaving as one big round dial: the device's icon in a ring that shows
/// its battery (full when connected without a reading, empty when gone), the name and the state.
/// `s` cut short with an ellipsis so that it fits `w` wide in style `st`.
fn fit_text(ui: &mut Ui, mut s: String, st: &TextStyle, w: f32) -> String {
    if ui.p.measure(&s, st).0 <= w {
        return s;
    }
    while s.chars().count() > 1 {
        s.pop();
        let t = format!("{}\u{2026}", s.trim_end());
        if ui.p.measure(&t, st).0 <= w {
            return t;
        }
    }
    s
}

fn draw_device_dial(ui: &mut Ui, body: Rect, pk: &Peek) {
    let on = pk.subtitle.starts_with("Connected");
    let battery = match &pk.trailing {
        Trailing::Battery { level, .. } => Some(*level),
        _ => None,
    };
    let (cx, cy) = (body.cx(), body.y + ui.cfg.appearance.notch_height + 66.0);
    let target = battery.unwrap_or(if on { 1.0 } else { 0.0 });
    let level = ui.glide(gkey("alcove-device-dial", 0), target);
    let fill = if !on {
        Color::white(0.38)
    } else if battery.is_some_and(|l| l < 0.2) {
        palette::RED
    } else {
        Color::white(0.97)
    };
    ui.p.stroke_circle(cx, cy, 46.0, 12.0, Color::white(0.12));
    ui.p.arc(cx, cy, 46.0, 0.0, dial_sweep(ui.cfg, level), 12.0, fill);
    let ic = Rect::centered(cx, cy - 13.0, 26.0, 26.0);
    let tint = if on { Color::white(0.97) } else { Color::white(0.5) };
    ui.p.icon(pk.icon, ic, tint);
    if !on {
        // a slash through the icon when the device has gone
        ui.p.line(ic.x + 3.0, ic.y + 3.0, ic.right() - 3.0, ic.bottom() - 3.0, 3.6, Color::black(0.95));
        ui.p.line(ic.x + 3.0, ic.y + 3.0, ic.right() - 3.0, ic.bottom() - 3.0, 1.8, tint);
    }
    // the name sits inside the ring, clear of the soft blur along the bottom edge; whether the
    // device is connected is shown by the ring and the slash
    let r = Rect::new(cx - 38.0, cy + 3.0, 76.0, 22.0);
    let st = TextStyle::new(12.0, Color::white(0.95)).semibold().center();
    let name = fit_text(ui, pk.title.to_string(), &st, r.w);
    ui.p.text(&name, r, &st);
}

/// "45%" or "45" → ("45", "%") / ("45", ""), when the text is only a number.
fn number_parts(s: &str) -> Option<(&str, &str)> {
    let digits = s.strip_suffix('%').unwrap_or(s);
    let suffix = &s[digits.len()..];
    (!digits.is_empty() && digits.len() <= 4 && digits.bytes().all(|b| b.is_ascii_digit())).then_some((digits, suffix))
}

/// Which rolling number a volume, brightness or battery readout is (so each keeps its own).
fn roll_key(pk: &Peek) -> u64 {
    let slot = match pk.key {
        Some("osd-volume") => 0,
        Some("osd-brightness") => 1,
        Some("battery") => 2,
        _ => 3,
    };
    gkey("hud-number", slot)
}

fn draw_hud(ui: &mut Ui, body: Rect, pk: &Peek, frac: f32, color: Color, text: Option<&str>) {
    let pad = 24.0;
    let row_y = if crate::ui::look_alcove() { body.cy() - 12.0 } else { body.bottom() - 36.0 - band_room(ui.cfg) };
    // Alcove: one big round dial instead of the slider (setting: General, round dial)
    let dial = dial_height(pk, ui.cfg).is_some();
    let dial_cy = body.y + ui.cfg.appearance.notch_height + 66.0;
    let inside = dial;
    let ir = if inside {
        Rect::centered(body.cx(), dial_cy - 13.0, 26.0, 26.0)
    } else if dial {
        Rect::centered(body.cx(), dial_cy, 40.0, 40.0)
    } else {
        Rect::new(body.x + pad, row_y, 24.0, 24.0)
    };
    // Volume only: rest the pointer on the speaker for a moment to move to the next output device
    let mut hint = false;
    if pk.key == Some("osd-volume") {
        let zone = ir.expand(8.0);
        let over = ui.hovered(zone);
        let go = HUD_DWELL.with(|d| {
            let (since, fired) = d.get();
            match (over, since) {
                (false, _) => {
                    d.set((None, false));
                    (0.0, false)
                }
                (true, None) => {
                    d.set((Some(ui.now), false));
                    (0.0, false)
                }
                (true, Some(t)) => {
                    let k = (ui.now.saturating_duration_since(t).as_secs_f32() / HUD_DWELL_SECS).min(1.0);
                    if k >= 1.0 && !fired {
                        d.set((Some(t), true));
                        (1.0, true)
                    } else {
                        (k, false)
                    }
                }
            }
        });
        if over {
            ui.hot = true;
            hint = true;
            if go.0 < 1.0 {
                ui.animate();
            }
        }
        if go.1 {
            modules::osd::request_next_output(ui.bus);
        }
    }
    if dial {
        let fill = if color.a < 0.9 {
            Color::white(0.38)
        } else if color == palette::RED {
            color
        } else {
            Color::white(0.97)
        };
        let level = ui.glide(gkey("alcove-hud", 0), frac.clamp(0.0, 1.0));
        // a thick ring with round ends that fills from the top, in the direction chosen in Settings
        ui.p.stroke_circle(body.cx(), dial_cy, 46.0, 12.0, Color::white(0.12));
        ui.p.arc(body.cx(), dial_cy, 46.0, 0.0, dial_sweep(ui.cfg, level), 12.0, fill);
        let battery = pk.key == Some("battery");
        if battery {
            // a battery in the middle of the ring (with a bolt while charging)
            let charging = text.is_some_and(|t| t.starts_with("Charging") || t.starts_with("Fully"));
            crate::alcove::battery_glyph(ui, Rect::centered(body.cx(), dial_cy - 12.0, 38.0, 20.0), frac, charging);
        } else {
            ui.p.icon(pk.icon, ir, fill.lerp(Color::white(1.0), 0.2));
        }
        // the value sits inside the ring, clear of the soft blur along the bottom edge
        let pct = format!("{:.0}%", frac.clamp(0.0, 1.0) * 100.0);
        let r = Rect::new(body.cx() - 38.0, dial_cy + 3.0, 76.0, 22.0);
        hud_text_zone(ui, pk, Rect::new(body.cx() - 40.0, dial_cy + 8.0, 80.0, 24.0));
        if hint {
            ui.p.text("Switch output", r, &TextStyle::new(10.5, palette::TEXT_DIM).medium().center());
            return;
        }
        let big = match text {
            // "Charging · 45%": the bolt in the glyph already says charging, so only the number
            Some(t) if battery => t.split_once(" · ").map_or(pct, |(_, p)| p.to_string()),
            Some(t) => t.to_string(),
            None => pct,
        };
        // a long name (a Bluetooth device) is set smaller and cut short to stay inside the ring
        let size = if big.chars().count() <= 5 { 17.0 } else { 12.0 };
        let st = TextStyle::new(size, Color::white(0.95)).semibold().display().center();
        if let Some((digits, suffix)) = number_parts(&big) {
            ui.roll_number(r, roll_key(pk), digits, suffix, &st);
        } else {
            let big = fit_text(ui, big, &st, r.w);
            ui.p.text(&big, r, &st);
        }
        return;
    }
    if crate::ui::look_alcove() {
        // a round, quiet pill: the icon, then one thick capsule that fills with the level
        let fill = if color.a < 0.9 {
            Color::white(0.38)
        } else if color == palette::RED {
            color
        } else {
            Color::white(0.97)
        };
        // laid out on a fixed-width frame, so it never squeezes while the notch is still widening
        let cw = 350.0_f32.min(ui.cfg.appearance.expanded_width);
        let frame = Rect::centered(body.cx(), body.cy(), cw, body.h);
        let a0 = ui.p.alpha;
        ui.p.alpha = a0 * smoothstep(0.75, 1.0, body.w / cw);
        ui.p.push_clip(body);
        let ir = Rect::new(frame.x + pad, body.cy() - 12.0, 24.0, 24.0);
        let level = ui.glide(gkey("alcove-hud", 0), frac.clamp(0.0, 1.0));
        // the name of the thing at the left; the bar and its number at the right
        let label = if hint {
            "Switch output"
        } else {
            match pk.key {
                Some("osd-volume") => "Volume",
                Some("osd-brightness") => "Brightness",
                _ => text.map(|t| t.split(" \u{b7} ").next().unwrap_or(t)).unwrap_or("Battery"),
            }
        };
        let ls = TextStyle::new(13.5, Color::white(0.7)).semibold();
        ui.p.text(label, Rect::new(frame.x + pad, ir.y, 120.0, ir.h), &ls);
        let num = format!("{:.0}", frac.clamp(0.0, 1.0) * 100.0);
        let ns = TextStyle::new(13.0, Color::white(0.92)).medium().display().right();
        let nw = 30.0;
        let nx = frame.right() - pad - nw;
        let bar = Rect::new(nx - 10.0 - 120.0, ir.cy() - 3.0, 120.0, 6.0);
        let rr = bar.h * 0.36;
        ui.p.fill_rounded(bar, rr, Color::white(0.14));
        let lv = level.clamp(0.0, 1.0);
        if lv > 0.0 {
            ui.p.fill_rounded(Rect::new(bar.x, bar.y, (bar.w * lv).max(bar.h), bar.h), rr, fill);
        }
        ui.roll_number(Rect::new(nx, ir.y, nw, ir.h), roll_key(pk), &num, "", &ns);
        hud_text_zone(ui, pk, Rect::new(bar.x - 8.0, ir.y - 4.0, bar.w + nw + 26.0, ir.h + 8.0));
        ui.p.pop_clip();
        ui.p.alpha = a0;
        return;
    }
    ui.p.icon(pk.icon, ir, color);
    let frac = frac.clamp(0.0, 1.0);
    let value = text.map(str::to_string).unwrap_or_else(|| format!("{:.0}", frac * 100.0));
    let vs = TextStyle::new(13.0, Color::white(0.80)).medium().display().right();
    let vw = 46.0;
    let value_rect = Rect::new(body.right() - pad - vw, ir.y, vw, ir.h);
    if let Some((digits, suffix)) = number_parts(&value) {
        ui.roll_number(value_rect, roll_key(pk), digits, suffix, &vs);
    } else {
        ui.p.text(&value, value_rect, &vs);
    }
    hud_text_zone(ui, pk, Rect::new(body.right() - pad - vw - 6.0, ir.y - 4.0, vw + 12.0, ir.h + 8.0));
    let bx = if hint { ir.right() + 14.0 } else { ir.right() + 16.0 };
    if hint {
        ui.p.text("Switch output", Rect::new(bx, ir.y, 110.0, ir.h), &TextStyle::new(11.5, palette::TEXT_DIM));
    } else {
        let bar = Rect::new(bx, ir.cy() - 7.0, (body.right() - pad - vw - 12.0 - bx).max(20.0), 14.0);
        if color.a >= 0.9 {
            ui.ticks_hud(bar, frac); // blue with white highlights
        } else {
            ui.beams(|ui| ui.ticks_ex(bar, frac, color, true)); // muted: stays quiet
        }
    }
}

thread_local! {
    /// The last fitted hello text: (font|title, size, width).
    static HELLO_FIT: std::cell::RefCell<Option<(String, f32, f32)>> = const { std::cell::RefCell::new(None) };
}

/// When (0..1 of the peek) the scan gives way to the handwritten hello.
const SCAN_HELLO_AT: f32 = 0.46;

/// The unlock greeting in the Alcove look: a big open padlock in the middle of the same thick ring
/// the volume dial uses. The ring fills, the lock opens, then "Hello" and the name come in below.
fn draw_scan_alcove(ui: &mut Ui, body: Rect, _pk: &Peek, t: f32) {
    // closed lock + green scan mark, a soft swap, open lock + green check (no spinning)
    let alpha = ui.p.alpha;
    let cw = 260.0_f32.min(ui.cfg.appearance.expanded_width);
    let frame = Rect::centered(body.cx(), body.cy(), cw, body.h);
    let show = smoothstep(0.75, 1.0, body.w / cw) * (1.0 - smoothstep(0.90, 1.0, t));
    let out = smoothstep(0.26, 0.38, t);
    let inn = smoothstep(0.40, 0.54, t);
    let cy = body.cy();
    let (lx, rx) = (frame.x + 15.0, frame.right() - 15.0);

    ui.p.push_clip(body);
    // the padlock itself stays; only its shackle spins and swings out
    ui.p.alpha = alpha * show;
    lock_swing(ui, lx, cy, smoothstep(0.28, 0.64, t));
    // before
    ui.p.alpha = alpha * show * (1.0 - out);
    if out < 0.999 {
        face_id(ui, rx, cy, 1.0 - 0.12 * out);
    }
    // after
    ui.p.alpha = alpha * show * inn;
    if inn > 0.001 {
        let k = 0.88 + 0.12 * inn;
        ui.p.fill_circle(rx, cy, 8.0 * k, palette::GREEN);
        ui.p.icon(Icon::Check, Rect::centered(rx, cy, 11.0 * k, 11.0 * k), Color::black(0.85));
        let ws = TextStyle::new(13.5, Color::white(0.85)).semibold().right();
        ui.p.text("Unlocked", Rect::new(rx - 8.0 - 12.0 - 100.0, cy - 10.0, 100.0, 20.0), &ws);
    }
    ui.p.pop_clip();

    ui.p.alpha = alpha;
    if t < 0.6 {
        ui.animate();
    }
}

/// The padlock of the unlock animation, 16 px tall at scale 1: a light grey body and a thin
/// shackle. Open, the shackle sits shifted to the right of the body, its right leg hanging free.
fn lock_glyph(ui: &mut Ui, cx: f32, cy: f32, s: f32, open: bool) {
    let c = Color::white(0.84);
    ui.p.fill_rounded(Rect::new(cx - 5.5 * s, cy, 11.0 * s, 8.0 * s), 1.8 * s, c);
    let w = 2.0 * s;
    let hw = if open { 3.0 } else { 3.4 } * s;
    let sx = if open { 7.5 * s } else { 0.0 };
    let (xl, xr) = (cx - hw + sx, cx + hw + sx);
    let ac = cy - 7.0 * s + hw;
    ui.p.line(xl, cy + 0.5 * s, xl, ac, w, c);
    ui.p.line(xr, cy + 0.5 * s, xr, ac, w, c);
    let mut prev = (xl, ac);
    for i in 1..=8 {
        let a = std::f32::consts::PI * (1.0 - i as f32 / 8.0);
        let to = (cx + sx + hw * a.cos(), ac - hw * a.sin());
        ui.p.line(prev.0, prev.1, to.0, to.1, w, c);
        prev = to;
    }
}

/// The padlock with its shackle on the move, as on a real one: `k` 0..1 first pops the shackle up
/// a little, then swivels it about its left leg (the one that stays in the body) until it hangs
/// open to the left. Seen from the front, the arc squeezes to a line as it turns edge-on and
/// then opens out again on the other side. The body stays put, in front of the shackle.
fn lock_swing(ui: &mut Ui, cx: f32, cy: f32, k: f32) {
    let c = Color::white(0.84);
    let hw = 3.4;
    let lift = 3.0 * smoothstep(0.0, 0.3, k);
    let cp = (std::f32::consts::PI * 0.9 * smoothstep(0.25, 1.0, k)).cos();
    let xl = cx - hw;
    let ac = cy - 7.0 + hw - lift;
    let foot = cy + 4.0 - lift;
    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(11);
    pts.push((xl, foot));
    for i in 0..=8 {
        let a = std::f32::consts::PI * (1.0 - i as f32 / 8.0);
        pts.push((xl + (hw + hw * a.cos()) * cp, ac - hw * a.sin()));
    }
    pts.push((xl + 2.0 * hw * cp, foot));
    for w in pts.windows(2) {
        ui.p.line(w[0].0, w[0].1, w[1].0, w[1].1, 2.0, c);
    }
    ui.p.fill_rounded(Rect::new(cx - 5.5, cy, 11.0, 8.0), 1.8, c);
}

/// The green Face ID mark: four corner brackets round a small face.
fn face_id(ui: &mut Ui, cx: f32, cy: f32, s: f32) {
    let g = palette::GREEN;
    let (h, arm, w) = (6.5 * s, 3.4 * s, 1.5 * s);
    for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let (x, y) = (cx + sx * h, cy + sy * h);
        ui.p.line(x, y, x - sx * arm, y, w, g);
        ui.p.line(x, y, x, y - sy * arm, w, g);
    }
    ui.p.line(cx - 2.2 * s, cy - 2.2 * s, cx - 2.2 * s, cy - 0.6 * s, w * 0.9, g);
    ui.p.line(cx + 2.2 * s, cy - 2.2 * s, cx + 2.2 * s, cy - 0.6 * s, w * 0.9, g);
    let mut prev = (cx - 2.6 * s, cy + 2.2 * s);
    for i in 1..=4 {
        let x = -2.6 + 5.2 * i as f32 / 4.0;
        let to = (cx + x * s, cy + (2.2 + 1.2 * (1.0 - (x / 2.6) * (x / 2.6))) * s);
        ui.p.line(prev.0, prev.1, to.0, to.1, w * 0.9, g);
        prev = to;
    }
}

/// A Bluetooth device connecting or leaving, in the Alcove look: shown as the wide pill.
fn bt_pill(p: &Peek) -> bool {
    crate::ui::look_alcove() && p.key == Some("bluetooth") && !p.is_hud() && !p.scan
}

/// Caps lock and the other lock keys, the microphone, and USB drives (Alcove look): no bar,
/// just the name at the left and the state at the right.
fn status_pill(p: &Peek) -> bool {
    crate::ui::look_alcove() && !p.scan && matches!(p.key, Some("locks" | "osd-mic" | "devices"))
}

fn draw_status_pill(ui: &mut Ui, body: Rect, pk: &Peek) {
    let (label, value, on): (String, &str, bool) = match pk.key {
        Some("locks") => {
            let text = match &pk.trailing {
                Trailing::Level { text: Some(t), .. } => t.as_str(),
                _ => "",
            };
            let mut it = text.splitn(2, " \u{b7} ");
            let l = it.next().unwrap_or("").to_string();
            let on = it.next() == Some("On");
            (l, if on { "On" } else { "Off" }, on)
        }
        Some("osd-mic") => {
            let muted = pk.subtitle == "Muted";
            ("Microphone".to_string(), if muted { "Muted" } else { "Unmuted" }, !muted)
        }
        _ => {
            let up = pk.title == "Drive connected";
            let d = pk.subtitle.trim_end_matches('\\');
            let l = if d.is_empty() { "USB drive".to_string() } else { format!("USB drive {d}") };
            (l, if up { "Connected" } else { "Disconnected" }, up)
        }
    };
    let cw = 300.0_f32.min(ui.cfg.appearance.expanded_width);
    let frame = Rect::centered(body.cx(), body.cy(), cw, body.h);
    let a0 = ui.p.alpha;
    ui.p.alpha = a0 * smoothstep(0.75, 1.0, body.w / cw);
    ui.p.push_clip(body);
    let r = Rect::new(frame.x + 24.0, body.cy() - 12.0, cw - 48.0, 24.0);
    ui.p.text(&label, r, &TextStyle::new(13.5, Color::white(0.7)).semibold());
    let vc = if on { Color::white(0.95) } else { Color::white(0.55) };
    ui.p.text(value, r, &TextStyle::new(13.5, vc).semibold().right());
    ui.p.pop_clip();
    ui.p.alpha = a0;
}

/// A pair of earbuds seen from the front that turns about its vertical axis: both buds orbit
/// the axis (the nearer one is drawn last) and each carries its stem and its nozzle round with it.
fn airpods(ui: &mut Ui, cx: f32, cy: f32, turn: f32, k: f32) {
    let (sn, cs) = turn.sin_cos();
    let (d, n) = (4.2f32, 3.2f32);
    let mut order = [(-1.0f32, sn * d), (1.0, -sn * d)];
    if order[0].1 > order[1].1 {
        order.swap(0, 1);
    }
    for (sx, z) in order {
        let zn = ((z / d) + 1.0) * 0.5;
        let c = Color::white(k * (0.70 + 0.28 * zn));
        let x = cx + sx * d * cs;
        let (hy, hr) = (cy - 1.8, 3.6);
        // the nozzle faces the other bud; once the pair has turned it can be hidden behind the head
        let nz = sx * n * sn;
        let nozzle = |ui: &mut Ui| ui.p.fill_circle(x - sx * n * cs, cy - 1.2, 1.9, Color::white(k * (0.60 + 0.2 * zn)));
        if nz < 0.0 {
            nozzle(ui);
        }
        let sxs = x + sx * 1.4 * cs;
        ui.p.fill_rounded(Rect::new(sxs - 1.3, hy, 2.6, 7.3), 1.3, c);
        ui.p.fill_circle(x, hy, hr, c);
        if nz >= 0.0 {
            nozzle(ui);
        }
    }
}

/// Earbuds (or the device's icon) at the left, a mint battery ring at the right, no text.
/// `t` runs 0..1 over the peek.
fn draw_bt_pill(ui: &mut Ui, body: Rect, pk: &Peek, t: f32) {
    let alpha = ui.p.alpha;
    let gone = pk.subtitle == "Disconnected";
    let cw = (ui.cfg.appearance.notch_width + 40.0).max(220.0).min(ui.cfg.appearance.expanded_width);
    let frame = Rect::centered(body.cx(), body.cy(), cw, body.h);
    let show = smoothstep(0.75, 1.0, body.w / cw) * (1.0 - smoothstep(0.90, 1.0, t));
    let pop = smoothstep(0.0, 0.2, t);
    let cy = body.cy();
    let cx = frame.x + 20.0;
    let lower = pk.title.to_lowercase();
    let buds = matches!(pk.icon, Icon::Headphones)
        || ["airpods", "pods", "buds", "earbud", "earphone"].iter().any(|k| lower.contains(k));

    ui.p.push_clip(body);
    ui.p.alpha = alpha * show;
    if buds && gone {
        // spinning while they fall: slow at first, then faster, out through the bottom edge
        let v = ((t - 0.28) / 0.5).clamp(0.0, 1.0);
        let turn = std::f32::consts::TAU * 0.9 * smoothstep(0.0, 0.8, t);
        airpods(ui, cx, cy + 30.0 * v * v, turn, 1.0);
    } else if buds {
        let turn = std::f32::consts::PI * smoothstep(0.12, 0.62, t);
        airpods(ui, cx, cy, turn, 1.0);
    } else {
        let k = 0.85 + 0.15 * pop;
        let tone = if gone { Color::white(0.45) } else { Color::white(0.97) };
        ui.p.icon(pk.icon, Rect::centered(cx, cy, 20.0 * k, 20.0 * k), tone);
    }
    if gone && !buds {
        ui.p.line(cx - 8.0, cy + 7.0, cx + 8.0, cy - 7.0, 1.8, Color::white(0.7));
    }

    // the battery ring, filling as it appears (a full ring when the device reports no level)
    let rx = frame.right() - 20.0;
    let appear = smoothstep(0.08, 0.40, t);
    let mint = Color::hex(0x7BF3A6);
    ui.p.alpha = alpha * show * appear;
    ui.p.stroke_circle(rx, cy, 6.2, 2.2, mint.with_a(0.22));
    if !gone {
        let lv = match &pk.trailing {
            Trailing::Battery { level, .. } => ui.glide(gkey("bt-ring", 0), level.clamp(0.0, 1.0)),
            _ => 1.0,
        };
        ui.p.arc(rx, cy, 6.2, 0.0, dial_sweep(ui.cfg, (lv * appear).min(0.999)), 2.2, mint);
    }
    ui.p.pop_clip();

    ui.p.alpha = alpha;
    if t < 0.9 {
        ui.animate();
    }
}

/// The name after "Hello" in the peek title ("" for a plain hello).
fn hello_name(title: &str) -> &str {
    title.strip_prefix("Hello").map_or("", str::trim)
}

/// Size of the stage for the hello: (width, height). A name adds a line below.
fn hello_stage(title: &str, custom_font: bool) -> (f32, f32) {
    let name = hello_name(title);
    if custom_font {
        ((52.0 + title.chars().count() as f32 * 22.0).clamp(210.0, 440.0), 112.0)
    } else if name.is_empty() {
        (210.0, 112.0)
    } else {
        ((52.0 + name.chars().count() as f32 * 17.0).clamp(210.0, 420.0), 158.0)
    }
}

/// "hello" as one continuous pen stroke (monoline cursive), in a 240 x 110 box.
const HELLO_PATH: [(f32, f32); 63] = [
    (10.0, 96.0),
    (18.0, 72.0),
    (28.0, 40.0),
    (37.0, 17.0),
    (43.0, 9.0),
    (50.0, 11.0),
    (51.0, 22.0),
    (46.0, 44.0),
    (38.0, 68.0),
    (33.0, 86.0),
    (34.0, 80.0),
    (38.0, 68.0),
    (46.0, 62.0),
    (55.0, 60.0),
    (63.0, 65.0),
    (65.0, 76.0),
    (65.0, 86.0),
    (70.0, 90.0),
    (77.0, 84.0),
    (83.0, 72.0),
    (91.0, 64.0),
    (98.0, 66.0),
    (97.0, 75.0),
    (88.0, 79.0),
    (79.0, 80.0),
    (83.0, 88.0),
    (93.0, 91.0),
    (102.0, 85.0),
    (110.0, 66.0),
    (119.0, 37.0),
    (125.0, 15.0),
    (130.0, 8.0),
    (135.0, 13.0),
    (131.0, 40.0),
    (125.0, 68.0),
    (122.0, 84.0),
    (127.0, 91.0),
    (135.0, 86.0),
    (142.0, 66.0),
    (151.0, 37.0),
    (157.0, 15.0),
    (162.0, 8.0),
    (167.0, 13.0),
    (163.0, 40.0),
    (157.0, 68.0),
    (154.0, 84.0),
    (159.0, 91.0),
    (167.0, 86.0),
    (173.0, 75.0),
    (180.0, 64.0),
    (190.0, 60.0),
    (198.0, 66.0),
    (198.0, 78.0),
    (190.0, 89.0),
    (179.0, 90.0),
    (172.0, 82.0),
    (173.0, 72.0),
    (182.0, 64.0),
    (195.0, 59.0),
    (210.0, 56.0),
    (226.0, 51.0),
    (226.0, 51.0),
    (226.0, 51.0),
];

/// The pen path as evenly spaced points (smoothed with a Catmull-Rom spline), built once.
fn hello_points() -> &'static [(f32, f32)] {
    static PTS: std::sync::OnceLock<Vec<(f32, f32)>> = std::sync::OnceLock::new();
    PTS.get_or_init(|| {
        // the last points only pad the end of the spline
        let src = &HELLO_PATH[..HELLO_PATH.len() - 2];
        let n = src.len();
        let at = |i: isize| -> (f32, f32) {
            if i < 0 {
                (2.0 * src[0].0 - src[1].0, 2.0 * src[0].1 - src[1].1)
            } else if i as usize >= n {
                (2.0 * src[n - 1].0 - src[n - 2].0, 2.0 * src[n - 1].1 - src[n - 2].1)
            } else {
                src[i as usize]
            }
        };
        let mut dense = Vec::new();
        for i in 0..(n as isize - 1) {
            let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
            for k in 0..16 {
                let t = k as f32 / 16.0;
                let f = |a: f32, b: f32, c: f32, d: f32| {
                    0.5 * (2.0 * b
                        + (-a + c) * t
                        + (2.0 * a - 5.0 * b + 4.0 * c - d) * t * t
                        + (-a + 3.0 * b - 3.0 * c + d) * t * t * t)
                };
                dense.push((f(p0.0, p1.0, p2.0, p3.0), f(p0.1, p1.1, p2.1, p3.1)));
            }
        }
        dense.push(src[n - 1]);
        // resample at a fixed spacing so the pen moves at an even speed
        let step = 2.2f32;
        let mut out = vec![dense[0]];
        let mut carry = 0.0f32;
        for w in dense.windows(2) {
            let (a, b) = (w[0], w[1]);
            let seg = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
            let mut d = step - carry;
            while d <= seg {
                let k = d / seg.max(0.0001);
                out.push((a.0 + (b.0 - a.0) * k, a.1 + (b.1 - a.1) * k));
                d += step;
            }
            carry = seg - (d - step);
        }
        out
    })
}

/// Face-ID style unlock animation, in the app's blue: four corner brackets
/// close in, a ring of beams fills while a smiley waits, the smiley turns into
/// a check, then the stage shrinks and "Hello" (or "Hello <name>") is written
/// out in handwriting with a beam at the pen's edge. `t` runs 0..1 over the peek.
fn draw_scan(ui: &mut Ui, body: Rect, pk: &Peek, t: f32) {
    if crate::ui::look_alcove() {
        return draw_scan_alcove(ui, body, pk, t);
    }
    let blue = palette::BLUE;
    let cx = body.cx();
    let cy = body.y + 66.0;
    let alpha = ui.p.alpha;
    // everything of the scan fades out as the stage shrinks
    let gone = smoothstep(0.40, 0.50, t);
    let scan_a = alpha * (1.0 - gone);

    if gone < 0.999 {
        // corner brackets: fly in from slightly outside
        let appear = smoothstep(0.0, 0.08, t);
        let half = 40.0 * (1.0 + 0.25 * (1.0 - appear)) * (1.0 - 0.1 * gone);
        ui.p.alpha = scan_a * appear;
        let (arm, th) = (17.0, 3.4);
        for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
            let (x, y) = (cx + sx * half, cy + sy * half);
            let hx = if sx < 0.0 { x - th * 0.5 } else { x - arm + th * 0.5 };
            let vy = if sy < 0.0 { y - th * 0.5 } else { y - arm + th * 0.5 };
            ui.p.fill_rounded(Rect::new(hx, y - th * 0.5, arm, th), th * 0.5, blue);
            ui.p.fill_rounded(Rect::new(x - th * 0.5, vy, th, arm), th * 0.5, blue);
        }

        // ring of beams filling while it "scans"
        ui.p.alpha = scan_a * smoothstep(0.02, 0.1, t);
        ui.ring(cx, cy, 62.0, 4.0, smoothstep(0.06, 0.30, t), blue);

        // smiley → check
        let done = smoothstep(0.26, 0.34, t);
        ui.p.alpha = scan_a * (1.0 - done) * smoothstep(0.02, 0.1, t);
        let blink = 1.0 - 0.8 * (smoothstep(0.16, 0.19, t) - smoothstep(0.19, 0.22, t));
        draw_smiley(ui, cx, cy, blink, blue);
        ui.p.alpha = scan_a * done;
        let k = 0.6 + 0.4 * done;
        ui.p.icon(Icon::Check, Rect::centered(cx, cy, 30.0 * k, 30.0 * k), Color::white(1.0));

        // label and a bar of beams underneath
        ui.p.alpha = scan_a * smoothstep(0.32, 0.40, t);
        ui.p.text(
            "Unlocked",
            Rect::new(body.x + 12.0, cy + 52.0, body.w - 24.0, 20.0),
            &TextStyle::new(14.0, palette::TEXT).semibold().center(),
        );
        ui.p.alpha = scan_a * smoothstep(0.0, 0.1, t);
        ui.ticks(Rect::new(body.x + 40.0, body.bottom() - 22.0, body.w - 80.0, 8.0), smoothstep(0.04, 0.40, t), blue);
    }

    // the handwritten hello: "hello" is drawn by a pen along one flowing stroke,
    // a name (if there is one) is written below it in a script font
    if t > SCAN_HELLO_AT {
        let name = hello_name(&pk.title);
        let fade_in = smoothstep(SCAN_HELLO_AT, SCAN_HELLO_AT + 0.05, t);
        let ease = |x: f32| x * x * (3.0 - 2.0 * x);

        // an installed handwriting font was chosen: write the whole greeting with it
        let font = ui.cfg.greeting.font_name().to_string();
        if ui.p.gfx.font_installed(&font) {
            gfx::set_hello_font(&font);
            let stage_w = hello_stage(&pk.title, true).0;
            // measuring builds a text layout, so the fitted size is worked out once per greeting
            let key = format!("{font}|{}", pk.title);
            let cached = HELLO_FIT.with(|c| c.borrow().as_ref().filter(|f| f.0 == key).map(|f| (f.1, f.2)));
            let (size, tw) = match cached {
                Some(v) => v,
                None => {
                    let probe = TextStyle::new(48.0, blue).hello_font();
                    let (pw, _) = ui.p.measure(&pk.title, &probe);
                    let size = 48.0 * ((stage_w - 44.0) / pw.max(1.0)).min(1.0);
                    let (tw, _) = ui.p.measure(&pk.title, &TextStyle::new(size, blue).hello_font());
                    HELLO_FIT.with(|c| *c.borrow_mut() = Some((key, size, tw)));
                    (size, tw)
                }
            };
            let style = TextStyle::new(size, blue).hello_font().center();
            let box_h = size * 1.6;
            let ty = body.cy() - box_h * 0.5 - 8.0;
            let tx = cx - tw * 0.5 - 10.0;
            let wr = ease(smoothstep(SCAN_HELLO_AT + 0.04, 0.82, t));
            let reveal = (tw + 20.0) * wr;
            ui.p.alpha = alpha * fade_in;
            ui.p.push_clip(Rect::new(tx, ty - 6.0, reveal, box_h + 12.0));
            ui.p.text(&pk.title, Rect::new(tx, ty, tw + 20.0, box_h), &style);
            ui.p.pop_clip();
            let pen_a = alpha * fade_in * (1.0 - smoothstep(0.82, 0.90, t));
            if pen_a > 0.003 && wr > 0.0 && wr < 1.0 {
                let lead = crate::ui::beam_lead();
                let ex = tx + reveal;
                for (i, a) in [0.40f32, 0.20, 0.09].iter().enumerate() {
                    ui.p.alpha = pen_a * a;
                    ui.p.fill_rounded(
                        Rect::new(ex - 6.0 * (i as f32 + 1.0) - 1.2, ty + 4.0, 2.4, box_h - 8.0),
                        1.2,
                        blue,
                    );
                }
                ui.p.alpha = pen_a;
                ui.p.fill_rounded(Rect::new(ex - 1.7, ty, 3.4, box_h), 1.7, lead);
            }
            ui.p.alpha = alpha * smoothstep(SCAN_HELLO_AT + 0.02, SCAN_HELLO_AT + 0.08, t);
            ui.ticks(Rect::new(body.x + 40.0, body.bottom() - 20.0, body.w - 80.0, 8.0), wr, blue);
            ui.p.alpha = alpha;
            // nothing moves once the writing is done: no more frames until the peek ends
            if t < 0.92 {
                ui.animate();
            }
            return;
        }
        let path_end = if name.is_empty() { 0.76 } else { 0.70 };
        let wr = ease(smoothstep(SCAN_HELLO_AT + 0.04, path_end, t));
        let pts = hello_points();
        let s = 0.84f32;
        let (bw, bh) = (240.0 * s, 110.0 * s);
        let top = if name.is_empty() { body.cy() - bh * 0.5 - 8.0 } else { body.y + 16.0 };
        let (ox, oy) = (cx - bw * 0.5, top);
        let n_vis = ((pts.len() as f32) * wr) as usize;
        ui.p.alpha = alpha * fade_in;
        for &(px, py) in &pts[..n_vis.min(pts.len())] {
            ui.p.fill_circle(ox + px * s, oy + py * s, 1.6, blue);
        }
        // a beam rides at the pen with a short trail behind it
        let write_end = if name.is_empty() { path_end } else { 0.90 };
        let pen_a = alpha * fade_in * (1.0 - smoothstep(write_end, write_end + 0.08, t));
        let mut tip = (ox, oy);
        if pen_a > 0.003 && wr > 0.0 {
            let lead = crate::ui::beam_lead();
            let at_pen = |back: usize| {
                let i = n_vis.saturating_sub(back).min(pts.len() - 1);
                (ox + pts[i].0 * s, oy + pts[i].1 * s)
            };
            if wr < 1.0 {
                for (k, a) in [(30usize, 0.10f32), (20, 0.18), (10, 0.32)] {
                    let (x, y) = at_pen(k);
                    ui.p.alpha = pen_a * a;
                    ui.p.fill_rounded(Rect::new(x - 1.2, y - 11.0, 2.4, 22.0), 1.2, blue);
                }
                let (x, y) = at_pen(0);
                tip = (x, y);
                ui.p.alpha = pen_a;
                ui.p.fill_rounded(Rect::new(x - 1.7, y - 13.0, 3.4, 26.0), 1.7, lead);
            }
        }
        let _ = tip;

        let mut frac = wr;
        if !name.is_empty() {
            let nt = ease(smoothstep(0.72, 0.90, t));
            frac = 0.5 * wr + 0.5 * nt;
            let max_w = hello_stage(&pk.title, false).0 - 40.0;
            let probe = TextStyle::new(28.0, blue).script();
            let (pw, _) = ui.p.measure(name, &probe);
            let size = 28.0 * (max_w / pw.max(1.0)).min(1.0);
            let style = TextStyle::new(size, blue).script().center();
            let (tw, _) = ui.p.measure(name, &style);
            let box_h = size * 1.5;
            let ty = body.y + 16.0 + bh + 2.0;
            let tx = cx - tw * 0.5 - 8.0;
            let reveal = (tw + 16.0) * nt;
            ui.p.alpha = alpha * fade_in;
            ui.p.push_clip(Rect::new(tx, ty - 4.0, reveal, box_h + 8.0));
            ui.p.text(name, Rect::new(tx, ty, tw + 16.0, box_h), &style);
            ui.p.pop_clip();
            if nt > 0.0 && nt < 1.0 {
                let lead = crate::ui::beam_lead();
                let ex = tx + reveal;
                for (i, a) in [0.40f32, 0.20, 0.09].iter().enumerate() {
                    ui.p.alpha = alpha * a;
                    ui.p.fill_rounded(
                        Rect::new(ex - 6.0 * (i as f32 + 1.0) - 1.2, ty + 2.0, 2.4, box_h - 4.0),
                        1.2,
                        blue,
                    );
                }
                ui.p.alpha = alpha;
                ui.p.fill_rounded(Rect::new(ex - 1.7, ty, 3.4, box_h), 1.7, lead);
            }
        }

        // a rule of beams along the bottom fills as the pen moves
        ui.p.alpha = alpha * smoothstep(SCAN_HELLO_AT + 0.02, SCAN_HELLO_AT + 0.08, t);
        ui.ticks(Rect::new(body.x + 40.0, body.bottom() - 20.0, body.w - 80.0, 8.0), frac, blue);
    }
    ui.p.alpha = alpha;
    if t < 0.92 {
        ui.animate();
    }
}

/// The Face ID smiley: two eyes, a hooked nose and a smile, all round-capped strokes.
fn draw_smiley(ui: &mut Ui, cx: f32, cy: f32, eye_open: f32, color: Color) {
    let w = 3.6;
    // eyes (they squash towards their middle when blinking)
    let (top, bot) = (cy - 19.0, cy - 9.0);
    let mid = (top + bot) * 0.5;
    let half = (bot - top) * 0.5 * eye_open.clamp(0.1, 1.0);
    for x in [cx - 12.0, cx + 12.0] {
        ui.p.line(x, mid - half, x, mid + half, w, color);
    }
    // nose: a stroke that hooks to the left at the bottom
    let nx = cx + 3.0;
    ui.p.line(nx, cy - 13.0, nx, cy + 3.0, w, color);
    ui.p.arc(nx - 5.0, cy + 3.0, 5.0, std::f32::consts::FRAC_PI_2, std::f32::consts::PI * 0.62, w, color);
    // smile
    ui.p.arc(cx, cy + 2.0, 15.0, std::f32::consts::PI - 1.0, 2.0, w, color);
}

fn draw_peek(ui: &mut Ui, body: Rect, pk: &Peek, left: f32, swap: Option<(f32, &Peek)>) {
    if ui.hovered(body) {
        ui.hot = true;
    }
    // One look for every peek, taken from the unlock animation: a plain white
    // icon on the left, a small coloured status badge on the right.
    if pk.scan {
        draw_scan(ui, body, pk, 1.0 - left);
        return;
    }
    if status_pill(pk) {
        draw_status_pill(ui, body, pk);
        return;
    }
    if bt_pill(pk) {
        draw_bt_pill(ui, body, pk, 1.0 - left);
        return;
    }
    if let Trailing::Level { frac, color, text } = &pk.trailing {
        draw_hud(ui, body, pk, *frac, *color, text.as_deref());
        return;
    }
    if dial_height(pk, ui.cfg).is_some() {
        draw_device_dial(ui, body, pk);
        return;
    }
    // text peeks carry a rail of ticks along the bottom that drains while the peek is up
    let rail = !pk.is_compact() && !crate::ui::look_alcove();
    let room = band_room(ui.cfg);
    let inner = Rect::new(body.x + 22.0, body.y + 8.0, body.w - 44.0, body.h - 16.0 - room - if rail { 12.0 } else { 0.0 });
    if rail {
        let c = pk.accent.lerp(Color::white(1.0), 0.35);
        ui.ticks(Rect::new(body.x + 26.0, body.bottom() - 17.0, body.w - 52.0, 6.0), left, c);
        ui.animate();
    }
    match swap {
        // the next track: the old content drifts up and fades, the new one rises in
        Some((t, old)) => {
            let e = t.clamp(0.0, 1.0);
            let a0 = ui.p.alpha;
            ui.p.push_clip(body);
            let out = smoothstep(0.0, 0.5, e);
            let inn = smoothstep(0.35, 1.0, e);
            ui.p.alpha = a0 * (1.0 - out);
            draw_peek_content(ui, inner.translate(0.0, -10.0 * out), old);
            ui.p.alpha = a0 * inn;
            draw_peek_content(ui, inner.translate(0.0, 10.0 * (1.0 - inn)), pk);
            ui.p.alpha = a0;
            ui.p.pop_clip();
            ui.animate();
        }
        None => draw_peek_content(ui, inner, pk),
    }
}

/// Icon or cover, title, subtitle and trailing part of a text peek, inside `inner`.
fn draw_peek_content(ui: &mut Ui, inner: Rect, pk: &Peek) {
    if crate::ui::look_alcove() {
        return crate::alcove::peek_content(ui, inner, pk);
    }
    let is = if pk.is_compact() { 22.0 } else { 26.0 };
    let ir = Rect::new(inner.x, inner.cy() - is * 0.5, is, is);
    match &pk.image {
        Some(img) => ui.p.image(img, ir, 8.0, 1.0),
        None => {
            if crate::ui::look_alcove() {
                // a round tint of the accent with the icon inside
                ui.p.fill_circle(ir.cx(), ir.cy(), is * 0.62, pk.accent.with_a(0.24));
                ui.p.icon(pk.icon, ir.inset(is * 0.06), pk.accent.lerp(Color::white(1.0), 0.25));
            } else {
                ui.p.icon(pk.icon, ir.inset(is * 0.04), Color::white(0.96))
            }
        }
    }
    let badge = false;
    if pk.is_compact() {
        return;
    }
    let trailing_w = match &pk.trailing {
        Trailing::None | Trailing::Level { .. } => {
            if badge {
                30.0
            } else {
                0.0
            }
        }
        Trailing::Text(s, _) => ui.p.measure(s, &TextStyle::new(15.0, palette::TEXT).semibold()).0 + 8.0,
        Trailing::Ring(..) => 34.0,
        Trailing::Battery { .. } => 40.0,
    };
    let tx = ir.right() + 12.0;
    let tw = inner.right() - tx - trailing_w;
    let has_sub = !pk.subtitle.is_empty();
    let title_r =
        if has_sub { Rect::new(tx, inner.cy() - 19.0, tw, 20.0) } else { Rect::new(tx, inner.cy() - 10.0, tw, 20.0) };
    ui.p.text(&pk.title, title_r, &TextStyle::new(14.5, palette::TEXT).semibold());
    if has_sub {
        ui.p.text(&pk.subtitle, Rect::new(tx, inner.cy() + 1.0, tw, 18.0), &TextStyle::new(12.5, palette::TEXT_DIM));
    }
    let tr = Rect::new(inner.right() - trailing_w, inner.y, trailing_w, inner.h);
    match &pk.trailing {
        Trailing::None | Trailing::Level { .. } => {}
        Trailing::Text(s, c) => ui.p.text(s, tr, &TextStyle::new(15.0, *c).semibold().display().right()),
        Trailing::Ring(f, c) => {
            let (cx, cy) = (tr.right() - 14.0, tr.cy());
            ui.ring(cx, cy, 24.0, 3.0, *f, *c);
        }
        Trailing::Battery { level, charging } => {
            let r = Rect::new(tr.right() - 36.0, tr.cy() - 8.0, 36.0, 16.0);
            icons::battery(ui.p, r, *level, *charging, battery_color(*level, *charging));
        }
    }
}

/// The soft bottom edge: the blended strip drawn in one fill through the notch outline.
#[allow(clippy::too_many_arguments)]
fn band_draw(ui: &mut Ui, snap: &Snapshot, body: Rect, radius: f32, ear: f32) {
    if let Some(g) = gfx::notch_path(ui.p, body, radius, ear) {
        ui.p.snapshot_baked(snap, &g);
    }
}

/// Extra height under the text of a popup while the soft bottom edge is on, so that edge only
/// ever meets empty glass and never blurs a line of text.
fn band_room(cfg: &Config) -> f32 {
    if crate::ui::look_alcove() && cfg.appearance.bottom_fade && cfg.appearance.blur {
        40.0
    } else {
        0.0
    }
}

/// How tall the soft-blur strip along the bottom of the open notch is.
const BAND_H: f32 = 64.0;

/// The open notch. In the Alcove look the page dots sit on top of it, and while their page list
/// is open the page underneath does not react to the mouse.
#[allow(clippy::too_many_arguments)]
fn draw_expanded(
    ui: &mut Ui,
    full: Rect,
    view: Rect,
    modules: &mut [Box<dyn Module>],
    started: &[bool],
    tabs: &[(ModuleId, Icon, String)],
    page: ModuleId,
    trans: Option<(ModuleId, f32, f32)>,
    indicators: &[Indicator],
    snaps: Option<(&Snapshot, &Snapshot)>,
) {
    if !(crate::ui::look_alcove() || crate::ui::look_tuna()) {
        return draw_expanded_core(ui, full, modules, started, tabs, page, trans, indicators, snaps);
    }
    crate::ui::reset_wheel_taken();
    draw_expanded_core(ui, full, modules, started, tabs, page, trans, indicators, snaps);
    if crate::ui::look_alcove() && page != modules::alcove_launcher::ID && page != modules::mixer::ID {
        crate::alcove::rail(ui, view, tabs, page);
    }

    // anywhere on the notch: a swipe sideways, or the wheel where nothing scrolls, turns the page
    // (the launcher page has no neighbours: it is only opened by its hotkeys)
    if tabs.is_empty() || page == modules::alcove_launcher::ID || page == modules::mixer::ID || !ui.hovered(full) {
        return;
    }
    let (h, v) = (ui.input.hwheel, ui.input.wheel);
    let dir = if h != 0.0 {
        if h > 0.0 {
            1
        } else {
            -1
        }
    } else if v != 0.0 && !crate::ui::wheel_taken() {
        if v < 0.0 {
            1
        } else {
            -1
        }
    } else {
        0
    };
    if dir == 0 {
        return;
    }
    let ready = WHEEL_FLIP.with(|c| c.get().is_none_or(|t| t.elapsed() > Duration::from_millis(450)));
    if !ready {
        return;
    }
    let n = tabs.len();
    let next = match tabs.iter().position(|t| t.0 == page) {
        Some(i) if dir > 0 => (i + 1) % n,
        Some(i) => (i + n - 1) % n,
        None => 0,
    };
    ui.fx.open_page = Some(tabs[next].0);
    WHEEL_FLIP.with(|c| c.set(Some(Instant::now())));
}

#[allow(clippy::too_many_arguments)]
fn draw_expanded_core(
    ui: &mut Ui,
    full: Rect,
    modules: &mut [Box<dyn Module>],
    started: &[bool],
    tabs: &[(ModuleId, Icon, String)],
    page: ModuleId,
    trans: Option<(ModuleId, f32, f32)>,
    indicators: &[Indicator],
    snaps: Option<(&Snapshot, &Snapshot)>,
) {
    if crate::ui::look_alcove() {
        // no header: the page dots on the right edge do the navigating
    } else if crate::ui::look_tuna() {
        crate::tuna::panel(ui, full);
        crate::tuna::strip(ui, full, modules, started, tabs, page);
    } else if crate::ui::look_bloom() {
        crate::bloom::header(ui, full, modules, tabs, page, indicators);
    } else {
        // header: tabs on the left, status and the settings gear on the right
        let ts = 32.0;
        let hy = full.y + (HEADER_H - ts) * 0.5 - 2.0;
        let mut x = full.x + PAD;
        // bare icons; the open tab is bright with a short lit beam under it
        let tab_style = |selected: bool| ButtonStyle {
            fg: if selected { Color::white(1.0) } else { Color::white(0.50) },
            bg: Color::white(0.0),
            hover_bg: Color::white(0.0),
            radius: 999.0,
            icon_scale: 0.54,
        };
        let avail = full.w - 2.0 * PAD - 200.0;
        let tab = ((avail + 4.0) / tabs.len().max(1) as f32 - 4.0).clamp(22.0, ts);
        for (id, icon, _title) in tabs {
            let sel = *id == page;
            let r = Rect::new(x, hy + (ts - tab) * 0.5, tab, tab);
            x += tab + 4.0;
            let hover = ui.hovered(r);
            if hover {
                ui.hot = true;
            }
            let lit = ui.glide(gkey(id, 11), if sel { 1.0 } else { 0.0 });
            let fg = Color::white(if hover { 0.85 } else { 0.50 }).lerp(Color::white(1.0), lit);
            ui.p.icon(*icon, r.center_square(ts * 0.52), fg);
            if lit > 0.02 {
                let bw = tab * 0.62 * lit;
                ui.underline(Rect::new(r.cx() - bw * 0.5, hy + ts + 1.0, bw, 3.0), Color::white(0.9 * lit));
            }
            if !sel && ui.clicked(r) {
                ui.fx.open_page = Some(id);
            }
        }
        // scroll the wheel over the header to flip through the tabs
        let wheel = ui.wheel(Rect::new(full.x, full.y, full.w, HEADER_H));
        if wheel != 0.0 && !tabs.is_empty() {
            let ready = WHEEL_FLIP.with(|c| c.get().is_none_or(|t| t.elapsed() > Duration::from_millis(450)));
            if ready {
                let cur = tabs.iter().position(|t| t.0 == page);
                let next = match cur {
                    Some(i) if wheel < 0.0 => (i + 1).min(tabs.len() - 1),
                    Some(i) => i.saturating_sub(1),
                    // e.g. on the settings page: jump back into the tabs
                    None => 0,
                };
                if cur != Some(next) {
                    ui.fx.open_page = Some(tabs[next].0);
                }
                WHEEL_FLIP.with(|c| c.set(Some(Instant::now())));
            }
        }
        // status (right): the settings gear sits at the far end, next to the battery
        let mut rx = full.right() - PAD;
        if modules.iter().enumerate().any(|(i, m)| started[i] && m.id() == "settings") {
            let g = Rect::new(rx - ts, hy, ts, ts);
            if ui.icon_button(g, Icon::Settings, tab_style(page == "settings")) {
                ui.fx.open_page = Some("settings");
            }
            if page == "settings" {
                ui.underline(Rect::new(g.cx() - ts * 0.31, hy + ts + 1.0, ts * 0.62, 3.0), Color::white(0.9));
            }
            rx = g.x - 12.0;
        }
        for ind in indicators.iter().rev() {
            if let Some((level, charging)) = ind.battery {
                let pct = format!("{:.0}%", level * 100.0);
                let st = TextStyle::new(12.5, palette::TEXT_DIM).medium().right();
                let (pw, _) = ui.p.measure(&pct, &st);
                let br = Rect::new(rx - 30.0, hy + ts * 0.5 - 7.0, 30.0, 14.0);
                icons::battery(ui.p, br, level, charging, battery_color(level, charging));
                let mut tx = br.x - 8.0;
                // time left (or an infinity sign while plugged in), always in blue
                if let Some(extra) = &ind.label {
                    let blue = palette::BLUE;
                    if extra == "\u{221E}" {
                        let (cx, cy, a) = (tx - 11.0, hy + ts * 0.5, 9.0);
                        let pt = |t: f32| {
                            let d = 1.0 + t.sin() * t.sin();
                            (cx + a * t.cos() / d, cy + a * t.sin() * t.cos() / d)
                        };
                        let n = 28;
                        for k in 0..n {
                            let t0 = k as f32 / n as f32 * std::f32::consts::TAU;
                            let t1 = (k + 1) as f32 / n as f32 * std::f32::consts::TAU;
                            let (x0, y0) = pt(t0);
                            let (x1, y1) = pt(t1);
                            ui.p.line(x0, y0, x1, y1, 1.9, blue);
                        }
                        tx -= 28.0;
                    } else {
                        let es = TextStyle::new(12.5, blue).medium().right();
                        let (ew, _) = ui.p.measure(extra, &es);
                        ui.p.text(extra, Rect::new(tx - ew - 2.0, hy, ew + 2.0, ts), &es);
                        tx -= ew + 12.0;
                    }
                }
                ui.p.text(&pct, Rect::new(tx - pw - 2.0, hy, pw + 2.0, ts), &st);
                rx = tx - pw - 14.0;
            } else if let Some(icon) = ind.icon {
                let r = Rect::new(rx - 22.0, hy + (ts - 22.0) * 0.5, 22.0, 22.0);
                ui.p.icon(icon, r.inset(3.0), ind.color);
                rx -= 28.0;
            }
        }
    }

    let content = Rect::new(
        full.x + side_pad(),
        full.y + top_h(),
        full.w - side_pad() - right_pad(),
        full.h - top_h() - bottom_pad(),
    );
    let Some((from, dir, t_in)) = trans else {
        draw_body(ui, content, modules, started, page);
        return;
    };
    // (Alcove with the soft pictures: `t_in` is seconds since the switch; otherwise the spring's 0..1)
    let measured = crate::ui::look_alcove() && ui.cfg.appearance.motion_cache;
    let t = if measured { (t_in / (PAGE_SECS * ui.cfg.appearance.page_time)).clamp(0.0, 1.0) } else { t_in };
    // the page that is leaving keeps its own width and height
    let from_size = page_size(ui.cfg, modules, started, from);
    let from_content = page_content(full.cx(), full.y, from_size);
    // One page is drawn at a time: the old one slides a little and fades out, then the new one
    // slides in and fades up. (Drawing both at once doubled the work for no visible gain.)
    const SLIDE: f32 = 16.0;
    let base = ui.p.alpha;
    let was_interactive = ui.interactive;
    let wide = full.w.max(from_size.0);
    let clip = Rect::new(full.cx() - wide * 0.5, content.y - 4.0, wide, content.h.max(from_content.h) + 8.0);
    ui.p.push_clip(clip);
    // Alcove: both pages blur and fade where they stand, timed from a reference recording
    if let (true, Some((old, new))) = (measured, snaps) {
        let (old_l, new_l) = page_layers(t_in, ui.cfg.appearance.page_time);
        if let Some(l) = old_l {
            ui.p.alpha = base * l.alpha;
            ui.interactive = false;
            ui.p.snapshot_blurred(old, 0.0, 0.0, 1.0);
        }
        ui.p.alpha = base * new_l.alpha;
        ui.interactive = false;
        ui.p.snapshot_blurred(new, 0.0, 0.0, 1.0);
        ui.p.alpha = base;
        ui.interactive = was_interactive && t_in > 0.27 * PAGE_T * ui.cfg.appearance.page_time;
        ui.p.pop_clip();
        return;
    }
    // Setting "Blur between pages": the old page blurs out where it is, the new one blurs in
    if let (true, Some((old, new))) = (ui.cfg.appearance.page_blur, snaps) {
        if t < 0.55 {
            let u = t / 0.55;
            ui.p.alpha = base * (1.0 - smoothstep(0.45, 1.0, u));
            ui.interactive = false;
            let b = smoothstep(0.0, 0.75, u);
            snapshot_motion(ui, old, 0.0, 0.0, b);
        }
        if t >= 0.36 {
            let u = (t - 0.36) / 0.64;
            ui.p.alpha = base * smoothstep(0.0, 0.4, u);
            ui.interactive = was_interactive && u > 0.75;
            let b = 1.0 - smoothstep(0.0, 0.85, u);
            snapshot_motion(ui, new, 0.0, 0.0, b);
        }
        ui.p.alpha = base;
        ui.interactive = was_interactive;
        ui.p.pop_clip();
        return;
    }
    // Alcove: the pages move the way the wheel does (up and down) and smear along that
    // direction while they move, like a quick scroll
    let al = crate::ui::look_alcove();
    let slide = if al { 28.0 } else { SLIDE };
    // Alcove, with the pictures ready: the new page starts rising while the old one is still
    // fading, so there is never an empty moment between the two
    let overlap = al && snaps.is_some();
    let (old_end, new_start) = if overlap { (0.46, 0.38) } else { (0.5, 0.22) };
    if t < old_end {
        let u = t / old_end;
        let e = smoothstep(0.0, 1.0, u);
        // overlapping: the old page drops away fast, so two similar pages never show as a double image
        ui.p.alpha = base * if overlap { (1.0 - e) * (1.0 - e) } else { 1.0 - e };
        ui.interactive = false;
        let (dx, dy) = if al { (0.0, (-dir * slide * e).round()) } else { ((-dir * slide * e).round(), 0.0) };
        match snaps {
            Some((old, _)) if al => snapshot_depth(ui, old, 1.0 - 0.04 * e, -dir * slide * 0.6 * e, 0.8 * e),
            Some((old, _)) => ui.p.snapshot(old, dx, dy, 1.0),
            None => draw_body(ui, from_content.translate(dx, dy), modules, started, from),
        }
    }
    if t >= new_start {
        let u = (t - new_start) / (1.0 - new_start);
        // Alcove: the new page is already clearly there while it is still being stretched
        ui.p.alpha = base * if al { smoothstep(0.0, 0.4, u) } else { smoothstep(0.0, 0.45, u) };
        ui.interactive = was_interactive && u > 0.6;
        let e = 1.0 - smoothstep(0.0, 1.0, u);
        let (dx, dy) = if al { (0.0, (dir * slide * e).round()) } else { ((dir * slide * e).round(), 0.0) };
        match snaps {
            // Alcove: the new page rises into place from a little larger and further down, goes a
            // touch past and settles, a soft blur clearing as it lands
            Some((_, new)) if al => {
                let c = 0.6_f32;
                let back = 1.0 + (c + 1.0) * (u - 1.0).powi(3) + c * (u - 1.0).powi(2);
                let k = 1.0 - back;
                snapshot_depth(ui, new, 1.0 + 0.04 * k, dir * slide * 1.2 * k, 0.8 * k.max(0.0));
            }
            Some((_, new)) => ui.p.snapshot(new, dx, dy, 1.0),
            None => draw_body(ui, content.translate(dx, dy), modules, started, page),
        }
    }
    ui.p.alpha = base;
    ui.interactive = was_interactive;
    ui.p.pop_clip();
}

/// A page picture drawn between sharp and soft: `blur` 0 is the crisp picture, 1 the blurred
/// copy. The two are cross-faded, so the page seems to lose or gain focus as it moves.
/// A page picture scaled about its centre and shifted, drawn between sharp and soft like
/// `snapshot_motion`: one solid piece, so nothing can look cut while it moves.
fn snapshot_depth(ui: &mut Ui, s: &Snapshot, scale: f32, dy: f32, blur: f32) {
    let dy = dy.round();
    if blur < 0.03 {
        ui.p.snapshot_scaled(s, scale, 0.0, dy, 1.0, false);
    } else {
        ui.p.snapshot_scaled(s, scale, 0.0, dy, 1.0 - blur, false);
        ui.p.snapshot_scaled(s, scale, 0.0, dy, blur, true);
    }
}

fn snapshot_motion(ui: &mut Ui, s: &Snapshot, dx: f32, dy: f32, blur: f32) {
    if blur < 0.03 {
        ui.p.snapshot(s, dx, dy, 1.0);
    } else {
        ui.p.snapshot(s, dx, dy, 1.0 - blur);
        ui.p.snapshot_soft(s, dx, dy, blur);
    }
}

fn draw_body(ui: &mut Ui, content: Rect, modules: &mut [Box<dyn Module>], started: &[bool], page: ModuleId) {
    if page == HOME && crate::ui::look_alcove() {
        crate::alcove::home(ui, content, modules);
    } else if page == HOME && crate::ui::look_tuna() {
        let large = (0..modules.len()).find(|&i| started[i] && modules[i].card() == Some(CardSize::Large));
        let tiles = home_tiles(modules, started, ui.cfg);
        crate::tuna::home(ui, content, modules, large, &tiles);
    } else if page == HOME && crate::ui::look_bloom() {
        crate::bloom::control_center(ui, content, modules, started);
    } else if page == HOME {
        draw_home(ui, content, modules, started);
    } else if let Some(i) = modules.iter().position(|m| m.id() == page) {
        modules[i].draw_page(ui, content);
    }
}

/// The widgets to show in the right-hand column of the home page: the ones
/// chosen in `[home] widgets` (Settings → Home) that are available right now,
/// at most `HOME_TILES`. An empty list keeps the old behaviour: the first
/// modules with a small card. The clock is not a choice any more: it always
/// lives on the left.
fn home_tiles(modules: &[Box<dyn Module>], started: &[bool], cfg: &Config) -> Vec<usize> {
    let small = |i: usize| started[i] && modules[i].card() == Some(CardSize::Small);
    if cfg.home.widgets.is_empty() {
        return (0..modules.len()).filter(|&i| small(i)).take(HOME_TILES).collect();
    }
    let mut out = Vec::new();
    for id in &cfg.home.widgets {
        if out.len() >= HOME_TILES {
            break;
        }
        // Wi-Fi and Bluetooth are one module now; older configs still say "wifi" / "bluetooth"
        let id = if matches!(id.as_str(), "wifi" | "bluetooth") { "control" } else { id.as_str() };
        if let Some(i) = modules.iter().position(|m| m.id() == id) {
            if small(i) && !out.contains(&i) {
                out.push(i);
            }
        }
    }
    out
}

/// Where the small round Mirror button sits on the home page's clock card.
fn draw_home(ui: &mut Ui, r: Rect, modules: &mut [Box<dyn Module>], started: &[bool]) {
    let large = (0..modules.len()).find(|&i| started[i] && modules[i].card() == Some(CardSize::Large));
    let tiles = home_tiles(modules, started, ui.cfg);
    let (left, right) = if tiles.is_empty() { (r, Rect::default()) } else { r.split_left((r.w * 0.56).round(), 16.0) };
    // flat columns on black, separated by hairlines
    let line = Color::white(0.09);
    match large {
        // something is playing: the player on top, the clock in its own strip below
        Some(i) => {
            let (media, clock) = left.split_top((left.h - CLOCK_STRIP_H - 10.0).max(60.0), 10.0);
            modules[i].draw_card(ui, media);
            ui.p.fill_rect(Rect::new(left.x + 6.0, clock.y - 5.0, left.w - 12.0, 1.0), line);
            draw_clock_strip(ui, clock);
        }
        None => draw_clock_card(ui, left),
    }
    if !tiles.is_empty() {
        ui.p.fill_rect(Rect::new(left.right() + 8.0, r.y + 6.0, 1.0, r.h - 12.0), line);
        let n = tiles.len() as f32;
        let gap = 10.0;
        let ch = (right.h - gap * (n - 1.0)) / n;
        for (k, &i) in tiles.iter().enumerate() {
            let cr = Rect::new(right.x, right.y + k as f32 * (ch + gap), right.w, ch);
            modules[i].draw_card(ui, cr);
            if k > 0 {
                ui.p.fill_rect(Rect::new(right.x + 6.0, cr.y - gap * 0.5, right.w - 12.0, 1.0), line.alpha(0.8));
            }
        }
    }
}

/// Local time pieces for the flip clock: (hour, minute, second, "AM"/"PM" or "", date text).
pub(crate) fn clock_parts() -> (u8, u8, u8, String, String) {
    let st = unsafe { GetLocalTime() };
    // the formatted time and the date text only change when the minute does: work them out once per minute
    thread_local! {
        static FMT: std::cell::RefCell<Option<((u16, u16, u16), String, String)>> = const { std::cell::RefCell::new(None) };
    }
    let key = (st.wDay, st.wHour, st.wMinute);
    let cached = FMT.with(|c| c.borrow().as_ref().filter(|e| e.0 == key).map(|e| (e.1.clone(), e.2.clone())));
    let (suffix, date) = match cached {
        Some(v) => v,
        None => {
            let mut tbuf = [0u16; 64];
            let tn =
                unsafe { GetTimeFormatEx(PCWSTR::null(), TIME_NOSECONDS, Some(&st), PCWSTR::null(), Some(&mut tbuf)) };
            let time = String::from_utf16_lossy(&tbuf[..(tn.max(1) - 1) as usize]);
            // "Friday 26th": the day of the week and the date, without the month
            let day = st.wDay;
            let ord = match (day % 10, day % 100) {
                (_, 11..=13) => "th",
                (1, _) => "st",
                (2, _) => "nd",
                (3, _) => "rd",
                _ => "th",
            };
            let weekday = crate::util::WEEKDAY_NAMES[(st.wDayOfWeek as usize).min(6)];
            let date = format!("{weekday} {day}{ord}");
            // a letter in the formatted time means a 12 hour clock (AM / PM)
            let suffix: String = time.chars().filter(|c| c.is_alphabetic()).collect();
            FMT.with(|c| *c.borrow_mut() = Some((key, suffix.clone(), date.clone())));
            (suffix, date)
        }
    };
    let h = if suffix.is_empty() {
        st.wHour as u8
    } else {
        match st.wHour % 12 {
            0 => 12,
            h => h as u8,
        }
    };
    (h, st.wMinute as u8, st.wSecond as u8, suffix, date)
}

/// Flip clock under the media player: digits on the left, date on the right.
fn draw_clock_strip(ui: &mut Ui, r: Rect) {
    ui.card(r, false);
    let (h, m, s, suffix, date) = clock_parts();
    let inner = r.inset_xy(14.0, 0.0);
    let th = (r.h - 14.0).clamp(24.0, 40.0);
    let tw = th * 0.72;
    let used = ui.clock(inner.x, r.cy() - th * 0.5, tw, th, h, m, s, 100);
    let text = if suffix.is_empty() { date } else { format!("{suffix} · {date}") };
    ui.p.text(
        &text,
        Rect::new(inner.x + used + 12.0, r.y, (inner.w - used - 12.0).max(0.0), r.h),
        &TextStyle::new(12.0, palette::TEXT_DIM).right(),
    );
}

fn draw_clock_card(ui: &mut Ui, r: Rect) {
    ui.card(r, false);
    let (h, m, s, suffix, _date) = clock_parts();
    let inner = r.inset_xy(18.0, 14.0);
    // size the tiles so six digits and the gaps fill the card (but never get huge)
    let tw = ((inner.w - 24.0) / 6.6).clamp(26.0, 50.0);
    let th = (tw * 1.28).min(inner.h - 62.0).max(34.0);
    let used = if crate::ui::look_nothing() {
        draw_dot_clock(ui, Rect::new(inner.x, inner.y, inner.w, th), h, m)
    } else {
        ui.clock(inner.x, inner.y, tw, th, h, m, s, 0)
    };
    if !suffix.is_empty() {
        ui.p.text(
            &suffix,
            Rect::new(inner.x + used + 8.0, inner.y + th - 20.0, 40.0, 20.0),
            &TextStyle::new(12.0, palette::TEXT_DIM),
        );
    }
    // the calendar strip sits under the time
    let top = inner.y + th + 10.0;
    let strip = Rect::new(inner.x, top, inner.w, (inner.bottom() - top).clamp(30.0, 48.0));
    draw_calendar(ui, strip);
}

/// 5x7 dot-matrix digits, one row per byte (bit 4 = leftmost column).
const DOT_DIGITS: [[u8; 7]; 10] = [
    [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
    [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
];

/// The time as a dot-matrix display (the Nothing look): white dots, a red colon, faint unlit dots.
/// Returns the width used.
fn draw_dot_clock(ui: &mut Ui, area: Rect, h: u8, m: u8) -> f32 {
    // 4 digits of 5 columns, a 1-column gap between digits, and a 3-column colon block
    const COLS: f32 = 27.0;
    let pitch = (area.h / 7.0).min(area.w / COLS).max(3.0);
    let dot = pitch * 0.34;
    let top = area.y + (area.h - pitch * 7.0).max(0.0) * 0.5;
    let digits = [h / 10 % 10, h % 10, m / 10 % 10, m % 10];
    let mut col = 0.0;
    for (k, d) in digits.iter().enumerate() {
        if k == 2 {
            // the colon: two red dots
            let cx = area.x + (col + 1.5) * pitch;
            ui.p.fill_circle(cx, top + 2.5 * pitch, dot, crate::ui::NOTHING_RED);
            ui.p.fill_circle(cx, top + 4.5 * pitch, dot, crate::ui::NOTHING_RED);
            col += 3.0 + 1.0;
        }
        let rows = &DOT_DIGITS[*d as usize];
        for (ry, bits) in rows.iter().enumerate() {
            for cx in 0..5 {
                let on = bits & (0b10000 >> cx) != 0;
                let c = if on { Color::white(0.96) } else { Color::white(0.07) };
                ui.p.fill_circle(area.x + (col + cx as f32 + 0.5) * pitch, top + (ry as f32 + 0.5) * pitch, dot, c);
            }
        }
        col += 5.0 + 1.0;
    }
    (col - 1.0) * pitch
}

thread_local! {
    /// Days the calendar strip is moved from today (0 = today is selected).
    static CAL_OFF: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const DAY_LETTER: [&str; 7] = ["S", "M", "T", "W", "T", "F", "S"];
const DAY_SHORT: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

/// Days since 1970-01-01 for a calendar date (proleptic Gregorian).
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = (if m <= 2 { y - 1 } else { y }) as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The calendar date (year, month 1..=12, day) for a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    ((y + i64::from(m <= 2)) as i32, m, d)
}

/// 0 = Sunday for a day count since 1970-01-01 (a Thursday).
fn weekday_of(z: i64) -> usize {
    (z + 4).rem_euclid(7) as usize
}

/// A week strip: the month on the left, then days that scroll with the mouse wheel (or a click).
/// The chosen day is blue with a lit beam under it that glides along a rail of beams.
fn draw_calendar(ui: &mut Ui, area: Rect) {
    let st = unsafe { GetLocalTime() };
    let today = days_from_civil(st.wYear as i32, st.wMonth as u32, st.wDay as u32);
    let mut off = CAL_OFF.with(|c| c.get());

    // wheel over the strip moves through the days (down = later)
    if ui.hovered(area) {
        let wheel = ui.input.wheel;
        if wheel.abs() > 0.01 {
            let step = (wheel.abs().round() as i32).max(1);
            off += if wheel < 0.0 { step } else { -step };
        }
    }

    let month_w = 54.0;
    let days = Rect::new(area.x + month_w, area.y, (area.w - month_w).max(60.0), area.h);
    let cw = days.w / 6.0;
    let blue = Color::hex(0x3D95FF);

    // the month of the chosen day; a click on it jumps back to today
    let (_, month, _) = civil_from_days(today + off as i64);
    let label = Rect::new(area.x, area.y, month_w, area.h);
    ui.p.text(
        MONTHS[(month as usize - 1).min(11)],
        Rect::new(area.x, area.y + 2.0, month_w, 28.0),
        &TextStyle::new(22.0, Color::white(0.98)).semibold().display(),
    );
    if off != 0 {
        ui.p.text("today", Rect::new(area.x, area.y + 28.0, month_w, 14.0), &TextStyle::new(10.5, palette::TEXT_FAINT));
        if ui.clicked(label) {
            off = 0;
        }
    }

    let scroll = ui.glide(gkey("cal-scroll", 0), off as f32);
    let base_alpha = ui.p.alpha;
    ui.p.push_clip(days);
    let first = scroll.floor() as i32 - 3;
    for k in 0..=10 {
        let d = first + k;
        let idx = 2.0 + (d as f32 - scroll);
        // fade out at both ends of the strip
        let fade = (idx + 1.0).clamp(0.0, 1.0) * (6.0 - idx).clamp(0.0, 1.0);
        if fade <= 0.01 {
            continue;
        }
        let cell = Rect::new(days.x + idx * cw, area.y, cw, area.h - 8.0);
        let z = today + d as i64;
        let (_, _, dd) = civil_from_days(z);
        let wd = weekday_of(z);
        // how close this day is to being the chosen one (smooth while scrolling)
        let near = (1.0 - (d as f32 - scroll).abs()).clamp(0.0, 1.0);
        let body = if d < 0 { Color::white(0.50) } else { Color::white(0.90) };
        let col = body.lerp(blue, near);
        ui.p.alpha = base_alpha * fade;
        let name = if near > 0.5 { DAY_SHORT[wd] } else { DAY_LETTER[wd] };
        ui.p.text(
            name,
            Rect::new(cell.x, cell.y + 2.0, cell.w, 12.0),
            &TextStyle::new(9.5, Color::white(0.50).lerp(blue, near)).medium().center(),
        );
        ui.p.text(
            &dd.to_string(),
            Rect::new(cell.x, cell.y + 14.0, cell.w, 22.0),
            &TextStyle::new(16.0, col).semibold().display().center(),
        );
        if ui.clicked(cell) {
            off = d;
        }
    }
    ui.p.alpha = base_alpha;
    ui.p.pop_clip();

    // a rail of beams under the days; the lit stretch follows the chosen day
    let rail = Rect::new(days.x, area.bottom() - 5.0, days.w, 4.0);
    let cx = days.x + (2.5 + (off as f32 - scroll)) * cw;
    let from = ((cx - cw * 0.34 - days.x) / days.w).clamp(0.0, 1.0);
    let to = ((cx + cw * 0.34 - days.x) / days.w).clamp(0.0, 1.0);
    ui.tick_range(rail, from, to, blue);

    CAL_OFF.with(|c| c.set(off.clamp(-3650, 3650)));
}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

unsafe extern "system" fn win_event_cb(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if event == EVENT_OBJECT_LOCATIONCHANGE
        && (id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 || hwnd != GetForegroundWindow())
    {
        return;
    }
    let h = NOTCH_HWND.load(Ordering::Acquire);
    if h != 0 {
        let _ = PostMessageW(Some(HWND(h as *mut _)), WM_APP_FOREGROUND, WPARAM(event as usize), LPARAM(0));
    }
}

unsafe fn is_fullscreen_on(mon: HMONITOR, own: HWND) -> bool {
    let fg = GetForegroundWindow();
    if fg.is_invalid() || fg == own {
        return false;
    }
    let mut cls = [0u16; 64];
    let n = GetClassNameW(fg, &mut cls);
    let class = String::from_utf16_lossy(&cls[..n.max(0) as usize]);
    // Desktop, taskbar and shell overlays (Start, Alt+Tab, Task View, flyouts)
    // cover the screen but are not fullscreen apps.
    if matches!(
        class.as_str(),
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "XamlExplorerHostIslandWindow"
            | "Windows.UI.Core.CoreWindow"
            | "MultitaskingViewFrame"
            | "TaskSwitcherWnd"
            | "ForegroundStaging"
            | "TopLevelWindowForOverflowXamlIsland"
    ) {
        return false;
    }
    if MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) != mon {
        return false;
    }
    // The shell's own verdict covers D3D exclusive fullscreen and presentations.
    if let Ok(state) = SHQueryUserNotificationState() {
        if state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE {
            return true;
        }
    }
    // A maximized window is never "fullscreen" (matters with an auto-hide taskbar).
    if IsZoomed(fg).as_bool() {
        return false;
    }
    let mut wr = RECT::default();
    if GetWindowRect(fg, &mut wr).is_err() {
        return false;
    }
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if !GetMonitorInfoW(mon, &mut mi).as_bool() {
        return false;
    }
    let m = mi.rcMonitor;
    let style = GetWindowLongW(fg, GWL_STYLE) as u32;
    let has_caption = style & WS_CAPTION.0 == WS_CAPTION.0;
    let fs = wr.left <= m.left && wr.top <= m.top && wr.right >= m.right && wr.bottom >= m.bottom && !has_caption;
    if fs {
        crate::log!("fullscreen window: class={class} rect=({},{},{},{})", wr.left, wr.top, wr.right, wr.bottom);
    }
    fs
}

fn monitors() -> Vec<HMONITOR> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> windows::core::BOOL {
        let v = &mut *(lp.0 as *mut Vec<HMONITOR>);
        v.push(m);
        true.into()
    }
    let mut v: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn translate_key(vk: u32) -> Option<Key> {
    let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
    let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
    let vk = VIRTUAL_KEY(vk as u16);
    Some(match vk {
        VK_RETURN => Key::Enter { shift },
        VK_BACK => Key::Backspace,
        VK_DELETE => Key::Delete,
        VK_LEFT => Key::Left,
        VK_RIGHT => Key::Right,
        VK_HOME => Key::Home,
        VK_END => Key::End,
        VK_UP => Key::Up,
        VK_DOWN => Key::Down,
        VK_ESCAPE => Key::Escape,
        VK_TAB => Key::Tab,
        _ if ctrl && vk == VIRTUAL_KEY(b'A' as u16) => Key::SelectAll,
        _ if ctrl && vk == VIRTUAL_KEY(b'C' as u16) => Key::Copy,
        _ if ctrl && vk == VIRTUAL_KEY(b'V' as u16) => Key::Paste,
        _ if ctrl && vk == VIRTUAL_KEY(b'X' as u16) => Key::Cut,
        _ => return None,
    })
}

/// Banner for `dynamic-notch claude <event> <project> <message> <hwnd>` (see claude.rs).
fn claude_peek(args: &[String]) -> Peek {
    const CLAUDE: Color = Color::hex(0xD97757);
    let arg = |i: usize| args.get(i).map(|s| s.trim()).unwrap_or("");
    let (event, project, message) = (arg(0).to_ascii_lowercase(), arg(1), arg(2));
    let hwnd = arg(3).parse::<isize>().unwrap_or(0);
    let join = |a: &str, b: &str| match (a.is_empty(), b.is_empty()) {
        (false, false) => format!("{a} · {b}"),
        _ => format!("{a}{b}"),
    };
    let peek = if matches!(event.as_str(), "notification" | "attention" | "permission") {
        let msg = if message.is_empty() { "Waiting for your input" } else { message };
        Peek::new(Icon::Bell, CLAUDE, "Claude needs you", join(project, msg)).duration_ms(7000)
    } else {
        let sub = if project.is_empty() { "Ready for your next message".to_string() } else { project.to_string() };
        Peek::new(Icon::Sparkle, CLAUDE, "Claude finished", sub).duration_ms(4500)
    };
    peek.key("claude").focus(hwnd)
}

/// Map an IPC page name onto a static module id.
fn leak_page(p: &str) -> ModuleId {
    match p {
        "media" => "media",
        "timer" => "timer",
        "clipboard" => "clipboard",
        "downloads" => "downloads",
        "ai" => "ai",
        "voice" => "voice",
        "shelf" => "shelf",
        "ports" => "ports",
        "mirror" => "mirror",
        _ => HOME,
    }
}

/// Drive letter and direction ("in" / "out") of a volume arrival or removal.
fn usb_event(wp: u32, lp: isize) -> Option<Vec<String>> {
    const ARRIVAL: u32 = 0x8000;
    const REMOVED: u32 = 0x8004;
    if (wp != ARRIVAL && wp != REMOVED) || lp == 0 {
        return None;
    }
    // DEV_BROADCAST_VOLUME { size, device type (2 = volume), reserved, unit mask, flags }
    let hdr = lp as *const u32;
    let kind = unsafe { *hdr.add(1) };
    if kind != 2 {
        return None;
    }
    let mask = unsafe { *hdr.add(3) };
    if mask == 0 {
        return None;
    }
    let letter = (b'A' + mask.trailing_zeros() as u8) as char;
    let dir = if wp == ARRIVAL { "in" } else { "out" };
    Some(vec![format!("{letter}:"), dir.to_string()])
}

#[cfg(test)]
mod calendar_tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        for (y, m, d) in [(2000, 2, 29), (2024, 12, 31), (2026, 10, 26), (1999, 3, 1)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
        // 1970-01-01 was a Thursday, 2026-10-26 is a Monday
        assert_eq!(weekday_of(0), 4);
        assert_eq!(weekday_of(days_from_civil(2026, 10, 26)), 1);
    }
}


/// Alcove music: the clear space between the notch and each of its two side bubbles.
const SPLIT_GAP: f32 = 14.0;
