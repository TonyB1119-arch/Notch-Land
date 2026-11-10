//! Module system.
//!
//! A module is a self-contained feature (media, timer, battery, ...).  It
//! lives on the UI thread, receives messages from its own worker threads via
//! the `Bus`, and contributes UI in up to four places:
//!
//! * `activity()`    – a compact "live activity" shown inside the collapsed notch
//! * `indicators()`  – tiny status dots/icons (mic, camera, battery...)
//! * home card       – a tile on the expanded home page
//! * page            – a full tab inside the expanded notch
//!
//! Adding a module = implement `Module`, add it to `all()` below.

pub mod ai;
pub mod alcove_launcher;
pub mod mixer;
pub mod battery;
pub mod bluetooth;
pub mod calendar;
pub mod clipboard;
pub mod control;
pub mod traylist;
pub mod devices;
pub mod downloads;
pub mod gemini;
pub mod greeting;
pub mod locks;
pub mod media;
pub mod mirror;
pub mod notes;
pub mod notifications;
pub mod osd;
pub mod ports;
pub mod privacy;
pub mod settings;
pub mod shelf;
pub mod stats;
pub mod timer;
pub mod voice;
pub mod weather;
pub mod widgets;
pub mod wifi;

use std::any::Any;
use std::time::{Duration, Instant};

use crate::bus::Bus;
use crate::config::Config;
use crate::gfx::{Icon, ImageData};
use crate::ui::Ui;
use crate::util::{Color, Rect, SendHwnd};

pub type ModuleId = &'static str;

/// Content for one side of the compact live activity.
#[allow(dead_code)] // part of the module API, not every variant is used yet
#[derive(Clone, Debug)]
pub enum Slot {
    Empty,
    Image(ImageData),
    Icon(Icon, Color),
    Text(String, Color),
    /// Audio visualizer (animated by the compositor).
    Bars {
        color: Color,
        playing: bool,
    },
    /// Circular progress 0..1 (None = indeterminate spinner).
    Ring(Option<f32>, Color),
    Battery {
        level: f32,
        charging: bool,
    },
    /// A black record; the artwork (when there is some) is its label.
    Vinyl {
        color: Color,
        angle: f32,
        art: Option<ImageData>,
        /// Alcove: a track change flips the cover over; progress 0..1 and the old cover.
        flip: Option<(f32, Option<ImageData>)>,
    },
    /// Live level meter (recent 0..1 levels, newest last), drawn across the wing.
    Wave {
        levels: Vec<f32>,
        color: Color,
    },
}

#[derive(Clone, Debug)]
pub struct Activity {
    pub priority: i32,
    pub left: Slot,
    pub right: Slot,
    /// Extra width of each side "wing" in units of notch height (1.0 = square).
    pub wing: f32,
    /// Page to open when the activity is clicked.
    pub page: Option<ModuleId>,
}

/// Small status item: shown as a colored dot inside the collapsed notch
/// (when `dot`) and as icon/label in the expanded header.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Indicator {
    pub color: Color,
    pub icon: Option<Icon>,
    pub label: Option<String>,
    pub battery: Option<(f32, bool)>,
    pub dot: bool,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum Trailing {
    None,
    Text(String, Color),
    Ring(f32, Color),
    Battery {
        level: f32,
        charging: bool,
    },
    /// Slider-style level bar under the title with the percentage on the
    /// right (volume / brightness). `None` text shows the percentage.
    Level {
        frac: f32,
        color: Color,
        text: Option<String>,
    },
}

/// A transient banner ("peek") the notch expands into for a few seconds.
#[derive(Clone, Debug)]
pub struct Peek {
    pub icon: Icon,
    pub accent: Color,
    pub image: Option<ImageData>,
    pub title: String,
    pub subtitle: String,
    pub trailing: Trailing,
    pub duration: Duration,
    /// Page opened when the peek is clicked.
    pub page: Option<ModuleId>,
    /// Replace any queued/visible peek with the same key instead of stacking.
    pub key: Option<&'static str>,
    /// Window brought to the front when the peek is clicked (instead of `page`).
    pub focus: Option<isize>,
    /// Show right away, pushing a visible peek back into the queue
    /// (feedback for a key press is useless when it arrives late).
    pub instant: bool,
    /// Draw the icon on its own: no coloured circle behind it.
    #[allow(dead_code)] // kept for module code; every peek now uses the plain-icon look
    pub bare: bool,
    /// Face-ID style unlock animation (blue brackets, tick ring, check).
    pub scan: bool,
    /// A follow-up for a peek that is already showing: the visible peek swaps to
    /// this content with a short animation, and nothing new pops up when none is showing.
    pub quiet: bool,
}

impl Peek {
    /// Volume / brightness style: icon and value on top, a tick bar below.
    pub fn is_hud(&self) -> bool {
        matches!(self.trailing, Trailing::Level { .. })
    }
    /// Icon-only pill (no text), like the unlock animation.
    pub fn is_compact(&self) -> bool {
        self.title.is_empty()
            && self.subtitle.is_empty()
            && matches!(self.trailing, Trailing::None)
            && self.image.is_none()
    }

    pub fn new(icon: Icon, accent: Color, title: impl Into<String>, subtitle: impl Into<String>) -> Self {
        Self {
            icon,
            accent,
            image: None,
            title: title.into(),
            subtitle: subtitle.into(),
            trailing: Trailing::None,
            duration: Duration::from_millis(3200),
            page: None,
            key: None,
            focus: None,
            instant: false,
            bare: false,
            scan: false,
            quiet: false,
        }
    }
    /// Icon without a background circle (drawn in the accent colour).
    #[allow(dead_code)]
    pub fn bare(mut self) -> Self {
        self.bare = true;
        self
    }
    /// Play the unlock scan animation instead of a normal peek.
    pub fn scan(mut self) -> Self {
        self.scan = true;
        self
    }
    pub fn instant(mut self) -> Self {
        self.instant = true;
        self
    }
    pub fn focus(mut self, hwnd: isize) -> Self {
        self.focus = (hwnd != 0).then_some(hwnd);
        self
    }
    pub fn trailing(mut self, t: Trailing) -> Self {
        self.trailing = t;
        self
    }
    pub fn duration_ms(mut self, ms: u64) -> Self {
        self.duration = Duration::from_millis(ms);
        self
    }
    pub fn page(mut self, p: ModuleId) -> Self {
        self.page = Some(p);
        self
    }
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }
    pub fn key(mut self, k: &'static str) -> Self {
        self.key = Some(k);
        self
    }
    pub fn image(mut self, img: Option<ImageData>) -> Self {
        self.image = img;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardSize {
    /// Big tile on the left of the home page (media player).
    Large,
    /// One row in the right-hand column.
    Small,
}

/// Requests a module (or the UI) makes to the app shell.
#[derive(Default)]
pub struct Effects {
    pub peeks: Vec<Peek>,
    pub open_page: Option<ModuleId>,
    pub expand: bool,
    pub collapse: bool,
    pub redraw: bool,
    /// A click landed: the notch gives a little (the Alcove look's haptic feel).
    pub nudge: bool,
    /// Need another frame right away (caret blink, spinners...).
    pub animate: bool,
    /// Need more frames, but only a slow, gentle animation is running (a floating icon, a
    /// scanning spinner): half the frames are enough and nobody can tell the difference.
    pub animate_slow: bool,
    pub keyboard_focus: Option<bool>,
    pub layout_changed: bool,
    pub save_config: Vec<(String, String, String)>,
    /// Start an OLE drag of these files out of the notch.
    pub drag_files: Option<Vec<String>>,
    /// Read the config file again (the control center's refresh button).
    pub reload_config: bool,
    /// Move the output volume (0..1) / set the panel brightness (percent).
    pub set_volume: Option<f32>,
    pub set_brightness: Option<u8>,
}

impl Effects {
    pub fn peek(&mut self, p: Peek) {
        self.peeks.push(p);
    }
}

/// Context passed to non-drawing callbacks.
pub struct Cx<'a> {
    pub bus: &'a Bus,
    pub cfg: &'a Config,
    pub fx: &'a mut Effects,
    pub hwnd: SendHwnd,
    pub now: Instant,
}

/// Events the shell forwards to every module.
#[derive(Clone, Debug)]
pub enum SystemEvent {
    ClipboardChanged,
    PowerChanged,
    ConfigReloaded,
    /// The expanded notch opened/closed.
    Expanded(bool),
    /// A watched key went down/up anywhere in the system (raw input). Only
    /// lock keys and the dictation hotkey's key are forwarded; repeats are dropped.
    RawKey {
        vk: u16,
        down: bool,
    },
    /// Files are being dragged over the notch (true) or left it (false).
    DragHover(bool),
    /// Files were dropped onto the notch.
    Dropped(Vec<String>),
    /// A drag out of the notch ended (files may have moved).
    DragOutDone,
    /// A global hotkey a module registered on the notch window (`cx.hwnd`)
    /// fired. Ids 0x4E00..0x4F00 belong to the shell's own hotkeys.
    Hotkey(i32),
    /// Whether a peek pushed now would be seen changed (notch hidden,
    /// fullscreen app, or the notch is open). See `app::peeks_shown`.
    PeeksShown(bool),
    /// Text from the command line / IPC addressed to a module.
    Command {
        verb: String,
        args: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter { shift: bool },
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Up,
    Down,
    Escape,
    Tab,
    SelectAll,
    Copy,
    Paste,
    Cut,
}

#[allow(unused_variables)]
pub trait Module {
    fn id(&self) -> ModuleId;
    fn title(&self) -> &str;
    fn icon(&self) -> Icon;

    /// Called once after creation (spawn workers here).
    fn start(&mut self, cx: &mut Cx) {}
    /// Called on shutdown or when the module gets disabled.
    fn stop(&mut self) {}

    /// Message from this module's own worker threads.
    fn on_message(&mut self, msg: Box<dyn Any + Send>, cx: &mut Cx) {}
    fn on_system(&mut self, ev: &SystemEvent, cx: &mut Cx) {}

    /// Next instant this module wants `on_tick` (timers, countdowns...).
    fn next_tick(&self) -> Option<Instant> {
        None
    }
    fn on_tick(&mut self, cx: &mut Cx) {}

    /// Media is playing right now (used to decide whether the glass edge needs to stay live).
    fn is_playing(&self) -> bool {
        false
    }

    fn activity(&self) -> Option<Activity> {
        None
    }
    fn indicators(&self, out: &mut Vec<Indicator>) {}
    /// A tiny icon + text summary for the closed notch (Settings → Notch).
    fn glance(&self) -> Option<(Icon, String)> {
        None
    }
    /// Like `glance`, for modules that offer several (the control center: "wifi" or "bluetooth").
    fn glance_for(&self, id: &str) -> Option<(Icon, String)> {
        self.glance()
    }

    fn card(&self) -> Option<CardSize> {
        None
    }
    fn draw_card(&mut self, ui: &mut Ui, r: Rect) {}

    fn has_page(&self) -> bool {
        false
    }
    /// Preferred page content height for the given width.
    fn page_height(&self, width: f32) -> f32 {
        150.0
    }
    fn draw_page(&mut self, ui: &mut Ui, r: Rect) {}

    /// Keyboard input while this module's page has focus.
    fn wants_keyboard(&self) -> bool {
        false
    }
    fn on_key(&mut self, key: Key, cx: &mut Cx) {}
}

/// Factory for all built-in modules, in tab order.
pub fn all() -> Vec<Box<dyn Module>> {
    vec![
        Box::new(media::Media::new()),
        Box::new(timer::Timer::new()),
        Box::new(calendar::Calendar::new()),
        // right after the timer so its home tile isn't crowded out
        Box::new(stats::Stats::new()),
        Box::new(voice::Voice::new()),
        Box::new(clipboard::Clipboard::new()),
        Box::new(downloads::Downloads::new()),
        Box::new(shelf::Shelf::new()),
        Box::new(ports::Ports::new()),
        Box::new(ai::Ai::new()),
        Box::new(gemini::Gemini::new()),
        Box::new(alcove_launcher::AlcoveLauncher::new()),
        Box::new(mixer::Mixer::new()),
        Box::new(weather::Weather::new()),
        Box::new(control::ControlCenter::new()),
        Box::new(mirror::Mirror::new()),
        Box::new(privacy::Privacy::new()),
        Box::new(battery::Battery::new()),
        Box::new(locks::Locks::new()),
        Box::new(osd::Osd::new()),
        Box::new(notes::Notes::new()),
        Box::new(notifications::Notifications::new()),
        Box::new(devices::Devices::new()),
        Box::new(greeting::Greeting::new()),
        Box::new(settings::Settings::new()),
    ]
}
